//! Generic-inference helpers split out of `lib.rs`: constructor decomposition,
//! parameter unification, type-variable substitution, `Self` substitution, and
//! call-site return-type inference for generic functions.
//!
//! This is a behavior-preserving move. The checker and
//! `semantics_checker_calls.rs` reach these via crate-root re-exports; the
//! previously-`pub` items keep the same `lullaby_semantics::` public paths.

use std::collections::HashMap;

use lullaby_parser::{TypeRef, function_type, generic_type};

use super::Signature;

/// The outcome of unifying a call's arguments against a generic function's
/// parameter types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenericInferenceError {
    /// A type variable received two different concrete types (`L0395`).
    Conflict {
        param: String,
        first: TypeRef,
        second: TypeRef,
    },
    /// A type variable in the return type was never pinned by an argument
    /// (`L0396`).
    Unresolved { param: String },
}

/// Split a `ctor<...>` spelling into `(ctor, args)` when it is a compound
/// generic type. Recognizes every built-in generic constructor plus function
/// types spelled `fn(...) -> R`. Returns `None` for a plain type name.
pub(crate) fn decompose_generic(ty: &TypeRef) -> Option<(String, Vec<TypeRef>)> {
    if let Some((params, ret)) = ty.function_signature() {
        let mut args = params;
        args.push(ret);
        return Some(("fn".to_string(), args));
    }
    for ctor in [
        "array", "list", "option", "result", "map", "ptr", "ref", "rc",
    ] {
        if let Some(args) = ty.generic_args(ctor) {
            return Some((ctor.to_string(), args));
        }
    }
    None
}

/// Structurally unify a parameter type (which may contain type variables drawn
/// from `type_params`) against a concrete argument type, extending `subst`.
///
/// - A parameter type that is exactly a type-variable name binds that variable
///   to the whole argument type, reporting `L0395` on a conflicting rebinding.
/// - A compound parameter type (`list<T>`, `option<T>`, `fn(T) -> U`, ...)
///   unifies argument-wise against a same-constructor argument type.
/// - Any non-variable, non-compound parameter type is a fixed type and is not
///   unified here (ordinary argument type checking already validates it).
pub fn unify_param(
    param: &TypeRef,
    actual: &TypeRef,
    type_params: &[String],
    subst: &mut HashMap<String, TypeRef>,
) -> Result<(), GenericInferenceError> {
    if type_params.iter().any(|name| name == &param.name) {
        // `param` is a bare type variable: bind it to the whole actual type.
        match subst.get(&param.name) {
            Some(existing) if existing != actual => {
                return Err(GenericInferenceError::Conflict {
                    param: param.name.clone(),
                    first: existing.clone(),
                    second: actual.clone(),
                });
            }
            Some(_) => {}
            None => {
                subst.insert(param.name.clone(), actual.clone());
            }
        }
        return Ok(());
    }

    // A compound parameter type unifies component-wise when the argument shares
    // its constructor and arity. If the argument does not match structurally the
    // ordinary argument type check reports the mismatch, so we simply stop.
    if let (Some((param_ctor, param_args)), Some((actual_ctor, actual_args))) =
        (decompose_generic(param), decompose_generic(actual))
        && param_ctor == actual_ctor
        && param_args.len() == actual_args.len()
    {
        for (p, a) in param_args.iter().zip(actual_args.iter()) {
            unify_param(p, a, type_params, subst)?;
        }
    }
    Ok(())
}

/// Substitute inferred type variables into a declared type. Applies to a bare
/// type variable and recursively into compound types (`option<T>`, `fn(T) -> U`,
/// ...). Type variables with no binding are left as-is; the caller detects those
/// via `unresolved_type_vars`.
pub fn substitute_type(ty: &TypeRef, subst: &HashMap<String, TypeRef>) -> TypeRef {
    if let Some(bound) = subst.get(&ty.name) {
        return bound.clone();
    }
    if let Some((ctor, args)) = decompose_generic(ty) {
        let mapped: Vec<TypeRef> = args.iter().map(|arg| substitute_type(arg, subst)).collect();
        if ctor == "fn" {
            let (params, ret) = mapped.split_at(mapped.len() - 1);
            return function_type(params, &ret[0]);
        }
        return generic_type(&ctor, &mapped);
    }
    ty.clone()
}

/// Report the first declared type parameter that still appears unbound in `ty`
/// after substitution (`L0396`). Recurses into compound types.
pub fn first_unresolved_type_var(
    ty: &TypeRef,
    type_params: &[String],
    subst: &HashMap<String, TypeRef>,
) -> Option<String> {
    if type_params.iter().any(|name| name == &ty.name) && !subst.contains_key(&ty.name) {
        return Some(ty.name.clone());
    }
    if let Some((_, args)) = decompose_generic(ty) {
        for arg in &args {
            if let Some(found) = first_unresolved_type_var(arg, type_params, subst) {
                return Some(found);
            }
        }
    }
    None
}

/// True when `ty` mentions the type variable `var` anywhere (as the whole type
/// or nested inside a compound type).
pub(crate) fn type_contains_var(ty: &TypeRef, var: &str) -> bool {
    if ty.name == var {
        return true;
    }
    if let Some((_, args)) = decompose_generic(ty) {
        return args.iter().any(|arg| type_contains_var(arg, var));
    }
    false
}

/// Run full call-site inference for a generic function: unify every argument
/// against the corresponding parameter, then substitute into the return type.
/// Returns the concrete return type, or the first inference error.
pub fn infer_generic_return(
    signature: &Signature,
    arg_types: &[TypeRef],
) -> Result<TypeRef, GenericInferenceError> {
    let mut subst: HashMap<String, TypeRef> = HashMap::new();
    for (param, actual) in signature.params.iter().zip(arg_types.iter()) {
        unify_param(param, actual, &signature.type_params, &mut subst)?;
    }
    if let Some(param) =
        first_unresolved_type_var(&signature.return_type, &signature.type_params, &subst)
    {
        return Err(GenericInferenceError::Unresolved { param });
    }
    Ok(substitute_type(&signature.return_type, &subst))
}

/// Replace the `Self` type variable with the implementing type. Recurses into
/// compound generic types (`option<Self>`, `list<Self>`, ...).
pub(crate) fn substitute_self(ty: &TypeRef, self_ty: &TypeRef) -> TypeRef {
    let mut subst: HashMap<String, TypeRef> = HashMap::new();
    subst.insert("Self".to_string(), self_ty.clone());
    substitute_type(ty, &subst)
}
