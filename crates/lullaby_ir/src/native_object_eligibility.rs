//! Native-eligibility and escape analysis: which function signatures the native
//! backend can compile, array-length inference, parameter read-only analysis, and
//! the heap-escape / arena-eligibility analysis over bytecode function bodies.
//! Split out of native_object.rs; sees the parent's items via `use super::*`.

use super::*;

/// Whether a signature type (a parameter type or the return type) is native and
/// whether it is an aggregate. A native **integer** scalar (`i64`/fixed-width/
/// `bool`/`char`/`byte`) passes/returns in an integer register; an aggregate (a
/// scalar-field struct, a fixed array of scalars, or a scalar-payload enum)
/// passes/returns by pointer per the aggregate ABI.
///
/// A top-level **float** (`f64`/`f32`) scalar parameter or return is a register
/// value: it passes/returns in the Win64 SSE registers (`xmm0..3` for arguments,
/// `xmm0` for the return), positionally aligned with the integer registers — so a
/// float at position N consumes `xmm N` while an integer at position N consumes
/// integer register N (never both). Float payloads *inside* a by-pointer aggregate
/// are copied as raw bit-preserving words. A heap-containing aggregate
/// (`string`/`list`/`map`, or an aggregate whose element/field is heap) is not
/// native and skips gracefully.
///
/// Returns `Ok(true)` for an aggregate, `Ok(false)` for an integer scalar, and
/// `Err` for a non-native / deferred type.
pub(crate) fn native_signature_type_is_aggregate(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<bool, String> {
    // A plain integer scalar is a register value, not an aggregate. It is
    // resolvable by `resolve_native_type` but we classify it here directly so an
    // `array<T>` (whose length is unknown from the type) is treated as an
    // aggregate rather than the length error `resolve_native_type` raises.
    if ty.name == "i64"
        || fixed_int_kind(&ty.name).is_some()
        || matches!(ty.name.as_str(), "bool" | "char" | "byte")
    {
        return Ok(false);
    }
    // A top-level float scalar (`f64`/`f32`) is a register value routed through
    // the Win64 SSE argument registers (`xmm0..3`), positionally aligned with the
    // integer registers. It is a scalar, not an aggregate.
    if matches!(ty.name.as_str(), "f64" | "f32") {
        return Ok(false);
    }
    // A heap `string` crosses a boundary as a single immutable pointer word in an
    // integer register (by value; no deep copy, since strings are immutable). It
    // is a scalar for the signature classification, not a by-pointer aggregate.
    if ty.name == "string" {
        return Ok(false);
    }
    // A fixed array parameter/return is an aggregate. Its element layout must be a
    // native (non-heap) type; the length is not needed for the signature check
    // (the callee copies whole words by count derived from the caller's value at
    // the call site — see the call/return ABI), so we only validate the element.
    if let Some(rest) = ty.name.strip_prefix("array<") {
        let elem_name = rest.strip_suffix('>').unwrap_or(rest);
        let elem_ty = TypeRef::new(elem_name);
        // Recurse: the element must itself be a native scalar or native aggregate.
        native_signature_type_is_aggregate(&elem_ty, structs, enums)?;
        return Ok(true);
    }
    // A scalar-element growable `list<T>` crosses a boundary as a single pointer
    // word in an integer register (by value, value-semantic — its mutators copy),
    // so it is a scalar for the signature classification, not a by-pointer
    // aggregate. A heap-element list is rejected by `resolve_native_type` below.
    if ty.name.starts_with("list<") {
        resolve_native_type(ty, structs, enums)?;
        return Ok(false);
    }
    // A scalar-key/value growable `map<K, V>` likewise crosses a boundary as a
    // single pointer word in an integer register (by value, value-semantic — its
    // only mutator copies), so it is a scalar for the signature classification. A
    // heap-key/value or float-key map is rejected by `resolve_native_type` below.
    if ty.name.starts_with("map<") {
        resolve_native_type(ty, structs, enums)?;
        return Ok(false);
    }
    // A struct or scalar-payload enum resolves to an aggregate layout; a heap type
    // (`string`) or a heap-containing aggregate fails to resolve and is rejected
    // here so the function skips gracefully.
    let native = resolve_native_type(ty, structs, enums)?;
    match native {
        // Unreachable (`resolve_native_type` never yields `Void`, and a void RETURN
        // is classified by the caller); a `Void` here could only be a void PARAMETER.
        NativeType::Void => Err("`void` is not a valid parameter type".to_string()),
        // Unreachable: `Narrow` is an ARRAY-ELEMENT-only layout produced solely by
        // `narrow_array_element`; `resolve_native_type` (called just above)
        // maps a narrow scalar type to its normalized `I64` cell, so no signature
        // type can resolve to one. Refused rather than classified, so that if a
        // future change ever routed a narrow layout into a signature position the
        // function would skip cleanly (`L0339`) instead of silently passing a
        // sub-word value in a register slot the ABI sizes as a full word.
        NativeType::Narrow { .. } => Err(
            "a packed narrow value is an array-element layout and is not a valid signature type"
                .to_string(),
        ),
        NativeType::I64
        | NativeType::F64
        | NativeType::F32
        | NativeType::String
        | NativeType::List { .. }
        | NativeType::Map { .. } => Ok(false),
        // A `HeapStruct` is a collection-element-only representation and never
        // reaches a top-level signature type; treat it as a register pointer word
        // for completeness. A `FatArray` is only ever produced by the parameter
        // resolver (never by `resolve_native_type`), so it cannot appear here; it
        // crosses the boundary by pointer like an aggregate.
        NativeType::HeapStruct { .. } => Ok(false),
        NativeType::Struct { .. }
        | NativeType::Array { .. }
        | NativeType::FatArray { .. }
        | NativeType::Enum { .. } => Ok(true),
    }
}

/// Whether a function's signature is native-eligible. Scalars (`i64`, the fixed-
/// width integers, `bool`/`char`/`byte`, `f64`/`f32`) pass/return in a register;
/// scalar-field aggregates (structs, fixed arrays of scalars, scalar-payload
/// enums) pass/return **by pointer** (see the aggregate ABI). An aggregate return
/// consumes one integer register for the hidden result pointer, so the number of
/// *effective* register arguments (params + a hidden return pointer, if any) must
/// be at most four; otherwise, and for any non-native (heap-containing) type, the
/// function skips gracefully and runs on the interpreters.
pub(crate) fn native_signature_eligibility(
    function: &BytecodeFunction,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> Result<(), String> {
    // Refuse a non-native RETURN type (arity no longer gates — every effective argument
    // past the fourth spills to the stack). A VOID return skips the resolver (which
    // would rightly reject `void` as a parameter type). A `fn(...)` RETURN (a factory
    // returning a closure) is admitted as a scalar pointer word by
    // `native_fn_return_eligibility` — the tight gate (which, with
    // `arena_eligible_functions`, holds the factory off the arena).
    if function.return_type.is_function() {
        native_fn_return_eligibility(function)?;
    } else if !function.return_type.is_void() {
        native_signature_type_is_aggregate(&function.return_type, structs, enums).map_err(
            |reason| {
                format!(
                    "return type `{}` is not in the native subset: {reason}",
                    function.return_type.name
                )
            },
        )?;
    }

    // A `fn(...)`-typed parameter is native-eligible ONLY as a **non-escaping
    // higher-order parameter**: an all-native-scalar fn signature used call-only in
    // the body (see `hof_params`). Such a parameter is a single pointer word to a
    // closure env block a caller passes in; it is invoked through the block and never
    // escapes, so a caller's capture environment stays valid for the whole call. A fn
    // parameter that is stored, returned, reassigned, passed onward, or has a
    // non-scalar signature is refused here so the function skips cleanly (`L0339`)
    // rather than risking a dangling capture.
    let hof = hof_params(function);
    for param in &function.params {
        if param.ty.is_function() {
            if !hof.iter().any(|h| h.name == param.name) {
                return Err(format!(
                    "fn-typed parameter `{}` is not a call-only native-scalar higher-order \
                     parameter; a stored/returned/onward-passed fn value is deferred",
                    param.name
                ));
            }
            // A higher-order fn parameter is a scalar pointer word, not an aggregate.
            continue;
        }
        native_signature_type_is_aggregate(&param.ty, structs, enums).map_err(|reason| {
            format!(
                "parameter `{}` type `{}` is not in the native subset: {reason}",
                param.name, param.ty.name
            )
        })?;
    }

    // No fixed arity cap: the 5th+ effective argument (hidden return pointer plus each
    // parameter) spills to the stack, so eligibility is entirely the type checks above.
    Ok(())
}

// -- Array-length inference for signatures -----------------------------------
//
// A fixed array's length is absent from its `array<T>` type, so a function that
// takes or returns one has its length inferred: a returned array's length comes
// from the function's returned array value; a parameter array's length comes
// from every call site's argument in that position, which must all agree. A
// length that cannot be determined (or that disagrees across callers) demotes
// the function so it runs on the interpreters rather than miscompiling.

/// Compute a function's array-length environment: for each array-typed parameter
/// and (if array-typed) the return slot, the concrete element count. A function
/// with no array signature slots yields an empty map. An unsizable array slot is
/// an error the caller turns into a skip.
pub(crate) fn infer_array_lengths(
    function: &BytecodeFunction,
    module: &BytecodeModule,
    eligible_names: &[String],
) -> Result<ArrayLengths, String> {
    let mut lengths = ArrayLengths::new();

    // Return array: length taken from the function's returned array value(s). A
    // heap-backed `array<string>` return is a pointer word (a `list<string>` block)
    // and needs no length.
    if function.return_type.name.starts_with("array<")
        && heap_string_array_element(&function.return_type).is_none()
    {
        let len = infer_return_array_len(function).ok_or_else(|| {
            format!(
                "return array length of `{}` could not be inferred (return an array literal \
                 or a fixed array local)",
                function.name
            )
        })?;
        lengths.insert(RETURN_ARRAY_KEY.to_string(), len);
    }

    // Parameter arrays: every call site's argument in that position must resolve
    // to the same length. A function that is never called (e.g. an unreferenced
    // helper) has no callers to size its array params, so it is demoted.
    for (index, param) in function.params.iter().enumerate() {
        // A heap-backed `array<string>` param is a pointer word (a `list<string>`
        // block), not a stack array, so it needs no inferred length.
        if !param.ty.name.starts_with("array<") || heap_string_array_element(&param.ty).is_some() {
            continue;
        }
        let mut found: Option<usize> = None;
        let mut saw_call = false;
        for caller in &module.functions {
            if !eligible_names.contains(&caller.name) {
                continue;
            }
            collect_call_arg_lengths(
                &caller.instructions,
                caller,
                &function.name,
                index,
                &mut found,
                &mut saw_call,
            )?;
        }
        // A concrete, agreed-upon call-site length keeps the copy-in stack-array
        // path (unchanged): the caller passes the whole array by value, indexing is
        // statically bounds-checked, and array reductions can auto-vectorize.
        if let Some(len) = found {
            lengths.insert(param.name.clone(), len);
            continue;
        }
        // The length is NOT inferable (no call site, or disagreeing call sites). If
        // the parameter is read-only, fall back to passing it as a **fat pointer**
        // (data_ptr + runtime length) — the callee reads the caller's storage in
        // place with no copy — instead of demoting. This is what unlocks the
        // large family of helpers (`sum_array`, `count_frequency_of`, …) that take
        // an `array<T>` whose length is not known at compile time.
        if fat_array_param_elem(function, &param.name, &param.ty).is_some() {
            lengths.insert(param.name.clone(), FAT_ARRAY_LEN);
            continue;
        }
        // Not inferable and not read-only (e.g. a sort that mutates its parameter):
        // demote so the function runs on the interpreters, matching prior behavior.
        if !saw_call {
            return Err(format!(
                "parameter array `{}` of `{}` has no call site to infer its length from",
                param.name, function.name
            ));
        }
        return Err(format!(
            "parameter array `{}` of `{}` could not be sized from its call sites",
            param.name, function.name
        ));
    }

    Ok(lengths)
}

// -- Fat-pointer array parameters --------------------------------------------
//
// A read-only scalar-element `array<T>` parameter is passed as a fat pointer — a
// `(data_ptr, length)` descriptor — instead of requiring its length to be inferred
// from a call site. The callee reads the caller's storage in place (no copy),
// which is value-semantically identical to the interpreters' eager array copy
// BECAUSE the parameter is never written. A mutating array parameter keeps the
// copy-in `NativeType::Array` path (which needs an inferable length).

/// The (scalar) element `NativeType` when `param` of `function` is eligible to be
/// passed as a **fat-pointer** array — a read-only scalar-integer-element
/// `array<T>` parameter — else `None`.
///
/// Eligible when the parameter type is `array<E>` with `E` a native scalar integer
/// cell (`i64`/fixed-width/`bool`/`char`/`byte`; a heap `array<string>` and a
/// float element are handled elsewhere / deferred) AND the body uses `param`
/// read-only: it never assigns `param` (nor `param[i]`/`param.f`) and every use of
/// the `param` variable is either an index-read target (`param[i]`) or the sole
/// argument of `len(param)`. The rule is default-deny — any other use (aliasing,
/// returning, passing it onward, whole-value arithmetic) makes the parameter
/// ineligible, so it falls back to the copy-in path or the function skips.
fn fat_array_param_elem(
    function: &BytecodeFunction,
    param_name: &str,
    param_ty: &TypeRef,
) -> Option<NativeType> {
    let rest = param_ty.name.strip_prefix("array<")?;
    let elem_name = rest.strip_suffix('>').unwrap_or(rest);
    // Scalar element types only: integer cells (`i64`/fixed-width/`bool`/`char`/
    // `byte`, stored as normalized `i64` words) and the floats `f64`/`f32` (an
    // element read loads through an XMM register). `array<string>` is a distinct
    // heap representation handled by `heap_string_array_element`; a nested aggregate
    // element is not in the fat-pointer subset.
    let elem = match elem_name {
        "i64" => NativeType::I64,
        n if fixed_int_kind(n).is_some() => NativeType::I64,
        "bool" | "char" | "byte" => NativeType::I64,
        "f64" => NativeType::F64,
        "f32" => NativeType::F32,
        _ => return None,
    };
    if !param_is_read_only(&function.instructions, param_name) {
        return None;
    }
    Some(elem)
}

/// Whether every statement in `body` uses `param` read-only (see
/// [`fat_array_param_elem`]). Totally enumerated over the statement kinds so an
/// unhandled construct can never silently pass.
fn param_is_read_only(body: &[BytecodeInstruction], param: &str) -> bool {
    body.iter().all(|stmt| stmt_param_read_only(stmt, param))
}

fn stmt_param_read_only(stmt: &BytecodeInstruction, param: &str) -> bool {
    match stmt {
        BytecodeInstruction::Assign {
            name, path, value, ..
        } => {
            // An assignment whose target is `param` mutates it (whole-value, or an
            // element/field via `path`). Index expressions in `path` and the RHS
            // must also use `param` only in safe positions.
            name != param
                && path.iter().all(|p| match p {
                    BytecodePlace::Index(index) => expr_param_read_only(index, param),
                    BytecodePlace::Field(_) => true,
                })
                && expr_param_read_only(value, param)
        }
        BytecodeInstruction::Let { value, .. } => expr_param_read_only(value, param),
        BytecodeInstruction::Return(Some(expr))
        | BytecodeInstruction::Expr(expr)
        | BytecodeInstruction::Throw { value: expr, .. } => expr_param_read_only(expr, param),
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_) => true,
        // An asm `out <reg> = <param>` writes the parameter, so it is read-only
        // for `param` only when no `out` operand targets it. (`in` operands read
        // their expressions, which never mutate the bound parameter.)
        BytecodeInstruction::Asm { operands, .. } => operands.iter().all(|operand| {
            !matches!(
                operand,
                crate::BcAsmOperand::Out { place, .. }
                    if matches!(&place.kind, BytecodeExprKind::Variable(n) if n == param)
            )
        }),
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().all(|branch| {
                expr_param_read_only(&branch.condition, param)
                    && param_is_read_only(&branch.body, param)
            }) && param_is_read_only(else_body, param)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_param_read_only(condition, param) && param_is_read_only(body, param),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_param_read_only(start, param)
                && expr_param_read_only(end, param)
                && step.as_ref().is_none_or(|s| expr_param_read_only(s, param))
                && param_is_read_only(body, param)
        }
        BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::RegionBlock { body, .. } => {
            param_is_read_only(body, param)
        }
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => param_is_read_only(body, param) && param_is_read_only(catch_body, param),
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            expr_param_read_only(scrutinee, param)
                && arms.iter().all(|arm| param_is_read_only(&arm.body, param))
        }
    }
}

