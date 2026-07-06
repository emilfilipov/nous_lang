use std::collections::{HashMap, HashSet};

use lullaby_diagnostics::Span;
use lullaby_parser::{
    AssignOp, BinaryOp, Expr, ExprKind, Function, IfBranch, Param, Program, RegionDecl, Stmt,
    TypeRef, UnaryOp,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub function: Option<String>,
    pub span: Option<Span>,
}

impl SemanticDiagnostic {
    fn new(code: &'static str, message: impl Into<String>, function: Option<String>) -> Self {
        Self {
            code,
            message: message.into(),
            function,
            span: None,
        }
    }

    fn at(
        code: &'static str,
        message: impl Into<String>,
        function: Option<String>,
        span: Span,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            function,
            span: Some(span),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckedProgram {
    pub program: Program,
    pub info: SemanticInfo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticInfo {
    pub signatures: HashMap<String, Signature>,
    pub expression_types: Vec<ExpressionType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpressionType {
    pub function: String,
    pub span: Span,
    pub ty: TypeRef,
}

pub fn validate(program: &Program) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
    // Resolve type aliases to their canonical types before any checking, so the
    // rest of the pipeline (and IR/runtime) never sees an alias. Aliases carry
    // no runtime representation, so runtime layout is unchanged.
    let (resolved, alias_diagnostics) = resolve_program_aliases(program);

    let mut checker = Checker::new(&resolved);
    checker.diagnostics = alias_diagnostics;
    checker.validate();
    if !checker.diagnostics.is_empty() {
        return Err(std::mem::take(&mut checker.diagnostics));
    }

    let signatures = std::mem::take(&mut checker.signatures);
    let expression_types = std::mem::take(&mut checker.expression_types);
    drop(checker);
    Ok(CheckedProgram {
        program: resolved,
        info: SemanticInfo {
            signatures,
            expression_types,
        },
    })
}

/// Resolve all type aliases in a program to canonical types, returning the
/// rewritten program plus any alias-definition diagnostics (duplicate `N0360`,
/// cyclic `N0361`).
fn resolve_program_aliases(program: &Program) -> (Program, Vec<SemanticDiagnostic>) {
    let mut diagnostics = Vec::new();
    let mut map: HashMap<String, TypeRef> = HashMap::new();
    for alias in &program.aliases {
        if map.contains_key(&alias.name) {
            diagnostics.push(SemanticDiagnostic::at(
                "N0360",
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
                "N0361",
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
        })
        .collect();

    (
        Program {
            functions,
            aliases: program.aliases.clone(),
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
        other => other.clone(),
    }
}

pub fn validate_executable(program: &Program) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
    let checked = validate(program)?;
    validate_entrypoint(program)?;
    Ok(checked)
}

pub fn validate_entrypoint(program: &Program) -> Result<(), Vec<SemanticDiagnostic>> {
    let Some(main) = program
        .functions
        .iter()
        .find(|function| function.name == "main")
    else {
        return Err(vec![SemanticDiagnostic::new(
            "N0329",
            "executable source must define a zero-argument `main` function",
            None,
        )]);
    };

    if main.params.is_empty() {
        Ok(())
    } else {
        Err(vec![SemanticDiagnostic::at(
            "N0329",
            format!(
                "executable `main` must take zero arguments but declares {}",
                main.params.len()
            ),
            Some(main.name.clone()),
            main.span,
        )])
    }
}

struct Checker<'a> {
    program: &'a Program,
    signatures: HashMap<String, Signature>,
    expression_types: Vec<ExpressionType>,
    diagnostics: Vec<SemanticDiagnostic>,
    loop_depth: usize,
    unsafe_depth: usize,
    region_names: HashSet<String>,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program) -> Self {
        Self {
            program,
            signatures: HashMap::new(),
            expression_types: Vec::new(),
            diagnostics: Vec::new(),
            loop_depth: 0,
            unsafe_depth: 0,
            region_names: HashSet::new(),
        }
    }

    fn validate(&mut self) {
        self.collect_signatures();
        for function in &self.program.functions {
            self.validate_function(function);
        }
    }

    fn collect_signatures(&mut self) {
        for function in &self.program.functions {
            if self.signatures.contains_key(&function.name) {
                self.diagnostics.push(SemanticDiagnostic::new(
                    "N0300",
                    format!("duplicate function `{}`", function.name),
                    Some(function.name.clone()),
                ));
                continue;
            }
            self.signatures.insert(
                function.name.clone(),
                Signature {
                    params: function
                        .params
                        .iter()
                        .map(|param| param.ty.clone())
                        .collect(),
                    return_type: function.return_type.clone(),
                },
            );
        }
    }

    fn validate_function(&mut self, function: &Function) {
        self.region_names.clear();
        let mut scope = Scope::default();
        for param in &function.params {
            if scope
                .locals
                .insert(param.name.clone(), param.ty.clone())
                .is_some()
            {
                self.diagnostics.push(SemanticDiagnostic::new(
                    "N0302",
                    format!("duplicate parameter `{}`", param.name),
                    Some(function.name.clone()),
                ));
            }
        }

        let block_type = self.check_block(&function.body, &mut scope, function);
        self.check_lifetimes(function);
        if function.return_type.is_void() {
            return;
        }

        if block_type.as_ref() != Some(&function.return_type) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0301",
                format!(
                    "function `{}` declares `{}` but has no final return value of that type",
                    function.name, function.return_type.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
    }

    fn check_block(
        &mut self,
        statements: &[Stmt],
        scope: &mut Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let mut last_type = None;
        for statement in statements {
            last_type = self.check_statement(statement, scope, function);
            if matches!(
                statement,
                Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Throw { .. }
            ) {
                break;
            }
        }
        last_type
    }

    fn check_statement(
        &mut self,
        statement: &Stmt,
        scope: &mut Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        match statement {
            Stmt::Let {
                name, ty, value, ..
            } => {
                let value_type = self.check_expr(value, scope, function);
                let binding_type = match ty {
                    Some(declared) => {
                        if value_type.as_ref() != Some(declared) {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0303",
                                format!(
                                    "binding `{name}` declares `{}` but initializer has `{}`",
                                    declared.name,
                                    value_type
                                        .as_ref()
                                        .map(|ty| ty.name.as_str())
                                        .unwrap_or("<unknown>")
                                ),
                                Some(function.name.clone()),
                                value.span,
                            ));
                        }
                        declared.clone()
                    }
                    None => value_type
                        .clone()
                        .unwrap_or_else(|| TypeRef::new("<unknown>")),
                };
                if ty.is_none() && binding_type.is_void() {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0303",
                        format!("binding `{name}` cannot infer type from a void initializer"),
                        Some(function.name.clone()),
                        value.span,
                    ));
                }
                scope.locals.insert(name.clone(), binding_type);
                None
            }
            Stmt::Assign {
                name,
                op,
                value,
                span,
            } => {
                let expected = scope.locals.get(name).cloned();
                let value_type = self.check_expr(value, scope, function);
                match expected {
                    Some(expected) => {
                        if *op == AssignOp::Replace {
                            if value_type.as_ref() != Some(&expected) {
                                self.diagnostics.push(SemanticDiagnostic::at(
                                    "N0314",
                                    format!(
                                        "assignment to `{name}` expects `{}` but got `{}`",
                                        expected.name,
                                        value_type
                                            .as_ref()
                                            .map(|ty| ty.name.as_str())
                                            .unwrap_or("<unknown>")
                                    ),
                                    Some(function.name.clone()),
                                    value.span,
                                ));
                            }
                        } else if expected != TypeRef::new("i64")
                            || value_type.as_ref() != Some(&TypeRef::new("i64"))
                        {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0315",
                                format!("compound assignment to `{name}` requires i64 operands"),
                                Some(function.name.clone()),
                                value.span,
                            ));
                        }
                    }
                    None => {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "N0316",
                            format!("assignment target `{name}` is not declared"),
                            Some(function.name.clone()),
                            *span,
                        ));
                    }
                }
                None
            }
            Stmt::Return(expr) => {
                let actual = expr
                    .as_ref()
                    .map(|expr| self.check_expr(expr, scope, function))
                    .unwrap_or_else(|| Some(TypeRef::new("void")));
                if actual.as_ref() != Some(&function.return_type) {
                    let span = expr.as_ref().map(|expr| expr.span).unwrap_or(function.span);
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0304",
                        format!(
                            "return type `{}` does not match function return `{}`",
                            actual
                                .as_ref()
                                .map(|ty| ty.name.as_str())
                                .unwrap_or("<unknown>"),
                            function.return_type.name
                        ),
                        Some(function.name.clone()),
                        span,
                    ));
                }
                actual
            }
            Stmt::Break(span) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0317",
                        "`break` can only appear inside a loop",
                        Some(function.name.clone()),
                        *span,
                    ));
                }
                None
            }
            Stmt::Continue(span) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0318",
                        "`continue` can only appear inside a loop",
                        Some(function.name.clone()),
                        *span,
                    ));
                }
                None
            }
            Stmt::Expr(expr) => self.check_expr(expr, scope, function),
            Stmt::If {
                branches,
                else_body,
                ..
            } => {
                let mut branch_types = Vec::new();
                for branch in branches {
                    let condition_type = self.check_expr(&branch.condition, scope, function);
                    if condition_type.as_ref() != Some(&TypeRef::new("bool")) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "N0305",
                            "if condition must be bool",
                            Some(function.name.clone()),
                            branch.condition.span,
                        ));
                    }
                    let mut branch_scope = scope.clone();
                    branch_types.push(self.check_block(&branch.body, &mut branch_scope, function));
                }
                let mut else_scope = scope.clone();
                let else_type = self.check_block(else_body, &mut else_scope, function);
                if else_body.is_empty() {
                    return None;
                }
                if branch_types
                    .iter()
                    .all(|branch_type| branch_type.as_ref() == else_type.as_ref())
                {
                    else_type
                } else {
                    None
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                let condition_type = self.check_expr(condition, scope, function);
                if condition_type.as_ref() != Some(&TypeRef::new("bool")) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0305",
                        "while condition must be bool",
                        Some(function.name.clone()),
                        condition.span,
                    ));
                }
                let mut loop_scope = scope.clone();
                self.loop_depth += 1;
                self.check_block(body, &mut loop_scope, function);
                self.loop_depth -= 1;
                None
            }
            Stmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                for (label, expr) in [("start", start), ("end", end)] {
                    let expr_type = self.check_expr(expr, scope, function);
                    if expr_type.as_ref() != Some(&TypeRef::new("i64")) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "N0321",
                            format!("for loop {label} expression must be i64"),
                            Some(function.name.clone()),
                            expr.span,
                        ));
                    }
                }
                if let Some(step) = step {
                    let step_type = self.check_expr(step, scope, function);
                    if step_type.as_ref() != Some(&TypeRef::new("i64")) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "N0322",
                            "for loop step expression must be i64",
                            Some(function.name.clone()),
                            step.span,
                        ));
                    }
                }
                let mut loop_scope = scope.clone();
                loop_scope.locals.insert(name.clone(), TypeRef::new("i64"));
                self.loop_depth += 1;
                self.check_block(body, &mut loop_scope, function);
                self.loop_depth -= 1;
                None
            }
            Stmt::Loop { body, .. } => {
                let mut loop_scope = scope.clone();
                self.loop_depth += 1;
                self.check_block(body, &mut loop_scope, function);
                self.loop_depth -= 1;
                None
            }
            Stmt::Unsafe { body, .. } => {
                // `unsafe` is a transparent compile-time gate: its body runs in
                // the enclosing scope, but raw-pointer operations inside it are
                // permitted. Locals declared here remain visible afterward, to
                // match IR lowering, which inlines the body.
                self.unsafe_depth += 1;
                let block_type = self.check_block(body, scope, function);
                self.unsafe_depth -= 1;
                block_type
            }
            Stmt::Region(decl) => {
                self.check_region(decl, function);
                None
            }
            Stmt::Throw { value, .. } => {
                self.expect_arg_type("throw", 1, value, "string", scope, function);
                // `throw` diverges, so it is compatible with any return type.
                Some(function.return_type.clone())
            }
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => {
                let mut try_scope = scope.clone();
                self.check_block(body, &mut try_scope, function);
                let mut catch_scope = scope.clone();
                // The caught error is exposed to the handler as a string message.
                catch_scope
                    .locals
                    .insert(catch_name.clone(), TypeRef::new("string"));
                self.check_block(catch_body, &mut catch_scope, function);
                None
            }
        }
    }

    fn check_region(&mut self, decl: &RegionDecl, function: &Function) {
        if decl.size <= 0 {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0340",
                format!("region `{}` size must be positive", decl.name),
                Some(function.name.clone()),
                decl.span,
            ));
        }
        if let Some(align) = decl.align
            && (align <= 0 || (align & (align - 1)) != 0)
        {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0340",
                format!(
                    "region `{}` alignment must be a positive power of two",
                    decl.name
                ),
                Some(function.name.clone()),
                decl.span,
            ));
        }
        if !matches!(decl.kind.as_str(), "static" | "dynamic") {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0340",
                format!(
                    "region `{}` kind `{}` must be `static` or `dynamic`",
                    decl.name, decl.kind
                ),
                Some(function.name.clone()),
                decl.span,
            ));
        }
        if !self.region_names.insert(decl.name.clone()) {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0341",
                format!("duplicate region `{}`", decl.name),
                Some(function.name.clone()),
                decl.span,
            ));
        }
    }

    /// Conservative compile-time lifetime analysis.
    ///
    /// - A borrowed `ref<T>` may not be returned from a function, because the
    ///   borrow cannot outlive the owner it points into (`N0351`).
    /// - Straight-line use-after-free / double-free of a resource freed by
    ///   `dealloc`/`rc_release` is reported (`N0350`). The per-block cleanup
    ///   ordering itself is the deterministic plan produced by
    ///   `lullaby_ir::frame_layout`.
    fn check_lifetimes(&mut self, function: &Function) {
        if function.return_type.reference_target().is_some() {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0351",
                format!(
                    "function `{}` returns borrowed `{}`, which cannot escape its owner's scope",
                    function.name, function.return_type.name
                ),
                Some(function.name.clone()),
                function.span,
            ));
        }
        let mut freed: HashSet<String> = HashSet::new();
        self.walk_lifetimes(&function.body, &mut freed, function);
    }

    fn walk_lifetimes(&mut self, body: &[Stmt], freed: &mut HashSet<String>, function: &Function) {
        for statement in body {
            match statement {
                Stmt::Let { name, value, .. } => {
                    self.check_freed_uses(value, freed, function);
                    // Re-binding revives a name.
                    freed.remove(name);
                }
                Stmt::Assign { name, value, .. } => {
                    self.check_freed_uses(value, freed, function);
                    freed.remove(name);
                }
                Stmt::Return(Some(expr)) | Stmt::Expr(expr) => {
                    if let Some(target) = free_call_target(expr) {
                        // The freeing call may double-free an already-dead resource.
                        if freed.contains(target) {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0350",
                                format!("`{target}` is used after it was already freed"),
                                Some(function.name.clone()),
                                expr.span,
                            ));
                        }
                        freed.insert(target.to_string());
                    } else {
                        self.check_freed_uses(expr, freed, function);
                    }
                }
                Stmt::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for branch in branches {
                        self.check_freed_uses(&branch.condition, freed, function);
                        self.walk_lifetimes(&branch.body, &mut freed.clone(), function);
                    }
                    self.walk_lifetimes(else_body, &mut freed.clone(), function);
                }
                Stmt::While {
                    condition, body, ..
                } => {
                    self.check_freed_uses(condition, freed, function);
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                }
                Stmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    self.check_freed_uses(start, freed, function);
                    self.check_freed_uses(end, freed, function);
                    if let Some(step) = step {
                        self.check_freed_uses(step, freed, function);
                    }
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                }
                Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } => {
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                }
                Stmt::Throw { value, .. } => {
                    self.check_freed_uses(value, freed, function);
                }
                Stmt::Try {
                    body, catch_body, ..
                } => {
                    self.walk_lifetimes(body, &mut freed.clone(), function);
                    self.walk_lifetimes(catch_body, &mut freed.clone(), function);
                }
                Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::Region(_) => {}
            }
        }
    }

    /// Flag any use of a freed binding inside an expression.
    fn check_freed_uses(&mut self, expr: &Expr, freed: &HashSet<String>, function: &Function) {
        match &expr.kind {
            ExprKind::Variable(name) => {
                if freed.contains(name) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0350",
                        format!("`{name}` is used after it was freed"),
                        Some(function.name.clone()),
                        expr.span,
                    ));
                }
            }
            ExprKind::Array(values) => {
                for value in values {
                    self.check_freed_uses(value, freed, function);
                }
            }
            ExprKind::Index { target, index } => {
                self.check_freed_uses(target, freed, function);
                self.check_freed_uses(index, freed, function);
            }
            ExprKind::Unary { expr, .. } => self.check_freed_uses(expr, freed, function),
            ExprKind::Binary { left, right, .. } => {
                self.check_freed_uses(left, freed, function);
                self.check_freed_uses(right, freed, function);
            }
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.check_freed_uses(arg, freed, function);
                }
            }
            ExprKind::Integer(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::String(_) => {
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr, scope: &Scope, function: &Function) -> Option<TypeRef> {
        let inferred = match &expr.kind {
            ExprKind::Integer(_) => Some(TypeRef::new("i64")),
            ExprKind::Float(_) => Some(TypeRef::new("f64")),
            ExprKind::Bool(_) => Some(TypeRef::new("bool")),
            ExprKind::String(_) => Some(TypeRef::new("string")),
            ExprKind::Array(values) => self.check_array_literal(values, scope, function),
            ExprKind::Variable(name) => match scope.locals.get(name) {
                Some(ty) => Some(ty.clone()),
                None => {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0306",
                        format!("unknown variable `{name}`"),
                        Some(function.name.clone()),
                        expr.span,
                    ));
                    None
                }
            },
            ExprKind::Index { target, index } => {
                let target_type = self.check_expr(target, scope, function);
                let index_type = self.check_expr(index, scope, function);
                if index_type.as_ref() != Some(&TypeRef::new("i64")) {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0326",
                        "array index expression must be i64",
                        Some(function.name.clone()),
                        index.span,
                    ));
                }

                match target_type.and_then(|ty| ty.array_element()) {
                    Some(element_type) => Some(element_type),
                    None => {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "N0325",
                            "index target must be an array",
                            Some(function.name.clone()),
                            target.span,
                        ));
                        None
                    }
                }
            }
            ExprKind::Unary { op, expr } => {
                let expr_type = self.check_expr(expr, scope, function);
                match op {
                    UnaryOp::Not => {
                        if expr_type.as_ref() == Some(&TypeRef::new("bool")) {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0319",
                                "`not` operand must be bool",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                }
            }
            ExprKind::Binary { left, op, right } => {
                let left_type = self.check_expr(left, scope, function);
                let right_type = self.check_expr(right, scope, function);
                let same_numeric = same_numeric_type(&left_type, &right_type);
                match op {
                    BinaryOp::Add => {
                        let string_type = TypeRef::new("string");
                        if let Some(numeric) = same_numeric.clone() {
                            Some(numeric)
                        } else if left_type.as_ref() == Some(&string_type)
                            && right_type.as_ref() == Some(&string_type)
                        {
                            // `+` concatenates two strings.
                            Some(string_type)
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0307",
                                "operands of `+` must both be i64, both be f64, or both be string",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                        if let Some(numeric) = same_numeric.clone() {
                            Some(numeric)
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0307",
                                "arithmetic operands must both be i64 or both be f64",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Equal | BinaryOp::NotEqual => {
                        if left_type.is_some() && left_type == right_type {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0308",
                                "comparison operands must have the same type",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual => {
                        if same_numeric.is_some() {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0327",
                                "ordering comparison operands must both be i64 or both be f64",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                    BinaryOp::And | BinaryOp::Or => {
                        if left_type.as_ref() == Some(&TypeRef::new("bool"))
                            && right_type.as_ref() == Some(&TypeRef::new("bool"))
                        {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::at(
                                "N0320",
                                "logical operands must both be bool",
                                Some(function.name.clone()),
                                expr.span,
                            ));
                            None
                        }
                    }
                }
            }
            ExprKind::Call { name, args } => {
                self.check_call(name, args, expr.span, scope, function)
            }
        };

        if let Some(ty) = &inferred {
            self.expression_types.push(ExpressionType {
                function: function.name.clone(),
                span: expr.span,
                ty: ty.clone(),
            });
        }

        inferred
    }

    fn check_array_literal(
        &mut self,
        values: &[Expr],
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let Some((first, rest)) = values.split_first() else {
            self.diagnostics.push(SemanticDiagnostic::new(
                "N0323",
                "array literals must contain at least one value in the current alpha",
                Some(function.name.clone()),
            ));
            return None;
        };

        let element_type = self.check_expr(first, scope, function)?;
        for value in rest {
            let value_type = self.check_expr(value, scope, function);
            if value_type.as_ref() != Some(&element_type) {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "N0324",
                    "array literal values must all have the same type",
                    Some(function.name.clone()),
                    value.span,
                ));
            }
        }

        Some(TypeRef::new(format!("array<{}>", element_type.name)))
    }

    fn check_call(
        &mut self,
        name: &str,
        args: &[Expr],
        call_span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        match name {
            "alloc" => {
                self.expect_arg_count(name, args, 1, function)?;
                let value_type = self.check_expr(&args[0], scope, function)?;
                Some(TypeRef::new(format!("ptr_{}", value_type.name)))
            }
            "load" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                ptr_type
                    .name
                    .strip_prefix("ptr_")
                    .map(TypeRef::new)
                    .or_else(|| {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "N0310",
                            "load expects a pointer argument",
                            Some(function.name.clone()),
                            args[0].span,
                        ));
                        None
                    })
            }
            "store" => {
                self.expect_arg_count(name, args, 2, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                let value_type = self.check_expr(&args[1], scope, function)?;
                let Some(expected) = ptr_type.name.strip_prefix("ptr_").map(TypeRef::new) else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0310",
                        "store expects a pointer as its first argument",
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    return None;
                };
                if value_type != expected {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0328",
                        format!(
                            "store expects value `{}` for pointer `{}` but got `{}`",
                            expected.name, ptr_type.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("void"))
            }
            "dealloc" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                if ptr_type.name.starts_with("ptr_") {
                    Some(TypeRef::new("void"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0311",
                        "dealloc expects a pointer argument",
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "read_file" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("string"))
            }
            "write_file" | "append_file" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_arg_type(name, 2, &args[1], "string", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "file_exists" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("bool"))
            }
            "sys_status" | "sys_output" => {
                self.expect_arg_count(name, args, 2, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                self.expect_arg_type(name, 2, &args[1], "array<string>", scope, function)?;
                Some(TypeRef::new(if name == "sys_status" {
                    "i64"
                } else {
                    "string"
                }))
            }
            "print" | "println" | "warn" => {
                self.expect_arg_count(name, args, 1, function)?;
                self.expect_arg_type(name, 1, &args[0], "string", scope, function)?;
                Some(TypeRef::new("void"))
            }
            "flush" => {
                self.expect_arg_count(name, args, 0, function)?;
                Some(TypeRef::new("void"))
            }
            "to_string" => {
                self.expect_arg_count(name, args, 1, function)?;
                let arg_type = self.check_expr(&args[0], scope, function)?;
                if matches!(arg_type.name.as_str(), "i64" | "f64" | "bool" | "string") {
                    Some(TypeRef::new("string"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0313",
                        format!(
                            "to_string expects an i64, f64, bool, or string value but got `{}`",
                            arg_type.name
                        ),
                        Some(function.name.clone()),
                        args[0].span,
                    ));
                    None
                }
            }
            "rc_new" => {
                self.expect_arg_count(name, args, 1, function)?;
                let value_type = self.check_expr(&args[0], scope, function)?;
                Some(TypeRef::new(format!("rc<{}>", value_type.name)))
            }
            "rc_clone" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("rc_clone", "rc", &ty, args[0].span, function)?;
                Some(ty)
            }
            "rc_release" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("rc_release", "rc", &ty, args[0].span, function)?;
                Some(TypeRef::new("void"))
            }
            "rc_get" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("rc_get", "rc", &ty, args[0].span, function)
            }
            "rc_borrow" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                let inner =
                    self.expect_reference("rc_borrow", "rc", &ty, args[0].span, function)?;
                Some(TypeRef::new(format!("ref<{}>", inner.name)))
            }
            "ref_get" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                self.expect_reference("ref_get", "ref", &ty, args[0].span, function)
            }
            "ptr_read" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ty = self.check_expr(&args[0], scope, function)?;
                let inner = self.expect_raw_pointer("ptr_read", &ty, args[0].span, function)?;
                self.require_unsafe("ptr_read", call_span, function)?;
                Some(inner)
            }
            "ptr_write" => {
                self.expect_arg_count(name, args, 2, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                let value_type = self.check_expr(&args[1], scope, function)?;
                let inner =
                    self.expect_raw_pointer("ptr_write", &ptr_type, args[0].span, function)?;
                self.require_unsafe("ptr_write", call_span, function)?;
                if value_type != inner {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0331",
                        format!(
                            "ptr_write expects value `{}` for pointer `{}` but got `{}`",
                            inner.name, ptr_type.name, value_type.name
                        ),
                        Some(function.name.clone()),
                        args[1].span,
                    ));
                    return None;
                }
                Some(TypeRef::new("void"))
            }
            _ => {
                let Some(signature) = self.signatures.get(name).cloned() else {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0309",
                        format!("unknown function `{name}`"),
                        Some(function.name.clone()),
                        call_span,
                    ));
                    return None;
                };

                if signature.params.len() != args.len() {
                    self.diagnostics.push(SemanticDiagnostic::at(
                        "N0312",
                        format!(
                            "function `{name}` expects {} arguments but got {}",
                            signature.params.len(),
                            args.len()
                        ),
                        Some(function.name.clone()),
                        call_span,
                    ));
                    return None;
                }

                for (index, (arg, expected)) in args.iter().zip(signature.params.iter()).enumerate()
                {
                    let actual = self.check_expr(arg, scope, function);
                    if actual.as_ref() != Some(expected) {
                        self.diagnostics.push(SemanticDiagnostic::at(
                            "N0313",
                            format!(
                                "argument {} for `{name}` must be `{}` but got `{}`",
                                index + 1,
                                expected.name,
                                actual
                                    .as_ref()
                                    .map(|ty| ty.name.as_str())
                                    .unwrap_or("<unknown>")
                            ),
                            Some(function.name.clone()),
                            arg.span,
                        ));
                    }
                }

                Some(signature.return_type)
            }
        }
    }

    /// Verify `ty` is a `<ctor><T>` reference (`rc` or `ref`) and return its
    /// inner type `T`.
    fn expect_reference(
        &mut self,
        name: &str,
        ctor: &str,
        ty: &TypeRef,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        match ty.generic_arg(ctor) {
            Some(inner) => Some(inner),
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "N0331",
                    format!("{name} expects a `{ctor}<T>` value but got `{}`", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    /// Verify `ty` is a raw pointer and return its pointee type.
    fn expect_raw_pointer(
        &mut self,
        name: &str,
        ty: &TypeRef,
        span: Span,
        function: &Function,
    ) -> Option<TypeRef> {
        match ty.pointer_target() {
            Some(inner) => Some(inner),
            None => {
                self.diagnostics.push(SemanticDiagnostic::at(
                    "N0331",
                    format!("{name} expects a raw pointer value but got `{}`", ty.name),
                    Some(function.name.clone()),
                    span,
                ));
                None
            }
        }
    }

    /// Require the current context to be inside an `unsafe` block.
    fn require_unsafe(&mut self, name: &str, span: Span, function: &Function) -> Option<()> {
        if self.unsafe_depth > 0 {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0330",
                format!("raw pointer operation `{name}` requires an `unsafe` block"),
                Some(function.name.clone()),
                span,
            ));
            None
        }
    }

    fn expect_arg_count(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: usize,
        function: &Function,
    ) -> Option<()> {
        if args.len() == expected {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0312",
                format!(
                    "function `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
                args.first().map(|arg| arg.span).unwrap_or(function.span),
            ));
            None
        }
    }

    fn expect_arg_type(
        &mut self,
        name: &str,
        index: usize,
        arg: &Expr,
        expected: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let expected = TypeRef::new(expected);
        let actual = self.check_expr(arg, scope, function);
        if actual.as_ref() == Some(&expected) {
            Some(())
        } else {
            self.diagnostics.push(SemanticDiagnostic::at(
                "N0313",
                format!(
                    "argument {index} for `{name}` must be `{}` but got `{}`",
                    expected.name,
                    actual
                        .as_ref()
                        .map(|ty| ty.name.as_str())
                        .unwrap_or("<unknown>")
                ),
                Some(function.name.clone()),
                arg.span,
            ));
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub params: Vec<TypeRef>,
    pub return_type: TypeRef,
}

#[derive(Debug, Clone, Default)]
struct Scope {
    locals: HashMap<String, TypeRef>,
}

/// If both operand types are the same numeric type (`i64` or `f64`), return it.
fn same_numeric_type(left: &Option<TypeRef>, right: &Option<TypeRef>) -> Option<TypeRef> {
    match (left, right) {
        (Some(l), Some(r)) if l == r && matches!(l.name.as_str(), "i64" | "f64") => Some(l.clone()),
        _ => None,
    }
}

/// If `expr` is a resource-freeing call (`dealloc(x)` or `rc_release(x)`) whose
/// argument is a plain variable, return that variable name.
fn free_call_target(expr: &Expr) -> Option<&str> {
    let ExprKind::Call { name, args } = &expr.kind else {
        return None;
    };
    if !matches!(name.as_str(), "dealloc" | "rc_release") {
        return None;
    }
    match args.as_slice() {
        [arg] => match &arg.kind {
            ExprKind::Variable(name) => Some(name),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use lullaby_lexer::lex;
    use lullaby_parser::parse;

    use super::*;

    fn validate_source(source: &str) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        validate(&program)
    }

    #[test]
    fn non_void_function_may_return_last_expression() {
        assert!(validate_source("fn add x i64 y i64 -> i64\n    x + y\n").is_ok());
    }

    #[test]
    fn validates_calls_and_bindings() {
        let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(1, 2)\n    value\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_inferred_bindings() {
        let source = "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value = add(1, 2)\n    let values = [value, 4]\n    values[0]\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_memory_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_store_builtin() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_assignment_and_loops() {
        let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_for_loop() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_logical_expressions() {
        let source = "fn main -> bool\n    not false and true or false\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_array_literal_and_index() {
        let source = "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn checked_program_exposes_function_signatures() {
        let checked = validate_source("fn add x i64 y i64 -> i64\n    x + y\n").expect("semantic");
        let signature = checked.info.signatures.get("add").expect("signature");
        assert_eq!(
            signature.params,
            vec![TypeRef::new("i64"), TypeRef::new("i64")]
        );
        assert_eq!(signature.return_type, TypeRef::new("i64"));
    }

    #[test]
    fn checked_program_exposes_expression_types() {
        let checked = validate_source(
            "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n",
        )
        .expect("semantic");
        assert!(checked.info.expression_types.iter().any(|expr_type| {
            expr_type.function == "main" && expr_type.ty == TypeRef::new("array<i64>")
        }));
        assert!(checked.info.expression_types.iter().any(|expr_type| {
            expr_type.function == "main" && expr_type.ty == TypeRef::new("i64")
        }));
    }

    #[test]
    fn non_void_function_rejects_empty_return() {
        let diagnostics = validate_source("fn bad -> i64\n    return\n").expect_err("semantic");
        assert_eq!(diagnostics[0].code, "N0304");
    }

    #[test]
    fn catches_type_mismatch() {
        let diagnostics = validate_source("fn bad -> i64\n    let value bool = 1\n    value\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0303")
        );
    }

    #[test]
    fn catches_assignment_type_mismatch() {
        let diagnostics = validate_source(
            "fn bad -> bool\n    let value bool = false\n    value = 1\n    value\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0314")
        );
    }

    #[test]
    fn catches_assignment_type_mismatch_after_inference() {
        let diagnostics =
            validate_source("fn bad -> i64\n    let value = 1\n    value = false\n    value\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0314")
        );
    }

    #[test]
    fn catches_undeclared_assignment() {
        let diagnostics =
            validate_source("fn bad -> i64\n    value = 1\n    value\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0316")
        );
    }

    #[test]
    fn catches_break_outside_loop() {
        let diagnostics = validate_source("fn bad -> void\n    break\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0317")
        );
    }

    #[test]
    fn catches_invalid_logical_operand() {
        let diagnostics =
            validate_source("fn bad -> bool\n    1 and true\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0320")
        );
    }

    #[test]
    fn catches_invalid_for_range_type() {
        let diagnostics =
            validate_source("fn bad -> i64\n    for i from false to 3\n        i\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0321")
        );
    }

    #[test]
    fn catches_invalid_for_step_type() {
        let diagnostics =
            validate_source("fn bad -> i64\n    for i from 1 to 3 by false\n        i\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0322")
        );
    }

    #[test]
    fn catches_array_literal_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> array<i64>\n    [1, false]\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0324")
        );
    }

    #[test]
    fn catches_array_index_type_mismatch() {
        let diagnostics = validate_source(
            "fn bad -> i64\n    let values array<i64> = [1, 2]\n    values[true]\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0326")
        );
    }

    #[test]
    fn catches_ordering_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> bool\n    false < true\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0327")
        );
    }

    #[test]
    fn catches_store_value_type_mismatch() {
        let diagnostics = validate_source(
            "fn bad -> void\n    let ptr ptr_i64 = alloc(1)\n    store(ptr, false)\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0328")
        );
    }

    #[test]
    fn validates_io_and_system_builtins() {
        let source = "fn main -> bool\n    write_file(\"target/lullaby_semantics_io.txt\", \"alpha\")\n    append_file(\"target/lullaby_semantics_io.txt\", \" beta\")\n    let content string = read_file(\"target/lullaby_semantics_io.txt\")\n    let exists bool = file_exists(\"target/lullaby_semantics_io.txt\")\n    let status i64 = sys_status(\"rustc\", [\"--version\"])\n    content == \"alpha beta\" and exists and status == 0\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn resolves_type_aliases_structurally() {
        // `Count` is an alias for `i64`, so alias and target are interchangeable.
        let source = "alias Count = i64\n\nfn main -> Count\n    let a Count = 41\n    let b i64 = a\n    b + 1\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn resolves_alias_inside_generic_argument() {
        let source = "alias Count = i64\n\nfn main -> i64\n    let values array<Count> = [1, 2]\n    values[0]\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_duplicate_type_alias() {
        let diagnostics =
            validate_source("alias A = i64\nalias A = bool\n\nfn main -> i64\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0360")
        );
    }

    #[test]
    fn rejects_cyclic_type_alias() {
        let diagnostics = validate_source("alias A = B\nalias B = A\n\nfn main -> i64\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0361")
        );
    }

    #[test]
    fn detects_use_after_free_at_compile_time() {
        let diagnostics = validate_source(
            "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    dealloc(p)\n    unsafe\n        ptr_read(p)\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0350")
        );
    }

    #[test]
    fn detects_double_free_at_compile_time() {
        let diagnostics = validate_source(
            "fn main -> void\n    let p ptr_i64 = alloc(1)\n    dealloc(p)\n    dealloc(p)\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0350")
        );
    }

    #[test]
    fn allows_use_before_free() {
        let source = "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_returning_borrowed_reference() {
        let diagnostics = validate_source("fn leak h rc<i64> -> ref<i64>\n    rc_borrow(h)\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0351")
        );
    }

    #[test]
    fn validates_try_catch_and_throw() {
        let source = "fn main -> void\n    try\n        throw \"oops\"\n    catch message\n        warn(message)\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_throwing_non_string() {
        let diagnostics = validate_source("fn main -> void\n    throw 42\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0313")
        );
    }

    #[test]
    fn validates_region_declarations() {
        let source = "fn main -> i64\n    region pool: size=4096, align=16, kind=static\n    region scratch: size=1024, kind=dynamic, mutable=true\n    0\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_region_with_bad_size() {
        let diagnostics = validate_source("fn main -> i64\n    region pool: size=0\n    0\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0340")
        );
    }

    #[test]
    fn rejects_region_with_non_power_of_two_alignment() {
        let diagnostics =
            validate_source("fn main -> i64\n    region pool: size=1024, align=15\n    0\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0340")
        );
    }

    #[test]
    fn rejects_duplicate_region() {
        let diagnostics = validate_source(
            "fn main -> i64\n    region pool: size=16\n    region pool: size=32\n    0\n",
        )
        .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0341")
        );
    }

    #[test]
    fn validates_reference_builtins() {
        let source = "fn main -> i64\n    let h rc<i64> = rc_new(1)\n    let s rc<i64> = rc_clone(h)\n    let v ref<i64> = rc_borrow(h)\n    let a i64 = ref_get(v)\n    rc_release(s)\n    rc_release(h)\n    a\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn requires_unsafe_for_raw_pointer_read() {
        let diagnostics =
            validate_source("fn main -> i64\n    let p ptr_i64 = alloc(1)\n    ptr_read(p)\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0330")
        );
    }

    #[test]
    fn allows_raw_pointer_read_inside_unsafe() {
        let source = "fn main -> i64\n    let p ptr_i64 = alloc(1)\n    let v i64 = 0\n    unsafe\n        v = ptr_read(p)\n    dealloc(p)\n    v\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_reference_builtin_type_mismatch() {
        let diagnostics =
            validate_source("fn main -> i64\n    let x i64 = 1\n    rc_get(x)\n    x\n")
                .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0331")
        );
    }

    #[test]
    fn validates_f64_arithmetic() {
        let source = "fn main -> f64\n    let x f64 = 1.5\n    x * 2.0 - 0.5\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_mixing_i64_and_f64() {
        let diagnostics = validate_source("fn main -> f64\n    let x f64 = 1.5\n    x + 2\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0307")
        );
    }

    #[test]
    fn validates_string_concatenation_and_to_string() {
        let source =
            "fn main -> string\n    \"n=\" + to_string(1 + 2) + \" b=\" + to_string(true)\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn rejects_mixed_string_and_int_addition() {
        let diagnostics =
            validate_source("fn main -> string\n    \"n=\" + 5\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0307")
        );
    }

    #[test]
    fn validates_standard_stream_builtins() {
        let source = "fn main -> void\n    println(\"hello\")\n    print(\"partial\")\n    warn(\"careful\")\n    flush()\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn catches_stream_builtin_argument_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> void\n    println(1)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0313")
        );
    }

    #[test]
    fn catches_stream_builtin_arity_mismatch() {
        let diagnostics =
            validate_source("fn bad -> void\n    flush(\"x\")\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0312")
        );
    }

    #[test]
    fn catches_file_builtin_argument_type_mismatch() {
        let diagnostics =
            validate_source("fn bad -> string\n    read_file(1)\n").expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0313")
        );
    }

    #[test]
    fn catches_system_builtin_argument_type_mismatch() {
        let diagnostics = validate_source("fn bad -> i64\n    sys_status(\"rustc\", [1])\n")
            .expect_err("semantic");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "N0313")
        );
    }

    #[test]
    fn executable_validation_requires_main_entrypoint() {
        let tokens = lex("fn add x i64 y i64 -> i64\n    x + y\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let diagnostics = validate_executable(&program).expect_err("entrypoint");

        assert_eq!(diagnostics[0].code, "N0329");
    }

    #[test]
    fn executable_validation_rejects_main_parameters() {
        let tokens = lex("fn main arg i64 -> i64\n    arg\n").expect("lex");
        let program = parse(&tokens).expect("parse");
        let diagnostics = validate_executable(&program).expect_err("entrypoint");

        assert_eq!(diagnostics[0].code, "N0329");
        assert_eq!(diagnostics[0].function.as_deref(), Some("main"));
    }
}
