//! Native backend: aggregate place / address resolution. Resolves struct-field and
//! array-index access paths to stack addresses for both the assignment (write) and
//! read sides. Split out of native_object_stmt.rs; shared items via
//! `use super::super::*`.
use super::super::*;

/// One hop of an aggregate access path: a struct field name or an array index
/// expression. Shared between statement-side (`Assign` path) and read-side
/// (`Field`/`Index` expression) resolution.
pub(crate) enum PathStep<'a> {
    Field(&'a str),
    Index(&'a BytecodeExpr),
}

/// Walk a root local plus a list of field/index steps down to a single scalar
/// word, accumulating a constant word offset and, if a runtime index is
/// encountered, deferring to a `Dynamic` place. A constant integer-literal index
/// folds into the constant offset (so `xs[2]` stays static); any other index
/// expression makes the place dynamic. The final layout must be `i64`.
pub(crate) fn resolve_place_steps(
    ctx: &NativeCtx,
    root: &str,
    steps: &[PathStep],
) -> Result<ScalarPlace, String> {
    // The strict i64-only resolver: every existing scalar/SIMD caller relies on
    // this rejecting a float element, so float arrays never reach the integer
    // load/store or the packed-integer SIMD detectors.
    let (place, ty) = resolve_place_steps_typed(ctx, root, steps)?;
    if ty != NativeType::I64 {
        return Err("native access must resolve to an i64 scalar".to_string());
    }
    Ok(place)
}

/// Like [`resolve_place_steps`] but also accepts an `f64`/`f32` final element and
/// returns the resolved element type, so the float read/store paths can pick
/// `movsd`/`movss`. Kept separate from the strict i64 resolver so the SIMD
/// detectors (which call the strict one) never fire on a float array.
pub(crate) fn resolve_place_steps_typed(
    ctx: &NativeCtx,
    root: &str,
    steps: &[PathStep],
) -> Result<(ScalarPlace, NativeType), String> {
    let local = ctx.local(root)?;
    let base_slot = local.slot;
    // A fat-pointer array parameter supports exactly one runtime index read
    // (`param[i]`); the address comes from the descriptor's data pointer, not the
    // frame base. Any other path shape (a field hop, a second index, or a bare
    // whole-array use) is not lowered and demotes the function gracefully.
    if let NativeType::FatArray { elem } = &local.ty {
        let [PathStep::Index(index)] = steps else {
            return Err("a fat-pointer array parameter supports only a single index".to_string());
        };
        let elem_ty = (**elem).clone();
        // Descriptor word 1 (the runtime length) is 8 bytes ABOVE word 0 in the
        // ascending layout, i.e. at the smaller displacement `base_slot - 8`.
        let place = ScalarPlace::FatIndex {
            ptr_slot: base_slot,
            len_slot: base_slot - 8,
            elem_words: elem_ty.words() as i64,
            index: (*index).clone(),
        };
        return Ok((place, elem_ty));
    }
    let mut ty = local.ty.clone();
    let mut const_words: i64 = 0;
    let mut dynamic: Option<(i64, i64, BytecodeExpr)> = None;

    for step in steps {
        match (step, &ty) {
            (PathStep::Field(field), NativeType::Struct { fields, .. }) => {
                let mut offset = 0i64;
                let mut found = None;
                for (fname, fty) in fields {
                    if fname == *field {
                        found = Some(fty.clone());
                        break;
                    }
                    offset += fty.words() as i64;
                }
                let fty = found.ok_or_else(|| format!("unknown field `{field}`"))?;
                const_words += offset;
                ty = fty;
            }
            (PathStep::Index(index), NativeType::Array { elem, len }) => {
                let stride = elem.words() as i64;
                if let BytecodeExprKind::Integer(literal) = index.kind {
                    // A constant index is bounds-checked at compile time: an
                    // out-of-range literal is rejected so the function skips
                    // gracefully rather than emitting an out-of-bounds access.
                    if literal < 0 || literal >= *len as i64 {
                        return Err(format!(
                            "array index `{literal}` is out of bounds for length {len}"
                        ));
                    }
                    const_words += literal * stride;
                } else if dynamic.is_none() {
                    dynamic = Some((stride, *len as i64, (*index).clone()));
                } else {
                    return Err(
                        "at most one runtime array index is supported per access".to_string()
                    );
                }
                ty = (**elem).clone();
            }
            (PathStep::Field(_), _) => {
                return Err("field access on a non-struct native value".to_string());
            }
            (PathStep::Index(_), _) => {
                return Err("index access on a non-array native value".to_string());
            }
        }
    }

    // The final resolved element is a single flat word: an integer cell (`I64`), a
    // float (`F64`/`F32`), or a `string` — an immutable heap pointer stored in one
    // word exactly like an `i64` cell (a heap-typed struct field). A `string` field
    // read loads its pointer word; the strict `resolve_place_steps` caller still
    // requires `I64`, so string fields never reach the integer store / SIMD paths.
    if !matches!(
        ty,
        NativeType::I64 | NativeType::F64 | NativeType::F32 | NativeType::String
    ) {
        return Err("native access must resolve to an i64, f64, or string scalar".to_string());
    }

    // ASCENDING layout: word `k` of an aggregate is 8·k bytes HIGHER than word 0,
    // i.e. at the SMALLER displacement `base_slot - 8*k`.
    let place = match dynamic {
        None => ScalarPlace::Const {
            slot: base_slot - const_words as i32 * 8,
        },
        Some((elem_words, index_len, index)) => ScalarPlace::Dynamic {
            base_slot,
            const_words,
            elem_words,
            index_len,
            index,
        },
    };
    Ok((place, ty))
}

/// Read-place decomposition (like [`resolve_read_place`]) that also permits a
/// float element and returns its type — for the float `Index`/`Field` read path.
pub(crate) fn resolve_read_place_typed(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
) -> Result<(ScalarPlace, NativeType), String> {
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = expr;
    let root = loop {
        match &cursor.kind {
            BytecodeExprKind::Variable(name) => break name.as_str(),
            BytecodeExprKind::Field { target, field } => {
                steps.push(PathStep::Field(field.as_str()));
                cursor = target;
            }
            BytecodeExprKind::Index { target, index } => {
                steps.push(PathStep::Index(index));
                cursor = target;
            }
            _ => return Err("native access must be rooted at a local variable".to_string()),
        }
    };
    steps.reverse();
    resolve_place_steps_typed(ctx, root, &steps)
}

/// Resolve an assignment target `(name, path)` to a scalar place.
pub(crate) fn resolve_scalar_place(
    ctx: &NativeCtx,
    name: &str,
    path: &[BytecodePlace],
) -> Result<ScalarPlace, String> {
    let steps: Vec<PathStep> = path
        .iter()
        .map(|place| match place {
            BytecodePlace::Field(field) => PathStep::Field(field.as_str()),
            BytecodePlace::Index(index) => PathStep::Index(index),
        })
        .collect();
    resolve_place_steps(ctx, name, &steps)
}

/// Like [`resolve_scalar_place`] but permits a float element and returns its type
/// — for the float array-element store path (`a[i] = <f64>`).
pub(crate) fn resolve_scalar_place_typed(
    ctx: &NativeCtx,
    name: &str,
    path: &[BytecodePlace],
) -> Result<(ScalarPlace, NativeType), String> {
    let steps: Vec<PathStep> = path
        .iter()
        .map(|place| match place {
            BytecodePlace::Field(field) => PathStep::Field(field.as_str()),
            BytecodePlace::Index(index) => PathStep::Index(index),
        })
        .collect();
    resolve_place_steps_typed(ctx, name, &steps)
}

/// Decompose a nested `Field`/`Index` read expression into a root variable and
/// an ordered list of steps, then resolve it to a scalar place. Returns `None`
/// (as an `Err`) if the expression is not an aggregate-rooted lvalue.
pub(crate) fn resolve_read_place(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
) -> Result<ScalarPlace, String> {
    let mut steps: Vec<PathStep> = Vec::new();
    let mut cursor = expr;
    let root = loop {
        match &cursor.kind {
            BytecodeExprKind::Variable(name) => break name.as_str(),
            BytecodeExprKind::Field { target, field } => {
                steps.push(PathStep::Field(field.as_str()));
                cursor = target;
            }
            BytecodeExprKind::Index { target, index } => {
                steps.push(PathStep::Index(index));
                cursor = target;
            }
            _ => return Err("native access must be rooted at a local variable".to_string()),
        }
    };
    steps.reverse();
    resolve_place_steps(ctx, root, &steps)
}

/// Load the i64 scalar at a resolved place into `rax`.
pub(crate) fn emit_load_place(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match place {
        ScalarPlace::Const { slot } => {
            load_local(code, *slot);
            Ok(())
        }
        ScalarPlace::Dynamic { .. } | ScalarPlace::FatIndex { .. } => {
            emit_dynamic_addr_into_rcx(ctx, place, code)?; // rcx = &word
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            Ok(())
        }
    }
}

/// Compute the effective address of a dynamic scalar word into `rcx`:
/// `rcx = rbp - base_slot + 8*const_words + 8*elem_words*index`
/// (ASCENDING layout — element/field `k` is 8·k bytes ABOVE word 0).
/// Leaves the stack balanced.
pub(crate) fn emit_dynamic_addr_into_rcx(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // A fat-pointer array element addresses off the descriptor's runtime data
    // pointer, not the frame base: `data_ptr + 8*elem_words*index`, with the index
    // bounds-checked against the descriptor's runtime length word.
    if let ScalarPlace::FatIndex {
        ptr_slot,
        len_slot,
        elem_words,
        index,
        ..
    } = place
    {
        // rax = index
        lower_native_expr(ctx, index, code)?;
        // Bounds check against the RUNTIME length word `[rbp - len_slot]`: one
        // UNSIGNED compare traps a negative or over-large index (`ud2`), matching
        // the interpreters' L0413.
        emit_bounds_check_rax_against_slot(code, *len_slot);
        // rax = index * elem_words   (imul rax, rax, imm32)
        emit_imul_rax_imm(code, *elem_words);
        // rax = rax * 8  -> byte stride  (shl rax, 3)
        code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]);
        // rcx = data_ptr (descriptor word 0)
        emit_mov_rcx_from_slot(code, *ptr_slot);
        // rcx = rcx + rax  (element i is at data_ptr + 8*elem_words*i; elements
        // ASCEND from element 0 exactly like the caller's stack array layout).
        code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
        return Ok(());
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_words,
        elem_words,
        index_len,
        index,
    } = place
    else {
        return Err("expected a dynamic place".to_string());
    };
    // rax = index
    lower_native_expr(ctx, index, code)?;
    // Bounds check: trap on out-of-range, mirroring the interpreters' L0413.
    // One UNSIGNED compare catches both `index < 0` (a huge unsigned value) and
    // `index >= len`, so a negative or over-large index faults deterministically
    // (`ud2`) instead of reading adjacent stack memory.
    emit_bounds_check_rax(code, *index_len);
    // rax = index * elem_words   (imul rax, rax, imm32)
    emit_imul_rax_imm(code, *elem_words);
    // rax = rax * 8  -> byte stride  (shl rax, 3)
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]);
    // rcx = rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    // rcx = rcx + rax  (ADD the dynamic byte offset: element `index` is
    // 8*elem_words*index bytes ABOVE the array's element 0).
    code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
    // rcx = rcx - (base_slot - 8*const_words)  (the static displacement of the
    // indexed array's element 0 within the enclosing local).
    let static_disp = *base_slot - (*const_words as i32) * 8;
    emit_sub_rcx_imm(code, static_disp);
    Ok(())
}
