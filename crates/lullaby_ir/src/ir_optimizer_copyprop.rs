//! Copy-propagation pass. Split out of `ir_optimizer.rs`; behavior-preserving
//! move. The shared `expr_requires_optimizer_barrier` predicate lives in
//! `ir_optimizer.rs` (also used by CSE). See `ir_optimizer.rs` for the pass
//! pipeline that drives it.

use super::*;

#[derive(Default)]
pub(crate) struct CopyPropagator {
    pub(crate) propagated_copies: usize,
}

impl CopyPropagator {
    pub(crate) fn propagate_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
            structs: module.structs.clone(),
            enums: module.enums.clone(),
            impls: module.impls.clone(),
            trait_methods: module.trait_methods.clone(),
            async_functions: module.async_functions.clone(),
            extern_functions: module.extern_functions.clone(),
            extern_signatures: module.extern_signatures.clone(),
            export_functions: module.export_functions.clone(),
            // Closure bodies are carried through unchanged; this pass only
            // rewrites top-level function bodies. (Closures run on the
            // interpreters, so optimizing their bodies is a separate concern.)
            closures: module.closures.clone(),
            functions: module
                .functions
                .iter()
                .map(|function| self.propagate_function(function))
                .collect(),
        }
    }

    fn propagate_function(&mut self, function: &IrFunction) -> IrFunction {
        IrFunction {
            name: function.name.clone(),
            params: function.params.clone(),
            return_type: function.return_type.clone(),
            body: self.propagate_block(&function.body, &mut HashMap::new()),
            span: function.span,
        }
    }

    fn propagate_block(
        &mut self,
        statements: &[IrStmt],
        aliases: &mut HashMap<String, String>,
    ) -> Vec<IrStmt> {
        statements
            .iter()
            .map(|statement| self.propagate_statement(statement, aliases))
            .collect()
    }

    fn propagate_statement(
        &mut self,
        statement: &IrStmt,
        aliases: &mut HashMap<String, String>,
    ) -> IrStmt {
        match statement {
            IrStmt::Let {
                name,
                ty,
                value,
                span,
            } => {
                let value = self.propagate_expr(value, aliases);
                let has_optimizer_barrier = expr_requires_optimizer_barrier(&value);
                if has_optimizer_barrier {
                    aliases.clear();
                }
                invalidate_alias(name, aliases);
                if let IrExprKind::Variable(source) = &value.kind {
                    let source = resolve_alias(source, aliases);
                    if source != *name {
                        aliases.insert(name.clone(), source);
                    }
                }
                IrStmt::Let {
                    name: name.clone(),
                    ty: ty.clone(),
                    value,
                    span: *span,
                }
            }
            IrStmt::Assign {
                name,
                path,
                op,
                value,
                span,
            } => {
                let value = self.propagate_expr(value, aliases);
                if expr_requires_optimizer_barrier(&value) {
                    aliases.clear();
                }
                invalidate_alias(name, aliases);
                IrStmt::Assign {
                    name: name.clone(),
                    path: path.clone(),
                    op: *op,
                    value,
                    span: *span,
                }
            }
            IrStmt::Return(expr) => {
                let expr = expr.as_ref().map(|expr| self.propagate_expr(expr, aliases));
                if expr.as_ref().is_some_and(expr_requires_optimizer_barrier) {
                    aliases.clear();
                }
                IrStmt::Return(expr)
            }
            IrStmt::Break(span) => IrStmt::Break(*span),
            IrStmt::Continue(span) => IrStmt::Continue(*span),
            IrStmt::Expr(expr) => {
                let expr = self.propagate_expr(expr, aliases);
                if expr_requires_optimizer_barrier(&expr) {
                    aliases.clear();
                }
                IrStmt::Expr(expr)
            }
            IrStmt::If {
                branches,
                else_body,
                span,
            } => {
                let branches = branches
                    .iter()
                    .map(|branch| IrIfBranch {
                        condition: self.propagate_expr(&branch.condition, aliases),
                        body: self.propagate_block(&branch.body, &mut HashMap::new()),
                    })
                    .collect();
                let else_body = self.propagate_block(else_body, &mut HashMap::new());
                aliases.clear();
                IrStmt::If {
                    branches,
                    else_body,
                    span: *span,
                }
            }
            IrStmt::While {
                condition,
                body,
                span,
            } => {
                let condition = self.propagate_expr(condition, aliases);
                let body = self.propagate_block(body, &mut HashMap::new());
                aliases.clear();
                IrStmt::While {
                    condition,
                    body,
                    span: *span,
                }
            }
            IrStmt::For {
                name,
                start,
                end,
                step,
                body,
                span,
            } => {
                let start = self.propagate_expr(start, aliases);
                let end = self.propagate_expr(end, aliases);
                let step = step.as_ref().map(|step| self.propagate_expr(step, aliases));
                let body = self.propagate_block(body, &mut HashMap::new());
                aliases.clear();
                IrStmt::For {
                    name: name.clone(),
                    start,
                    end,
                    step,
                    body,
                    span: *span,
                }
            }
            IrStmt::Loop { body, span } => {
                let body = self.propagate_block(body, &mut HashMap::new());
                aliases.clear();
                IrStmt::Loop { body, span: *span }
            }
            // Inline assembly is opaque: clear aliases and pass the bytes through.
            IrStmt::Asm { bytes, span } => {
                aliases.clear();
                IrStmt::Asm {
                    bytes: bytes.clone(),
                    span: *span,
                }
            }
            IrStmt::Throw { value, span } => {
                let value = self.propagate_expr(value, aliases);
                aliases.clear();
                IrStmt::Throw { value, span: *span }
            }
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => {
                let body = self.propagate_block(body, &mut HashMap::new());
                let catch_body = self.propagate_block(catch_body, &mut HashMap::new());
                aliases.clear();
                IrStmt::Try {
                    body,
                    catch_name: catch_name.clone(),
                    catch_body,
                    span: *span,
                }
            }
            IrStmt::Match {
                scrutinee,
                arms,
                span,
            } => {
                let scrutinee = self.propagate_expr(scrutinee, aliases);
                let arms = arms
                    .iter()
                    .map(|arm| IrMatchArm {
                        pattern: arm.pattern.clone(),
                        body: self.propagate_block(&arm.body, &mut HashMap::new()),
                    })
                    .collect();
                aliases.clear();
                IrStmt::Match {
                    scrutinee,
                    arms,
                    span: *span,
                }
            }
        }
    }

    fn propagate_expr(&mut self, expr: &IrExpr, aliases: &HashMap<String, String>) -> IrExpr {
        match &expr.kind {
            IrExprKind::Variable(name) => {
                let replacement = resolve_alias(name, aliases);
                if replacement != *name {
                    self.propagated_copies += 1;
                    IrExpr {
                        kind: IrExprKind::Variable(replacement),
                        ty: expr.ty.clone(),
                        span: expr.span,
                    }
                } else {
                    expr.clone()
                }
            }
            IrExprKind::Array(values) => IrExpr {
                kind: IrExprKind::Array(
                    values
                        .iter()
                        .map(|value| self.propagate_expr(value, aliases))
                        .collect(),
                ),
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Index { target, index } => IrExpr {
                kind: IrExprKind::Index {
                    target: Box::new(self.propagate_expr(target, aliases)),
                    index: Box::new(self.propagate_expr(index, aliases)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Field { target, field } => IrExpr {
                kind: IrExprKind::Field {
                    target: Box::new(self.propagate_expr(target, aliases)),
                    field: field.clone(),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Unary { op, expr: inner } => IrExpr {
                kind: IrExprKind::Unary {
                    op: *op,
                    expr: Box::new(self.propagate_expr(inner, aliases)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Binary { left, op, right } => IrExpr {
                kind: IrExprKind::Binary {
                    left: Box::new(self.propagate_expr(left, aliases)),
                    op: *op,
                    right: Box::new(self.propagate_expr(right, aliases)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Call { name, args } => IrExpr {
                kind: IrExprKind::Call {
                    name: name.clone(),
                    args: args
                        .iter()
                        .map(|arg| self.propagate_expr(arg, aliases))
                        .collect(),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Await { expr: inner } => IrExpr {
                kind: IrExprKind::Await {
                    expr: Box::new(self.propagate_expr(inner, aliases)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            // A closure literal node carries only an id; its captured values are
            // materialized at runtime and its body lives in the module table, so
            // copy propagation has nothing to rewrite here.
            // A `Local` is only introduced after every optimization pass (at
            // interpretation time), so it never reaches copy propagation; copy it
            // through unchanged for match completeness.
            IrExprKind::Closure { .. }
            | IrExprKind::Integer(_)
            | IrExprKind::Float(_)
            | IrExprKind::Bool(_)
            | IrExprKind::String(_)
            | IrExprKind::Char(_)
            | IrExprKind::Local { .. } => expr.clone(),
        }
    }
}

fn resolve_alias(name: &str, aliases: &HashMap<String, String>) -> String {
    let mut current = name;
    let mut seen = HashSet::new();
    while let Some(next) = aliases.get(current).map(String::as_str) {
        if !seen.insert(current) {
            break;
        }
        current = next;
    }
    current.to_string()
}

fn invalidate_alias(name: &str, aliases: &mut HashMap<String, String>) {
    let stale = aliases
        .keys()
        .filter(|alias| alias.as_str() == name || resolve_alias(alias, aliases) == name)
        .cloned()
        .collect::<Vec<_>>();
    for alias in stale {
        aliases.remove(&alias);
    }
}
