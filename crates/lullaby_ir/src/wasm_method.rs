//! WASM inherent-method dispatch: expand receiver-dispatched method calls into
//! ordinary direct calls to monomorphized method-instance functions — the WASM
//! analogue of [`crate::native_object::expand_method_instances`], operating
//! on the WASM backend's `IrModule`/`IrFunction`/`IrStmt`/`IrExpr` instead of the
//! native backend's `BytecodeModule`.
//!
//! # Representation the WASM backend consumes
//!
//! A source method call `recv.method(args)` is desugared **at parse time** (UFCS)
//! into an ordinary [`IrExprKind::Call`] `{ name: "method", args: [recv, args...] }`
//! — there is no distinct method-call IR node, and the receiver is simply `args[0]`.
//! The method bodies live in [`IrModule::impls`] (each an [`crate::IrImplMethod`] keyed by
//! `(type_name, method_name)`), **not** in [`IrModule::functions`], and the set of
//! names that are receiver-dispatched (trait methods plus inherent `impl Type<T>`
//! methods) is [`IrModule::trait_methods`]. The interpreters dispatch such a call by
//! the receiver's *runtime* type; the WASM backend resolves it statically by the
//! receiver's *static* type, exactly like native.
//!
//! # What this pass does
//!
//! [`expand_method_instances`] returns a NEW module (used only for WASM emission —
//! the interpreters keep the original) in which:
//!
//! - every method-call site whose receiver resolves to a concrete struct/enum type
//!   (including a monomorphized generic instantiation like `Box<i64>`) that has a
//!   matching `impl` method is rewritten from the bare method name to a unique,
//!   collision-free **mangled** instance name (`$mth$Box_i64_$peek`); and
//! - for each such instance, a synthesized [`IrFunction`] under that mangled name is
//!   appended to `functions`, with the method body monomorphized — every `TypeRef`
//!   (params, return, and every embedded expression/`let`/`match` type) has the
//!   receiver's type arguments substituted (`{T: i64}`), reusing the same
//!   `substitute_type` the generic-type layout path uses.
//!
//! The synthesized function's `self` parameter is an ordinary aggregate parameter
//! (`self: Box<i64>`), so it flows through the **existing** WASM aggregate-argument
//! ABI (an `i32` pointer, deep-copied at the call site for a mutable aggregate — see
//! `wasm_lowering::lower_expr`'s call arm and `emit_deep_copy`). Because a method is
//! thus just a function whose first argument is a copied aggregate, the interpreters'
//! by-value `self` (each call clones the receiver `Value`) is matched: mutating
//! `self` inside a method cannot affect the caller, and a method returning a fresh
//! aggregate leaves the receiver unchanged.
//!
//! # Default-deny
//!
//! A call is rewritten to a direct instance call ONLY when the receiver type is a
//! concrete user struct/enum with a resolvable impl. Anything else — a bare type
//! parameter `T`, a trait-object/dynamic receiver, a builtin whose name happens to
//! collide, or a receiver whose monomorphized layout is outside the WASM subset — is
//! left untouched. An untouched name that is not a compiled function, or a
//! synthesized instance that later fails WASM eligibility/lowering, demotes its
//! caller through the existing WASM skip fixpoint, so the affected function skips
//! cleanly (`L0338`) rather than miscompiling. This is the WASM analogue of native's
//! `L0339` skip and enforces the **same** default-deny boundary: the method pass is a
//! pure mechanical rewrite, and the per-backend eligibility (which method shapes each
//! backend can lay out) is what decides compile-vs-skip, exactly as on native.
//!
//! Purely additive: a module that declares no receiver-dispatched methods is
//! structurally cloned with byte-identical function bodies (identity substitution, no
//! name rewrites), so existing non-method WASM snapshots are unaffected.

use super::*;
use crate::{IrIfBranch, IrMatchArm, IrParam, IrPlace};
use lullaby_semantics::substitute_type;
use std::collections::{HashMap, HashSet};

/// A method instance discovered at a call site and queued for synthesis: the unique
/// mangled symbol name, the (borrowed) generic impl-method body to monomorphize, and
/// the `{type_param -> concrete}` substitution for this receiver instantiation (empty
/// for a non-generic receiver).
struct MethodInstance<'a> {
    mangled: String,
    impl_fn: &'a IrFunction,
    subst: HashMap<String, TypeRef>,
}

