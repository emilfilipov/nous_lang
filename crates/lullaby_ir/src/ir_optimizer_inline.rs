//! Function-inlining pass. Split out of `ir_optimizer.rs`; behavior-preserving
//! move. See `ir_optimizer.rs` for the pass pipeline that drives it.

use super::*;

/// Inlines calls to small, non-recursive **leaf** helper functions: a function
/// whose body is a single `return <expr>` where `<expr>` is pure scalar
/// arithmetic/logic (literals, parameters, and `+ - * / % & | ^ ~ << >> == …`,
/// no calls, indexing, field access, or heap construction). At a call site whose
/// arguments are all simple (a variable or a literal), the call is replaced by
/// the body with each parameter substituted by its argument. Restricting the
/// body to leaf expressions makes recursion impossible; restricting arguments to
/// variables/literals makes substitution side-effect- and duplication-safe (a
/// parameter used twice re-reads a cheap, pure operand). Type is preserved (the
/// call's type is the callee's return type, which is the body expression's type).
pub(crate) struct Inliner {
    /// name -> (parameter names, body expression) for each inlinable helper.
    inlinable: HashMap<String, (Vec<String>, IrExpr)>,
    pub(crate) inlined_calls: usize,
}

impl Inliner {
    pub(crate) fn new(module: &IrModule) -> Self {
        let mut inlinable = HashMap::new();
        for function in &module.functions {
            // An `async fn` call yields a `Future<T>`, not `T`, so inlining its
            // body (a `T`) would break the caller's `await`. Skip async functions.
            if module.async_functions.contains(&function.name) {
                continue;
            }
            // A single-statement body that yields one expression: either an
            // explicit `return e` or a tail expression `e` (both spellings occur
            // after lowering).
            let body = match function.body.as_slice() {
                [IrStmt::Return(Some(body))] | [IrStmt::Expr(body)] => Some(body),
                _ => None,
            };
            if let Some(body) = body
                && Self::is_pure_leaf(body)
            {
                let params = function.params.iter().map(|p| p.name.clone()).collect();
                inlinable.insert(function.name.clone(), (params, body.clone()));
            }
        }
        Self {
            inlinable,
            inlined_calls: 0,
        }
    }

    /// A body expression eligible for inlining: pure scalar computation with no
    /// calls, indexing, field access, array/closure construction, or `await`.
    fn is_pure_leaf(expr: &IrExpr) -> bool {
        match &expr.kind {
            IrExprKind::Integer(_)
            | IrExprKind::Float(_)
            | IrExprKind::Bool(_)
            | IrExprKind::Char(_)
            | IrExprKind::String(_)
            | IrExprKind::Variable(_) => true,
            IrExprKind::Unary { expr, .. } => Self::is_pure_leaf(expr),
            IrExprKind::Binary { left, right, .. } => {
                Self::is_pure_leaf(left) && Self::is_pure_leaf(right)
            }
            _ => false,
        }
    }

    /// A call argument safe to substitute (and possibly duplicate): a bare
    /// variable or a literal — pure and cheap, so re-evaluating it changes
    /// nothing.
    fn is_simple_arg(expr: &IrExpr) -> bool {
        matches!(
            expr.kind,
            IrExprKind::Variable(_)
                | IrExprKind::Integer(_)
                | IrExprKind::Float(_)
                | IrExprKind::Bool(_)
                | IrExprKind::Char(_)
                | IrExprKind::String(_)
        )
    }

    /// Clone `expr`, replacing every `Variable(param)` found in `bindings` with
    /// its argument expression. A leaf body's only variables are parameters.
    fn substitute(expr: &IrExpr, bindings: &HashMap<&str, &IrExpr>) -> IrExpr {
        let kind = match &expr.kind {
            IrExprKind::Variable(name) => {
                if let Some(arg) = bindings.get(name.as_str()) {
                    return (*arg).clone();
                }
                IrExprKind::Variable(name.clone())
            }
            IrExprKind::Unary { op, expr: inner } => IrExprKind::Unary {
                op: *op,
                expr: Box::new(Self::substitute(inner, bindings)),
            },
            IrExprKind::Binary { left, op, right } => IrExprKind::Binary {
                left: Box::new(Self::substitute(left, bindings)),
                op: *op,
                right: Box::new(Self::substitute(right, bindings)),
            },
            other => other.clone(),
        };
        IrExpr {
            kind,
            ty: expr.ty.clone(),
            span: expr.span,
        }
    }

