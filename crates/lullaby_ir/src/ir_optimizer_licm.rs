//! Loop-invariant code motion (LICM) pass. Split out of `ir_optimizer.rs`;
//! behavior-preserving move. The shared `ExprSignature` type and
//! `combine_signatures` helper live in `ir_optimizer.rs` (also used by CSE). See
//! `ir_optimizer.rs` for the pass pipeline that drives it.

use super::*;

#[derive(Default)]
pub(crate) struct LoopInvariantMover {
    pub(crate) hoisted_loop_invariants: usize,
    next_temp: usize,
    reserved_names: HashSet<String>,
}

impl LoopInvariantMover {
    pub(crate) fn move_module(&mut self, module: &IrModule) -> IrModule {
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
                .map(|function| self.move_function(function))
                .collect(),
        }
    }

    fn move_function(&mut self, function: &IrFunction) -> IrFunction {
        self.next_temp = 0;
        self.reserved_names = collect_function_variable_names(function);
        let mut available = function
            .params
            .iter()
            .map(|param| param.name.clone())
            .collect::<HashSet<_>>();

        IrFunction {
            name: function.name.clone(),
            params: function.params.clone(),
            return_type: function.return_type.clone(),
            body: self.move_block(&function.body, &mut available),
            span: function.span,
        }
    }

    fn move_block(
        &mut self,
        statements: &[IrStmt],
        available: &mut HashSet<String>,
    ) -> Vec<IrStmt> {
        let mut moved = Vec::new();

        for statement in statements {
            let statements = self.move_statement(statement, available);
            for statement in statements {
                add_available_declaration(&statement, available);
                moved.push(statement);
            }
        }

        moved
    }

    fn move_statement(&mut self, statement: &IrStmt, available: &HashSet<String>) -> Vec<IrStmt> {
        match statement {
            IrStmt::If {
                branches,
                else_body,
                span,
            } => {
                let branches = branches
                    .iter()
                    .map(|branch| {
                        let mut branch_available = available.clone();
                        IrIfBranch {
                            condition: branch.condition.clone(),
                            body: self.move_block(&branch.body, &mut branch_available),
                        }
                    })
                    .collect();
                let mut else_available = available.clone();
                vec![IrStmt::If {
                    branches,
                    else_body: self.move_block(else_body, &mut else_available),
                    span: *span,
                }]
            }
            IrStmt::While {
                condition,
                body,
                span,
            } => {
                let mut body_available = available.clone();
                let body = self.move_block(body, &mut body_available);
                let (mut hoisted, body) = self.hoist_loop_body(body, available);
                hoisted.push(IrStmt::While {
                    condition: condition.clone(),
                    body,
                    span: *span,
                });
                hoisted
            }
            IrStmt::For {
                name,
                start,
                end,
                step,
                body,
                span,
            } => {
                let mut body_available = available.clone();
                body_available.insert(name.clone());
                let body = self.move_block(body, &mut body_available);
                let (mut hoisted, body) = self.hoist_loop_body(body, available);
                hoisted.push(IrStmt::For {
                    name: name.clone(),
                    start: start.clone(),
                    end: end.clone(),
                    step: step.clone(),
                    body,
                    span: *span,
                });
                hoisted
            }
            IrStmt::Loop { body, span } => {
                let mut body_available = available.clone();
                let body = self.move_block(body, &mut body_available);
                let (mut hoisted, body) = self.hoist_loop_body(body, available);
                hoisted.push(IrStmt::Loop { body, span: *span });
                hoisted
            }
            // A region block is treated as an opaque passthrough, exactly like
            // `if`/`try`/`match`: LICM does not hoist across or into it (conservative,
            // and it preserves the region's scope boundary intact for slot planning).
            IrStmt::Let { .. }
            | IrStmt::Assign { .. }
            | IrStmt::Return(_)
            | IrStmt::Break(_)
            | IrStmt::Continue(_)
            | IrStmt::Throw { .. }
            | IrStmt::Try { .. }
            | IrStmt::Match { .. }
            | IrStmt::RegionBlock { .. }
            | IrStmt::Asm { .. }
            | IrStmt::Expr(_) => vec![statement.clone()],
        }
    }

    fn hoist_loop_body(
        &mut self,
        body: Vec<IrStmt>,
        pre_loop_available: &HashSet<String>,
    ) -> (Vec<IrStmt>, Vec<IrStmt>) {
        let mut loop_declared = HashSet::new();
        collect_declared_names(&body, &mut loop_declared);
        let mut loop_mutated = HashSet::new();
        collect_mutated_names(&body, &mut loop_mutated);

        let mut hoisted = Vec::new();
        let mut rewritten_body = Vec::new();

        for statement in body {
            let IrStmt::Let {
                name,
                ty,
                value,
                span,
            } = statement
            else {
                rewritten_body.push(statement);
                continue;
            };

            let Some(signature) = loop_invariant_expr_signature(&value) else {
                rewritten_body.push(IrStmt::Let {
                    name,
                    ty,
                    value,
                    span,
                });
                continue;
            };

            if !is_hoist_worthwhile(&value)
                || !signature
                    .dependencies
                    .iter()
                    .all(|name| pre_loop_available.contains(name))
                || signature
                    .dependencies
                    .iter()
                    .any(|name| loop_declared.contains(name) || loop_mutated.contains(name))
            {
                rewritten_body.push(IrStmt::Let {
                    name,
                    ty,
                    value,
                    span,
                });
                continue;
            }

            let temp = self.next_temp_name();
            let temp_expr_span = value.span;
            hoisted.push(IrStmt::Let {
                name: temp.clone(),
                ty: ty.clone(),
                value,
                span,
            });
            rewritten_body.push(IrStmt::Let {
                name,
                ty: ty.clone(),
                value: IrExpr {
                    kind: IrExprKind::Variable(temp),
                    ty,
                    span: temp_expr_span,
                },
                span,
            });
            self.hoisted_loop_invariants += 1;
        }

        (hoisted, rewritten_body)
    }

    fn next_temp_name(&mut self) -> String {
        loop {
            let name = format!("__lullaby_loop_invariant_{}", self.next_temp);
            self.next_temp += 1;
            if self.reserved_names.insert(name.clone()) {
                return name;
            }
        }
    }
}

