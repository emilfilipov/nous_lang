//! WASM monomorphization of user-defined generic types — A1 parity with native.
//!
//! A user-defined generic `struct`/`enum` instantiated with concrete type
//! arguments — `Box<i64>`, `Pair<string, i64>`, `Opt<string>`, `Either<i64, bool>`
//! — is monomorphized here into a fully-concrete layout keyed by its full
//! instantiation spelling, and registered into the WASM backend's `structs`/`enums`
//! tables so ALL the existing name-keyed layout machinery (construction, field/
//! payload read, value-semantic deep copy, `match`, and the by-pointer call
//! boundary) lays it out UNCHANGED — exactly as if the user had hand-written a
//! concrete `struct BoxString { value string }` / string-payload enum. The declared
//! type parameters are substituted with the instantiation's concrete arguments
//! (reusing the semantic `substitute_type`), mirroring the native backend's
//! per-backend monomorphization (`native_object_types::resolve_native_type`).
//!
//! Value-neutral: generics are erased on the interpreters, so a monomorphized
//! `Box<string>` has the identical linear-memory layout (one immutable-`string`
//! pointer word, shared on the value-semantic copy) to a hand-written string-field
//! struct — the WASM output agrees with the interpreter result bit-for-bit.
//!
//! Default-deny scope gate, matching native's A1 boundary EXACTLY: an instantiation
//! is registered only when its monomorphized layout is scalar-only OR scalars plus
//! one-level immutable `string` words. A deeper heap shape — a mutable heap field/
//! payload (`Stack<i64>`'s `list<i64>`, `Tree<i64>`'s `list<Tree<T>>`), a nested
//! heap-carrying aggregate, or a two-level `string` nesting — is NOT registered, so
//! its spelling stays unresolvable and the enclosing function skips cleanly to the
//! interpreters (`L0338`), never miscompiled.
//!
//! Purely additive: a module with no generic instantiations leaves both tables
//! untouched, so non-generic WASM output stays byte-identical (existing snapshots
//! are unaffected).

use super::*;
use crate::{IrEnumVariant, IrPlace};
use lullaby_semantics::substitute_type;
use std::collections::HashSet;

