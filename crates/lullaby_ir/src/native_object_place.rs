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
            // The element's C width. A fat-pointer parameter views the CALLER's
            // storage in place, so this stride must be the same one the caller's
            // array was laid out with — packed for a narrow element.
            elem_bytes: elem_ty.byte_size() as i64,
            index: (*index).clone(),
        };
        return Ok((place, elem_ty));
    }
    let mut ty = local.ty.clone();
    // The static offset from the local's word 0, in BYTES. Struct fields are still
    // word-granular (each field starts on a word boundary, so a field hop always
    // contributes a multiple of 8); only an array index can contribute a sub-word
    // amount, and only when the array's element is packed.
    let mut const_bytes: i64 = 0;
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
                    // Struct fields remain word-granular: a preceding field occupies
                    // whole words, so its byte contribution is `words * 8`. (A
                    // narrow-element array is word-ALIGNED and owns its tail padding
                    // — packing is internal to the array's own span — so this stays
                    // exact even when `fty` is such an array.)
                    offset += fty.words() as i64 * 8;
                }
                let fty = found.ok_or_else(|| format!("unknown field `{field}`"))?;
                const_bytes += offset;
                ty = fty;
            }
            (PathStep::Index(index), NativeType::Array { elem, len }) => {
                // BYTE stride: `byte_size()` is the element's own C width (1/2/4 for
                // a packed narrow element, 8*words for everything else). For an
                // 8-byte element this is the historical `words * 8` and the emitted
                // code is unchanged.
                let stride = elem.byte_size() as i64;
                if let BytecodeExprKind::Integer(literal) = index.kind {
                    // A constant index is bounds-checked at compile time: an
                    // out-of-range literal is rejected so the function skips
                    // gracefully rather than emitting an out-of-bounds access.
                    if literal < 0 || literal >= *len as i64 {
                        return Err(format!(
                            "array index `{literal}` is out of bounds for length {len}"
                        ));
                    }
                    const_bytes += literal * stride;
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
        NativeType::I64
            | NativeType::F64
            | NativeType::F32
            | NativeType::String
            | NativeType::Narrow { .. }
    ) {
        return Err("native access must resolve to an i64, f64, or string scalar".to_string());
    }

    // ASCENDING layout: byte `k` of an aggregate is k bytes HIGHER than byte 0,
    // i.e. at the SMALLER displacement `base_slot - k`.
    let place = match dynamic {
        None => ScalarPlace::Const {
            slot: base_slot - const_bytes as i32,
        },
        Some((elem_bytes, index_len, index)) => ScalarPlace::Dynamic {
            base_slot,
            const_bytes,
            elem_bytes,
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

/// The compile-time element count of a fixed-array-typed lvalue PATH — a
/// struct-field array (`f.pixels`), or a nested one (`m.frame.pixels`,
/// `grid[i]` when the element is itself a fixed array) — or `None` when the path is
/// not rooted at a local or does not resolve to a [`NativeType::Array`]. It walks
/// the same `Field`/`Index` steps as [`resolve_read_place_typed`] but computes only
/// the resolved type (tolerating an aggregate final), so `len(<path>)` over an
/// inline fixed array folds to a compile-time constant exactly like `len` over a
/// whole-array local. A fat-pointer root or any non-array final yields `None`, so
/// those `len` shapes fall through to their own runtime-length paths.
pub(crate) fn resolve_path_array_len(ctx: &NativeCtx, expr: &BytecodeExpr) -> Option<usize> {
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
            _ => return None,
        }
    };
    steps.reverse();
    let mut ty = ctx.local(root).ok()?.ty.clone();
    for step in &steps {
        match (step, &ty) {
            (PathStep::Field(field), NativeType::Struct { fields, .. }) => {
                let (_, fty) = fields.iter().find(|(fname, _)| fname == field)?;
                ty = fty.clone();
            }
            (PathStep::Index(_), NativeType::Array { elem, .. }) => {
                ty = (**elem).clone();
            }
            _ => return None,
        }
    }
    match ty {
        NativeType::Array { len, .. } => Some(len),
        _ => None,
    }
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

/// The width/signedness of a packed narrow array element, as the same
/// [`PointeeAccess`] the raw-pointer surface uses.
///
/// A packed element and an `int_to_ptr` pointee are the *same problem* — read or
/// write `n` C-natural bytes and extend back into the normalized 8-byte cell — so
/// they share one set of emitters (`emit_load_through_rcx` /
/// `emit_store_through_rcx`) rather than growing a second, subtly different copy.
/// That shared path is what keeps `a[i]` and `ptr_read(addr_of(a[i]))` agreeing by
/// construction.
pub(crate) fn narrow_access(ty: &NativeType) -> Option<PointeeAccess> {
    match ty {
        NativeType::Narrow { bytes, signed } => Some(PointeeAccess {
            size: *bytes as i64,
            signed: *signed,
        }),
        _ => None,
    }
}

/// Load the **packed narrow** element at a resolved place into `rax`,
/// sign/zero-extended into its normalized 8-byte cell — so the value in `rax` is
/// indistinguishable from the cell an 8-byte element would have produced, and
/// every downstream integer path is unchanged.
pub(crate) fn emit_load_place_narrow(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    access: PointeeAccess,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match place {
        // rcx = &elem
        ScalarPlace::Const { slot } => emit_lea_rcx_slot(code, *slot),
        ScalarPlace::Dynamic { .. } | ScalarPlace::FatIndex { .. } => {
            emit_dynamic_addr_into_rcx(ctx, place, code)?;
        }
    }
    emit_load_through_rcx(code, access);
    Ok(())
}

/// Store the low `access.size` bytes of the cell in `rax` to a **packed narrow**
/// element.
///
/// This is width-exact rather than lossy: the value in `rax` is already normalized
/// to the element type's range (a narrow cell is kept sign/zero-extended by
/// `emit_normalize_rax` and the `to_iN`/`to_uN` conversions), so the discarded high
/// bytes are pure extension and the `emit_load_place_narrow` round-trip reproduces
/// the cell exactly.
pub(crate) fn emit_store_place_narrow(
    ctx: &mut NativeCtx,
    place: &ScalarPlace,
    access: PointeeAccess,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match place {
        ScalarPlace::Const { slot } => {
            // rcx = &elem. `rax` already holds the value, so the address must not
            // be computed through it.
            emit_lea_rcx_slot(code, *slot);
        }
        ScalarPlace::Dynamic { .. } => {
            // The dynamic address computation CLOBBERS rax (it evaluates the index
            // expression into it), so the value is spilled across it — the same
            // shape the float element store uses with `push_xmm0`/`pop_xmm0`.
            code.push(0x50); // push rax (the value)
            emit_dynamic_addr_into_rcx(ctx, place, code)?; // rcx = &elem
            code.push(0x58); // pop rax (the value)
        }
        // A fat-pointer array parameter is READ-ONLY — that is the whole reason the
        // no-copy descriptor is value-semantically sound — so an element store must
        // never resolve to one. Refused defensively, mirroring the float store path.
        ScalarPlace::FatIndex { .. } => {
            return Err("cannot assign to a fat-pointer array element".to_string());
        }
    }
    emit_store_through_rcx(code, access);
    Ok(())
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
        elem_bytes,
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
        // rax = index * elem_bytes
        emit_scale_rax_by_stride(code, *elem_bytes)?;
        // rcx = data_ptr (descriptor word 0)
        emit_mov_rcx_from_slot(code, *ptr_slot);
        // rcx = rcx + rax  (element i is at data_ptr + 8*elem_words*i; elements
        // ASCEND from element 0 exactly like the caller's stack array layout).
        code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
        return Ok(());
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_bytes,
        elem_bytes,
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
    // rax = index * elem_bytes
    emit_scale_rax_by_stride(code, *elem_bytes)?;
    // rcx = rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    // rcx = rcx + rax  (ADD the dynamic byte offset: element `index` is
    // elem_bytes*index bytes ABOVE the array's element 0).
    code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
    // rcx = rcx - (base_slot - const_bytes)  (the static displacement of the
    // indexed array's element 0 within the enclosing local).
    let static_disp = *base_slot - (*const_bytes as i32);
    emit_sub_rcx_imm(code, static_disp);
    Ok(())
}

/// `rax = rax * stride`, where `stride` is an element's byte width.
///
/// Emits the **exact byte sequence the word-granular code emitted** for every
/// stride that is a multiple of 8 — `imul rax, rax, stride/8` followed by
/// `shl rax, 3` — so the existing `array<i64>`/`array<f64>`/struct-array codegen is
/// unchanged bit-for-bit (and every COFF snapshot over it still matches). A packed
/// narrow stride (1/2/4) is a power of two, so it scales with a single `shl` —
/// or, for a byte array, no instruction at all.
///
/// Every stride reaching here is either a power of two below 8 (a packed narrow
/// element) or a multiple of 8 (an 8-byte cell or a whole-word aggregate element),
/// because `NativeType::byte_size` returns `bytes` only for `Narrow` (1/2/4) and
/// `8 * words()` otherwise. A stride outside that set would mean a layout invariant
/// broke, so it is refused and the function skips cleanly rather than emitting a
/// silently wrong address.
fn emit_scale_rax_by_stride(code: &mut Vec<u8>, stride: i64) -> Result<(), String> {
    match stride {
        // A packed narrow element: a single shift (or nothing for a byte array).
        1 => Ok(()),
        2 => {
            code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x01]); // shl rax, 1
            Ok(())
        }
        4 => {
            code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x02]); // shl rax, 2
            Ok(())
        }
        // A whole-word element: reproduce the historical `imul` + `shl rax, 3`
        // pair exactly, so pre-existing codegen does not move.
        n if n > 0 && n % 8 == 0 => {
            emit_imul_rax_imm(code, n / 8); // imul rax, rax, elem_words
            code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
            Ok(())
        }
        other => Err(format!(
            "array element stride {other} is neither a packed narrow width (1/2/4) nor a \
             whole number of 8-byte words; the native backend refuses to scale an index by it"
        )),
    }
}