    pub(crate) fn inline_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
            structs: module.structs.clone(),
            enums: module.enums.clone(),
            impls: module.impls.clone(),
            trait_methods: module.trait_methods.clone(),
            async_functions: module.async_functions.clone(),
            extern_functions: module.extern_functions.clone(),
            extern_signatures: module.extern_signatures.clone(),
            export_functions: module.export_functions.clone(),
            closures: module.closures.clone(),
            functions: module
                .functions
                .iter()
                .map(|function| IrFunction {
                    name: function.name.clone(),
                    params: function.params.clone(),
                    return_type: function.return_type.clone(),
                    body: self.inline_block(&function.body),
                    span: function.span,
                })
                .collect(),
        }
    }

    fn inline_block(&mut self, statements: &[IrStmt]) -> Vec<IrStmt> {
        statements
            .iter()
            .map(|statement| self.inline_statement(statement))
            .collect()
    }

    fn inline_statement(&mut self, statement: &IrStmt) -> IrStmt {
        match statement {
            IrStmt::Let {
                name,
                ty,
                value,
                span,
            } => IrStmt::Let {
                name: name.clone(),
                ty: ty.clone(),
                value: self.inline_expr(value),
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
                path: path.clone(),
                op: *op,
                value: self.inline_expr(value),
                span: *span,
            },
            IrStmt::Return(expr) => {
                IrStmt::Return(expr.as_ref().map(|expr| self.inline_expr(expr)))
            }
            IrStmt::Break(span) => IrStmt::Break(*span),
            IrStmt::Continue(span) => IrStmt::Continue(*span),
            IrStmt::Expr(expr) => IrStmt::Expr(self.inline_expr(expr)),
            IrStmt::If {
                branches,
                else_body,
                span,
            } => IrStmt::If {
                branches: branches
                    .iter()
                    .map(|branch| IrIfBranch {
                        condition: self.inline_expr(&branch.condition),
                        body: self.inline_block(&branch.body),
                    })
                    .collect(),
                else_body: self.inline_block(else_body),
                span: *span,
            },
            IrStmt::While {
                condition,
                body,
                span,
            } => IrStmt::While {
                condition: self.inline_expr(condition),
                body: self.inline_block(body),
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
                start: self.inline_expr(start),
                end: self.inline_expr(end),
                step: step.as_ref().map(|step| self.inline_expr(step)),
                body: self.inline_block(body),
                span: *span,
            },
            IrStmt::Loop { body, span } => IrStmt::Loop {
                body: self.inline_block(body),
                span: *span,
            },
            IrStmt::Asm { bytes, span } => IrStmt::Asm {
                bytes: bytes.clone(),
                span: *span,
            },
            IrStmt::Throw { value, span } => IrStmt::Throw {
                value: self.inline_expr(value),
                span: *span,
            },
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => IrStmt::Try {
                body: self.inline_block(body),
                catch_name: catch_name.clone(),
                catch_body: self.inline_block(catch_body),
                span: *span,
            },
            IrStmt::Match {
                scrutinee,
                arms,
                span,
            } => IrStmt::Match {
                scrutinee: self.inline_expr(scrutinee),
                arms: arms
                    .iter()
                    .map(|arm| IrMatchArm {
                        pattern: arm.pattern.clone(),
                        body: self.inline_block(&arm.body),
                    })
                    .collect(),
                span: *span,
            },
        }
    }

    fn inline_expr(&mut self, expr: &IrExpr) -> IrExpr {
        // Rewrite children first (so nested calls inline bottom-up).
        let kind = match &expr.kind {
            IrExprKind::Array(items) => {
                IrExprKind::Array(items.iter().map(|item| self.inline_expr(item)).collect())
            }
            IrExprKind::Index { target, index } => IrExprKind::Index {
                target: Box::new(self.inline_expr(target)),
                index: Box::new(self.inline_expr(index)),
            },
            IrExprKind::Unary { op, expr: inner } => IrExprKind::Unary {
                op: *op,
                expr: Box::new(self.inline_expr(inner)),
            },
            IrExprKind::Binary { left, op, right } => IrExprKind::Binary {
                left: Box::new(self.inline_expr(left)),
                op: *op,
                right: Box::new(self.inline_expr(right)),
            },
            IrExprKind::Field { target, field } => IrExprKind::Field {
                target: Box::new(self.inline_expr(target)),
                field: field.clone(),
            },
            IrExprKind::Await { expr: inner } => IrExprKind::Await {
                expr: Box::new(self.inline_expr(inner)),
            },
            IrExprKind::Call { name, args } => {
                let args: Vec<IrExpr> = args.iter().map(|arg| self.inline_expr(arg)).collect();
                if let Some((params, body)) = self.inlinable.get(name)
                    && params.len() == args.len()
                    && args.iter().all(Self::is_simple_arg)
                {
                    let bindings: HashMap<&str, &IrExpr> =
                        params.iter().map(String::as_str).zip(args.iter()).collect();
                    self.inlined_calls += 1;
                    return Self::substitute(body, &bindings);
                }
                IrExprKind::Call {
                    name: name.clone(),
                    args,
                }
            }
            other => other.clone(),
        };
        IrExpr {
            kind,
            ty: expr.ty.clone(),
            span: expr.span,
        }
    }
}