/// Monomorphize every reachable user-generic struct/enum instantiation in `module`
/// and register the SUPPORTED ones (scalar-only or one-level `string`) into
/// `structs`/`enums` under their full instantiation spelling (`Box<i64>`,
/// `Opt<string>`). Unsupported instantiations (deeper-than-one-level heap) are left
/// unregistered so their spelling stays unresolvable and the enclosing function
/// skips gracefully.
pub(crate) fn expand_generic_instantiations(
    module: &IrModule,
    structs: &mut HashMap<String, Vec<(String, TypeRef)>>,
    enums: &mut HashMap<String, IrEnumDef>,
) {
    // 1. Collect every user-generic instantiation reachable in the module's
    //    signatures and bodies. `for_each_type` recurses into nested generic
    //    arguments, so `Box<Pair<i64, bool>>` yields both `Box<Pair<i64, bool>>`
    //    and `Pair<i64, bool>`.
    let mut queue: Vec<TypeRef> = Vec::new();
    for function in &module.functions {
        for param in &function.params {
            collect_user_generics(&param.ty, module, &mut queue);
        }
        collect_user_generics(&function.return_type, module, &mut queue);
        collect_type_refs_in_body(&function.body, module, &mut queue);
    }

    // 2. Worklist: monomorphize each instantiation, registering it PROVISIONALLY,
    //    and enqueue any further user-generic instantiations that its SUBSTITUTED
    //    field/payload types introduce (`struct Wrap<T> { inner Box<T> }`
    //    instantiated `Wrap<i64>` introduces `Box<i64>`, which is not syntactically
    //    present in the `Wrap<i64>` spelling). This mirrors the native backend,
    //    where `substitute_type` + recursive `resolve_native_type` reach the same
    //    concrete set.
    let mut seen: HashSet<String> = HashSet::new();
    while let Some(ty) = queue.pop() {
        if !seen.insert(ty.name.clone()) {
            continue;
        }
        let Some((head, args)) = split_user_generic(&ty) else {
            continue;
        };
        if let Some(def) = module
            .structs
            .iter()
            .find(|s| s.name == head && !s.type_params.is_empty())
        {
            let subst = subst_map(&def.type_params, &args);
            let concrete: Vec<(String, TypeRef)> = def
                .fields
                .iter()
                .map(|(fname, fty)| (fname.clone(), substitute_type(fty, &subst)))
                .collect();
            for (_, fty) in &concrete {
                collect_user_generics(fty, module, &mut queue);
            }
            structs.insert(ty.name.clone(), concrete);
        } else if let Some(def) = module
            .enums
            .iter()
            .find(|e| e.name == head && !e.type_params.is_empty())
        {
            let subst = subst_map(&def.type_params, &args);
            let variants: Vec<IrEnumVariant> = def
                .variants
                .iter()
                .map(|v| IrEnumVariant {
                    name: v.name.clone(),
                    payload: v
                        .payload
                        .iter()
                        .map(|p| substitute_type(p, &subst))
                        .collect(),
                })
                .collect();
            for v in &variants {
                for p in &v.payload {
                    collect_user_generics(p, module, &mut queue);
                }
            }
            enums.insert(
                ty.name.clone(),
                IrEnumDef {
                    name: ty.name.clone(),
                    type_params: Vec::new(),
                    variants,
                },
            );
        }
    }

    // 3. Prune (default-deny) every registered instantiation whose monomorphized
    //    layout is NOT scalar-only-plus-one-level-`string`. A fixpoint, because
    //    removing an unsupported nested instantiation can make a dependent's field
    //    unresolvable and therefore unsupported too. Only registered instantiations
    //    (their spelling contains `<`) are considered; base declarations and
    //    non-generic structs/enums are never touched.
    loop {
        let struct_remove: Vec<String> = structs
            .iter()
            .filter(|(name, fields)| {
                name.contains('<')
                    && !fields
                        .iter()
                        .all(|(_, f)| is_one_level_string_or_scalar(f, structs, enums))
            })
            .map(|(name, _)| name.clone())
            .collect();
        let enum_remove: Vec<String> = enums
            .iter()
            .filter(|(name, def)| {
                name.contains('<')
                    && !def.variants.iter().all(|v| {
                        v.payload
                            .iter()
                            .all(|p| is_one_level_string_or_scalar(p, structs, enums))
                    })
            })
            .map(|(name, _)| name.clone())
            .collect();
        if struct_remove.is_empty() && enum_remove.is_empty() {
            break;
        }
        for name in struct_remove {
            structs.remove(&name);
        }
        for name in enum_remove {
            enums.remove(&name);
        }
    }
}

/// Build the substitution map pinning each declared type parameter to its concrete
/// argument (`["T"]` + `["i64"]` -> `{T: i64}`). Semantics has already validated
/// arity; a shorter argument list leaves surplus parameters unbound (so a field
/// type fails to resolve and the instantiation is pruned/skipped).
fn subst_map(type_params: &[String], args: &[TypeRef]) -> HashMap<String, TypeRef> {
    type_params
        .iter()
        .cloned()
        .zip(args.iter().cloned())
        .collect()
}

/// Split a `TypeRef` spelling into `(head, args)` when it is a `Name<args>` generic
/// instantiation (`Box<i64>` -> `("Box", ["i64"])`). `None` for a plain name, a
/// function type (`fn(...) -> R`), or a non-`<...>` spelling. Not restricted to
/// USER generics — the caller checks the head against the declared generic set.
fn split_user_generic(ty: &TypeRef) -> Option<(String, Vec<TypeRef>)> {
    let open = ty.name.find('<')?;
    if !ty.name.ends_with('>') || ty.name.starts_with("fn(") {
        return None;
    }
    let head = ty.name[..open].to_string();
    let args = ty.generic_args(&head)?;
    Some((head, args))
}

