//! Common-subexpression-elimination pass. Split out of `ir_optimizer.rs`;
//! behavior-preserving move. The shared `ExprSignature` type and
//! `combine_signatures` helper live in `ir_optimizer.rs` (also used by LICM); the
//! shared `expr_requires_optimizer_barrier` predicate lives there too (also used
//! by copy propagation). See `ir_optimizer.rs` for the pass pipeline.

use super::*;

#[derive(Default)]
pub(crate) struct CommonSubexpressionEliminator {
    pub(crate) eliminated_common_subexpressions: usize,
}

#[derive(Debug, Clone)]
struct AvailableExpr {
    variable: String,
    dependencies: HashSet<String>,
}

impl CommonSubexpressionEliminator {
    pub(crate) fn eliminate_module(&mut self, module: &IrModule) -> IrModule {
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
                .map(|function| self.eliminate_function(function))
                .collect(),
        }
    }

    fn eliminate_function(&mut self, function: &IrFunction) -> IrFunction {
        IrFunction {
            name: function.name.clone(),
            params: function.params.clone(),
            return_type: function.return_type.clone(),
            body: self.eliminate_block(&function.body, &mut HashMap::new()),
            span: function.span,
        }
    }

    fn eliminate_block(
        &mut self,
        statements: &[IrStmt],
        available: &mut HashMap<String, AvailableExpr>,
    ) -> Vec<IrStmt> {
        statements
            .iter()
            .map(|statement| self.eliminate_statement(statement, available))
            .collect()
    }

    fn eliminate_statement(
        &mut self,
        statement: &IrStmt,
        available: &mut HashMap<String, AvailableExpr>,
    ) -> IrStmt {
        match statement {
            IrStmt::Let {
                name,
                ty,
                value,
                span,
            } => {
                let value = self.rewrite_expr(value);
                if expr_requires_optimizer_barrier(&value) {
                    available.clear();
                }
                invalidate_available_exprs(name, available);

                let value = match pure_expr_signature(&value).filter(|_| is_cse_eligible(&value)) {
                    Some(signature) => match available.get(&signature.key) {
                        Some(existing) => {
                            self.eliminated_common_subexpressions += 1;
                            IrExpr {
                                kind: IrExprKind::Variable(existing.variable.clone()),
                                ty: value.ty.clone(),
                                span: value.span,
                            }
                        }
                        None => {
                            available.insert(
                                signature.key,
                                AvailableExpr {
                                    variable: name.clone(),
                                    dependencies: signature.dependencies,
                                },
                            );
                            value
                        }
                    },
                    None => value,
                };

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
                let value = self.rewrite_expr(value);
                if expr_requires_optimizer_barrier(&value) {
                    available.clear();
                }
                // Mutating a field of `name` invalidates expressions over `name`.
                invalidate_available_exprs(name, available);
                IrStmt::Assign {
                    name: name.clone(),
                    path: path.clone(),
                    op: *op,
                    value,
                    span: *span,
                }
            }
            IrStmt::Return(expr) => {
                let expr = expr.as_ref().map(|expr| self.rewrite_expr(expr));
                if expr.as_ref().is_some_and(expr_requires_optimizer_barrier) {
                    available.clear();
                }
                IrStmt::Return(expr)
            }
            IrStmt::Break(span) => IrStmt::Break(*span),
            IrStmt::Continue(span) => IrStmt::Continue(*span),
            IrStmt::Expr(expr) => {
                let expr = self.rewrite_expr(expr);
                if expr_requires_optimizer_barrier(&expr) {
                    available.clear();
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
                        condition: self.rewrite_expr(&branch.condition),
                        body: self.eliminate_block(&branch.body, &mut HashMap::new()),
                    })
                    .collect();
                let else_body = self.eliminate_block(else_body, &mut HashMap::new());
                available.clear();
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
                let condition = self.rewrite_expr(condition);
                let body = self.eliminate_block(body, &mut HashMap::new());
                available.clear();
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
                let start = self.rewrite_expr(start);
                let end = self.rewrite_expr(end);
                let step = step.as_ref().map(|step| self.rewrite_expr(step));
                let body = self.eliminate_block(body, &mut HashMap::new());
                invalidate_available_exprs(name, available);
                available.clear();
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
                let body = self.eliminate_block(body, &mut HashMap::new());
                available.clear();
                IrStmt::Loop { body, span: *span }
            }
            // A region block is a run-once nested scope preserved as its own node.
            // Eliminate within its body with a fresh table, then clear, mirroring a
            // loop body (conservative — a block-local binding never leaks out).
            IrStmt::RegionBlock { body, span } => {
                let body = self.eliminate_block(body, &mut HashMap::new());
                available.clear();
                IrStmt::RegionBlock { body, span: *span }
            }
            // Inline assembly is an opaque barrier: clear any available
            // expressions and pass the bytes through unchanged.
            IrStmt::Asm { bytes, span } => {
                available.clear();
                IrStmt::Asm {
                    bytes: bytes.clone(),
                    span: *span,
                }
            }
            IrStmt::Throw { value, span } => {
                available.clear();
                IrStmt::Throw {
                    value: value.clone(),
                    span: *span,
                }
            }
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => {
                let body = self.eliminate_block(body, &mut HashMap::new());
                let catch_body = self.eliminate_block(catch_body, &mut HashMap::new());
                available.clear();
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
                let scrutinee = self.rewrite_expr(scrutinee);
                let arms = arms
                    .iter()
                    .map(|arm| IrMatchArm {
                        pattern: arm.pattern.clone(),
                        body: self.eliminate_block(&arm.body, &mut HashMap::new()),
                    })
                    .collect();
                available.clear();
                IrStmt::Match {
                    scrutinee,
                    arms,
                    span: *span,
                }
            }
        }
    }

    fn rewrite_expr(&mut self, expr: &IrExpr) -> IrExpr {
        match &expr.kind {
            IrExprKind::Array(values) => IrExpr {
                kind: IrExprKind::Array(
                    values
                        .iter()
                        .map(|value| self.rewrite_expr(value))
                        .collect(),
                ),
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Index { target, index } => IrExpr {
                kind: IrExprKind::Index {
                    target: Box::new(self.rewrite_expr(target)),
                    index: Box::new(self.rewrite_expr(index)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Field { target, field } => IrExpr {
                kind: IrExprKind::Field {
                    target: Box::new(self.rewrite_expr(target)),
                    field: field.clone(),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Unary { op, expr: inner } => IrExpr {
                kind: IrExprKind::Unary {
                    op: *op,
                    expr: Box::new(self.rewrite_expr(inner)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Binary { left, op, right } => IrExpr {
                kind: IrExprKind::Binary {
                    left: Box::new(self.rewrite_expr(left)),
                    op: *op,
                    right: Box::new(self.rewrite_expr(right)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Call { name, args } => IrExpr {
                kind: IrExprKind::Call {
                    name: name.clone(),
                    args: args.iter().map(|arg| self.rewrite_expr(arg)).collect(),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Await { expr: inner } => IrExpr {
                kind: IrExprKind::Await {
                    expr: Box::new(self.rewrite_expr(inner)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            // A closure literal node carries only an id (its body lives in the
            // module's closure table, untouched by this pass), so it is copied
            // through unchanged like any other leaf.
            IrExprKind::Closure { .. }
            | IrExprKind::Integer(_)
            | IrExprKind::Float(_)
            | IrExprKind::Bool(_)
            | IrExprKind::String(_)
            | IrExprKind::Char(_)
            | IrExprKind::Variable(_)
            | IrExprKind::Local { .. } => expr.clone(),
        }
    }
}

fn invalidate_available_exprs(name: &str, available: &mut HashMap<String, AvailableExpr>) {
    available.retain(|_, expr| expr.variable != name && !expr.dependencies.contains(name));
}

fn pure_expr_signature(expr: &IrExpr) -> Option<ExprSignature> {
    let (key, dependencies) = match &expr.kind {
        IrExprKind::Integer(value) => (format!("i64:{value}:{}", expr.ty.name), HashSet::new()),
        IrExprKind::Float(value) => (
            format!("f64:{}:{}", value.to_bits(), expr.ty.name),
            HashSet::new(),
        ),
        IrExprKind::Bool(value) => (format!("bool:{value}:{}", expr.ty.name), HashSet::new()),
        IrExprKind::String(value) => (format!("string:{value:?}:{}", expr.ty.name), HashSet::new()),
        IrExprKind::Char(value) => (format!("char:{value}:{}", expr.ty.name), HashSet::new()),
        IrExprKind::Variable(name) | IrExprKind::Local { name, .. } => {
            let mut dependencies = HashSet::new();
            dependencies.insert(name.clone());
            (format!("var:{name}:{}", expr.ty.name), dependencies)
        }
        IrExprKind::Array(values) => {
            let signatures = values
                .iter()
                .map(pure_expr_signature)
                .collect::<Option<Vec<_>>>()?;
            combine_signatures("array", &expr.ty.name, signatures)
        }
        IrExprKind::Index { target, index } => {
            let target = pure_expr_signature(target)?;
            let index = pure_expr_signature(index)?;
            combine_signatures("index", &expr.ty.name, vec![target, index])
        }
        IrExprKind::Field { target, field } => {
            let target = pure_expr_signature(target)?;
            combine_signatures(&format!("field:{field}"), &expr.ty.name, vec![target])
        }
        IrExprKind::Unary { op, expr: inner } => {
            let inner = pure_expr_signature(inner)?;
            combine_signatures(&format!("unary:{op:?}"), &expr.ty.name, vec![inner])
        }
        IrExprKind::Binary { left, op, right } => {
            let left = pure_expr_signature(left)?;
            let right = pure_expr_signature(right)?;
            combine_signatures(&format!("binary:{op:?}"), &expr.ty.name, vec![left, right])
        }
        // Calls and `await`s are not pure: they may have side effects (an
        // `await` spawns/joins a thread), so they are never CSE candidates. A
        // closure literal captures the live environment at evaluation time, so
        // two evaluations at different points may differ; it is not CSE-eligible.
        IrExprKind::Call { .. } | IrExprKind::Await { .. } | IrExprKind::Closure { .. } => {
            return None;
        }
    };

    Some(ExprSignature { key, dependencies })
}

/// Only compound pure expressions are worth eliminating as common
/// subexpressions. Reusing a variable for a bare literal or another variable is
/// never a win, and — because copy propagation can then alias the new binding
/// to a variable that is later mutated — it is unsound inside loops (a
/// `let i = 0` next to a `let a = 0` must not become `let i = a`).
fn is_cse_eligible(expr: &IrExpr) -> bool {
    matches!(
        expr.kind,
        IrExprKind::Unary { .. }
            | IrExprKind::Binary { .. }
            | IrExprKind::Index { .. }
            | IrExprKind::Field { .. }
            | IrExprKind::Array(_)
    )
}
