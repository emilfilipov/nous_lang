//! Dead-code-elimination pass. Split out of `ir_optimizer.rs`;
//! behavior-preserving move. Drops statements that follow an unconditional
//! terminator in a block. See `ir_optimizer.rs` for the pass pipeline that
//! drives it; `is_unconditional_terminator` lives in the crate root.

use super::*;

#[derive(Default)]
pub(crate) struct DeadCodeEliminator {
    pub(crate) removed_statements: usize,
}

impl DeadCodeEliminator {
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
            body: self.eliminate_block(&function.body),
            span: function.span,
        }
    }

    fn eliminate_block(&mut self, statements: &[IrStmt]) -> Vec<IrStmt> {
        let mut kept = Vec::new();
        let mut terminated = false;

        for statement in statements {
            if terminated {
                self.removed_statements += 1;
                continue;
            }

            let statement = self.eliminate_statement(statement);
            terminated = is_unconditional_terminator(&statement);
            kept.push(statement);
        }

        kept
    }

    fn eliminate_statement(&mut self, statement: &IrStmt) -> IrStmt {
        match statement {
            IrStmt::If {
                branches,
                else_body,
                span,
            } => IrStmt::If {
                branches: branches
                    .iter()
                    .map(|branch| IrIfBranch {
                        condition: branch.condition.clone(),
                        body: self.eliminate_block(&branch.body),
                    })
                    .collect(),
                else_body: self.eliminate_block(else_body),
                span: *span,
            },
            IrStmt::While {
                condition,
                body,
                span,
            } => IrStmt::While {
                condition: condition.clone(),
                body: self.eliminate_block(body),
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
                start: start.clone(),
                end: end.clone(),
                step: step.clone(),
                body: self.eliminate_block(body),
                span: *span,
            },
            IrStmt::Loop { body, span } => IrStmt::Loop {
                body: self.eliminate_block(body),
                span: *span,
            },
            // Inline assembly has an observable machine-code effect; it is never
            // dead. Preserve it verbatim.
            IrStmt::Asm { bytes, span } => IrStmt::Asm {
                bytes: bytes.clone(),
                span: *span,
            },
            IrStmt::Throw { value, span } => IrStmt::Throw {
                value: value.clone(),
                span: *span,
            },
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => IrStmt::Try {
                body: self.eliminate_block(body),
                catch_name: catch_name.clone(),
                catch_body: self.eliminate_block(catch_body),
                span: *span,
            },
            IrStmt::Match {
                scrutinee,
                arms,
                span,
            } => IrStmt::Match {
                scrutinee: scrutinee.clone(),
                arms: arms
                    .iter()
                    .map(|arm| IrMatchArm {
                        pattern: arm.pattern.clone(),
                        body: self.eliminate_block(&arm.body),
                    })
                    .collect(),
                span: *span,
            },
            IrStmt::Let { .. }
            | IrStmt::Assign { .. }
            | IrStmt::Return(_)
            | IrStmt::Break(_)
            | IrStmt::Continue(_)
            | IrStmt::Expr(_) => statement.clone(),
        }
    }
}