fn add_available_declaration(statement: &IrStmt, available: &mut HashSet<String>) {
    if let IrStmt::Let { name, .. } = statement {
        available.insert(name.clone());
    }
}

fn collect_function_variable_names(function: &IrFunction) -> HashSet<String> {
    let mut names = function
        .params
        .iter()
        .map(|param| param.name.clone())
        .collect::<HashSet<_>>();
    collect_declared_names(&function.body, &mut names);
    names
}

fn collect_declared_names(statements: &[IrStmt], names: &mut HashSet<String>) {
    for statement in statements {
        match statement {
            IrStmt::Let { name, .. } => {
                names.insert(name.clone());
            }
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    collect_declared_names(&branch.body, names);
                }
                collect_declared_names(else_body, names);
            }
            IrStmt::While { body, .. }
            | IrStmt::Loop { body, .. }
            | IrStmt::RegionBlock { body, .. } => {
                collect_declared_names(body, names);
            }
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => {
                names.insert(catch_name.clone());
                collect_declared_names(body, names);
                collect_declared_names(catch_body, names);
            }
            IrStmt::Match { arms, .. } => {
                for arm in arms {
                    if let IrMatchPattern::Variant { bindings, .. } = &arm.pattern {
                        for binding in bindings {
                            names.insert(binding.clone());
                        }
                    }
                    collect_declared_names(&arm.body, names);
                }
            }
            IrStmt::For { name, body, .. } => {
                names.insert(name.clone());
                collect_declared_names(body, names);
            }
            IrStmt::Assign { .. }
            | IrStmt::Return(_)
            | IrStmt::Break(_)
            | IrStmt::Continue(_)
            | IrStmt::Throw { .. }
            | IrStmt::Asm { .. }
            | IrStmt::Expr(_) => {}
        }
    }
}

fn collect_mutated_names(statements: &[IrStmt], names: &mut HashSet<String>) {
    for statement in statements {
        match statement {
            IrStmt::Assign { name, .. } => {
                names.insert(name.clone());
            }
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    collect_mutated_names(&branch.body, names);
                }
                collect_mutated_names(else_body, names);
            }
            IrStmt::While { body, .. }
            | IrStmt::Loop { body, .. }
            | IrStmt::RegionBlock { body, .. } => {
                collect_mutated_names(body, names);
            }
            IrStmt::Try {
                body, catch_body, ..
            } => {
                collect_mutated_names(body, names);
                collect_mutated_names(catch_body, names);
            }
            IrStmt::Match { arms, .. } => {
                for arm in arms {
                    collect_mutated_names(&arm.body, names);
                }
            }
            IrStmt::For { name, body, .. } => {
                names.insert(name.clone());
                collect_mutated_names(body, names);
            }
            IrStmt::Let { .. }
            | IrStmt::Return(_)
            | IrStmt::Break(_)
            | IrStmt::Continue(_)
            | IrStmt::Throw { .. }
            | IrStmt::Asm { .. }
            | IrStmt::Expr(_) => {}
        }
    }
}

fn loop_invariant_expr_signature(expr: &IrExpr) -> Option<ExprSignature> {
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
        IrExprKind::Unary { op, expr: inner } => {
            let inner = loop_invariant_expr_signature(inner)?;
            combine_signatures(&format!("unary:{op:?}"), &expr.ty.name, vec![inner])
        }
        IrExprKind::Binary { left, op, right } => {
            if matches!(op, BinaryOp::Divide | BinaryOp::Remainder) {
                return None;
            }
            let left = loop_invariant_expr_signature(left)?;
            let right = loop_invariant_expr_signature(right)?;
            combine_signatures(&format!("binary:{op:?}"), &expr.ty.name, vec![left, right])
        }
        IrExprKind::Array(_)
        | IrExprKind::Index { .. }
        | IrExprKind::Field { .. }
        | IrExprKind::Call { .. }
        | IrExprKind::Await { .. }
        // A closure captures the live environment at evaluation time, so it is
        // never loop-invariant (its captured values may change per iteration).
        | IrExprKind::Closure { .. } => return None,
    };

    Some(ExprSignature { key, dependencies })
}

fn is_hoist_worthwhile(expr: &IrExpr) -> bool {
    matches!(
        expr.kind,
        IrExprKind::Unary { .. } | IrExprKind::Binary { .. }
    )
}