/// Whether every occurrence of the `param` variable inside `expr` is in a
/// **read-only** position: the target of an index read (`param[i]`) or the sole
/// argument of `len(param)`. A bare value use of `param` anywhere else returns
/// `false`.
fn expr_param_read_only(expr: &BytecodeExpr, param: &str) -> bool {
    match &expr.kind {
        // A bare read of `param` (as a value) is not read-only-safe on its own; it
        // is only allowed in the recognized `param[i]` / `len(param)` shapes below.
        BytecodeExprKind::Variable(name) => name != param,
        BytecodeExprKind::Index { target, index } => {
            // `param[i]` is allowed: the target may be exactly `param`, otherwise it
            // must itself be safe. The index must always be safe.
            let target_ok = matches!(&target.kind, BytecodeExprKind::Variable(name) if name == param)
                || expr_param_read_only(target, param);
            target_ok && expr_param_read_only(index, param)
        }
        BytecodeExprKind::Call { name, args } => {
            // `len(param)` is the one whole-value use that is allowed.
            if name == "len"
                && args.len() == 1
                && matches!(&args[0].kind, BytecodeExprKind::Variable(n) if n == param)
            {
                return true;
            }
            args.iter().all(|arg| expr_param_read_only(arg, param))
        }
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::Array(elements) => {
            elements.iter().all(|e| expr_param_read_only(e, param))
        }
        BytecodeExprKind::Unary { expr, .. } => expr_param_read_only(expr, param),
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_param_read_only(left, param) && expr_param_read_only(right, param)
        }
        // `param.f` is a value use of `param` we do not lower for a fat array; the
        // recursive check flags a bare `param` target as unsafe.
        BytecodeExprKind::Field { target, .. } => expr_param_read_only(target, param),
        BytecodeExprKind::Await { expr } => expr_param_read_only(expr, param),
    }
}