struct MethodExpander<'a> {
    /// `(base_type_name, method_name)` -> the generic impl-method body.
    impls: HashMap<(String, String), &'a IrFunction>,
    /// Names that are receiver-dispatched (trait + inherent method names).
    method_names: HashSet<&'a str>,
    structs: &'a [IrStructDef],
    enums: &'a [IrEnumDef],
    /// Mangled instance names already queued/synthesized, for dedup.
    seen: HashSet<String>,
    /// Instances still to synthesize.
    worklist: Vec<MethodInstance<'a>>,
}

/// Expand every WASM-resolvable inherent/impl method call in `module` into a direct
/// call to a synthesized, monomorphized instance function. Returns a new module for
/// WASM emission; the input is left untouched. When the module declares no
/// receiver-dispatched methods this is a structural clone whose function bodies are
/// byte-identical to the input (identity substitution, no name rewrites), so existing
/// WASM snapshots are unaffected.
pub(crate) fn expand_method_instances(module: &IrModule) -> IrModule {
    let mut expander = MethodExpander::new(module);

    // Transform every top-level function body (identity substitution). Method calls
    // are rewritten to mangled names and their instances queued.
    let mut functions: Vec<IrFunction> = Vec::with_capacity(module.functions.len());
    let empty: HashMap<String, TypeRef> = HashMap::new();
    for function in &module.functions {
        functions.push(IrFunction {
            name: function.name.clone(),
            params: function.params.clone(),
            return_type: function.return_type.clone(),
            body: expander.transform_stmts(&function.body, &empty),
            span: function.span,
        });
    }

    // Drain the worklist, synthesizing each queued instance (which may queue more via
    // chained method calls). An instance whose generic body contains a closure is
    // skipped (its shared closure body cannot be monomorphized per instantiation); the
    // caller then skips through the WASM fixpoint.
    while let Some(instance) = expander.worklist.pop() {
        if let Some(function) = expander.synthesize(&instance) {
            functions.push(function);
        }
    }

    IrModule {
        functions,
        structs: module.structs.clone(),
        enums: module.enums.clone(),
        impls: module.impls.clone(),
        trait_methods: module.trait_methods.clone(),
        async_functions: module.async_functions.clone(),
        extern_functions: module.extern_functions.clone(),
        extern_signatures: module.extern_signatures.clone(),
        export_functions: module.export_functions.clone(),
        closures: module.closures.clone(),
    }
}

impl<'a> MethodExpander<'a> {
    fn new(module: &'a IrModule) -> Self {
        let mut impls = HashMap::new();
        for method in &module.impls {
            impls.insert(
                (method.type_name.clone(), method.method_name.clone()),
                &method.function,
            );
        }
        let method_names = module.trait_methods.iter().map(String::as_str).collect();
        Self {
            impls,
            method_names,
            structs: &module.structs,
            enums: &module.enums,
            seen: HashSet::new(),
            worklist: Vec::new(),
        }
    }

    /// Synthesize a monomorphized method-instance function, or `None` when the
    /// instance is out of scope for a per-instantiation body (a generic method whose
    /// body contains a closure — the closure body is shared by id across
    /// instantiations and cannot be safely monomorphized). Returning `None` leaves the
    /// caller's rewritten call dangling, so the caller skips cleanly.
    fn synthesize(&mut self, instance: &MethodInstance<'a>) -> Option<IrFunction> {
        let impl_fn = instance.impl_fn;
        let subst = &instance.subst;
        if !subst.is_empty() && stmts_contain_closure(&impl_fn.body) {
            return None;
        }
        let params = impl_fn
            .params
            .iter()
            .map(|param| IrParam {
                name: param.name.clone(),
                ty: substitute_type(&param.ty, subst),
            })
            .collect();
        let return_type = substitute_type(&impl_fn.return_type, subst);
        let body = self.transform_stmts(&impl_fn.body, subst);
        Some(IrFunction {
            name: instance.mangled.clone(),
            params,
            return_type,
            body,
            span: impl_fn.span,
        })
    }

