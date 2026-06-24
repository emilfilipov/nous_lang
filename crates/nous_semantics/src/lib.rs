use std::collections::HashMap;

use nous_parser::{AssignOp, BinaryOp, Expr, ExprKind, Function, Program, Stmt, TypeRef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub function: Option<String>,
}

impl SemanticDiagnostic {
    fn new(code: &'static str, message: impl Into<String>, function: Option<String>) -> Self {
        Self {
            code,
            message: message.into(),
            function,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedProgram {
    pub program: Program,
}

pub fn validate(program: &Program) -> Result<CheckedProgram, Vec<SemanticDiagnostic>> {
    let mut checker = Checker::new(program);
    checker.validate();
    if checker.diagnostics.is_empty() {
        Ok(CheckedProgram {
            program: program.clone(),
        })
    } else {
        Err(checker.diagnostics)
    }
}

struct Checker<'a> {
    program: &'a Program,
    signatures: HashMap<String, Signature>,
    diagnostics: Vec<SemanticDiagnostic>,
    loop_depth: usize,
}

impl<'a> Checker<'a> {
    fn new(program: &'a Program) -> Self {
        Self {
            program,
            signatures: HashMap::new(),
            diagnostics: Vec::new(),
            loop_depth: 0,
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
        if function.return_type.is_void() {
            return;
        }

        if block_type.as_ref() != Some(&function.return_type) {
            self.diagnostics.push(SemanticDiagnostic::new(
                "N0301",
                format!(
                    "function `{}` declares `{}` but has no final return value of that type",
                    function.name, function.return_type.name
                ),
                Some(function.name.clone()),
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
                Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_)
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
                if value_type.as_ref() != Some(ty) {
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0303",
                        format!(
                            "binding `{name}` declares `{}` but initializer has `{}`",
                            ty.name,
                            value_type
                                .as_ref()
                                .map(|ty| ty.name.as_str())
                                .unwrap_or("<unknown>")
                        ),
                        Some(function.name.clone()),
                    ));
                }
                scope.locals.insert(name.clone(), ty.clone());
                None
            }
            Stmt::Assign {
                name, op, value, ..
            } => {
                let expected = scope.locals.get(name).cloned();
                let value_type = self.check_expr(value, scope, function);
                match expected {
                    Some(expected) => {
                        if *op == AssignOp::Replace {
                            if value_type.as_ref() != Some(&expected) {
                                self.diagnostics.push(SemanticDiagnostic::new(
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
                                ));
                            }
                        } else if expected != TypeRef::new("i64")
                            || value_type.as_ref() != Some(&TypeRef::new("i64"))
                        {
                            self.diagnostics.push(SemanticDiagnostic::new(
                                "N0315",
                                format!("compound assignment to `{name}` requires i64 operands"),
                                Some(function.name.clone()),
                            ));
                        }
                    }
                    None => {
                        self.diagnostics.push(SemanticDiagnostic::new(
                            "N0316",
                            format!("assignment target `{name}` is not declared"),
                            Some(function.name.clone()),
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
                    self.diagnostics.push(SemanticDiagnostic::new(
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
                    ));
                }
                actual
            }
            Stmt::Break(_) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0317",
                        "`break` can only appear inside a loop",
                        Some(function.name.clone()),
                    ));
                }
                None
            }
            Stmt::Continue(_) => {
                if self.loop_depth == 0 {
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0318",
                        "`continue` can only appear inside a loop",
                        Some(function.name.clone()),
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
                        self.diagnostics.push(SemanticDiagnostic::new(
                            "N0305",
                            "if condition must be bool",
                            Some(function.name.clone()),
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
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0305",
                        "while condition must be bool",
                        Some(function.name.clone()),
                    ));
                }
                let mut loop_scope = scope.clone();
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
        }
    }

    fn check_expr(&mut self, expr: &Expr, scope: &Scope, function: &Function) -> Option<TypeRef> {
        match &expr.kind {
            ExprKind::Integer(_) => Some(TypeRef::new("i64")),
            ExprKind::Bool(_) => Some(TypeRef::new("bool")),
            ExprKind::String(_) => Some(TypeRef::new("string")),
            ExprKind::Variable(name) => match scope.locals.get(name) {
                Some(ty) => Some(ty.clone()),
                None => {
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0306",
                        format!("unknown variable `{name}`"),
                        Some(function.name.clone()),
                    ));
                    None
                }
            },
            ExprKind::Binary { left, op, right } => {
                let left_type = self.check_expr(left, scope, function);
                let right_type = self.check_expr(right, scope, function);
                match op {
                    BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                        if left_type.as_ref() == Some(&TypeRef::new("i64"))
                            && right_type.as_ref() == Some(&TypeRef::new("i64"))
                        {
                            Some(TypeRef::new("i64"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::new(
                                "N0307",
                                "arithmetic operands must both be i64",
                                Some(function.name.clone()),
                            ));
                            None
                        }
                    }
                    BinaryOp::Equal
                    | BinaryOp::NotEqual
                    | BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual => {
                        if left_type.is_some() && left_type == right_type {
                            Some(TypeRef::new("bool"))
                        } else {
                            self.diagnostics.push(SemanticDiagnostic::new(
                                "N0308",
                                "comparison operands must have the same type",
                                Some(function.name.clone()),
                            ));
                            None
                        }
                    }
                }
            }
            ExprKind::Call { name, args } => self.check_call(name, args, scope, function),
        }
    }

    fn check_call(
        &mut self,
        name: &str,
        args: &[Expr],
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
                        self.diagnostics.push(SemanticDiagnostic::new(
                            "N0310",
                            "load expects a pointer argument",
                            Some(function.name.clone()),
                        ));
                        None
                    })
            }
            "dealloc" => {
                self.expect_arg_count(name, args, 1, function)?;
                let ptr_type = self.check_expr(&args[0], scope, function)?;
                if ptr_type.name.starts_with("ptr_") {
                    Some(TypeRef::new("void"))
                } else {
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0311",
                        "dealloc expects a pointer argument",
                        Some(function.name.clone()),
                    ));
                    None
                }
            }
            _ => {
                let Some(signature) = self.signatures.get(name).cloned() else {
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0309",
                        format!("unknown function `{name}`"),
                        Some(function.name.clone()),
                    ));
                    return None;
                };

                if signature.params.len() != args.len() {
                    self.diagnostics.push(SemanticDiagnostic::new(
                        "N0312",
                        format!(
                            "function `{name}` expects {} arguments but got {}",
                            signature.params.len(),
                            args.len()
                        ),
                        Some(function.name.clone()),
                    ));
                    return None;
                }

                for (index, (arg, expected)) in args.iter().zip(signature.params.iter()).enumerate()
                {
                    let actual = self.check_expr(arg, scope, function);
                    if actual.as_ref() != Some(expected) {
                        self.diagnostics.push(SemanticDiagnostic::new(
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
                        ));
                    }
                }

                Some(signature.return_type)
            }
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
            self.diagnostics.push(SemanticDiagnostic::new(
                "N0312",
                format!(
                    "function `{name}` expects {expected} arguments but got {}",
                    args.len()
                ),
                Some(function.name.clone()),
            ));
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Signature {
    params: Vec<TypeRef>,
    return_type: TypeRef,
}

#[derive(Debug, Clone, Default)]
struct Scope {
    locals: HashMap<String, TypeRef>,
}

#[cfg(test)]
mod tests {
    use nous_lexer::lex;
    use nous_parser::parse;

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
    fn validates_memory_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        assert!(validate_source(source).is_ok());
    }

    #[test]
    fn validates_assignment_and_loops() {
        let source = "fn main -> i64\n    let x i64 = 0\n    while x < 3\n        x += 1\n    x\n";
        assert!(validate_source(source).is_ok());
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
}