/// Infer the element count of a function's returned array from its returned
/// array values (an explicit `return <arr>`, or a tail array expression). All
/// returned arrays must agree; a disagreement or an unsizable value yields `None`.
fn infer_return_array_len(function: &BytecodeFunction) -> Option<usize> {
    let mut result: Option<usize> = None;
    fn visit(
        body: &[BytecodeInstruction],
        function: &BytecodeFunction,
        result: &mut Option<usize>,
        ok: &mut bool,
    ) {
        for stmt in body {
            match stmt {
                BytecodeInstruction::Return(Some(expr)) | BytecodeInstruction::Expr(expr) => {
                    if let Some(len) = array_len_of_expr(expr, function) {
                        match result {
                            Some(existing) if *existing != len => *ok = false,
                            _ => *result = Some(len),
                        }
                    } else if matches!(
                        &expr.kind,
                        BytecodeExprKind::Array(_) | BytecodeExprKind::Variable(_)
                    ) {
                        // An array-valued return whose length we cannot read.
                        *ok = false;
                    }
                }
                BytecodeInstruction::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for branch in branches {
                        visit(&branch.body, function, result, ok);
                    }
                    visit(else_body, function, result, ok);
                }
                BytecodeInstruction::While { body, .. }
                | BytecodeInstruction::Loop { body, .. }
                | BytecodeInstruction::For { body, .. } => visit(body, function, result, ok),
                BytecodeInstruction::Match { arms, .. } => {
                    for arm in arms {
                        visit(&arm.body, function, result, ok);
                    }
                }
                _ => {}
            }
        }
    }
    let mut ok = true;
    visit(&function.instructions, function, &mut result, &mut ok);
    if ok { result } else { None }
}

