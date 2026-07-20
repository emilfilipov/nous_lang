//! Heap-classification predicates for the native arena escape analysis: which
//! types are (transitively) heap-backed, the module-wide heap-carrying-aggregate
//! fixpoint, heap-`T` generic-instantiation discovery, and the type-ref collector
//! it uses. Split out of native_object_eligibility.rs; sees the parent's items via
//! `use super::*`.

use super::*;

// Reused from semantics to substitute a generic type's type parameters with an
// instantiation's concrete arguments, so the escape analysis can decide whether a
// monomorphized generic value (`Box<string>`) transitively carries a heap record.
use lullaby_semantics::substitute_type;

/// Whether a type is DIRECTLY one of the heap-backed value types the native
/// backend allocates: `string`, growable `list<…>`/`map<…>`, or a heap
/// `array<string>`. Scalar `array<T>` is stack-allocated and is intentionally NOT
/// heap here. This does NOT account for a `struct`/`enum` that *transitively*
/// carries a heap field/payload — use [`type_is_heap`] with the module's
/// heap-carrying-aggregate set for that.
fn type_is_directly_heap(ty: &TypeRef) -> bool {
    let name = ty.name.as_str();
    name == "string"
        || name.starts_with("list<")
        || name.starts_with("map<")
        || heap_string_array_element(ty).is_some()
}

/// Whether a type carries a heap value the arena must account for: a directly-heap
/// type (string/list/map/`array<string>`) OR a `struct`/`enum` that transitively
/// contains a heap field/payload (its name is in `heap_aggregates`). A struct with
/// a `string` field, or an `option<string>`/user enum with a heap payload, is a
/// heap-carrying value even though its own type name is neither `string` nor a
/// `list`/`map` — storing it into a location that outlives an iteration lets the
/// referenced record escape, so the escape analysis MUST treat it as heap.
pub(crate) fn type_is_heap(
    ty: &TypeRef,
    heap_aggregates: &std::collections::HashSet<String>,
) -> bool {
    type_is_directly_heap(ty) || heap_aggregates.contains(ty.name.as_str())
}

