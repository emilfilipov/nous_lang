use std::collections::HashMap;

use nous_diagnostics::Span;
use nous_parser::{AssignOp, BinaryOp, Expr, ExprKind, Function, Program, Stmt, TypeRef, UnaryOp};
use nous_semantics::{CheckedProgram, Signature};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrModule {
    pub functions: Vec<IrFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrFunction {
    pub name: String,
    pub params: Vec<IrParam>,
    pub return_type: TypeRef,
    pub body: Vec<IrStmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrParam {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrStmt {
    Let {
        name: String,
        ty: TypeRef,
        value: IrExpr,
        span: Span,
    },
    Assign {
        name: String,
        op: AssignOp,
        value: IrExpr,
        span: Span,
    },
    Return(Option<IrExpr>),
    Break(Span),
    Continue(Span),
    Expr(IrExpr),
    If {
        branches: Vec<IrIfBranch>,
        else_body: Vec<IrStmt>,
        span: Span,
    },
    While {
        condition: IrExpr,
        body: Vec<IrStmt>,
        span: Span,
    },
    For {
        name: String,
        start: IrExpr,
        end: IrExpr,
        step: Option<IrExpr>,
        body: Vec<IrStmt>,
        span: Span,
    },
    Loop {
        body: Vec<IrStmt>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrIfBranch {
    pub condition: IrExpr,
    pub body: Vec<IrStmt>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrExpr {
    pub kind: IrExprKind,
    pub ty: TypeRef,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrExprKind {
    Integer(i64),
    Bool(bool),
    String(String),
    Array(Vec<IrExpr>),
    Variable(String),
    Index {
        target: Box<IrExpr>,
        index: Box<IrExpr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<IrExpr>,
    },
    Binary {
        left: Box<IrExpr>,
        op: BinaryOp,
        right: Box<IrExpr>,
    },
    Call {
        name: String,
        args: Vec<IrExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrLoweringError {
    pub message: String,
    pub span: Option<Span>,
}

impl IrLoweringError {
    fn new(message: impl Into<String>, span: Option<Span>) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }
}

pub fn lower(checked: &CheckedProgram) -> Result<IrModule, IrLoweringError> {
    Lowerer::new(&checked.program, &checked.info.signatures).lower_program()
}

struct Lowerer<'a> {
    program: &'a Program,
    signatures: &'a HashMap<String, Signature>,
}

impl<'a> Lowerer<'a> {
    fn new(program: &'a Program, signatures: &'a HashMap<String, Signature>) -> Self {
        Self {
            program,
            signatures,
        }
    }

    fn lower_program(&self) -> Result<IrModule, IrLoweringError> {
        let functions = self
            .program
            .functions
            .iter()
            .map(|function| self.lower_function(function))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(IrModule { functions })
    }

    fn lower_function(&self, function: &Function) -> Result<IrFunction, IrLoweringError> {
        let mut scope = function
            .params
            .iter()
            .map(|param| (param.name.clone(), param.ty.clone()))
            .collect::<HashMap<_, _>>();
        Ok(IrFunction {
            name: function.name.clone(),
            params: function
                .params
                .iter()
                .map(|param| IrParam {
                    name: param.name.clone(),
                    ty: param.ty.clone(),
                })
                .collect(),
            return_type: function.return_type.clone(),
            body: self.lower_block(&function.body, &mut scope)?,
            span: function.span,
        })
    }

    fn lower_block(
        &self,
        statements: &[Stmt],
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<Vec<IrStmt>, IrLoweringError> {
        statements
            .iter()
            .map(|statement| self.lower_statement(statement, scope))
            .collect()
    }

    fn lower_statement(
        &self,
        statement: &Stmt,
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<IrStmt, IrLoweringError> {
        match statement {
            Stmt::Let {
                name,
                ty,
                value,
                span,
            } => {
                let value = self.lower_expr(value, scope)?;
                scope.insert(name.clone(), ty.clone());
                Ok(IrStmt::Let {
                    name: name.clone(),
                    ty: ty.clone(),
                    value,
                    span: *span,
                })
            }
            Stmt::Assign {
                name,
                op,
                value,
                span,
            } => Ok(IrStmt::Assign {
                name: name.clone(),
                op: *op,
                value: self.lower_expr(value, scope)?,
                span: *span,
            }),
            Stmt::Return(expr) => Ok(IrStmt::Return(
                expr.as_ref()
                    .map(|expr| self.lower_expr(expr, scope))
                    .transpose()?,
            )),
            Stmt::Break(span) => Ok(IrStmt::Break(*span)),
            Stmt::Continue(span) => Ok(IrStmt::Continue(*span)),
            Stmt::Expr(expr) => Ok(IrStmt::Expr(self.lower_expr(expr, scope)?)),
            Stmt::If {
                branches,
                else_body,
                span,
            } => {
                let branches = branches
                    .iter()
                    .map(|branch| {
                        let condition = self.lower_expr(&branch.condition, scope)?;
                        let mut branch_scope = scope.clone();
                        let body = self.lower_block(&branch.body, &mut branch_scope)?;
                        Ok(IrIfBranch { condition, body })
                    })
                    .collect::<Result<Vec<_>, IrLoweringError>>()?;
                let mut else_scope = scope.clone();
                let else_body = self.lower_block(else_body, &mut else_scope)?;
                Ok(IrStmt::If {
                    branches,
                    else_body,
                    span: *span,
                })
            }
            Stmt::While {
                condition,
                body,
                span,
            } => {
                let condition = self.lower_expr(condition, scope)?;
                let mut loop_scope = scope.clone();
                let body = self.lower_block(body, &mut loop_scope)?;
                Ok(IrStmt::While {
                    condition,
                    body,
                    span: *span,
                })
            }
            Stmt::For {
                name,
                start,
                end,
                step,
                body,
                span,
            } => {
                let start = self.lower_expr(start, scope)?;
                let end = self.lower_expr(end, scope)?;
                let step = step
                    .as_ref()
                    .map(|step| self.lower_expr(step, scope))
                    .transpose()?;
                let mut loop_scope = scope.clone();
                loop_scope.insert(name.clone(), TypeRef::new("i64"));
                let body = self.lower_block(body, &mut loop_scope)?;
                Ok(IrStmt::For {
                    name: name.clone(),
                    start,
                    end,
                    step,
                    body,
                    span: *span,
                })
            }
            Stmt::Loop { body, span } => {
                let mut loop_scope = scope.clone();
                let body = self.lower_block(body, &mut loop_scope)?;
                Ok(IrStmt::Loop { body, span: *span })
            }
        }
    }

    fn lower_expr(
        &self,
        expr: &Expr,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        let (kind, ty) = match &expr.kind {
            ExprKind::Integer(value) => (IrExprKind::Integer(*value), TypeRef::new("i64")),
            ExprKind::Bool(value) => (IrExprKind::Bool(*value), TypeRef::new("bool")),
            ExprKind::String(value) => (IrExprKind::String(value.clone()), TypeRef::new("string")),
            ExprKind::Array(values) => {
                let values = values
                    .iter()
                    .map(|value| self.lower_expr(value, scope))
                    .collect::<Result<Vec<_>, _>>()?;
                let element_type =
                    values
                        .first()
                        .map(|value| value.ty.clone())
                        .ok_or_else(|| {
                            IrLoweringError::new(
                                "cannot lower empty array literal",
                                Some(expr.span),
                            )
                        })?;
                (
                    IrExprKind::Array(values),
                    TypeRef::new(format!("array<{}>", element_type.name)),
                )
            }
            ExprKind::Variable(name) => {
                let ty = scope.get(name).cloned().ok_or_else(|| {
                    IrLoweringError::new(format!("unknown variable `{name}`"), Some(expr.span))
                })?;
                (IrExprKind::Variable(name.clone()), ty)
            }
            ExprKind::Index { target, index } => {
                let target = self.lower_expr(target, scope)?;
                let index = self.lower_expr(index, scope)?;
                let ty = target.ty.array_element().ok_or_else(|| {
                    IrLoweringError::new("index target is not an array", Some(target.span))
                })?;
                (
                    IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    ty,
                )
            }
            ExprKind::Unary { op, expr } => (
                IrExprKind::Unary {
                    op: *op,
                    expr: Box::new(self.lower_expr(expr, scope)?),
                },
                TypeRef::new("bool"),
            ),
            ExprKind::Binary { left, op, right } => {
                let left = self.lower_expr(left, scope)?;
                let right = self.lower_expr(right, scope)?;
                let ty = match op {
                    BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                        TypeRef::new("i64")
                    }
                    BinaryOp::Equal
                    | BinaryOp::NotEqual
                    | BinaryOp::Less
                    | BinaryOp::LessEqual
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEqual
                    | BinaryOp::And
                    | BinaryOp::Or => TypeRef::new("bool"),
                };
                (
                    IrExprKind::Binary {
                        left: Box::new(left),
                        op: *op,
                        right: Box::new(right),
                    },
                    ty,
                )
            }
            ExprKind::Call { name, args } => {
                let args = args
                    .iter()
                    .map(|arg| self.lower_expr(arg, scope))
                    .collect::<Result<Vec<_>, _>>()?;
                let ty = self.call_return_type(name, &args, expr.span)?;
                (
                    IrExprKind::Call {
                        name: name.clone(),
                        args,
                    },
                    ty,
                )
            }
        };

        Ok(IrExpr {
            kind,
            ty,
            span: expr.span,
        })
    }

    fn call_return_type(
        &self,
        name: &str,
        args: &[IrExpr],
        span: Span,
    ) -> Result<TypeRef, IrLoweringError> {
        Ok(match name {
            "alloc" => {
                let value = args.first().ok_or_else(|| {
                    IrLoweringError::new("alloc call missing value argument", Some(span))
                })?;
                TypeRef::new(format!("ptr_{}", value.ty.name))
            }
            "load" => {
                let ptr = args.first().ok_or_else(|| {
                    IrLoweringError::new("load call missing pointer argument", Some(span))
                })?;
                ptr.ty
                    .name
                    .strip_prefix("ptr_")
                    .map(TypeRef::new)
                    .ok_or_else(|| {
                        IrLoweringError::new("load call argument is not a pointer", Some(span))
                    })?
            }
            "store" | "dealloc" | "write_file" | "append_file" => TypeRef::new("void"),
            "read_file" | "sys_output" => TypeRef::new("string"),
            "file_exists" => TypeRef::new("bool"),
            "sys_status" => TypeRef::new("i64"),
            _ => self
                .signatures
                .get(name)
                .map(|signature| signature.return_type.clone())
                .ok_or_else(|| {
                    IrLoweringError::new(format!("unknown function `{name}`"), Some(span))
                })?,
        })
    }
}

#[cfg(test)]
mod tests {
    use nous_lexer::lex;
    use nous_parser::parse;
    use nous_semantics::validate;

    use super::*;

    fn lower_source(source: &str) -> IrModule {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate(&program).expect("semantic");
        lower(&checked).expect("lower")
    }

    #[test]
    fn lowers_functions_with_typed_params_and_return() {
        let module = lower_source("fn add x i64 y i64 -> i64\n    x + y\n");
        assert_eq!(module.functions.len(), 1);
        let function = &module.functions[0];
        assert_eq!(function.name, "add");
        assert_eq!(function.params[0].ty, TypeRef::new("i64"));
        assert_eq!(function.return_type, TypeRef::new("i64"));
        let IrStmt::Expr(expr) = &function.body[0] else {
            panic!("expected expression statement");
        };
        assert_eq!(expr.ty, TypeRef::new("i64"));
    }

    #[test]
    fn lowers_arrays_and_index_expression_types() {
        let module =
            lower_source("fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[1]\n");
        let function = &module.functions[0];
        let IrStmt::Let { value, .. } = &function.body[0] else {
            panic!("expected let statement");
        };
        assert_eq!(value.ty, TypeRef::new("array<i64>"));
        let IrStmt::Expr(expr) = &function.body[1] else {
            panic!("expected expression statement");
        };
        assert_eq!(expr.ty, TypeRef::new("i64"));
    }

    #[test]
    fn lowers_control_flow_and_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + total\n";
        let module = lower_source(source);
        let function = &module.functions[0];
        assert!(matches!(function.body[1], IrStmt::Expr(_)));
        assert!(matches!(function.body[3], IrStmt::For { .. }));
        let IrStmt::Let { value, .. } = &function.body[4] else {
            panic!("expected load binding");
        };
        assert_eq!(value.ty, TypeRef::new("i64"));
    }
}