    /// Resolve a `Call` to a method instance when its (already type-substituted)
    /// receiver is a concrete user struct/enum with a matching impl. Returns `None` for
    /// a non-method name, a non-concrete/non-user receiver, or a receiver with no
    /// matching impl — in every such case the call is left unchanged.
    fn resolve(&self, name: &str, receiver: &TypeRef) -> Option<MethodInstance<'a>> {
        if !self.method_names.contains(name) {
            return None;
        }
        let (base, type_args) = self.split_receiver(receiver)?;
        let impl_fn = self.impls.get(&(base.clone(), name.to_string())).copied()?;
        let type_params = self.type_params_of(&base);
        // A non-generic base (`type_params` empty, no type args) needs no
        // substitution. Any arity mismatch (which a checked program never produces) is
        // treated as unresolvable so the call is left untouched.
        if type_params.len() != type_args.len() {
            return None;
        }
        let subst: HashMap<String, TypeRef> = type_params.iter().cloned().zip(type_args).collect();
        Some(MethodInstance {
            mangled: mangle_method(&receiver.name, name),
            impl_fn,
            subst,
        })
    }

    /// Split a concrete receiver type into `(base_user_type, type_args)`. Returns
    /// `None` unless the head names a declared user struct/enum: a scalar, `string`, a
    /// builtin generic (`list<..>`), a bare type parameter, or a function type never
    /// resolves to a user impl and is left untouched.
    fn split_receiver(&self, ty: &TypeRef) -> Option<(String, Vec<TypeRef>)> {
        if let Some(open) = ty.name.find('<') {
            if !ty.name.ends_with('>') || ty.name.starts_with("fn(") {
                return None;
            }
            let head = ty.name[..open].to_string();
            if !self.is_user_type(&head) {
                return None;
            }
            let args = ty.generic_args(&head)?;
            Some((head, args))
        } else if self.is_user_type(&ty.name) {
            Some((ty.name.clone(), Vec::new()))
        } else {
            None
        }
    }

    fn is_user_type(&self, name: &str) -> bool {
        self.structs.iter().any(|s| s.name == name) || self.enums.iter().any(|e| e.name == name)
    }

    fn type_params_of(&self, base: &str) -> Vec<String> {
        if let Some(s) = self.structs.iter().find(|s| s.name == base) {
            return s.type_params.clone();
        }
        if let Some(e) = self.enums.iter().find(|e| e.name == base) {
            return e.type_params.clone();
        }
        Vec::new()
    }

    fn enqueue(&mut self, instance: MethodInstance<'a>) {
        if self.seen.insert(instance.mangled.clone()) {
            self.worklist.push(instance);
        }
    }

    // -- Body transformation (type substitution + method-call rewriting) ----------

    fn transform_stmts(
        &mut self,
        stmts: &[IrStmt],
        subst: &HashMap<String, TypeRef>,
    ) -> Vec<IrStmt> {
        stmts
            .iter()
            .map(|stmt| self.transform_stmt(stmt, subst))
            .collect()
    }

    fn transform_stmt(&mut self, stmt: &IrStmt, subst: &HashMap<String, TypeRef>) -> IrStmt {
        match stmt {
            IrStmt::Let {
                name,
                ty,
                value,
                span,
            } => IrStmt::Let {
                name: name.clone(),
                ty: substitute_type(ty, subst),
                value: self.transform_expr(value, subst),
                span: *span,
            },
            IrStmt::Assign {
                name,
                path,
                op,
                value,
                span,
            } => IrStmt::Assign {
                name: name.clone(),
                path: path
                    .iter()
                    .map(|place| self.transform_place(place, subst))
                    .collect(),
                op: *op,
                value: self.transform_expr(value, subst),
                span: *span,
            },
            IrStmt::Return(value) => {
                IrStmt::Return(value.as_ref().map(|v| self.transform_expr(v, subst)))
            }
            IrStmt::Break(span) => IrStmt::Break(*span),
            IrStmt::Continue(span) => IrStmt::Continue(*span),
            IrStmt::Expr(value) => IrStmt::Expr(self.transform_expr(value, subst)),
            IrStmt::If {
                branches,
                else_body,
                span,
            } => IrStmt::If {
                branches: branches
                    .iter()
                    .map(|branch| IrIfBranch {
                        condition: self.transform_expr(&branch.condition, subst),
                        body: self.transform_stmts(&branch.body, subst),
                    })
                    .collect(),
                else_body: self.transform_stmts(else_body, subst),
                span: *span,
            },
            IrStmt::While {
                condition,
                body,
                span,
            } => IrStmt::While {
                condition: self.transform_expr(condition, subst),
                body: self.transform_stmts(body, subst),
                span: *span,
            },
            IrStmt::For {
                name,
                start,
                end,
                step,
                body,
                span,
            } => IrStmt::For {
                name: name.clone(),
                start: self.transform_expr(start, subst),
                end: self.transform_expr(end, subst),
                step: step.as_ref().map(|s| self.transform_expr(s, subst)),
                body: self.transform_stmts(body, subst),
                span: *span,
            },
            IrStmt::Loop { body, span } => IrStmt::Loop {
                body: self.transform_stmts(body, subst),
                span: *span,
            },
            IrStmt::Asm { bytes, span } => IrStmt::Asm {
                bytes: bytes.clone(),
                span: *span,
            },
            IrStmt::Throw { value, span } => IrStmt::Throw {
                value: self.transform_expr(value, subst),
                span: *span,
            },
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => IrStmt::Try {
                body: self.transform_stmts(body, subst),
                catch_name: catch_name.clone(),
                catch_body: self.transform_stmts(catch_body, subst),
                span: *span,
            },
            IrStmt::Match {
                scrutinee,
                arms,
                span,
            } => IrStmt::Match {
                scrutinee: self.transform_expr(scrutinee, subst),
                arms: arms
                    .iter()
                    .map(|arm| IrMatchArm {
                        pattern: arm.pattern.clone(),
                        body: self.transform_stmts(&arm.body, subst),
                    })
                    .collect(),
                span: *span,
            },
        }
    }

    fn transform_place(&mut self, place: &IrPlace, subst: &HashMap<String, TypeRef>) -> IrPlace {
        match place {
            IrPlace::Field(name) => IrPlace::Field(name.clone()),
            IrPlace::Index(index) => IrPlace::Index(self.transform_expr(index, subst)),
        }
    }

    fn transform_expr(&mut self, expr: &IrExpr, subst: &HashMap<String, TypeRef>) -> IrExpr {
        let ty = substitute_type(&expr.ty, subst);
        let kind = match &expr.kind {
            IrExprKind::Integer(value) => IrExprKind::Integer(*value),
            IrExprKind::Float(value) => IrExprKind::Float(*value),
            IrExprKind::Bool(value) => IrExprKind::Bool(*value),
            IrExprKind::String(value) => IrExprKind::String(value.clone()),
            IrExprKind::Char(value) => IrExprKind::Char(*value),
            IrExprKind::Variable(name) => IrExprKind::Variable(name.clone()),
            // A slot-resolved `Local` never reaches the WASM path (see its doc on
            // `IrExprKind`), but pass it through unchanged for totality.
            IrExprKind::Local { name, packed } => IrExprKind::Local {
                name: name.clone(),
                packed: *packed,
            },
            IrExprKind::Closure { id } => IrExprKind::Closure { id: *id },
            IrExprKind::Array(items) => IrExprKind::Array(
                items
                    .iter()
                    .map(|item| self.transform_expr(item, subst))
                    .collect(),
            ),
            IrExprKind::Index { target, index } => IrExprKind::Index {
                target: Box::new(self.transform_expr(target, subst)),
                index: Box::new(self.transform_expr(index, subst)),
            },
            IrExprKind::Unary { op, expr: inner } => IrExprKind::Unary {
                op: *op,
                expr: Box::new(self.transform_expr(inner, subst)),
            },
            IrExprKind::Binary { left, op, right } => IrExprKind::Binary {
                left: Box::new(self.transform_expr(left, subst)),
                op: *op,
                right: Box::new(self.transform_expr(right, subst)),
            },
            IrExprKind::Field { target, field } => IrExprKind::Field {
                target: Box::new(self.transform_expr(target, subst)),
                field: field.clone(),
            },
            IrExprKind::Await { expr: inner } => IrExprKind::Await {
                expr: Box::new(self.transform_expr(inner, subst)),
            },
            IrExprKind::Call { name, args } => {
                let args: Vec<IrExpr> = args
                    .iter()
                    .map(|arg| self.transform_expr(arg, subst))
                    .collect();
                // The receiver's type is now concrete (args were substituted first), so
                // method resolution keys on the instantiation.
                let rewritten = args
                    .first()
                    .and_then(|receiver| self.resolve(name, &receiver.ty));
                match rewritten {
                    Some(instance) => {
                        let mangled = instance.mangled.clone();
                        self.enqueue(instance);
                        IrExprKind::Call {
                            name: mangled,
                            args,
                        }
                    }
                    None => IrExprKind::Call {
                        name: name.clone(),
                        args,
                    },
                }
            }
        };
        IrExpr {
            kind,
            ty,
            span: expr.span,
        }
    }
}