/// Whether `ty` is an instantiation of a declared USER generic `struct`/`enum`
/// (`Box<i64>`, `Opt<string>`) — a `Name<args>` spelling whose head names a
/// declared type with non-empty type parameters. Built-in generics (`list<...>`,
/// `option<...>`, `map<...>`) are not user generics.
fn is_user_generic(ty: &TypeRef, module: &IrModule) -> bool {
    let Some((head, _)) = split_user_generic(ty) else {
        return false;
    };
    module
        .structs
        .iter()
        .any(|s| s.name == head && !s.type_params.is_empty())
        || module
            .enums
            .iter()
            .any(|e| e.name == head && !e.type_params.is_empty())
}

/// Collect every user-generic instantiation reachable from `ty` (itself and every
/// nested generic argument) into `out`. `Box<Pair<i64, bool>>` yields both
/// `Box<Pair<i64, bool>>` and `Pair<i64, bool>`; `list<Box<i64>>` yields `Box<i64>`.
fn collect_user_generics(ty: &TypeRef, module: &IrModule, out: &mut Vec<TypeRef>) {
    for_each_type(ty, &mut |t| {
        if is_user_generic(t, module) {
            out.push(t.clone());
        }
    });
}

/// Visit `ty` and every nested generic argument, at any depth. Decomposes ANY
/// `head<args>` spelling (built-in or user) so nested user generics inside a
/// `list`/`map`/`option`/user-generic argument are reached.
fn for_each_type<F: FnMut(&TypeRef)>(ty: &TypeRef, f: &mut F) {
    f(ty);
    if let Some((_, args)) = split_user_generic(ty) {
        for arg in &args {
            for_each_type(arg, f);
        }
    }
}

