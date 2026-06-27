use std::collections::{HashMap, HashSet};
use std::fs;
use std::process::Command;

use nous_diagnostics::{Span, TraceFrame};
use nous_parser::{AssignOp, BinaryOp, Expr, ExprKind, Function, Program, Stmt, TypeRef, UnaryOp};
use nous_runtime::{RuntimeError, Value};
use nous_semantics::{CheckedProgram, Signature};
use serde::{Deserialize, Serialize};

pub const BYTECODE_ARTIFACT_FORMAT: &str = "nous-bytecode";
pub const BYTECODE_ARTIFACT_EXTENSION: &str = "nbc";
pub const BYTECODE_ARTIFACT_VERSION: u32 = 2;
const BYTECODE_ARTIFACT_PAYLOAD: &str = "structured-bytecode";
const BYTECODE_ARTIFACT_TARGET: &str = "alpha1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrModule {
    pub functions: Vec<IrFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrFunction {
    pub name: String,
    pub params: Vec<IrParam>,
    pub return_type: TypeRef,
    pub body: Vec<IrStmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrParam {
    pub name: String,
    pub ty: TypeRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrIfBranch {
    pub condition: IrExpr,
    pub body: Vec<IrStmt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrExpr {
    pub kind: IrExprKind,
    pub ty: TypeRef,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizationConfig {
    passes: Vec<OptimizationPass>,
}

impl OptimizationConfig {
    pub fn none() -> Self {
        Self { passes: Vec::new() }
    }

    pub fn constant_folding() -> Self {
        Self {
            passes: vec![OptimizationPass::ConstantFolding],
        }
    }

    pub fn dead_code_elimination() -> Self {
        Self {
            passes: vec![OptimizationPass::DeadCodeElimination],
        }
    }

    pub fn copy_propagation() -> Self {
        Self {
            passes: vec![OptimizationPass::CopyPropagation],
        }
    }

    pub fn alpha_default() -> Self {
        Self {
            passes: vec![
                OptimizationPass::ConstantFolding,
                OptimizationPass::CopyPropagation,
                OptimizationPass::DeadCodeElimination,
            ],
        }
    }

    pub fn with_passes(passes: Vec<OptimizationPass>) -> Self {
        Self { passes }
    }

    pub fn passes(&self) -> &[OptimizationPass] {
        &self.passes
    }
}

impl Default for OptimizationConfig {
    fn default() -> Self {
        Self::alpha_default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationPass {
    ConstantFolding,
    CopyPropagation,
    DeadCodeElimination,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OptimizationReport {
    pub applied_passes: Vec<OptimizationPass>,
    pub folded_expressions: usize,
    pub propagated_copies: usize,
    pub removed_dead_statements: usize,
}

pub fn optimize(module: &IrModule, config: &OptimizationConfig) -> (IrModule, OptimizationReport) {
    let mut optimized = module.clone();
    let mut report = OptimizationReport::default();

    for pass in config.passes() {
        match pass {
            OptimizationPass::ConstantFolding => {
                let mut folder = ConstantFolder::default();
                optimized = folder.fold_module(&optimized);
                report.folded_expressions += folder.folded_expressions;
                report.applied_passes.push(*pass);
            }
            OptimizationPass::CopyPropagation => {
                let mut propagator = CopyPropagator::default();
                optimized = propagator.propagate_module(&optimized);
                report.propagated_copies += propagator.propagated_copies;
                report.applied_passes.push(*pass);
            }
            OptimizationPass::DeadCodeElimination => {
                let mut eliminator = DeadCodeEliminator::default();
                optimized = eliminator.eliminate_module(&optimized);
                report.removed_dead_statements += eliminator.removed_statements;
                report.applied_passes.push(*pass);
            }
        }
    }

    (optimized, report)
}

pub fn run_main(module: &IrModule) -> Result<Value, RuntimeError> {
    let mut runtime = IrRuntime::new(module)?;
    runtime.call_function("main", Vec::new())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytecodeModule {
    pub functions: Vec<BytecodeFunction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytecodeFunction {
    pub name: String,
    pub params: Vec<IrParam>,
    pub return_type: TypeRef,
    pub body: Vec<IrStmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytecodeArtifact {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub metadata: BytecodeArtifactMetadata,
    pub entry: String,
    #[serde(default)]
    pub function_table: Vec<BytecodeFunctionSignature>,
    pub module: BytecodeModule,
}

impl BytecodeArtifact {
    pub fn new(module: BytecodeModule) -> Self {
        let function_table = module
            .functions
            .iter()
            .map(BytecodeFunctionSignature::from_function)
            .collect();
        Self {
            format: BYTECODE_ARTIFACT_FORMAT.to_string(),
            version: BYTECODE_ARTIFACT_VERSION,
            metadata: BytecodeArtifactMetadata::default(),
            entry: "main".to_string(),
            function_table,
            module,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytecodeArtifactMetadata {
    pub producer: String,
    pub target: String,
    pub payload: String,
}

impl Default for BytecodeArtifactMetadata {
    fn default() -> Self {
        Self {
            producer: format!("nous_ir {}", env!("CARGO_PKG_VERSION")),
            target: BYTECODE_ARTIFACT_TARGET.to_string(),
            payload: BYTECODE_ARTIFACT_PAYLOAD.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BytecodeFunctionSignature {
    pub name: String,
    pub params: Vec<IrParam>,
    pub return_type: TypeRef,
}

impl BytecodeFunctionSignature {
    fn from_function(function: &BytecodeFunction) -> Self {
        Self {
            name: function.name.clone(),
            params: function.params.clone(),
            return_type: function.return_type.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BytecodeArtifactError {
    pub message: String,
}

impl BytecodeArtifactError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

pub fn encode_bytecode_artifact(module: &BytecodeModule) -> Result<String, BytecodeArtifactError> {
    let artifact = BytecodeArtifact::new(module.clone());
    serde_json::to_string_pretty(&artifact).map_err(|error| {
        BytecodeArtifactError::new(format!("failed to encode bytecode artifact: {error}"))
    })
}

pub fn decode_bytecode_artifact(contents: &str) -> Result<BytecodeArtifact, BytecodeArtifactError> {
    let artifact: BytecodeArtifact = serde_json::from_str(contents).map_err(|error| {
        BytecodeArtifactError::new(format!("failed to decode bytecode artifact: {error}"))
    })?;

    if artifact.format != BYTECODE_ARTIFACT_FORMAT {
        return Err(BytecodeArtifactError::new(format!(
            "unsupported bytecode artifact format `{}`",
            artifact.format
        )));
    }
    if artifact.version != BYTECODE_ARTIFACT_VERSION {
        return Err(BytecodeArtifactError::new(format!(
            "unsupported bytecode artifact version `{}`",
            artifact.version
        )));
    }
    if artifact.entry != "main" {
        return Err(BytecodeArtifactError::new(format!(
            "unsupported bytecode artifact entry `{}`",
            artifact.entry
        )));
    }
    if artifact.metadata.target != BYTECODE_ARTIFACT_TARGET {
        return Err(BytecodeArtifactError::new(format!(
            "unsupported bytecode artifact target `{}`",
            artifact.metadata.target
        )));
    }
    if artifact.metadata.payload != BYTECODE_ARTIFACT_PAYLOAD {
        return Err(BytecodeArtifactError::new(format!(
            "unsupported bytecode artifact payload `{}`",
            artifact.metadata.payload
        )));
    }

    validate_bytecode_artifact_contract(&artifact)?;

    Ok(artifact)
}

fn validate_bytecode_artifact_contract(
    artifact: &BytecodeArtifact,
) -> Result<(), BytecodeArtifactError> {
    let mut names = HashSet::new();
    for function in &artifact.module.functions {
        if !names.insert(function.name.as_str()) {
            return Err(BytecodeArtifactError::new(format!(
                "duplicate bytecode function `{}`",
                function.name
            )));
        }
    }

    if !names.contains(artifact.entry.as_str()) {
        return Err(BytecodeArtifactError::new(format!(
            "bytecode artifact entry `{}` is not present in the module",
            artifact.entry
        )));
    }

    let expected: Vec<_> = artifact
        .module
        .functions
        .iter()
        .map(BytecodeFunctionSignature::from_function)
        .collect();
    if artifact.function_table != expected {
        return Err(BytecodeArtifactError::new(
            "bytecode artifact function_table does not match module functions",
        ));
    }

    Ok(())
}

pub fn lower_to_bytecode(module: &IrModule) -> BytecodeModule {
    BytecodeModule {
        functions: module
            .functions
            .iter()
            .map(|function| BytecodeFunction {
                name: function.name.clone(),
                params: function.params.clone(),
                return_type: function.return_type.clone(),
                body: function.body.clone(),
                span: function.span,
            })
            .collect(),
    }
}

pub fn run_bytecode_main(module: &BytecodeModule) -> Result<Value, RuntimeError> {
    let ir = IrModule {
        functions: module
            .functions
            .iter()
            .map(|function| IrFunction {
                name: function.name.clone(),
                params: function.params.clone(),
                return_type: function.return_type.clone(),
                body: function.body.clone(),
                span: function.span,
            })
            .collect(),
    };
    run_main(&ir)
}

#[derive(Default)]
struct ConstantFolder {
    folded_expressions: usize,
}

impl ConstantFolder {
    fn fold_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
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
                op,
                value,
                span,
            } => IrStmt::Assign {
                name: name.clone(),
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
            IrExprKind::Unary { op, expr: inner } => {
                let inner = self.fold_expr(inner);
                match (op, &inner.kind) {
                    (UnaryOp::Not, IrExprKind::Bool(value)) => {
                        self.literal(expr, IrExprKind::Bool(!value))
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
            IrExprKind::Integer(_)
            | IrExprKind::Bool(_)
            | IrExprKind::String(_)
            | IrExprKind::Variable(_) => expr.clone(),
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
        (IrExprKind::Integer(left), BinaryOp::Subtract, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left - right))
        }
        (IrExprKind::Integer(left), BinaryOp::Multiply, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left * right))
        }
        (IrExprKind::Integer(_), BinaryOp::Divide, IrExprKind::Integer(0)) => None,
        (IrExprKind::Integer(left), BinaryOp::Divide, IrExprKind::Integer(right)) => {
            Some(IrExprKind::Integer(left / right))
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
        _ => None,
    }
}

#[derive(Default)]
struct CopyPropagator {
    propagated_copies: usize,
}

impl CopyPropagator {
    fn propagate_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
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
                let has_call = expr_contains_call(&value);
                if has_call {
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
                op,
                value,
                span,
            } => {
                let value = self.propagate_expr(value, aliases);
                if expr_contains_call(&value) {
                    aliases.clear();
                }
                invalidate_alias(name, aliases);
                IrStmt::Assign {
                    name: name.clone(),
                    op: *op,
                    value,
                    span: *span,
                }
            }
            IrStmt::Return(expr) => {
                let expr = expr.as_ref().map(|expr| self.propagate_expr(expr, aliases));
                if expr.as_ref().is_some_and(expr_contains_call) {
                    aliases.clear();
                }
                IrStmt::Return(expr)
            }
            IrStmt::Break(span) => IrStmt::Break(*span),
            IrStmt::Continue(span) => IrStmt::Continue(*span),
            IrStmt::Expr(expr) => {
                let expr = self.propagate_expr(expr, aliases);
                if expr_contains_call(&expr) {
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
            IrExprKind::Integer(_) | IrExprKind::Bool(_) | IrExprKind::String(_) => expr.clone(),
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

fn expr_contains_call(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::Call { .. } => true,
        IrExprKind::Array(values) => values.iter().any(expr_contains_call),
        IrExprKind::Index { target, index } => {
            expr_contains_call(target) || expr_contains_call(index)
        }
        IrExprKind::Unary { expr, .. } => expr_contains_call(expr),
        IrExprKind::Binary { left, right, .. } => {
            expr_contains_call(left) || expr_contains_call(right)
        }
        IrExprKind::Integer(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Variable(_) => false,
    }
}

#[derive(Default)]
struct DeadCodeEliminator {
    removed_statements: usize,
}

impl DeadCodeEliminator {
    fn eliminate_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
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
            IrStmt::Let { .. }
            | IrStmt::Assign { .. }
            | IrStmt::Return(_)
            | IrStmt::Break(_)
            | IrStmt::Continue(_)
            | IrStmt::Expr(_) => statement.clone(),
        }
    }
}

fn is_unconditional_terminator(statement: &IrStmt) -> bool {
    matches!(
        statement,
        IrStmt::Return(_) | IrStmt::Break(_) | IrStmt::Continue(_)
    )
}

struct IrRuntime<'a> {
    functions: HashMap<&'a str, &'a IrFunction>,
    heap: Vec<Option<Value>>,
    call_stack: Vec<TraceFrame>,
}

impl<'a> IrRuntime<'a> {
    fn new(module: &'a IrModule) -> Result<Self, RuntimeError> {
        let functions = module
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect::<HashMap<_, _>>();

        if !functions.contains_key("main") {
            return Err(RuntimeError::new("N0400", "missing `main` function"));
        }

        Ok(Self {
            functions,
            heap: Vec::new(),
            call_stack: Vec::new(),
        })
    }

    fn call_function(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match name {
            "alloc" => self.builtin_alloc(args),
            "load" => self.builtin_load(args),
            "store" => self.builtin_store(args),
            "dealloc" => self.builtin_dealloc(args),
            "read_file" => self.builtin_read_file(args),
            "write_file" => self.builtin_write_file(args),
            "append_file" => self.builtin_append_file(args),
            "file_exists" => self.builtin_file_exists(args),
            "sys_status" => self.builtin_sys_status(args),
            "sys_output" => self.builtin_sys_output(args),
            _ => {
                let function = *self.functions.get(name).ok_or_else(|| {
                    RuntimeError::new("N0401", format!("unknown function `{name}`"))
                })?;

                if function.params.len() != args.len() {
                    return Err(RuntimeError::new(
                        "N0402",
                        format!(
                            "function `{name}` expects {} arguments but got {}",
                            function.params.len(),
                            args.len()
                        ),
                    ));
                }

                let mut env = Env::default();
                for (param, value) in function.params.iter().zip(args) {
                    env.define(param.name.clone(), value);
                }

                self.call_stack.push(TraceFrame {
                    function: function.name.clone(),
                    span: Some(function.span),
                });
                let result = self.eval_block(&function.body, &mut env);
                let traceback = self.call_stack.clone();
                self.call_stack.pop();

                match result.map_err(|error| error.with_traceback(traceback))? {
                    Control::Return(value) | Control::Value(value) => Ok(value),
                    Control::Break | Control::Continue => Err(RuntimeError::new(
                        "N0410",
                        "loop control escaped function body",
                    )),
                }
            }
        }
    }

    fn eval_block(
        &mut self,
        statements: &[IrStmt],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let mut last = Value::Void;

        for statement in statements {
            match self.eval_statement(statement, env)? {
                Control::Return(value) => return Ok(Control::Return(value)),
                Control::Break => return Ok(Control::Break),
                Control::Continue => return Ok(Control::Continue),
                Control::Value(value) => last = value,
            }
        }

        Ok(Control::Value(last))
    }

    fn eval_statement(
        &mut self,
        statement: &IrStmt,
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let span = statement_span(statement);
        let result = match statement {
            IrStmt::Let { name, value, .. } => {
                let value = self.eval_expr(value, env)?;
                env.define(name.clone(), value);
                Ok(Control::Value(Value::Void))
            }
            IrStmt::Assign {
                name, op, value, ..
            } => {
                let value = self.eval_expr(value, env)?;
                let value = match op {
                    AssignOp::Replace => value,
                    AssignOp::Add => Value::I64(env.get(name)?.as_i64()? + value.as_i64()?),
                    AssignOp::Subtract => Value::I64(env.get(name)?.as_i64()? - value.as_i64()?),
                    AssignOp::Multiply => Value::I64(env.get(name)?.as_i64()? * value.as_i64()?),
                    AssignOp::Divide => {
                        let divisor = value.as_i64()?;
                        if divisor == 0 {
                            return Err(RuntimeError::new("N0404", "division by zero"));
                        }
                        Value::I64(env.get(name)?.as_i64()? / divisor)
                    }
                };
                env.assign(name, value)?;
                Ok(Control::Value(Value::Void))
            }
            IrStmt::Return(expr) => {
                let value = expr
                    .as_ref()
                    .map(|expr| self.eval_expr(expr, env))
                    .unwrap_or(Ok(Value::Void))?;
                Ok(Control::Return(value))
            }
            IrStmt::Break(_) => Ok(Control::Break),
            IrStmt::Continue(_) => Ok(Control::Continue),
            IrStmt::Expr(expr) => self.eval_expr(expr, env).map(Control::Value),
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    let condition = self.eval_expr(&branch.condition, env)?;
                    if condition.as_bool()? {
                        return self.eval_scoped_block(&branch.body, env);
                    }
                }
                self.eval_scoped_block(else_body, env)
            }
            IrStmt::While {
                condition, body, ..
            } => {
                while self.eval_expr(condition, env)?.as_bool()? {
                    match self.eval_scoped_block(body, env)? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }
                }
                Ok(Control::Value(Value::Void))
            }
            IrStmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                let mut current = self.eval_expr(start, env)?.as_i64()?;
                let end = self.eval_expr(end, env)?.as_i64()?;
                let step = step
                    .as_ref()
                    .map(|expr| self.eval_expr(expr, env))
                    .unwrap_or(Ok(Value::I64(1)))?
                    .as_i64()?;
                if step == 0 {
                    return Err(RuntimeError::new("N0411", "for loop step cannot be zero"));
                }

                while if step > 0 {
                    current <= end
                } else {
                    current >= end
                } {
                    env.push_scope();
                    env.define(name.clone(), Value::I64(current));
                    let result = self.eval_block(body, env);
                    env.pop_scope();

                    match result? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }

                    current += step;
                }
                Ok(Control::Value(Value::Void))
            }
            IrStmt::Loop { body, .. } => {
                loop {
                    match self.eval_scoped_block(body, env)? {
                        Control::Return(value) => return Ok(Control::Return(value)),
                        Control::Break => break,
                        Control::Continue | Control::Value(_) => {}
                    }
                }
                Ok(Control::Value(Value::Void))
            }
        };
        result.map_err(|error| self.annotate_error(error, span))
    }

    fn eval_scoped_block(
        &mut self,
        statements: &[IrStmt],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        env.push_scope();
        let result = self.eval_block(statements, env);
        env.pop_scope();
        result
    }

    fn eval_expr(&mut self, expr: &IrExpr, env: &Env) -> Result<Value, RuntimeError> {
        let result = match &expr.kind {
            IrExprKind::Integer(value) => Ok(Value::I64(*value)),
            IrExprKind::Bool(value) => Ok(Value::Bool(*value)),
            IrExprKind::String(value) => Ok(Value::String(value.clone())),
            IrExprKind::Array(values) => values
                .iter()
                .map(|value| self.eval_expr(value, env))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            IrExprKind::Variable(name) => env.get(name),
            IrExprKind::Index { target, index } => {
                let target = self.eval_expr(target, env)?;
                let index = self.eval_expr(index, env)?.as_i64()?;
                let Value::Array(values) = target else {
                    return Err(RuntimeError::new("N0412", "index target is not an array"));
                };
                if index < 0 {
                    return Err(RuntimeError::new(
                        "N0413",
                        format!("array index `{index}` is out of bounds"),
                    ));
                }
                values.get(index as usize).cloned().ok_or_else(|| {
                    RuntimeError::new("N0413", format!("array index `{index}` is out of bounds"))
                })
            }
            IrExprKind::Unary { op, expr } => {
                let value = self.eval_expr(expr, env)?;
                match op {
                    UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
                }
            }
            IrExprKind::Binary { left, op, right } => {
                if *op == BinaryOp::And {
                    let left = self.eval_expr(left, env)?.as_bool()?;
                    if !left {
                        return Ok(Value::Bool(false));
                    }
                    let right = self.eval_expr(right, env)?.as_bool()?;
                    return Ok(Value::Bool(right));
                }
                if *op == BinaryOp::Or {
                    let left = self.eval_expr(left, env)?.as_bool()?;
                    if left {
                        return Ok(Value::Bool(true));
                    }
                    let right = self.eval_expr(right, env)?.as_bool()?;
                    return Ok(Value::Bool(right));
                }
                let left = self.eval_expr(left, env)?;
                let right = self.eval_expr(right, env)?;
                self.eval_binary(left, *op, right)
            }
            IrExprKind::Call { name, args } => {
                let values = args
                    .iter()
                    .map(|arg| self.eval_expr(arg, env))
                    .collect::<Result<Vec<_>, _>>()?;
                self.call_function(name, values)
            }
        };
        result.map_err(|error| self.annotate_error(error, expr.span))
    }

    fn annotate_error(&self, error: RuntimeError, span: Span) -> RuntimeError {
        let error = error.with_span(span);
        match self.call_stack.last() {
            Some(frame) => error
                .with_function(frame.function.clone())
                .with_traceback(self.call_stack.clone()),
            None => error,
        }
    }

    fn eval_binary(&self, left: Value, op: BinaryOp, right: Value) -> Result<Value, RuntimeError> {
        match op {
            BinaryOp::Add => Ok(Value::I64(left.as_i64()? + right.as_i64()?)),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()? - right.as_i64()?)),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()? * right.as_i64()?)),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("N0404", "division by zero"))
                } else {
                    Ok(Value::I64(left.as_i64()? / divisor))
                }
            }
            BinaryOp::Equal => Ok(Value::Bool(left == right)),
            BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
            BinaryOp::Less => Ok(Value::Bool(left.as_i64()? < right.as_i64()?)),
            BinaryOp::LessEqual => Ok(Value::Bool(left.as_i64()? <= right.as_i64()?)),
            BinaryOp::Greater => Ok(Value::Bool(left.as_i64()? > right.as_i64()?)),
            BinaryOp::GreaterEqual => Ok(Value::Bool(left.as_i64()? >= right.as_i64()?)),
            BinaryOp::And | BinaryOp::Or => unreachable!("logical ops short-circuit in eval_expr"),
        }
    }

    fn builtin_alloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("alloc", 1, args.len()))?;
        self.heap.push(Some(value));
        Ok(Value::Ptr(self.heap.len() - 1))
    }

    fn builtin_load(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("load", 1, args.len()))?;
        let slot = ptr.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("N0406", format!("invalid pointer `{slot}`")))
    }

    fn builtin_store(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("store", 2, args.len()))?;
        let slot = ptr.as_ptr()?;
        let Some(target) = self.heap.get_mut(slot) else {
            return Err(RuntimeError::new(
                "N0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if target.is_none() {
            return Err(RuntimeError::new(
                "N0406",
                format!("invalid pointer `{slot}`"),
            ));
        }
        *target = Some(value);
        Ok(Value::Void)
    }

    fn builtin_dealloc(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("dealloc", 1, args.len()))?;
        let slot = ptr.as_ptr()?;
        let Some(value) = self.heap.get_mut(slot) else {
            return Err(RuntimeError::new(
                "N0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if value.take().is_none() {
            return Err(RuntimeError::new(
                "N0406",
                format!("invalid pointer `{slot}`"),
            ));
        }
        Ok(Value::Void)
    }

    fn builtin_read_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_file", 1, args.len()))?;
        let path = path.as_string()?;
        fs::read_to_string(&path)
            .map(Value::String)
            .map_err(|error| {
                RuntimeError::resource("N0414", format!("failed to read `{path}`: {error}"))
            })
    }

    fn builtin_write_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, contents]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("write_file", 2, args.len()))?;
        let path = path.as_string()?;
        let contents = contents.as_string()?;
        fs::write(&path, contents)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("N0415", format!("failed to write `{path}`: {error}"))
            })
    }

    fn builtin_append_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, contents]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("append_file", 2, args.len()))?;
        let path = path.as_string()?;
        let contents = contents.as_string()?;
        use std::io::Write;
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(contents.as_bytes()))
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("N0415", format!("failed to append `{path}`: {error}"))
            })
    }

    fn builtin_file_exists(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_exists", 1, args.len()))?;
        Ok(Value::Bool(fs::metadata(path.as_string()?).is_ok()))
    }

    fn builtin_sys_status(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [program, command_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sys_status", 2, args.len()))?;
        let program = program.as_string()?;
        let command_args = command_args.as_string_array()?;
        let output = Command::new(&program)
            .args(command_args)
            .output()
            .map_err(|error| {
                RuntimeError::resource("N0416", format!("failed to run `{program}`: {error}"))
            })?;
        Ok(Value::I64(output.status.code().unwrap_or(-1).into()))
    }

    fn builtin_sys_output(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [program, command_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sys_output", 2, args.len()))?;
        let program = program.as_string()?;
        let command_args = command_args.as_string_array()?;
        let output = Command::new(&program)
            .args(command_args)
            .output()
            .map_err(|error| {
                RuntimeError::resource("N0416", format!("failed to run `{program}`: {error}"))
            })?;
        Ok(Value::String(
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    }

    fn wrong_arity(name: &str, expected: usize, actual: usize) -> RuntimeError {
        RuntimeError::new(
            "N0405",
            format!("function `{name}` expects {expected} arguments but got {actual}"),
        )
    }
}

enum Control {
    Return(Value),
    Break,
    Continue,
    Value(Value),
}

fn statement_span(statement: &IrStmt) -> Span {
    match statement {
        IrStmt::Let { span, .. }
        | IrStmt::Assign { span, .. }
        | IrStmt::Break(span)
        | IrStmt::Continue(span)
        | IrStmt::If { span, .. }
        | IrStmt::While { span, .. }
        | IrStmt::For { span, .. }
        | IrStmt::Loop { span, .. } => *span,
        IrStmt::Return(Some(expr)) | IrStmt::Expr(expr) => expr.span,
        IrStmt::Return(None) => Span::new(1, 1),
    }
}

#[derive(Debug, Clone)]
struct Env {
    scopes: Vec<HashMap<String, Value>>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            scopes: vec![HashMap::new()],
        }
    }
}

impl Env {
    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, value: Value) {
        self.scopes
            .last_mut()
            .expect("env always has a scope")
            .insert(name, value);
    }

    fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return Ok(());
            }
        }
        Err(RuntimeError::new(
            "N0403",
            format!("unknown variable `{name}`"),
        ))
    }

    fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .cloned()
            .ok_or_else(|| RuntimeError::new("N0403", format!("unknown variable `{name}`")))
    }
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
    use std::path::{Path, PathBuf};

    use nous_lexer::lex;
    use nous_parser::parse;
    use nous_runtime::run_main as run_ast_main;
    use nous_semantics::{validate, validate_executable};

    use super::*;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|path| path.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    fn lower_source(source: &str) -> IrModule {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate(&program).expect("semantic");
        lower(&checked).expect("lower")
    }

    fn run_all_backends(source: &str) -> (Value, Value, Value) {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate_executable(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        let bytecode = lower_to_bytecode(&ir);
        (
            run_ast_main(&program).expect("ast run"),
            run_main(&ir).expect("ir run"),
            run_bytecode_main(&bytecode).expect("bytecode run"),
        )
    }

    fn run_all_backend_variants(source: &str) -> (Value, Value, Value, Value, Value) {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate_executable(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        let bytecode = lower_to_bytecode(&ir);
        let (optimized, _) = optimize(&ir, &OptimizationConfig::alpha_default());
        let optimized_bytecode = lower_to_bytecode(&optimized);

        (
            run_ast_main(&program).expect("ast run"),
            run_main(&ir).expect("ir run"),
            run_bytecode_main(&bytecode).expect("bytecode run"),
            run_main(&optimized).expect("optimized ir run"),
            run_bytecode_main(&optimized_bytecode).expect("optimized bytecode run"),
        )
    }

    fn executable_fixture_source(path: &Path) -> Option<String> {
        let source = fs::read_to_string(path).expect("fixture source");
        let tokens = lex(&source).expect("fixture lex");
        let program = parse(&tokens).expect("fixture parse");
        validate_executable(&program).ok().map(|_| {
            source.replace(
                "target/nous_fixture_io.txt",
                "target/nous_ir_fixture_io.txt",
            )
        })
    }

    fn cleanup_parity_files() {
        fs::create_dir_all("target").expect("target directory");
        let _ = fs::remove_file("target/nous_ir_fixture_io.txt");
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

    #[test]
    fn constant_folding_rewrites_pure_literal_expressions() {
        let module = lower_source(
            "fn main -> i64\n    let value i64 = (2 + 3) * (10 - 6)\n    if not false and 1 < 2\n        value + 22\n    else\n        0\n",
        );

        let (optimized, report) = optimize(&module, &OptimizationConfig::constant_folding());
        assert_eq!(
            report.applied_passes,
            vec![OptimizationPass::ConstantFolding]
        );
        assert!(report.folded_expressions >= 5);

        let function = &optimized.functions[0];
        let IrStmt::Let { value, .. } = &function.body[0] else {
            panic!("expected let statement");
        };
        assert_eq!(value.kind, IrExprKind::Integer(20));
        let IrStmt::If { branches, .. } = &function.body[1] else {
            panic!("expected if statement");
        };
        assert_eq!(branches[0].condition.kind, IrExprKind::Bool(true));
    }

    #[test]
    fn constant_folding_preserves_runtime_divide_by_zero() {
        let module = lower_source("fn main -> i64\n    1 / 0\n");
        let (optimized, report) = optimize(&module, &OptimizationConfig::constant_folding());

        assert_eq!(report.folded_expressions, 0);
        assert_eq!(
            run_main(&optimized).expect_err("division by zero").code,
            "N0404"
        );
    }

    #[test]
    fn optimization_passes_can_be_disabled() {
        let module = lower_source("fn main -> i64\n    40 + 2\n");
        let (optimized, report) = optimize(&module, &OptimizationConfig::none());

        assert_eq!(optimized, module);
        assert!(report.applied_passes.is_empty());
        assert_eq!(report.folded_expressions, 0);
    }

    #[test]
    fn dead_code_elimination_removes_statements_after_return() {
        let module = lower_source("fn main -> i64\n    return 42\n    0\n");
        let (optimized, report) = optimize(&module, &OptimizationConfig::dead_code_elimination());

        assert_eq!(
            report.applied_passes,
            vec![OptimizationPass::DeadCodeElimination]
        );
        assert_eq!(report.removed_dead_statements, 1);
        assert_eq!(optimized.functions[0].body.len(), 1);
        assert!(matches!(optimized.functions[0].body[0], IrStmt::Return(_)));
    }

    #[test]
    fn dead_code_elimination_rewrites_nested_blocks() {
        let module = lower_source(
            "fn main -> i64\n    let total i64 = 0\n    if true\n        return 1\n        total + 1\n    else\n        return 2\n        total + 2\n    loop\n        break\n        total += 1\n    total\n",
        );
        let (optimized, report) = optimize(&module, &OptimizationConfig::dead_code_elimination());

        assert_eq!(report.removed_dead_statements, 3);
        let IrStmt::If {
            branches,
            else_body,
            ..
        } = &optimized.functions[0].body[1]
        else {
            panic!("expected if statement");
        };
        assert_eq!(branches[0].body.len(), 1);
        assert_eq!(else_body.len(), 1);

        let IrStmt::Loop { body, .. } = &optimized.functions[0].body[2] else {
            panic!("expected loop statement");
        };
        assert_eq!(body.len(), 1);
        assert!(matches!(body[0], IrStmt::Break(_)));
    }

    #[test]
    fn copy_propagation_rewrites_straight_line_aliases() {
        let module = lower_source(
            "fn main -> i64\n    let base i64 = 40\n    let alias i64 = base\n    let second i64 = alias\n    second + 2\n",
        );
        let (optimized, report) = optimize(&module, &OptimizationConfig::copy_propagation());

        assert_eq!(
            report.applied_passes,
            vec![OptimizationPass::CopyPropagation]
        );
        assert_eq!(report.propagated_copies, 2);

        let IrStmt::Let { value, .. } = &optimized.functions[0].body[2] else {
            panic!("expected propagated let binding");
        };
        assert_eq!(value.kind, IrExprKind::Variable("base".to_string()));

        let IrStmt::Expr(expr) = &optimized.functions[0].body[3] else {
            panic!("expected final expression");
        };
        let IrExprKind::Binary { left, .. } = &expr.kind else {
            panic!("expected binary expression");
        };
        assert_eq!(left.kind, IrExprKind::Variable("base".to_string()));
    }

    #[test]
    fn copy_propagation_invalidates_aliases_after_source_assignment() {
        let source = "fn main -> i64\n    let source i64 = 1\n    let alias i64 = source\n    source = 2\n    alias\n";
        let module = lower_source(source);
        let (optimized, report) = optimize(&module, &OptimizationConfig::copy_propagation());

        assert_eq!(report.propagated_copies, 0);
        assert_eq!(
            run_main(&optimized).expect("optimized run"),
            run_all_backends(source).0
        );

        let IrStmt::Expr(expr) = &optimized.functions[0].body[3] else {
            panic!("expected final expression");
        };
        assert_eq!(expr.kind, IrExprKind::Variable("alias".to_string()));
    }

    #[test]
    fn alpha_optimizer_runs_constant_folding_then_dead_code_elimination() {
        let module =
            lower_source("fn main -> i64\n    let value i64 = 40 + 2\n    return value\n    0\n");
        let (optimized, report) = optimize(&module, &OptimizationConfig::alpha_default());

        assert_eq!(
            report.applied_passes,
            vec![
                OptimizationPass::ConstantFolding,
                OptimizationPass::CopyPropagation,
                OptimizationPass::DeadCodeElimination
            ]
        );
        assert_eq!(report.folded_expressions, 1);
        assert_eq!(report.removed_dead_statements, 1);
        let IrStmt::Let { value, .. } = &optimized.functions[0].body[0] else {
            panic!("expected let statement");
        };
        assert_eq!(value.kind, IrExprKind::Integer(42));
        assert_eq!(optimized.functions[0].body.len(), 2);
    }

    #[test]
    fn bytecode_artifact_round_trips_and_executes() {
        let ir = lower_source("fn main -> i64\n    40 + 2\n");
        let bytecode = lower_to_bytecode(&ir);
        let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
        let artifact = decode_bytecode_artifact(&encoded).expect("decode artifact");

        assert_eq!(artifact.format, BYTECODE_ARTIFACT_FORMAT);
        assert_eq!(artifact.version, BYTECODE_ARTIFACT_VERSION);
        assert_eq!(artifact.metadata.target, "alpha1");
        assert_eq!(artifact.metadata.payload, "structured-bytecode");
        assert_eq!(artifact.entry, "main");
        assert_eq!(artifact.function_table.len(), 1);
        assert_eq!(artifact.function_table[0].name, "main");
        assert_eq!(
            run_bytecode_main(&artifact.module).expect("run artifact bytecode"),
            Value::I64(42)
        );
    }

    #[test]
    fn bytecode_artifact_rejects_wrong_version() {
        let invalid = format!(
            "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":999,\"entry\":\"main\",\"module\":{{\"functions\":[]}}}}"
        );
        let error = decode_bytecode_artifact(&invalid).expect_err("invalid version");

        assert!(
            error
                .message
                .contains("unsupported bytecode artifact version")
        );
    }

    #[test]
    fn bytecode_artifact_rejects_missing_entry_function() {
        let invalid = format!(
            "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":{BYTECODE_ARTIFACT_VERSION},\"entry\":\"main\",\"metadata\":{{\"producer\":\"test\",\"target\":\"alpha1\",\"payload\":\"structured-bytecode\"}},\"function_table\":[],\"module\":{{\"functions\":[]}}}}"
        );
        let error = decode_bytecode_artifact(&invalid).expect_err("missing entry");

        assert!(
            error
                .message
                .contains("entry `main` is not present in the module")
        );
    }

    #[test]
    fn bytecode_artifact_rejects_function_table_mismatch() {
        let ir = lower_source("fn main -> i64\n    42\n");
        let bytecode = lower_to_bytecode(&ir);
        let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
        let mut value: serde_json::Value = serde_json::from_str(&encoded).expect("json artifact");
        value["function_table"] = serde_json::json!([]);
        let invalid = serde_json::to_string(&value).expect("invalid json artifact");

        let error = decode_bytecode_artifact(&invalid).expect_err("table mismatch");

        assert!(error.message.contains("function_table does not match"));
    }

    #[test]
    fn optimized_ir_and_bytecode_match_ast_execution() {
        let source = "fn main -> i64\n    let folded i64 = (6 * 7) + (10 / 2)\n    return folded - 5\n    0\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        let (optimized, report) = optimize(&ir, &OptimizationConfig::alpha_default());
        let bytecode = lower_to_bytecode(&optimized);

        assert!(report.folded_expressions > 0);
        assert_eq!(report.removed_dead_statements, 1);
        assert_eq!(
            run_main(&optimized).expect("optimized ir run"),
            run_ast_main(&program).expect("ast run")
        );
        assert_eq!(
            run_bytecode_main(&bytecode).expect("optimized bytecode run"),
            run_ast_main(&program).expect("ast run")
        );
    }

    #[test]
    fn ir_and_bytecode_match_ast_for_core_execution() {
        let sources = [
            "fn add x i64 y i64 -> i64\n    x + y\n\nfn main -> i64\n    let value i64 = add(40, 2)\n    value\n",
            "fn main -> i64\n    let x i64 = 0\n    while x < 4\n        x += 1\n    x\n",
            "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        total += i\n    total\n",
            "fn main -> bool\n    false and (1 / 0 == 0) or true\n",
            "fn main -> i64\n    let values array<i64> = [1, 2, 3]\n    values[0] + values[2]\n",
        ];

        for source in sources {
            let (ast, ir, bytecode) = run_all_backends(source);
            assert_eq!(ir, ast);
            assert_eq!(bytecode, ast);
        }
    }

    #[test]
    fn ir_and_bytecode_match_ast_for_memory_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        let (ast, ir, bytecode) = run_all_backends(source);
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
    }

    #[test]
    fn executable_valid_fixtures_match_across_backend_variants() {
        cleanup_parity_files();
        let fixture_dir = workspace_root().join("tests/fixtures/valid");
        let mut fixtures = fs::read_dir(&fixture_dir)
            .expect("valid fixture directory")
            .map(|entry| entry.expect("fixture entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("nl"))
            .collect::<Vec<_>>();
        fixtures.sort();

        let mut covered = Vec::new();
        for fixture in fixtures {
            let Some(source) = executable_fixture_source(&fixture) else {
                continue;
            };
            cleanup_parity_files();
            let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
                run_all_backend_variants(&source);
            let name = fixture
                .file_name()
                .and_then(|name| name.to_str())
                .expect("fixture name");
            assert_eq!(ir, ast, "{name}: IR result differs from AST");
            assert_eq!(bytecode, ast, "{name}: bytecode result differs from AST");
            assert_eq!(
                optimized_ir, ast,
                "{name}: optimized IR result differs from AST"
            );
            assert_eq!(
                optimized_bytecode, ast,
                "{name}: optimized bytecode result differs from AST"
            );
            covered.push(name.to_string());
        }
        cleanup_parity_files();

        assert!(
            covered.len() >= 10,
            "expected broad executable fixture coverage, got {covered:?}"
        );
        assert!(covered.contains(&"run_file_io.nl".to_string()));
        assert!(covered.contains(&"run_store.nl".to_string()));
        assert!(covered.contains(&"run_for_step.nl".to_string()));
    }

    #[test]
    fn ir_and_bytecode_preserve_runtime_errors() {
        let source = "fn main -> i64\n    let values array<i64> = [1]\n    values[2]\n";
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        let bytecode = lower_to_bytecode(&ir);

        let ast_error = run_ast_main(&program).expect_err("ast error");
        let ir_error = run_main(&ir).expect_err("ir error");
        let bytecode_error = run_bytecode_main(&bytecode).expect_err("bytecode error");

        assert_eq!(ir_error.code, ast_error.code);
        assert_eq!(bytecode_error.code, ast_error.code);
        assert_eq!(ir_error.span, ast_error.span);
        assert_eq!(bytecode_error.span, ast_error.span);
    }
}