/// The element count of an array-valued expression within `function`'s context:
/// a direct array literal, or a variable bound to a fixed array local (its `let`
/// initializer array literal). Returns `None` for anything else.
fn array_len_of_expr(expr: &BytecodeExpr, function: &BytecodeFunction) -> Option<usize> {
    match &expr.kind {
        BytecodeExprKind::Array(elements) => Some(elements.len()),
        BytecodeExprKind::Variable(name) => local_array_len(&function.instructions, name),
        _ => None,
    }
}

/// Find the array length of a local `name` bound by a `let name array<...> = [..]`
/// anywhere in a body (including nested blocks). Returns `None` if not an array
/// local with a literal initializer.
fn local_array_len(body: &[BytecodeInstruction], name: &str) -> Option<usize> {
    for stmt in body {
        match stmt {
            BytecodeInstruction::Let {
                name: n, ty, value, ..
            } if n == name && ty.name.starts_with("array<") => {
                if let BytecodeExprKind::Array(elements) = &value.kind {
                    return Some(elements.len());
                }
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    if let Some(len) = local_array_len(&branch.body, name) {
                        return Some(len);
                    }
                }
                if let Some(len) = local_array_len(else_body, name) {
                    return Some(len);
                }
            }
            BytecodeInstruction::While { body, .. }
            | BytecodeInstruction::Loop { body, .. }
            | BytecodeInstruction::For { body, .. } => {
                if let Some(len) = local_array_len(body, name) {
                    return Some(len);
                }
            }
            BytecodeInstruction::Match { arms, .. } => {
                for arm in arms {
                    if let Some(len) = local_array_len(&arm.body, name) {
                        return Some(len);
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Scan a caller body for calls to `callee`, reading the array length of the
/// argument at `arg_index`. Every such call must agree on the length; a
/// disagreement or an unsizable argument is an error the caller turns into a skip.
fn collect_call_arg_lengths(
    body: &[BytecodeInstruction],
    caller: &BytecodeFunction,
    callee: &str,
    arg_index: usize,
    found: &mut Option<usize>,
    saw_call: &mut bool,
) -> Result<(), String> {
    fn visit_expr(
        expr: &BytecodeExpr,
        caller: &BytecodeFunction,
        callee: &str,
        arg_index: usize,
        found: &mut Option<usize>,
        saw_call: &mut bool,
    ) -> Result<(), String> {
        if let BytecodeExprKind::Call { name, args } = &expr.kind
            && name == callee
        {
            *saw_call = true;
            let arg = args
                .get(arg_index)
                .ok_or_else(|| format!("call to `{callee}` is missing argument {arg_index}"))?;
            let len = array_len_of_expr(arg, caller).ok_or_else(|| {
                format!(
                    "call to `{callee}` passes an array argument whose length is not \
                     statically known"
                )
            })?;
            match found {
                Some(existing) if *existing != len => {
                    return Err(format!(
                        "call sites of `{callee}` disagree on array argument {arg_index} length \
                         ({existing} vs {len})"
                    ));
                }
                _ => *found = Some(len),
            }
        }
        for child in expr_children(expr) {
            visit_expr(child, caller, callee, arg_index, found, saw_call)?;
        }
        Ok(())
    }
    for stmt in body {
        match stmt {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Return(Some(value))
            | BytecodeInstruction::Expr(value) => {
                visit_expr(value, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    visit_expr(
                        &branch.condition,
                        caller,
                        callee,
                        arg_index,
                        found,
                        saw_call,
                    )?;
                    collect_call_arg_lengths(
                        &branch.body,
                        caller,
                        callee,
                        arg_index,
                        found,
                        saw_call,
                    )?;
                }
                collect_call_arg_lengths(else_body, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => {
                visit_expr(condition, caller, callee, arg_index, found, saw_call)?;
                collect_call_arg_lengths(body, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::For { body, .. } => {
                collect_call_arg_lengths(body, caller, callee, arg_index, found, saw_call)?;
            }
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                visit_expr(scrutinee, caller, callee, arg_index, found, saw_call)?;
                for arm in arms {
                    collect_call_arg_lengths(
                        &arm.body, caller, callee, arg_index, found, saw_call,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Compute a function's native signature (parameter + return layouts) using the
/// inferred array lengths for its array-typed signature slots.
pub(crate) fn compute_native_signature(
    function: &BytecodeFunction,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    array_lengths: &ArrayLengths,
) -> Result<NativeSignature, String> {
    let mut params = Vec::with_capacity(function.params.len());
    for param in &function.params {
        // A higher-order `fn(...)` parameter crosses the boundary as a single pointer
        // word (a closure env-block pointer), so its signature layout is an `I64`
        // cell — the same class a caller stages the closure pointer through. Its
        // call-only eligibility was already checked by `native_signature_eligibility`;
        // here we only give it its register-word layout.
        if param.ty.is_function() {
            params.push(NativeType::I64);
            continue;
        }
        params.push(resolve_signature_native_type(
            &param.ty,
            structs,
            enums,
            array_lengths,
            &param.name,
        )?);
    }
    // A `fn(...)` RETURN (a returned closure) is an `I64` block-pointer word (eligibility
    // already checked); otherwise the return-only resolver maps `void` to `Void`.
    let ret = if function.return_type.is_function() {
        NativeType::I64
    } else {
        resolve_return_native_type(&function.return_type, structs, enums, array_lengths)?
    };
    Ok(NativeSignature { params, ret })
}

// -- Heap-escape and arena-eligibility analysis over bytecode bodies ---------

/// The immediate sub-expressions of an expression (for recursive scans).
pub(crate) fn expr_children(expr: &BytecodeExpr) -> Vec<&BytecodeExpr> {
    match &expr.kind {
        BytecodeExprKind::Binary { left, right, .. } => vec![left, right],
        BytecodeExprKind::Unary { expr, .. } => vec![expr],
        BytecodeExprKind::Call { args, .. } => args.iter().collect(),
        BytecodeExprKind::Array(elements) => elements.iter().collect(),
        BytecodeExprKind::Field { target, .. } => vec![target],
        BytecodeExprKind::Index { target, index } => vec![target, index],
        _ => Vec::new(),
    }
}

/// Whether any instruction in a body issues a call (so the frame needs shadow
/// space). Conservatively scans nested bodies and expressions.
pub(crate) fn body_has_call(body: &[BytecodeInstruction]) -> bool {
    body.iter().any(instruction_has_call)
}

fn instruction_has_call(instruction: &BytecodeInstruction) -> bool {
    match instruction {
        BytecodeInstruction::Let { value, .. } => expr_has_call(value),
        BytecodeInstruction::Assign { value, .. } => expr_has_call(value),
        BytecodeInstruction::Return(Some(expr)) | BytecodeInstruction::Expr(expr) => {
            expr_has_call(expr)
        }
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_) => false,
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches
                .iter()
                .any(|b| expr_has_call(&b.condition) || body_has_call(&b.body))
                || body_has_call(else_body)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_has_call(condition) || body_has_call(body),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_has_call(start)
                || expr_has_call(end)
                || step.as_ref().is_some_and(expr_has_call)
                || body_has_call(body)
        }
        BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::RegionBlock { body, .. } => {
            body_has_call(body)
        }
        // A `match` needs shadow space if its scrutinee (often a call) or any arm
        // body issues a call.
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => expr_has_call(scrutinee) || arms.iter().any(|arm| body_has_call(&arm.body)),
        BytecodeInstruction::Throw { .. } | BytecodeInstruction::Try { .. } => false,
        // An asm `in <reg> = <expr>` evaluates `<expr>` during marshalling; a call
        // inside it needs the 32-byte Win64 shadow space reserved, so the function
        // "has a call" whenever an input operand does.
        BytecodeInstruction::Asm { operands, .. } => operands.iter().any(|operand| {
            matches!(operand, crate::BcAsmOperand::In { value, .. } if expr_has_call(value))
        }),
    }
}

fn expr_has_call(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        BytecodeExprKind::Call { .. } => true,
        // A string literal used as a value materializes through the
        // `__lullaby_str_lit` runtime helper (a `call`), so it needs shadow space.
        BytecodeExprKind::String(_) => true,
        // A `string + string` concatenation calls `__lullaby_str_concat`, so a
        // Binary whose result type is `string` issues a call even if neither
        // operand does. (Any other Binary just recurses into its operands.)
        BytecodeExprKind::Binary { left, right, .. } => {
            is_string_type(&expr.ty) || expr_has_call(left) || expr_has_call(right)
        }
        BytecodeExprKind::Unary { expr, .. } => expr_has_call(expr),
        _ => false,
    }
}

// -- Arena-first memory: function-scoped-region eligibility (stage 1) ---------
//
// A function routes its heap allocations through a function-scoped arena (the
// shared bump heap, reclaimed by rewinding `heap_next` on every return edge)
// ONLY when every heap value it allocates provably stays local. The criterion is
// tight, locally checkable, and DEFAULT-DENY — anything not provably local keeps
// the unchanged RC / free-list codegen (arena and RC coexist). See
// `documents/native_backend_contract.md`.

/// Whether an expression tree references any heap-typed value (an allocation or a
/// read of a heap value). Used both to require an arena function actually touches
/// the heap (else the arena is a pointless no-op) and to reject loops that touch
/// the heap (a function-scoped arena would grow unboundedly across iterations).
pub(crate) fn expr_touches_heap(
    expr: &BytecodeExpr,
    heap_aggs: &std::collections::HashSet<String>,
) -> bool {
    if type_is_heap(&expr.ty, heap_aggs) {
        return true;
    }
    match &expr.kind {
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_) => false,
        // A closure literal ALLOCATES its `[code_ptr][captures…]` block on the
        // heap, so it touches the heap — this is what makes an arena-eligible
        // function recognize a closure-allocating loop and give it a per-iteration
        // sub-region (the loop-reclaim path). The literal's own static type is a
        // `fn(...)` (not a heap type), so without this arm the allocation would be
        // invisible to the escape analysis.
        BytecodeExprKind::Closure { .. } => true,
        BytecodeExprKind::String(_) => true,
        BytecodeExprKind::Array(elements) => {
            elements.iter().any(|e| expr_touches_heap(e, heap_aggs))
        }
        BytecodeExprKind::Index { target, index } => {
            expr_touches_heap(target, heap_aggs) || expr_touches_heap(index, heap_aggs)
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            expr_touches_heap(expr, heap_aggs)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_touches_heap(left, heap_aggs) || expr_touches_heap(right, heap_aggs)
        }
        BytecodeExprKind::Call { args, .. } => args.iter().any(|a| expr_touches_heap(a, heap_aggs)),
        BytecodeExprKind::Field { target, .. } => expr_touches_heap(target, heap_aggs),
    }
}

/// Whether a body references any heap-typed value anywhere.
pub(crate) fn body_touches_heap(
    body: &[BytecodeInstruction],
    heap_aggs: &std::collections::HashSet<String>,
) -> bool {
    body.iter().any(|i| instruction_touches_heap(i, heap_aggs))
}

fn instruction_touches_heap(
    instruction: &BytecodeInstruction,
    heap_aggs: &std::collections::HashSet<String>,
) -> bool {
    match instruction {
        BytecodeInstruction::Let { value, .. }
        | BytecodeInstruction::Assign { value, .. }
        | BytecodeInstruction::Return(Some(value))
        | BytecodeInstruction::Expr(value)
        | BytecodeInstruction::Throw { value, .. } => expr_touches_heap(value, heap_aggs),
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_) => false,
        // An asm `in <reg> = <expr>` evaluates its expression; if that allocates,
        // the asm touches the heap (conservative arena-confinement input).
        BytecodeInstruction::Asm { operands, .. } => operands.iter().any(|operand| {
            matches!(
                operand,
                crate::BcAsmOperand::In { value, .. } if expr_touches_heap(value, heap_aggs)
            )
        }),
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().any(|b| {
                expr_touches_heap(&b.condition, heap_aggs) || body_touches_heap(&b.body, heap_aggs)
            }) || body_touches_heap(else_body, heap_aggs)
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => expr_touches_heap(condition, heap_aggs) || body_touches_heap(body, heap_aggs),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_touches_heap(start, heap_aggs)
                || expr_touches_heap(end, heap_aggs)
                || step
                    .as_ref()
                    .is_some_and(|s| expr_touches_heap(s, heap_aggs))
                || body_touches_heap(body, heap_aggs)
        }
        BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::RegionBlock { body, .. } => {
            body_touches_heap(body, heap_aggs)
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            expr_touches_heap(scrutinee, heap_aggs)
                || arms
                    .iter()
                    .any(|arm| body_touches_heap(&arm.body, heap_aggs))
        }
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => body_touches_heap(body, heap_aggs) || body_touches_heap(catch_body, heap_aggs),
    }
}

/// Whether an expression calls a user-defined or `extern` function (as opposed to
/// a native builtin), by NAME membership in `user_names`. Used by the native closure
/// escape analysis (`native_object_closure.rs`). NOTE: arena eligibility no longer
/// uses this coarse "any user call denies" test — the cross-call retention summary
/// ([`all_callees_non_retaining`], `native_object_retain.rs`) admits a caller whose
/// module callees are all provably non-retaining, while denying `extern`/indirect
/// callees, which this name-membership test cannot distinguish.
pub(crate) fn expr_calls_user(
    expr: &BytecodeExpr,
    user_names: &std::collections::HashSet<&str>,
) -> bool {
    match &expr.kind {
        BytecodeExprKind::Call { name, args } => {
            user_names.contains(name.as_str())
                || args.iter().any(|a| expr_calls_user(a, user_names))
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_calls_user(left, user_names) || expr_calls_user(right, user_names)
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            expr_calls_user(expr, user_names)
        }
        BytecodeExprKind::Index { target, index } => {
            expr_calls_user(target, user_names) || expr_calls_user(index, user_names)
        }
        BytecodeExprKind::Field { target, .. } => expr_calls_user(target, user_names),
        BytecodeExprKind::Array(elements) => {
            elements.iter().any(|e| expr_calls_user(e, user_names))
        }
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_)
        | BytecodeExprKind::Closure { .. } => false,
    }
}

/// Maximum loop-nesting depth on any path through a body: `0` when there is no
/// loop, `1` for an unnested loop, `2` for a loop inside a loop, etc. Used to
/// reserve one arena sub-region "mark" word per level so nested loops that each
/// get a per-iteration reset never share a mark slot.
pub(crate) fn max_loop_nesting(body: &[BytecodeInstruction]) -> usize {
    body.iter().map(instruction_loop_nesting).max().unwrap_or(0)
}

fn instruction_loop_nesting(instruction: &BytecodeInstruction) -> usize {
    match instruction {
        BytecodeInstruction::While { body, .. }
        | BytecodeInstruction::For { body, .. }
        | BytecodeInstruction::Loop { body, .. } => 1 + max_loop_nesting(body),
        // A region block is NOT a loop, so it adds no nesting level — but a loop
        // nested inside it still needs a mark word, so recurse without the `+1`
        // (like `if`/`match`/`try`). Missing this would under-size the loop mark pool
        // for a loop inside a region block.
        BytecodeInstruction::RegionBlock { body, .. } => max_loop_nesting(body),
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => branches
            .iter()
            .map(|b| max_loop_nesting(&b.body))
            .chain(std::iter::once(max_loop_nesting(else_body)))
            .max()
            .unwrap_or(0),
        BytecodeInstruction::Match { arms, .. } => arms
            .iter()
            .map(|arm| max_loop_nesting(&arm.body))
            .max()
            .unwrap_or(0),
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => max_loop_nesting(body).max(max_loop_nesting(catch_body)),
        _ => 0,
    }
}

/// Whether an instruction is a loop (`while`/`for`/`loop`) whose header or body
/// touches the heap AND whose body does NOT confine that heap to the iteration —
/// i.e. an allocating loop with no bounding sub-region. Such a loop would grow the
/// arena region unboundedly within one call, so its presence disqualifies the
/// function from arena routing (it stays on the RC / free-list path). A heap loop
/// that IS confined gets a per-iteration sub-region (stage 2) and is fine.
fn instruction_is_unbounded_heap_loop(
    instruction: &BytecodeInstruction,
    heap_aggs: &std::collections::HashSet<String>,
) -> bool {
    let (touches, confined) = match instruction {
        BytecodeInstruction::While {
            condition, body, ..
        } => (
            expr_touches_heap(condition, heap_aggs) || body_touches_heap(body, heap_aggs),
            loop_body_confines_heap(body, heap_aggs),
        ),
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => (
            expr_touches_heap(start, heap_aggs)
                || expr_touches_heap(end, heap_aggs)
                || step
                    .as_ref()
                    .is_some_and(|s| expr_touches_heap(s, heap_aggs))
                || body_touches_heap(body, heap_aggs),
            loop_body_confines_heap(body, heap_aggs),
        ),
        BytecodeInstruction::Loop { body, .. } => (
            body_touches_heap(body, heap_aggs),
            loop_body_confines_heap(body, heap_aggs),
        ),
        // Non-loop statements: recurse into nested bodies below.
        _ => (false, true),
    };
    if touches && !confined {
        return true;
    }
    // Recurse into nested control flow; a nested unbounded heap loop anywhere
    // disqualifies the function too.
    match instruction {
        BytecodeInstruction::While { body, .. }
        | BytecodeInstruction::For { body, .. }
        | BytecodeInstruction::Loop { body, .. }
        | BytecodeInstruction::RegionBlock { body, .. } => {
            // A region block is not itself a loop, but a nested unbounded heap loop
            // inside it disqualifies the function just the same; recurse into it.
            body_has_unbounded_heap_loop(body, heap_aggs)
        }
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            branches
                .iter()
                .any(|b| body_has_unbounded_heap_loop(&b.body, heap_aggs))
                || body_has_unbounded_heap_loop(else_body, heap_aggs)
        }
        BytecodeInstruction::Match { arms, .. } => arms
            .iter()
            .any(|arm| body_has_unbounded_heap_loop(&arm.body, heap_aggs)),
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => {
            body_has_unbounded_heap_loop(body, heap_aggs)
                || body_has_unbounded_heap_loop(catch_body, heap_aggs)
        }
        _ => false,
    }
}

/// Whether a body contains any loop that touches the heap but does not confine it
/// to the iteration (see [`instruction_is_unbounded_heap_loop`]).
fn body_has_unbounded_heap_loop(
    body: &[BytecodeInstruction],
    heap_aggs: &std::collections::HashSet<String>,
) -> bool {
    body.iter()
        .any(|i| instruction_is_unbounded_heap_loop(i, heap_aggs))
}

/// Compute the set of arena-eligible function names for a module. Default-deny:
/// a function qualifies only when ALL hold — (1) its return type is a native
/// scalar (`i64`/`bool`/fixed-width lowered as `i64`, `f64`, `f32` — never a heap
/// type, so no heap value can escape via the return) OR a **promotable closure
/// factory** (criterion 1b), (2) it actually touches the
/// heap (else the arena is a no-op), (3) **every callee it invokes is provably
/// non-retaining** — a native builtin, or a module function the cross-call retention
/// summary ([`all_callees_non_retaining`], I2) proves cannot stash a pointer to a
/// heap cell this function's return-edge rewind frees; an `extern` call, an indirect
/// (`fn`-param / closure) call, or a retaining module callee keeps it off the arena
/// (this subsumes and widens the earlier LEAF-only rule), and (4) it has no
/// **unbounded** heap loop — every heap-touching loop
/// **confines** its allocations to the iteration (stage 2), so each such loop gets
/// a per-iteration sub-region that bounds it; a loop whose heap escapes the
/// iteration would grow the region unboundedly and keeps the function on the RC
/// path, and (5) it has no `alloc` heap box (see [`alloc_defeats_arena`]). All heap
/// allocations of a qualifying function are dead at every return edge (and, for a
/// confined loop, at every iteration edge), so rewinding the bump pointer there
/// reclaims them soundly.
pub(crate) fn arena_eligible_functions(
    module: &BytecodeModule,
    eligible_names: &[String],
    signatures: &HashMap<String, NativeSignature>,
    closure_layouts: &HashMap<usize, ClosureLayout>,
) -> std::collections::HashSet<String> {
    // Aggregate types that transitively carry a heap field/payload (a struct with a
    // `string` field, an `option<string>`/user enum with a heap payload). The escape
    // analysis treats storing one of these into an iteration-outliving location as a
    // heap escape, so a confined arena sub-region never reclaims a record such an
    // aggregate still references (default-deny).
    let mut heap_aggs = heap_carrying_aggregates(&module.structs, &module.enums);
    // Also recognize heap-`T` user-generic INSTANTIATIONS (`Box<string>`,
    // `Opt<string>`, `Pair<string, i64>`) as heap-carrying, keyed by their full
    // spelling. Without this the escape check `type_is_heap(value.ty)` would miss a
    // stored/returned monomorphized heap-`T` value (its base name `Box` is not a
    // heap type), and a per-iteration arena sub-region could reclaim a record a live
    // variable still references. Scalar instantiations (`Box<i64>`) are never added.
    let generic_heap: std::collections::HashSet<String> = module
        .functions
        .iter()
        .flat_map(|f| {
            heap_carrying_generic_instantiations(f, &module.structs, &module.enums, &heap_aggs)
        })
        .collect();
    heap_aggs.extend(generic_heap);

    // Cross-call arena (I2): the per-function retention summary over the WHOLE module,
    // computed once. A caller stays arena-eligible (criterion 3 below) iff every callee
    // it invokes is provably non-retaining per this summary. Default-deny; cycles
    // pre-poisoned; one reverse-topological sweep, no fixpoint. See
    // `native_object_retain.rs`.
    let summary = retaining_summary(module, &heap_aggs, closure_layouts);
    let module_fns: std::collections::HashSet<&str> =
        module.functions.iter().map(|f| f.name.as_str()).collect();
    let extern_fns: std::collections::HashSet<&str> =
        module.extern_functions.iter().map(String::as_str).collect();

    let mut arena = std::collections::HashSet::new();
    for name in eligible_names {
        let Some(sig) = signatures.get(name) else {
            continue;
        };
        // (1) Scalar (non-heap) return.
        if !matches!(sig.ret, NativeType::I64 | NativeType::F64 | NativeType::F32) {
            continue;
        }
        let Some(function) = module.functions.iter().find(|f| &f.name == name) else {
            continue;
        };
        // (1b, stage-4b) A `fn(...)`-returning function (admitted as `I64`, so it passes
        // criterion 1) arenas ONLY when it is a PROMOTABLE factory — every return edge a
        // fresh flat scalar-capture literal the return-edge PROMOTING reset relocates to
        // the region mark (`emit_arena_reset`). A non-promotable `fn` return (returned
        // param, heap-capturing / above-cap / call-returned closure) stays off-arena
        // (stage-4a — a plain rewind would reclaim the live returned block).
        // See `returns_promotable_closure`.
        if function.return_type.is_function()
            && !returns_promotable_closure(function, closure_layouts)
        {
            continue;
        }
        // (2) Actually uses the heap.
        if !body_touches_heap(&function.instructions, &heap_aggs) {
            continue;
        }
        // (3) Every user/`extern`/indirect callee is provably NON-retaining (I2). A
        // caller is no longer required to be a leaf: it may call module functions the
        // retention summary proves cannot stash a pointer to a heap cell this caller's
        // return-edge rewind frees. An `extern` call, an indirect (`fn`-param /
        // closure) call, or a module callee that is (or transitively reaches) retaining
        // keeps the function off the arena (default-deny). This subsumes the old leaf
        // test — a leaf calls only builtins, which are always non-retaining.
        if !all_callees_non_retaining(function, &module_fns, &extern_fns, &summary) {
            continue;
        }
        // (4) No UNBOUNDED heap loop — every heap-touching loop confines its
        // allocations to the iteration (stage 2 gives it a per-iteration
        // sub-region); a loop whose heap escapes stays on the RC path.
        if body_has_unbounded_heap_loop(&function.instructions, &heap_aggs) {
            continue;
        }
        // (5) No `alloc` heap box — a manually-managed cell this analysis cannot see,
        // so a rewind could reclaim a live box (see `native_object_heapbox.rs`).
        if alloc_defeats_arena(&function.instructions, &module.closures) {
            continue;
        }
        arena.insert(name.clone());
    }
    arena
}