/// A field/payload slot a monomorphized generic instantiation may carry at the TOP
/// level: an immutable `string` word (one-level heap), or an entirely scalar-only
/// cell/aggregate. A `string` reachable only through a NESTED aggregate is a
/// two-level nesting and is rejected (falls through [`is_scalar_only`]); a mutable
/// heap value (`list`/`map`) is likewise rejected. Mirrors native's
/// `is_one_level_string_or_scalar`.
fn is_one_level_string_or_scalar(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> bool {
    ty.name == "string" || is_scalar_only(ty, structs, enums)
}

/// Whether `ty` resolves to an ENTIRELY scalar layout — an `i64`/fixed-width/`bool`/
/// `char`/`byte` cell, an `f64`/`f32`, or a `struct`/`array`/`enum` whose whole
/// contents are recursively scalar. A `string`, `list`, or `map` (at any depth)
/// makes it non-scalar. Nested struct/enum spellings (including registered generic
/// instantiations) are resolved by name against `structs`/`enums`. Mirrors native's
/// `is_scalar_only_layout`.
fn is_scalar_only(
    ty: &TypeRef,
    structs: &HashMap<String, Vec<(String, TypeRef)>>,
    enums: &HashMap<String, IrEnumDef>,
) -> bool {
    if scalar_val_type(ty).is_some() {
        return true;
    }
    if ty.name == "string" || ty.list_element().is_some() || ty.map_args().is_some() {
        return false;
    }
    if let Some(elem) = ty.array_element() {
        return is_scalar_only(&elem, structs, enums);
    }
    if let Some(fields) = structs.get(&ty.name) {
        return fields
            .iter()
            .all(|(_, f)| is_scalar_only(f, structs, enums));
    }
    if let Some(inner) = ty.option_element() {
        return is_scalar_only(&inner, structs, enums);
    }
    if let Some((ok, err)) = ty.result_args() {
        return is_scalar_only(&ok, structs, enums) && is_scalar_only(&err, structs, enums);
    }
    if let Some(def) = enums.get(&ty.name) {
        return def
            .variants
            .iter()
            .all(|v| v.payload.iter().all(|p| is_scalar_only(p, structs, enums)));
    }
    false
}

// -- Body traversal: collect the user-generic instantiations a function uses -----
//
// Mirrors the native `collect_type_refs_in_*` walk (over `BytecodeInstruction`),
// but over the WASM backend's `IrStmt`/`IrExpr` — a `let`/parameter/annotated type,
// a match scrutinee's type, and every expression's static type (which carries the
// concrete instantiation of a construction node).

fn collect_type_refs_in_body(body: &[IrStmt], module: &IrModule, out: &mut Vec<TypeRef>) {
    for stmt in body {
        collect_type_refs_in_stmt(stmt, module, out);
    }
}

fn collect_type_refs_in_stmt(stmt: &IrStmt, module: &IrModule, out: &mut Vec<TypeRef>) {
    match stmt {
        IrStmt::Let { ty, value, .. } => {
            collect_user_generics(ty, module, out);
            collect_type_refs_in_expr(value, module, out);
        }
        IrStmt::Assign { path, value, .. } => {
            for place in path {
                if let IrPlace::Index(e) = place {
                    collect_type_refs_in_expr(e, module, out);
                }
            }
            collect_type_refs_in_expr(value, module, out);
        }
        IrStmt::Return(Some(v)) | IrStmt::Expr(v) | IrStmt::Throw { value: v, .. } => {
            collect_type_refs_in_expr(v, module, out);
        }
        IrStmt::Return(None) | IrStmt::Break(_) | IrStmt::Continue(_) | IrStmt::Asm { .. } => {}
        IrStmt::If {
            branches,
            else_body,
            ..
        } => {
            for branch in branches {
                collect_type_refs_in_expr(&branch.condition, module, out);
                collect_type_refs_in_body(&branch.body, module, out);
            }
            collect_type_refs_in_body(else_body, module, out);
        }
        IrStmt::While {
            condition, body, ..
        } => {
            collect_type_refs_in_expr(condition, module, out);
            collect_type_refs_in_body(body, module, out);
        }
        IrStmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            collect_type_refs_in_expr(start, module, out);
            collect_type_refs_in_expr(end, module, out);
            if let Some(s) = step {
                collect_type_refs_in_expr(s, module, out);
            }
            collect_type_refs_in_body(body, module, out);
        }
        IrStmt::Loop { body, .. } | IrStmt::RegionBlock { body, .. } => {
            collect_type_refs_in_body(body, module, out)
        }
        IrStmt::Try {
            body, catch_body, ..
        } => {
            collect_type_refs_in_body(body, module, out);
            collect_type_refs_in_body(catch_body, module, out);
        }
        IrStmt::Match {
            scrutinee, arms, ..
        } => {
            collect_type_refs_in_expr(scrutinee, module, out);
            for arm in arms {
                collect_type_refs_in_body(&arm.body, module, out);
            }
        }
    }
}

fn collect_type_refs_in_expr(expr: &IrExpr, module: &IrModule, out: &mut Vec<TypeRef>) {
    collect_user_generics(&expr.ty, module, out);
    match &expr.kind {
        IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
        | IrExprKind::Variable(_)
        | IrExprKind::Local { .. }
        | IrExprKind::Closure { .. } => {}
        IrExprKind::Array(items) => {
            for item in items {
                collect_type_refs_in_expr(item, module, out);
            }
        }
        IrExprKind::Index { target, index } => {
            collect_type_refs_in_expr(target, module, out);
            collect_type_refs_in_expr(index, module, out);
        }
        IrExprKind::Unary { expr, .. } | IrExprKind::Await { expr } => {
            collect_type_refs_in_expr(expr, module, out);
        }
        IrExprKind::Binary { left, right, .. } => {
            collect_type_refs_in_expr(left, module, out);
            collect_type_refs_in_expr(right, module, out);
        }
        IrExprKind::Call { args, .. } => {
            for arg in args {
                collect_type_refs_in_expr(arg, module, out);
            }
        }
        IrExprKind::Field { target, .. } => collect_type_refs_in_expr(target, module, out),
    }
}
