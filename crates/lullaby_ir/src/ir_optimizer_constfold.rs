//! Constant-folding pass. Split out of `ir_optimizer.rs`; behavior-preserving
//! move. See `ir_optimizer.rs` for the pass pipeline that drives it.

use super::*;

#[derive(Default)]
pub(crate) struct ConstantFolder {
    pub(crate) folded_expressions: usize,
}

impl ConstantFolder {
    pub(crate) fn fold_module(&mut self, module: &IrModule) -> IrModule {
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
                .map(|function| self.fold_function(function))
                .collect(),
        }
    }

    fn fold_function(&mut self, function: &IrFunction) -> IrFunction {
        IrFunction {
            name: function.name.clone(),
            params: function.params.clone(),
            return_type: function.return_type.clone(),
            body: self.fold_block(&function.body),
            span: function.span,
        }
    }

    fn fold_block(&mut self, statements: &[IrStmt]) -> Vec<IrStmt> {
        statements
            .iter()
            .map(|statement| self.fold_statement(statement))
            .collect()
    }

    fn fold_statement(&mut self, statement: &IrStmt) -> IrStmt {
        match statement {
            IrStmt::Let {
                name,
                ty,
                value,
                span,
            } => IrStmt::Let {
                name: name.clone(),
                ty: ty.clone(),
                value: self.fold_expr(value),
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
                value: self.fold_expr(value),
                span: *span,
            },
            IrStmt::Return(expr) => IrStmt::Return(expr.as_ref().map(|expr| self.fold_expr(expr))),
            IrStmt::Break(span) => IrStmt::Break(*span),
            IrStmt::Continue(span) => IrStmt::Continue(*span),
            IrStmt::Expr(expr) => IrStmt::Expr(self.fold_expr(expr)),
            IrStmt::If {
                branches,
                else_body,
                span,
            } => IrStmt::If {
                branches: branches
                    .iter()
                    .map(|branch| IrIfBranch {
                        condition: self.fold_expr(&branch.condition),
                        body: self.fold_block(&branch.body),
                    })
                    .collect(),
                else_body: self.fold_block(else_body),
                span: *span,
            },
            IrStmt::While {
                condition,
                body,
                span,
            } => IrStmt::While {
                condition: self.fold_expr(condition),
                body: self.fold_block(body),
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
                start: self.fold_expr(start),
                end: self.fold_expr(end),
                step: step.as_ref().map(|step| self.fold_expr(step)),
                body: self.fold_block(body),
                span: *span,
            },
            IrStmt::Loop { body, span } => IrStmt::Loop {
                body: self.fold_block(body),
                span: *span,
            },
            // A region block is preserved as its own node (never flattened) so the
            // downstream scope structure survives; fold its body in place.
            IrStmt::RegionBlock { body, span } => IrStmt::RegionBlock {
                body: self.fold_block(body),
                span: *span,
            },
            // Inline assembly is opaque bytes; folding leaves it unchanged.
            IrStmt::Asm { bytes, span } => IrStmt::Asm {
                bytes: bytes.clone(),
                span: *span,
            },
            IrStmt::Throw { value, span } => IrStmt::Throw {
                value: self.fold_expr(value),
                span: *span,
            },
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => IrStmt::Try {
                body: self.fold_block(body),
                catch_name: catch_name.clone(),
                catch_body: self.fold_block(catch_body),
                span: *span,
            },
            IrStmt::Match {
                scrutinee,
                arms,
                span,
            } => IrStmt::Match {
                scrutinee: self.fold_expr(scrutinee),
                arms: arms
                    .iter()
                    .map(|arm| IrMatchArm {
                        pattern: arm.pattern.clone(),
                        body: self.fold_block(&arm.body),
                    })
                    .collect(),
                span: *span,
            },
        }
    }

    fn fold_expr(&mut self, expr: &IrExpr) -> IrExpr {
        match &expr.kind {
            IrExprKind::Array(values) => IrExpr {
                kind: IrExprKind::Array(values.iter().map(|value| self.fold_expr(value)).collect()),
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Index { target, index } => IrExpr {
                kind: IrExprKind::Index {
                    target: Box::new(self.fold_expr(target)),
                    index: Box::new(self.fold_expr(index)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Field { target, field } => IrExpr {
                kind: IrExprKind::Field {
                    target: Box::new(self.fold_expr(target)),
                    field: field.clone(),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Unary { op, expr: inner } => {
                let inner = self.fold_expr(inner);
                match (op, &inner.kind) {
                    (UnaryOp::Not, IrExprKind::Bool(value)) => {
                        self.literal(expr, IrExprKind::Bool(!value))
                    }
                    // Fold `~` over an integer literal (one's complement).
                    (UnaryOp::BitNot, IrExprKind::Integer(value)) => {
                        self.literal(expr, IrExprKind::Integer(!value))
                    }
                    // Fold unary `-` over a numeric literal (wrapping for ints).
                    (UnaryOp::Negate, IrExprKind::Integer(value)) => {
                        self.literal(expr, IrExprKind::Integer(value.wrapping_neg()))
                    }
                    (UnaryOp::Negate, IrExprKind::Float(value)) => {
                        self.literal(expr, IrExprKind::Float(-value))
                    }
                    _ => IrExpr {
                        kind: IrExprKind::Unary {
                            op: *op,
                            expr: Box::new(inner),
                        },
                        ty: expr.ty.clone(),
                        span: expr.span,
                    },
                }
            }
            IrExprKind::Binary { left, op, right } => {
                let left = self.fold_expr(left);
                let right = self.fold_expr(right);
                match fold_binary(&left, *op, &right) {
                    Some(kind) => self.literal(expr, kind),
                    None => IrExpr {
                        kind: IrExprKind::Binary {
                            left: Box::new(left),
                            op: *op,
                            right: Box::new(right),
                        },
                        ty: expr.ty.clone(),
                        span: expr.span,
                    },
                }
            }
            IrExprKind::Call { name, args } => IrExpr {
                kind: IrExprKind::Call {
                    name: name.clone(),
                    args: args.iter().map(|arg| self.fold_expr(arg)).collect(),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            IrExprKind::Await { expr: inner } => IrExpr {
                kind: IrExprKind::Await {
                    expr: Box::new(self.fold_expr(inner)),
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

    fn literal(&mut self, original: &IrExpr, kind: IrExprKind) -> IrExpr {
        self.folded_expressions += 1;
        IrExpr {
            kind,
            ty: original.ty.clone(),
            span: original.span,
        }
    }
}

fn fold_binary(left: &IrExpr, op: BinaryOp, right: &IrExpr) -> Option<IrExprKind> {
    match (&left.kind, op, &right.kind) {
        (IrExprKind::Integer(left), BinaryOp::Add, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left + right))
        }
        (IrExprKind::String(left), BinaryOp::Add, IrExprKind::String(right)) => {
            Some(IrExprKind::String(format!("{left}{right}")))
        }
        (IrExprKind::Float(left), BinaryOp::Add, IrExprKind::Float(right)) => {
            Some(IrExprKind::Float(left + right))
        }
        (IrExprKind::Float(left), BinaryOp::Subtract, IrExprKind::Float(right)) => {
            Some(IrExprKind::Float(left - right))
        }
        (IrExprKind::Float(left), BinaryOp::Multiply, IrExprKind::Float(right)) => {
            Some(IrExprKind::Float(left * right))
        }
        (IrExprKind::Float(left), BinaryOp::Divide, IrExprKind::Float(right)) => {
            Some(IrExprKind::Float(left / right))
        }
        (IrExprKind::Integer(left), BinaryOp::Subtract, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left - right))
        }
        (IrExprKind::Integer(left), BinaryOp::Multiply, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left * right))
        }
        (
            IrExprKind::Integer(_) | IrExprKind::Float(_),
            BinaryOp::Divide | BinaryOp::Remainder,
            IrExprKind::Integer(0),
        ) => None,
        (IrExprKind::Integer(left), BinaryOp::Divide, IrExprKind::Integer(right)) => {
            // Fold with wrapping semantics so the one signed-overflow case
            // (`i64::MIN / -1`) yields `i64::MIN` at compile time instead of
            // panicking, matching the runtime interpreters and native backend.
            Some(IrExprKind::Integer(left.wrapping_div(*right)))
        }
        (IrExprKind::Integer(left), BinaryOp::Remainder, IrExprKind::Integer(right)) => {
            // `i64::MIN % -1` is 0; `wrapping_rem` yields it without panicking,
            // matching the interpreters and native backend.
            Some(IrExprKind::Integer(left.wrapping_rem(*right)))
        }
        (IrExprKind::Integer(left), BinaryOp::Equal, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Bool(left == right))
        }
        (IrExprKind::Integer(left), BinaryOp::NotEqual, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Bool(left != right))
        }
        (IrExprKind::Integer(left), BinaryOp::Less, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Bool(left < right))
        }
        (IrExprKind::Integer(left), BinaryOp::LessEqual, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Bool(left <= right))
        }
        (IrExprKind::Integer(left), BinaryOp::Greater, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Bool(left > right))
        }
        (IrExprKind::Integer(left), BinaryOp::GreaterEqual, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Bool(left >= right))
        }
        (IrExprKind::Bool(left), BinaryOp::Equal, IrExprKind::Bool(right)) => {
            Some(IrExprKind::Bool(left == right))
        }
        (IrExprKind::Bool(left), BinaryOp::NotEqual, IrExprKind::Bool(right)) => {
            Some(IrExprKind::Bool(left != right))
        }
        (IrExprKind::Bool(left), BinaryOp::And, IrExprKind::Bool(right)) => {
            Some(IrExprKind::Bool(*left && *right))
        }
        (IrExprKind::Bool(left), BinaryOp::Or, IrExprKind::Bool(right)) => {
            Some(IrExprKind::Bool(*left || *right))
        }
        (IrExprKind::String(left), BinaryOp::Equal, IrExprKind::String(right)) => {
            Some(IrExprKind::Bool(left == right))
        }
        (IrExprKind::String(left), BinaryOp::NotEqual, IrExprKind::String(right)) => {
            Some(IrExprKind::Bool(left != right))
        }
        // Integer bitwise folds. Shifts reuse the shared masked-shift helpers so
        // a folded constant is bit-identical to the interpreted result.
        (IrExprKind::Integer(left), BinaryOp::BitAnd, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left & right))
        }
        (IrExprKind::Integer(left), BinaryOp::BitOr, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left | right))
        }
        (IrExprKind::Integer(left), BinaryOp::BitXor, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left ^ right))
        }
        (IrExprKind::Integer(left), BinaryOp::Shl, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(shift_left(*left, *right)))
        }
        (IrExprKind::Integer(left), BinaryOp::Shr, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(shift_right(*left, *right)))
        }
        _ => None,
    }
}