/// Compute the set of `struct`/`enum` type names that transitively carry a heap
/// value (a `string`/`list`/`map`/`array<string>` field or payload, directly or via
/// a nested aggregate that itself carries heap). Computed as a fixpoint so a struct
/// whose field is another heap-carrying struct is itself heap-carrying. Used by the
/// arena escape analysis so that storing a heap-carrying aggregate into a location
/// that outlives an iteration is correctly recognized as an escape (default-deny).
pub(crate) fn heap_carrying_aggregates(
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
) -> std::collections::HashSet<String> {
    let mut heap: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Iterate to a fixpoint: each pass may discover a new heap-carrying aggregate
    // because one of its fields/payloads references an aggregate marked in a prior
    // pass. Bounded by the number of aggregate declarations.
    loop {
        let mut changed = false;
        for def in structs {
            if heap.contains(&def.name) {
                continue;
            }
            if def
                .fields
                .iter()
                .any(|(_, ty)| type_ref_transitively_heap(ty, structs, enums, &heap, 0))
            {
                heap.insert(def.name.clone());
                changed = true;
            }
        }
        for def in enums {
            if heap.contains(&def.name) {
                continue;
            }
            if def.variants.iter().any(|v| {
                v.payload
                    .iter()
                    .any(|ty| type_ref_transitively_heap(ty, structs, enums, &heap, 0))
            }) {
                heap.insert(def.name.clone());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    heap
}

/// Whether a concrete `TypeRef` transitively owns a heap value, resolving a
/// user-generic instantiation (`Box<string>`) by substituting its type arguments
/// into the generic declaration's fields/payloads. This is what lets the arena
/// escape analysis recognize a heap-`T` MONOMORPHIZED value (`Box<string>`,
/// `Pair<string, i64>`, `Opt<string>`) as heap-carrying even though its own base
/// name (`Box`) is neither `string` nor a `list`/`map`. A scalar instantiation
/// (`Box<i64>`) is NOT heap, so scalar-generic programs keep their exact prior
/// classification (no codegen change). `base` carries the non-generic
/// heap-carrying aggregate names from [`heap_carrying_aggregates`]. `depth` guards
/// a pathological deeply-nested spelling: past the bound it returns heap
/// conservatively — sound for the escape analysis (it then simply denies arena),
/// and unreachable for a well-formed value-semantic instantiation set (a direct
/// recursive generic is rejected upstream and indirected recursion passes through
/// a `list`/`map`, caught as directly-heap before recursing).
fn type_ref_transitively_heap(
    ty: &TypeRef,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    base: &std::collections::HashSet<String>,
    depth: usize,
) -> bool {
    if type_is_directly_heap(ty) {
        return true;
    }
    if base.contains(ty.name.as_str()) {
        return true;
    }
    let Some((head, args)) = split_generic_instantiation(ty) else {
        return false;
    };
    if depth >= 16 {
        return true;
    }
    if let Some(def) = structs
        .iter()
        .find(|s| s.name == head && !s.type_params.is_empty())
    {
        let subst = generic_subst_map(&def.type_params, &args);
        return def.fields.iter().any(|(_, fty)| {
            type_ref_transitively_heap(
                &substitute_type(fty, &subst),
                structs,
                enums,
                base,
                depth + 1,
            )
        });
    }
    if let Some(def) = enums
        .iter()
        .find(|e| e.name == head && !e.type_params.is_empty())
    {
        let subst = generic_subst_map(&def.type_params, &args);
        return def.variants.iter().any(|v| {
            v.payload.iter().any(|p| {
                type_ref_transitively_heap(
                    &substitute_type(p, &subst),
                    structs,
                    enums,
                    base,
                    depth + 1,
                )
            })
        });
    }
    false
}

/// Split a user-generic instantiation spelling `Name<args>` into `(head, args)`;
/// `None` for a plain name, a `fn(...)` type, or a non-`<...>` spelling. Built-in
/// generics (`list<...>`/`map<...>`/…) are handled by [`type_is_directly_heap`]
/// before this is reached.
fn split_generic_instantiation(ty: &TypeRef) -> Option<(String, Vec<TypeRef>)> {
    let open = ty.name.find('<')?;
    if !ty.name.ends_with('>') || ty.name.starts_with("fn(") {
        return None;
    }
    let head = ty.name[..open].to_string();
    let args = ty.generic_args(&head)?;
    Some((head, args))
}

/// Zip declared type-parameter names against an instantiation's concrete type
/// arguments (`["T"]` + `["string"]` -> `{T: string}`).
fn generic_subst_map(type_params: &[String], args: &[TypeRef]) -> HashMap<String, TypeRef> {
    type_params
        .iter()
        .cloned()
        .zip(args.iter().cloned())
        .collect()
}

/// Collect the user-generic instantiations reachable in `func` (its parameter
/// types, its return type, and every expression's static type / `let` binding type)
/// whose MONOMORPHIZED layout transitively carries a heap value, so the arena escape
/// analysis recognizes a heap-`T` generic value as heap-carrying. Two keys are added
/// per heap instantiation:
/// - the **full spelling** (`Box<string>`, `Pair<string, i64>`, `Opt<string>`) — the
///   type a `let`/parameter/annotated binding carries; and
/// - the **bare head** (`Box`, `Pair`, `Opt`) — the type a bare CONSTRUCTOR call
///   expression carries (`Box("x")` is typed `Box`, not `Box<string>`), which is the
///   value type an `Assign`/`Return`/`Throw` escape check inspects.
///
/// Unioning these into the heap set makes `type_is_heap` treat a stored heap-`T`
/// generic value as heap, so a per-iteration arena sub-region never reclaims a record
/// such a value still references (default-deny). A SCALAR instantiation (`Box<i64>`)
/// is never heap, so neither its spelling nor its head is added when only scalar
/// instantiations of that generic exist — scalar-generic programs keep their exact
/// escape classification (value-neutral, no COFF change). If a program mixes a heap
/// and a scalar instantiation of the SAME generic (`Box<string>` and `Box<i64>`), the
/// shared head `Box` is added, which conservatively treats the scalar `Box<i64>`
/// value as heap too — sound (it only ever DENIES arena, never miscompiles).
pub(crate) fn heap_carrying_generic_instantiations(
    func: &BytecodeFunction,
    structs: &[IrStructDef],
    enums: &[IrEnumDef],
    base: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut spellings: Vec<TypeRef> = Vec::new();
    for param in &func.params {
        spellings.push(param.ty.clone());
    }
    spellings.push(func.return_type.clone());
    collect_type_refs_in_body(&func.instructions, &mut spellings);
    let mut heap = std::collections::HashSet::new();
    for ty in spellings {
        let Some((head, _)) = split_generic_instantiation(&ty) else {
            continue;
        };
        if heap.contains(ty.name.as_str()) {
            continue;
        }
        if type_ref_transitively_heap(&ty, structs, enums, base, 0) {
            heap.insert(ty.name.clone());
            heap.insert(head);
        }
    }
    heap
}

fn collect_type_refs_in_body(body: &[BytecodeInstruction], out: &mut Vec<TypeRef>) {
    for inst in body {
        collect_type_refs_in_instruction(inst, out);
    }
}

fn collect_type_refs_in_instruction(inst: &BytecodeInstruction, out: &mut Vec<TypeRef>) {
    match inst {
        BytecodeInstruction::Let { ty, value, .. } => {
            out.push(ty.clone());
            collect_type_refs_in_expr(value, out);
        }
        BytecodeInstruction::Assign { path, value, .. } => {
            for p in path {
                if let BytecodePlace::Index(e) = p {
                    collect_type_refs_in_expr(e, out);
                }
            }
            collect_type_refs_in_expr(value, out);
        }
        BytecodeInstruction::Return(Some(v))
        | BytecodeInstruction::Expr(v)
        | BytecodeInstruction::Throw { value: v, .. } => collect_type_refs_in_expr(v, out),
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_) => {}
        // Collect the types referenced by asm operand expressions, so a generic
        // instantiation used only inside an asm clause is still monomorphized.
        BytecodeInstruction::Asm { operands, .. } => {
            for operand in operands {
                match operand {
                    crate::BcAsmOperand::In { value, .. } => collect_type_refs_in_expr(value, out),
                    crate::BcAsmOperand::Out { place, .. } => collect_type_refs_in_expr(place, out),
                }
            }
        }
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => {
            for b in branches {
                collect_type_refs_in_expr(&b.condition, out);
                collect_type_refs_in_body(&b.body, out);
            }
            collect_type_refs_in_body(else_body, out);
        }
        BytecodeInstruction::While {
            condition, body, ..
        } => {
            collect_type_refs_in_expr(condition, out);
            collect_type_refs_in_body(body, out);
        }
        BytecodeInstruction::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            collect_type_refs_in_expr(start, out);
            collect_type_refs_in_expr(end, out);
            if let Some(s) = step {
                collect_type_refs_in_expr(s, out);
            }
            collect_type_refs_in_body(body, out);
        }
        BytecodeInstruction::Loop { body, .. } | BytecodeInstruction::RegionBlock { body, .. } => {
            collect_type_refs_in_body(body, out)
        }
        BytecodeInstruction::Match {
            scrutinee, arms, ..
        } => {
            collect_type_refs_in_expr(scrutinee, out);
            for arm in arms {
                collect_type_refs_in_body(&arm.body, out);
            }
        }
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => {
            collect_type_refs_in_body(body, out);
            collect_type_refs_in_body(catch_body, out);
        }
    }
}

fn collect_type_refs_in_expr(expr: &BytecodeExpr, out: &mut Vec<TypeRef>) {
    out.push(expr.ty.clone());
    match &expr.kind {
        BytecodeExprKind::Array(elems) => {
            for e in elems {
                collect_type_refs_in_expr(e, out);
            }
        }
        BytecodeExprKind::Index { target, index } => {
            collect_type_refs_in_expr(target, out);
            collect_type_refs_in_expr(index, out);
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            collect_type_refs_in_expr(expr, out);
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            collect_type_refs_in_expr(left, out);
            collect_type_refs_in_expr(right, out);
        }
        BytecodeExprKind::Call { args, .. } => {
            for a in args {
                collect_type_refs_in_expr(a, out);
            }
        }
        BytecodeExprKind::Field { target, .. } => collect_type_refs_in_expr(target, out),
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_)
        | BytecodeExprKind::Closure { .. } => {}
    }
}