/// A collision-free, deterministic symbol name for a method instance, derived from
/// the receiver's concrete type spelling and the method name. The leading `$` (never
/// valid in a source identifier: `[A-Za-z_][A-Za-z0-9_]*`) guarantees no collision
/// with a user function name; non-identifier characters in the type spelling (`<`,
/// `>`, `,`, spaces) are sanitized to `_` so the symbol is a plain WASM-internal name,
/// while distinct instantiations stay distinct (the trailing marker from `>` keeps
/// `Box<i64>` apart from a literal `Box_i64`). Kept identical to the native backend's
/// `native_object_method.rs` scheme so the two backends mangle a given instance
/// to the same symbol.
fn mangle_method(receiver_type: &str, method: &str) -> String {
    let sanitized: String = receiver_type
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("$mth${sanitized}${method}")
}

/// Whether any statement in the body constructs a closure literal. A generic method
/// whose body does is skipped by [`MethodExpander::synthesize`] because the closure
/// body is keyed by id in [`IrModule::closures`] and shared across all
/// instantiations, so it cannot carry a per-instantiation monomorphization.
fn stmts_contain_closure(stmts: &[IrStmt]) -> bool {
    stmts.iter().any(stmt_contains_closure)
}

fn stmt_contains_closure(stmt: &IrStmt) -> bool {
    match stmt {
        IrStmt::Let { value, .. } | IrStmt::Expr(value) | IrStmt::Throw { value, .. } => {
            expr_contains_closure(value)
        }
        IrStmt::Assign { path, value, .. } => {
            expr_contains_closure(value)
                || path.iter().any(|place| match place {
                    IrPlace::Field(_) => false,
                    IrPlace::Index(index) => expr_contains_closure(index),
                })
        }
        IrStmt::Return(value) => value.as_ref().is_some_and(expr_contains_closure),
        IrStmt::Break(_) | IrStmt::Continue(_) | IrStmt::Asm { .. } => false,
        IrStmt::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().any(|branch| {
                expr_contains_closure(&branch.condition) || stmts_contain_closure(&branch.body)
            }) || stmts_contain_closure(else_body)
        }
        IrStmt::While {
            condition, body, ..
        } => expr_contains_closure(condition) || stmts_contain_closure(body),
        IrStmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_contains_closure(start)
                || expr_contains_closure(end)
                || step.as_ref().is_some_and(expr_contains_closure)
                || stmts_contain_closure(body)
        }
        IrStmt::Loop { body, .. } => stmts_contain_closure(body),
        IrStmt::Try {
            body, catch_body, ..
        } => stmts_contain_closure(body) || stmts_contain_closure(catch_body),
        IrStmt::Match {
            scrutinee, arms, ..
        } => {
            expr_contains_closure(scrutinee)
                || arms.iter().any(|arm| stmts_contain_closure(&arm.body))
        }
    }
}

fn expr_contains_closure(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::Closure { .. } => true,
        IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
        | IrExprKind::Variable(_)
        | IrExprKind::Local { .. } => false,
        IrExprKind::Array(items) => items.iter().any(expr_contains_closure),
        IrExprKind::Index { target, index } => {
            expr_contains_closure(target) || expr_contains_closure(index)
        }
        IrExprKind::Unary { expr: inner, .. } | IrExprKind::Await { expr: inner } => {
            expr_contains_closure(inner)
        }
        IrExprKind::Binary { left, right, .. } => {
            expr_contains_closure(left) || expr_contains_closure(right)
        }
        IrExprKind::Field { target, .. } => expr_contains_closure(target),
        IrExprKind::Call { args, .. } => args.iter().any(expr_contains_closure),
    }
}
