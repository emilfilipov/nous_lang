//! Type-alias resolution split out of `lib.rs`: expands user `alias`
//! declarations to their canonical types across a whole `Program` before any
//! checking, so the rest of the pipeline (and IR/runtime) never sees an alias.
//!
//! This is a behavior-preserving move. `resolve_program_aliases` is re-exported
//! at the crate root for `validate`; the remaining helpers stay module-private.

use std::collections::{HashMap, HashSet};

use lullaby_parser::{
    EnumDecl, EnumVariant, Expr, ExprKind, Function, IfBranch, MatchArm, Param, Program, Stmt,
    StructDecl, StructField, TypeRef,
};

use super::SemanticDiagnostic;

/// Resolve all type aliases in a program to canonical types, returning the
/// rewritten program plus any alias-definition diagnostics (duplicate `L0360`,
/// cyclic `L0361`).
pub(crate) fn resolve_program_aliases(program: &Program) -> (Program, Vec<SemanticDiagnostic>) {
    let mut diagnostics = Vec::new();
    let mut map: HashMap<String, TypeRef> = HashMap::new();
    for alias in &program.aliases {
        if map.contains_key(&alias.name) {
            diagnostics.push(SemanticDiagnostic::at(
                "L0360",
                format!("duplicate type alias `{}`", alias.name),
                None,
                alias.span,
            ));
            continue;
        }
        map.insert(alias.name.clone(), alias.target.clone());
    }

    // Detect cyclic alias chains (e.g. `alias A = B` / `alias B = A`).
    for alias in &program.aliases {
        if chain_is_cyclic(&alias.name, &map) {
            diagnostics.push(SemanticDiagnostic::at(
                "L0361",
                format!("type alias `{}` is defined in terms of itself", alias.name),
                None,
                alias.span,
            ));
        }
    }

    let functions = program
        .functions
        .iter()
        .map(|function| Function {
            name: function.name.clone(),
            type_params: function.type_params.clone(),
            params: function
                .params
                .iter()
                .map(|param| Param {
                    name: param.name.clone(),
                    ty: resolve_alias_type(&param.ty, &map),
                })
                .collect(),
            return_type: resolve_alias_type(&function.return_type, &map),
            body: function
                .body
                .iter()
                .map(|stmt| rewrite_stmt_types(stmt, &map))
                .collect(),
            span: function.span,
            is_public: function.is_public,
            is_async: function.is_async,
            is_extern: function.is_extern,
            is_export: function.is_export,
        })
        .collect();

    let structs = program
        .structs
        .iter()
        .map(|declaration| StructDecl {
            name: declaration.name.clone(),
            fields: declaration
                .fields
                .iter()
                .map(|field| StructField {
                    name: field.name.clone(),
                    ty: resolve_alias_type(&field.ty, &map),
                })
                .collect(),
            span: declaration.span,
            is_public: declaration.is_public,
        })
        .collect();

    let enums = program
        .enums
        .iter()
        .map(|declaration| EnumDecl {
            name: declaration.name.clone(),
            variants: declaration
                .variants
                .iter()
                .map(|variant| EnumVariant {
                    name: variant.name.clone(),
                    payload: variant
                        .payload
                        .iter()
                        .map(|ty| resolve_alias_type(ty, &map))
                        .collect(),
                })
                .collect(),
            span: declaration.span,
            is_public: declaration.is_public,
        })
        .collect();

    (
        Program {
            functions,
            aliases: program.aliases.clone(),
            structs,
            enums,
            imports: program.imports.clone(),
            // Trait/impl method types are resolved against aliases below via the
            // same `resolve_alias_type` mapping so the checker never sees an alias.
            traits: program
                .traits
                .iter()
                .map(|decl| lullaby_parser::TraitDecl {
                    name: decl.name.clone(),
                    methods: decl
                        .methods
                        .iter()
                        .map(|method| lullaby_parser::MethodSig {
                            name: method.name.clone(),
                            params: method
                                .params
                                .iter()
                                .map(|param| Param {
                                    name: param.name.clone(),
                                    ty: resolve_alias_type(&param.ty, &map),
                                })
                                .collect(),
                            return_type: resolve_alias_type(&method.return_type, &map),
                            span: method.span,
                        })
                        .collect(),
                    span: decl.span,
                    is_public: decl.is_public,
                })
                .collect(),
            impls: program
                .impls
                .iter()
                .map(|decl| lullaby_parser::ImplDecl {
                    trait_name: decl.trait_name.clone(),
                    type_name: decl.type_name.clone(),
                    methods: decl
                        .methods
                        .iter()
                        .map(|function| Function {
                            name: function.name.clone(),
                            type_params: function.type_params.clone(),
                            params: function
                                .params
                                .iter()
                                .map(|param| Param {
                                    name: param.name.clone(),
                                    ty: resolve_alias_type(&param.ty, &map),
                                })
                                .collect(),
                            return_type: resolve_alias_type(&function.return_type, &map),
                            body: function
                                .body
                                .iter()
                                .map(|stmt| rewrite_stmt_types(stmt, &map))
                                .collect(),
                            span: function.span,
                            is_public: function.is_public,
                            is_async: function.is_async,
                            is_extern: function.is_extern,
                            is_export: function.is_export,
                        })
                        .collect(),
                    span: decl.span,
                })
                .collect(),
        },
        diagnostics,
    )
}

/// True if following the alias chain from `name` revisits `name` (a cycle).
fn chain_is_cyclic(name: &str, map: &HashMap<String, TypeRef>) -> bool {
    let mut seen = HashSet::new();
    let mut current = name.to_string();
    while let Some(target) = map.get(&current) {
        if !map.contains_key(&target.name) {
            return false;
        }
        current = target.name.clone();
        if current == name {
            return true;
        }
        if !seen.insert(current.clone()) {
            return false;
        }
    }
    false
}

/// Expand alias names inside a type, including generic arguments, to canonical
/// form. Bounded by a depth guard so cyclic aliases cannot loop forever.
fn resolve_alias_type(ty: &TypeRef, map: &HashMap<String, TypeRef>) -> TypeRef {
    resolve_alias_type_depth(ty, map, 0)
}

fn resolve_alias_type_depth(ty: &TypeRef, map: &HashMap<String, TypeRef>, depth: usize) -> TypeRef {
    if depth > 32 {
        return ty.clone();
    }
    for ctor in ["array", "ptr", "ref", "rc"] {
        if let Some(inner) = ty.generic_arg(ctor) {
            let resolved = resolve_alias_type_depth(&inner, map, depth + 1);
            return TypeRef::new(format!("{ctor}<{}>", resolved.name));
        }
    }
    if let Some(target) = map.get(&ty.name) {
        return resolve_alias_type_depth(target, map, depth + 1);
    }
    ty.clone()
}

/// Rewrite alias types in a statement's type annotations, recursing into blocks.
fn rewrite_stmt_types(stmt: &Stmt, map: &HashMap<String, TypeRef>) -> Stmt {
    match stmt {
        Stmt::Let {
            name,
            ty,
            value,
            span,
        } => Stmt::Let {
            name: name.clone(),
            ty: ty.as_ref().map(|ty| resolve_alias_type(ty, map)),
            value: value.clone(),
            span: *span,
        },
        Stmt::If {
            branches,
            else_body,
            span,
        } => Stmt::If {
            branches: branches
                .iter()
                .map(|branch| IfBranch {
                    condition: branch.condition.clone(),
                    body: branch
                        .body
                        .iter()
                        .map(|stmt| rewrite_stmt_types(stmt, map))
                        .collect(),
                })
                .collect(),
            else_body: else_body
                .iter()
                .map(|stmt| rewrite_stmt_types(stmt, map))
                .collect(),
            span: *span,
        },
        Stmt::While {
            condition,
            body,
            span,
        } => Stmt::While {
            condition: condition.clone(),
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::For {
            name,
            start,
            end,
            step,
            body,
            span,
        } => Stmt::For {
            name: name.clone(),
            start: start.clone(),
            end: end.clone(),
            step: step.clone(),
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::ForEach {
            name,
            iterable,
            body,
            span,
        } => Stmt::ForEach {
            name: name.clone(),
            iterable: iterable.clone(),
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::Loop { body, span } => Stmt::Loop {
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::Unsafe { body, span } => Stmt::Unsafe {
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            span: *span,
        },
        Stmt::Try {
            body,
            catch_name,
            catch_body,
            span,
        } => Stmt::Try {
            body: body.iter().map(|s| rewrite_stmt_types(s, map)).collect(),
            catch_name: catch_name.clone(),
            catch_body: catch_body
                .iter()
                .map(|s| rewrite_stmt_types(s, map))
                .collect(),
            span: *span,
        },
        // A `match` reaches semantics wrapped in a `Stmt::Expr`; rewrite type
        // annotations inside its arm bodies so aliases in arm `let`s resolve.
        Stmt::Expr(Expr {
            kind: ExprKind::Match { scrutinee, arms },
            span,
        }) => Stmt::Expr(Expr {
            kind: ExprKind::Match {
                scrutinee: scrutinee.clone(),
                arms: arms
                    .iter()
                    .map(|arm| MatchArm {
                        pattern: arm.pattern.clone(),
                        body: arm
                            .body
                            .iter()
                            .map(|s| rewrite_stmt_types(s, map))
                            .collect(),
                    })
                    .collect(),
            },
            span: *span,
        }),
        other => other.clone(),
    }
}
