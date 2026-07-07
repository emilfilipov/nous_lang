use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::Command;
use std::sync::Arc;

use lullaby_diagnostics::{Span, TraceFrame};
use lullaby_parser::{
    AssignOp, BinaryOp, Expr, ExprKind, Function, MatchArm, MatchPattern, Place, Program, Stmt,
    TypeRef, UnaryOp, function_type, generic_type,
};
use lullaby_runtime::{
    ResolvedPlace, RuntimeError, SharedMutex, SocketResource, Task, Value, apply_compound,
    char_find, expect_chan, expect_i64, expect_list, expect_map, expect_mutex, expect_string,
    expect_task, get_place, http_exchange, join_task, net_err, new_chan, option_value,
    result_value, scalar_order_keys, set_place, value_type_name,
};
use lullaby_semantics::{CheckedProgram, Signature};
use serde::{Deserialize, Serialize};

pub mod frame_layout;
pub mod native_contract;
pub mod native_object;
pub mod wasm;

pub use native_object::{
    NATIVE_ENTRY_SYMBOL, NATIVE_NO_ELIGIBLE_CODE, NativeProgram, NativeProgramError,
    NativeSkippedFunction, emit_alpha1_native_program,
};
pub use wasm::{SkippedFunction, WasmArtifact, WasmError, emit_wasm_module};

pub const BYTECODE_ARTIFACT_FORMAT: &str = "lullaby-bytecode";
pub const BYTECODE_ARTIFACT_EXTENSION: &str = "lbc";
pub const BYTECODE_ARTIFACT_VERSION: u32 = 5;
const BYTECODE_ARTIFACT_PAYLOAD: &str = "instruction-bytecode";
const BYTECODE_ARTIFACT_TARGET: &str = "alpha1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrModule {
    pub functions: Vec<IrFunction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structs: Vec<IrStructDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<IrEnumDef>,
    /// Trait implementations: each maps a `(type, method)` to a lowered function.
    /// Serde-defaulted so existing artifacts and snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impls: Vec<IrImplMethod>,
    /// Names declared as trait methods. A call to one dispatches on the
    /// receiver's runtime type via `impls`. Serde-defaulted for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trait_methods: Vec<String>,
}

/// One trait impl method in the IR: the implementing type name, the method name,
/// and the lowered function body. Dispatch keys on `(type_name, method_name)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrImplMethod {
    pub type_name: String,
    pub method_name: String,
    pub function: IrFunction,
}

/// A struct type in the IR: name plus ordered `(field, type)` pairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrStructDef {
    pub name: String,
    pub fields: Vec<(String, TypeRef)>,
}

/// An enum type in the IR: name plus ordered variants, each a name plus an
/// ordered list of positional payload types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrEnumDef {
    pub name: String,
    pub variants: Vec<IrEnumVariant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrEnumVariant {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<TypeRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IrStmt {
    Let {
        name: String,
        ty: TypeRef,
        value: IrExpr,
        span: Span,
    },
    Assign {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        path: Vec<IrPlace>,
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
    Throw {
        value: IrExpr,
        span: Span,
    },
    Try {
        body: Vec<IrStmt>,
        catch_name: String,
        catch_body: Vec<IrStmt>,
        span: Span,
    },
    Match {
        scrutinee: IrExpr,
        arms: Vec<IrMatchArm>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrIfBranch {
    pub condition: IrExpr,
    pub body: Vec<IrStmt>,
}

/// One arm of an IR `match`: a pattern plus a lowered body block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrMatchArm {
    pub pattern: IrMatchPattern,
    pub body: Vec<IrStmt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrMatchPattern {
    Variant {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bindings: Vec<String>,
    },
    Wildcard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrExpr {
    pub kind: IrExprKind,
    pub ty: TypeRef,
    pub span: Span,
}

/// One hop of an assignment target in the IR: a struct field or an array index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IrPlace {
    Field(String),
    Index(IrExpr),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IrExprKind {
    Integer(i64),
    Float(f64),
    Bool(bool),
    String(String),
    /// A `'c'` char literal: exactly one Unicode scalar.
    Char(char),
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
    Field {
        target: Box<IrExpr>,
        field: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrMemoryOperation {
    pub function: String,
    pub sequence: usize,
    pub span: Span,
    pub kind: IrMemoryOperationKind,
    pub safety: IrMemorySafety,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrMemoryOperationKind {
    Allocate {
        value_type: TypeRef,
        pointer_type: TypeRef,
    },
    Load {
        pointer_type: TypeRef,
        value_type: TypeRef,
    },
    Store {
        pointer_type: TypeRef,
        value_type: TypeRef,
    },
    Deallocate {
        pointer_type: TypeRef,
    },
    BoundsCheck {
        target_type: TypeRef,
        index_type: TypeRef,
    },
    RegionCreate {
        region_type: TypeRef,
    },
    RegionResize {
        region_type: TypeRef,
    },
    Copy {
        source_type: TypeRef,
        target_type: TypeRef,
    },
    Cleanup {
        resource_type: TypeRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrMemorySafety {
    pub requires_live_resource: bool,
    pub requires_bounds_check: bool,
    pub mutates_memory: bool,
    pub cleanup_role: IrCleanupRole,
    pub unsafe_boundary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IrCleanupRole {
    None,
    CreatesResource,
    UsesResource,
    ReleasesResource,
    CheckedAccess,
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

    pub fn common_subexpression_elimination() -> Self {
        Self {
            passes: vec![OptimizationPass::CommonSubexpressionElimination],
        }
    }

    pub fn copy_propagation() -> Self {
        Self {
            passes: vec![OptimizationPass::CopyPropagation],
        }
    }

    pub fn loop_invariant_motion() -> Self {
        Self {
            passes: vec![OptimizationPass::LoopInvariantMotion],
        }
    }

    pub fn alpha_default() -> Self {
        Self {
            passes: vec![
                OptimizationPass::ConstantFolding,
                OptimizationPass::CommonSubexpressionElimination,
                OptimizationPass::LoopInvariantMotion,
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
    CommonSubexpressionElimination,
    LoopInvariantMotion,
    CopyPropagation,
    DeadCodeElimination,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OptimizationReport {
    pub applied_passes: Vec<OptimizationPass>,
    pub folded_expressions: usize,
    pub eliminated_common_subexpressions: usize,
    pub hoisted_loop_invariants: usize,
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
            OptimizationPass::CommonSubexpressionElimination => {
                let mut eliminator = CommonSubexpressionEliminator::default();
                optimized = eliminator.eliminate_module(&optimized);
                report.eliminated_common_subexpressions +=
                    eliminator.eliminated_common_subexpressions;
                report.applied_passes.push(*pass);
            }
            OptimizationPass::LoopInvariantMotion => {
                let mut mover = LoopInvariantMover::default();
                optimized = mover.move_module(&optimized);
                report.hoisted_loop_invariants += mover.hoisted_loop_invariants;
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
    run_main_with_args(module, Vec::new())
}

/// Run `main` on the IR interpreter with the running program's CLI arguments,
/// which the `args()` builtin exposes. `run_main` is the zero-argument wrapper.
///
/// The module is wrapped in an `Arc<IrModule>` here so a detached thread created
/// by `spawn` can own a share of the module and run Lullaby independently. The
/// interpreter keeps its usual `&IrModule` borrow (from `&*arc`) for normal use
/// and ALSO holds an owned `Arc<IrModule>` clone purely to hand to spawned
/// threads — two separate handles to the same shared data, not self-referential.
pub fn run_main_with_args(module: &IrModule, args: Vec<String>) -> Result<Value, RuntimeError> {
    let arc = Arc::new(module.clone());
    run_main_shared(arc, args)
}

/// Shared-module entry: build an interpreter borrowing `&*arc` while retaining an
/// owned `Arc<IrModule>` clone for detached-thread spawning.
fn run_main_shared(arc: Arc<IrModule>, args: Vec<String>) -> Result<Value, RuntimeError> {
    let mut runtime = IrRuntime::new(&arc, Arc::clone(&arc))?;
    runtime.program_args = args;
    runtime.call_function("main", Vec::new())
}

pub fn analyze_memory_operations(module: &IrModule) -> Vec<IrMemoryOperation> {
    let mut operations = Vec::new();
    for function in &module.functions {
        collect_memory_operations_from_block(&function.name, &function.body, &mut operations);
    }
    operations
}

fn collect_memory_operations_from_block(
    function: &str,
    statements: &[IrStmt],
    operations: &mut Vec<IrMemoryOperation>,
) {
    for statement in statements {
        match statement {
            IrStmt::Let { value, .. } | IrStmt::Assign { value, .. } | IrStmt::Expr(value) => {
                collect_memory_operations_from_expr(function, value, operations);
            }
            IrStmt::Return(Some(value)) => {
                collect_memory_operations_from_expr(function, value, operations);
            }
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    collect_memory_operations_from_expr(function, &branch.condition, operations);
                    collect_memory_operations_from_block(function, &branch.body, operations);
                }
                collect_memory_operations_from_block(function, else_body, operations);
            }
            IrStmt::While {
                condition, body, ..
            } => {
                collect_memory_operations_from_expr(function, condition, operations);
                collect_memory_operations_from_block(function, body, operations);
            }
            IrStmt::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                collect_memory_operations_from_expr(function, start, operations);
                collect_memory_operations_from_expr(function, end, operations);
                if let Some(step) = step {
                    collect_memory_operations_from_expr(function, step, operations);
                }
                collect_memory_operations_from_block(function, body, operations);
            }
            IrStmt::Loop { body, .. } => {
                collect_memory_operations_from_block(function, body, operations);
            }
            IrStmt::Throw { value, .. } => {
                collect_memory_operations_from_expr(function, value, operations);
            }
            IrStmt::Try {
                body, catch_body, ..
            } => {
                collect_memory_operations_from_block(function, body, operations);
                collect_memory_operations_from_block(function, catch_body, operations);
            }
            IrStmt::Match {
                scrutinee, arms, ..
            } => {
                collect_memory_operations_from_expr(function, scrutinee, operations);
                for arm in arms {
                    collect_memory_operations_from_block(function, &arm.body, operations);
                }
            }
            IrStmt::Return(None) | IrStmt::Break(_) | IrStmt::Continue(_) => {}
        }
    }
}

fn collect_memory_operations_from_expr(
    function: &str,
    expr: &IrExpr,
    operations: &mut Vec<IrMemoryOperation>,
) {
    match &expr.kind {
        IrExprKind::Array(values) => {
            for value in values {
                collect_memory_operations_from_expr(function, value, operations);
            }
        }
        IrExprKind::Field { target, .. } => {
            collect_memory_operations_from_expr(function, target, operations);
        }
        IrExprKind::Index { target, index } => {
            collect_memory_operations_from_expr(function, target, operations);
            collect_memory_operations_from_expr(function, index, operations);
            operations.push(IrMemoryOperation {
                function: function.to_string(),
                sequence: operations.len(),
                span: expr.span,
                kind: IrMemoryOperationKind::BoundsCheck {
                    target_type: target.ty.clone(),
                    index_type: index.ty.clone(),
                },
                safety: IrMemorySafety {
                    requires_live_resource: false,
                    requires_bounds_check: true,
                    mutates_memory: false,
                    cleanup_role: IrCleanupRole::CheckedAccess,
                    unsafe_boundary: false,
                },
            });
        }
        IrExprKind::Unary { expr, .. } => {
            collect_memory_operations_from_expr(function, expr, operations);
        }
        IrExprKind::Binary { left, right, .. } => {
            collect_memory_operations_from_expr(function, left, operations);
            collect_memory_operations_from_expr(function, right, operations);
        }
        IrExprKind::Call { name, args } => {
            for arg in args {
                collect_memory_operations_from_expr(function, arg, operations);
            }
            if let Some(operation) = classify_memory_call(function, name, args, expr) {
                operations.push(IrMemoryOperation {
                    sequence: operations.len(),
                    ..operation
                });
            }
        }
        IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
        | IrExprKind::Variable(_) => {}
    }
}

fn classify_memory_call(
    function: &str,
    name: &str,
    args: &[IrExpr],
    expr: &IrExpr,
) -> Option<IrMemoryOperation> {
    let kind = match name {
        "alloc" => {
            let value = args.first()?;
            IrMemoryOperationKind::Allocate {
                value_type: value.ty.clone(),
                pointer_type: expr.ty.clone(),
            }
        }
        "load" => {
            let pointer = args.first()?;
            IrMemoryOperationKind::Load {
                pointer_type: pointer.ty.clone(),
                value_type: expr.ty.clone(),
            }
        }
        "store" => {
            let pointer = args.first()?;
            let value = args.get(1)?;
            IrMemoryOperationKind::Store {
                pointer_type: pointer.ty.clone(),
                value_type: value.ty.clone(),
            }
        }
        "dealloc" => {
            let pointer = args.first()?;
            IrMemoryOperationKind::Deallocate {
                pointer_type: pointer.ty.clone(),
            }
        }
        // Reference-counted and raw-reference operations feed the same memory
        // analysis so optimizers and codegen see their allocation, sharing,
        // cleanup, and dereference effects.
        "rc_new" => {
            let value = args.first()?;
            IrMemoryOperationKind::Allocate {
                value_type: value.ty.clone(),
                pointer_type: expr.ty.clone(),
            }
        }
        "rc_clone" => {
            let handle = args.first()?;
            IrMemoryOperationKind::Copy {
                source_type: handle.ty.clone(),
                target_type: expr.ty.clone(),
            }
        }
        "rc_release" => {
            let handle = args.first()?;
            IrMemoryOperationKind::Cleanup {
                resource_type: handle.ty.clone(),
            }
        }
        "rc_get" | "ref_get" | "ptr_read" => {
            let reference = args.first()?;
            IrMemoryOperationKind::Load {
                pointer_type: reference.ty.clone(),
                value_type: expr.ty.clone(),
            }
        }
        "ptr_write" => {
            let pointer = args.first()?;
            let value = args.get(1)?;
            IrMemoryOperationKind::Store {
                pointer_type: pointer.ty.clone(),
                value_type: value.ty.clone(),
            }
        }
        "region_create" => IrMemoryOperationKind::RegionCreate {
            region_type: region_type_of(args.first()),
        },
        _ => return None,
    };

    let safety = memory_safety_for_kind(&kind)?;

    Some(IrMemoryOperation {
        function: function.to_string(),
        sequence: 0,
        span: expr.span,
        kind,
        safety,
    })
}

/// Build a region type name from a `region_create` marker's leading name arg.
fn region_type_of(name_arg: Option<&IrExpr>) -> TypeRef {
    match name_arg.map(|arg| &arg.kind) {
        Some(IrExprKind::String(name)) => TypeRef::new(format!("region<{name}>")),
        _ => TypeRef::new("region"),
    }
}

fn memory_safety_for_kind(kind: &IrMemoryOperationKind) -> Option<IrMemorySafety> {
    match kind {
        IrMemoryOperationKind::Allocate { .. } => Some(IrMemorySafety {
            requires_live_resource: false,
            requires_bounds_check: false,
            mutates_memory: true,
            cleanup_role: IrCleanupRole::CreatesResource,
            unsafe_boundary: false,
        }),
        IrMemoryOperationKind::Load { .. } => Some(IrMemorySafety {
            requires_live_resource: true,
            requires_bounds_check: false,
            mutates_memory: false,
            cleanup_role: IrCleanupRole::UsesResource,
            unsafe_boundary: true,
        }),
        IrMemoryOperationKind::Store { .. } => Some(IrMemorySafety {
            requires_live_resource: true,
            requires_bounds_check: false,
            mutates_memory: true,
            cleanup_role: IrCleanupRole::UsesResource,
            unsafe_boundary: true,
        }),
        IrMemoryOperationKind::Deallocate { .. } => Some(IrMemorySafety {
            requires_live_resource: true,
            requires_bounds_check: false,
            mutates_memory: true,
            cleanup_role: IrCleanupRole::ReleasesResource,
            unsafe_boundary: true,
        }),
        IrMemoryOperationKind::BoundsCheck { .. } => Some(IrMemorySafety {
            requires_live_resource: false,
            requires_bounds_check: true,
            mutates_memory: false,
            cleanup_role: IrCleanupRole::CheckedAccess,
            unsafe_boundary: false,
        }),
        IrMemoryOperationKind::RegionCreate { .. } => Some(IrMemorySafety {
            requires_live_resource: false,
            requires_bounds_check: false,
            mutates_memory: true,
            cleanup_role: IrCleanupRole::CreatesResource,
            unsafe_boundary: false,
        }),
        IrMemoryOperationKind::RegionResize { .. } => Some(IrMemorySafety {
            requires_live_resource: true,
            requires_bounds_check: false,
            mutates_memory: true,
            cleanup_role: IrCleanupRole::UsesResource,
            unsafe_boundary: true,
        }),
        IrMemoryOperationKind::Copy { .. } => Some(IrMemorySafety {
            requires_live_resource: true,
            requires_bounds_check: false,
            mutates_memory: true,
            cleanup_role: IrCleanupRole::UsesResource,
            unsafe_boundary: true,
        }),
        IrMemoryOperationKind::Cleanup { .. } => Some(IrMemorySafety {
            requires_live_resource: true,
            requires_bounds_check: false,
            mutates_memory: true,
            cleanup_role: IrCleanupRole::ReleasesResource,
            unsafe_boundary: false,
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeModule {
    pub functions: Vec<BytecodeFunction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub structs: Vec<IrStructDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<IrEnumDef>,
    /// Trait implementations, carried through to the bytecode VM. Each maps a
    /// `(type, method)` to a lowered instruction-body function.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impls: Vec<BytecodeImplMethod>,
    /// Names declared as trait methods, carried through for dispatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trait_methods: Vec<String>,
}

/// One trait impl method in the bytecode module: the implementing type name, the
/// method name, and the instruction-body function.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeImplMethod {
    pub type_name: String,
    pub method_name: String,
    pub function: BytecodeFunction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeFunction {
    pub name: String,
    pub params: Vec<IrParam>,
    pub return_type: TypeRef,
    pub instructions: Vec<BytecodeInstruction>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BytecodeInstruction {
    Let {
        name: String,
        ty: TypeRef,
        value: BytecodeExpr,
        span: Span,
    },
    Assign {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        path: Vec<BytecodePlace>,
        op: AssignOp,
        value: BytecodeExpr,
        span: Span,
    },
    Return(Option<BytecodeExpr>),
    Break(Span),
    Continue(Span),
    Expr(BytecodeExpr),
    If {
        branches: Vec<BytecodeIfBranch>,
        else_body: Vec<BytecodeInstruction>,
        span: Span,
    },
    While {
        condition: BytecodeExpr,
        body: Vec<BytecodeInstruction>,
        span: Span,
    },
    For {
        name: String,
        start: BytecodeExpr,
        end: BytecodeExpr,
        step: Option<BytecodeExpr>,
        body: Vec<BytecodeInstruction>,
        span: Span,
    },
    Loop {
        body: Vec<BytecodeInstruction>,
        span: Span,
    },
    Throw {
        value: BytecodeExpr,
        span: Span,
    },
    Try {
        body: Vec<BytecodeInstruction>,
        catch_name: String,
        catch_body: Vec<BytecodeInstruction>,
        span: Span,
    },
    Match {
        scrutinee: BytecodeExpr,
        arms: Vec<BytecodeMatchArm>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeIfBranch {
    pub condition: BytecodeExpr,
    pub body: Vec<BytecodeInstruction>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeMatchArm {
    pub pattern: BytecodeMatchPattern,
    pub body: Vec<BytecodeInstruction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BytecodeMatchPattern {
    Variant {
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bindings: Vec<String>,
    },
    Wildcard,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeExpr {
    pub kind: BytecodeExprKind,
    pub ty: TypeRef,
    pub span: Span,
}

/// One hop of an assignment target in bytecode: a struct field or array index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BytecodePlace {
    Field(String),
    Index(BytecodeExpr),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BytecodeExprKind {
    Integer(i64),
    Float(f64),
    Bool(bool),
    String(String),
    /// A `'c'` char literal: exactly one Unicode scalar.
    Char(char),
    Array(Vec<BytecodeExpr>),
    Variable(String),
    Index {
        target: Box<BytecodeExpr>,
        index: Box<BytecodeExpr>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<BytecodeExpr>,
    },
    Binary {
        left: Box<BytecodeExpr>,
        op: BinaryOp,
        right: Box<BytecodeExpr>,
    },
    Call {
        name: String,
        args: Vec<BytecodeExpr>,
    },
    Field {
        target: Box<BytecodeExpr>,
        field: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeArtifact {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub metadata: BytecodeArtifactMetadata,
    pub entry: String,
    #[serde(default)]
    pub function_table: Vec<BytecodeFunctionSignature>,
    #[serde(default)]
    pub memory_operations: Vec<IrMemoryOperation>,
    pub module: BytecodeModule,
}

impl BytecodeArtifact {
    pub fn new(module: BytecodeModule) -> Self {
        let function_table = module
            .functions
            .iter()
            .map(BytecodeFunctionSignature::from_function)
            .collect();
        let memory_operations = analyze_bytecode_memory_operations(&module);
        Self {
            format: BYTECODE_ARTIFACT_FORMAT.to_string(),
            version: BYTECODE_ARTIFACT_VERSION,
            metadata: BytecodeArtifactMetadata::default(),
            entry: "main".to_string(),
            function_table,
            memory_operations,
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
            producer: format!("lullaby_ir {}", env!("CARGO_PKG_VERSION")),
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

pub fn analyze_bytecode_memory_operations(module: &BytecodeModule) -> Vec<IrMemoryOperation> {
    let mut operations = Vec::new();
    for function in &module.functions {
        collect_bytecode_memory_operations_from_block(
            &function.name,
            &function.instructions,
            &mut operations,
        );
    }
    operations
}

fn collect_bytecode_memory_operations_from_block(
    function: &str,
    instructions: &[BytecodeInstruction],
    operations: &mut Vec<IrMemoryOperation>,
) {
    for instruction in instructions {
        match instruction {
            BytecodeInstruction::Let { value, .. }
            | BytecodeInstruction::Assign { value, .. }
            | BytecodeInstruction::Expr(value) => {
                collect_bytecode_memory_operations_from_expr(function, value, operations);
            }
            BytecodeInstruction::Return(Some(value)) => {
                collect_bytecode_memory_operations_from_expr(function, value, operations);
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    collect_bytecode_memory_operations_from_expr(
                        function,
                        &branch.condition,
                        operations,
                    );
                    collect_bytecode_memory_operations_from_block(
                        function,
                        &branch.body,
                        operations,
                    );
                }
                collect_bytecode_memory_operations_from_block(function, else_body, operations);
            }
            BytecodeInstruction::While {
                condition, body, ..
            } => {
                collect_bytecode_memory_operations_from_expr(function, condition, operations);
                collect_bytecode_memory_operations_from_block(function, body, operations);
            }
            BytecodeInstruction::For {
                start,
                end,
                step,
                body,
                ..
            } => {
                collect_bytecode_memory_operations_from_expr(function, start, operations);
                collect_bytecode_memory_operations_from_expr(function, end, operations);
                if let Some(step) = step {
                    collect_bytecode_memory_operations_from_expr(function, step, operations);
                }
                collect_bytecode_memory_operations_from_block(function, body, operations);
            }
            BytecodeInstruction::Loop { body, .. } => {
                collect_bytecode_memory_operations_from_block(function, body, operations);
            }
            BytecodeInstruction::Throw { value, .. } => {
                collect_bytecode_memory_operations_from_expr(function, value, operations);
            }
            BytecodeInstruction::Try {
                body, catch_body, ..
            } => {
                collect_bytecode_memory_operations_from_block(function, body, operations);
                collect_bytecode_memory_operations_from_block(function, catch_body, operations);
            }
            BytecodeInstruction::Match {
                scrutinee, arms, ..
            } => {
                collect_bytecode_memory_operations_from_expr(function, scrutinee, operations);
                for arm in arms {
                    collect_bytecode_memory_operations_from_block(function, &arm.body, operations);
                }
            }
            BytecodeInstruction::Return(None)
            | BytecodeInstruction::Break(_)
            | BytecodeInstruction::Continue(_) => {}
        }
    }
}

fn collect_bytecode_memory_operations_from_expr(
    function: &str,
    expr: &BytecodeExpr,
    operations: &mut Vec<IrMemoryOperation>,
) {
    match &expr.kind {
        BytecodeExprKind::Array(values) => {
            for value in values {
                collect_bytecode_memory_operations_from_expr(function, value, operations);
            }
        }
        BytecodeExprKind::Field { target, .. } => {
            collect_bytecode_memory_operations_from_expr(function, target, operations);
        }
        BytecodeExprKind::Index { target, index } => {
            collect_bytecode_memory_operations_from_expr(function, target, operations);
            collect_bytecode_memory_operations_from_expr(function, index, operations);
            let kind = IrMemoryOperationKind::BoundsCheck {
                target_type: target.ty.clone(),
                index_type: index.ty.clone(),
            };
            if let Some(safety) = memory_safety_for_kind(&kind) {
                operations.push(IrMemoryOperation {
                    function: function.to_string(),
                    sequence: operations.len(),
                    span: expr.span,
                    kind,
                    safety,
                });
            }
        }
        BytecodeExprKind::Unary { expr, .. } => {
            collect_bytecode_memory_operations_from_expr(function, expr, operations);
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            collect_bytecode_memory_operations_from_expr(function, left, operations);
            collect_bytecode_memory_operations_from_expr(function, right, operations);
        }
        BytecodeExprKind::Call { name, args } => {
            for arg in args {
                collect_bytecode_memory_operations_from_expr(function, arg, operations);
            }
            if let Some(operation) = classify_bytecode_memory_call(function, name, args, expr) {
                operations.push(IrMemoryOperation {
                    sequence: operations.len(),
                    ..operation
                });
            }
        }
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_) => {}
    }
}

fn classify_bytecode_memory_call(
    function: &str,
    name: &str,
    args: &[BytecodeExpr],
    expr: &BytecodeExpr,
) -> Option<IrMemoryOperation> {
    let kind = match name {
        "alloc" => {
            let value = args.first()?;
            IrMemoryOperationKind::Allocate {
                value_type: value.ty.clone(),
                pointer_type: expr.ty.clone(),
            }
        }
        "load" => {
            let pointer = args.first()?;
            IrMemoryOperationKind::Load {
                pointer_type: pointer.ty.clone(),
                value_type: expr.ty.clone(),
            }
        }
        "store" => {
            let pointer = args.first()?;
            let value = args.get(1)?;
            IrMemoryOperationKind::Store {
                pointer_type: pointer.ty.clone(),
                value_type: value.ty.clone(),
            }
        }
        "dealloc" => {
            let pointer = args.first()?;
            IrMemoryOperationKind::Deallocate {
                pointer_type: pointer.ty.clone(),
            }
        }
        "rc_new" => {
            let value = args.first()?;
            IrMemoryOperationKind::Allocate {
                value_type: value.ty.clone(),
                pointer_type: expr.ty.clone(),
            }
        }
        "rc_clone" => {
            let handle = args.first()?;
            IrMemoryOperationKind::Copy {
                source_type: handle.ty.clone(),
                target_type: expr.ty.clone(),
            }
        }
        "rc_release" => {
            let handle = args.first()?;
            IrMemoryOperationKind::Cleanup {
                resource_type: handle.ty.clone(),
            }
        }
        "rc_get" | "ref_get" | "ptr_read" => {
            let reference = args.first()?;
            IrMemoryOperationKind::Load {
                pointer_type: reference.ty.clone(),
                value_type: expr.ty.clone(),
            }
        }
        "ptr_write" => {
            let pointer = args.first()?;
            let value = args.get(1)?;
            IrMemoryOperationKind::Store {
                pointer_type: pointer.ty.clone(),
                value_type: value.ty.clone(),
            }
        }
        "region_create" => IrMemoryOperationKind::RegionCreate {
            region_type: match args.first().map(|arg| &arg.kind) {
                Some(BytecodeExprKind::String(name)) => TypeRef::new(format!("region<{name}>")),
                _ => TypeRef::new("region"),
            },
        },
        _ => return None,
    };
    let safety = memory_safety_for_kind(&kind)?;

    Some(IrMemoryOperation {
        function: function.to_string(),
        sequence: 0,
        span: expr.span,
        kind,
        safety,
    })
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

        let mut params = HashSet::new();
        for param in &function.params {
            if !params.insert(param.name.as_str()) {
                return Err(BytecodeArtifactError::new(format!(
                    "duplicate bytecode parameter `{}` in function `{}`",
                    param.name, function.name
                )));
            }
        }

        validate_bytecode_instructions(&function.name, &function.instructions, 0)?;
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

    let expected_memory_operations = analyze_bytecode_memory_operations(&artifact.module);
    if artifact.memory_operations != expected_memory_operations {
        return Err(BytecodeArtifactError::new(
            "bytecode artifact memory_operations does not match module instructions",
        ));
    }

    let entry = artifact
        .module
        .functions
        .iter()
        .find(|function| function.name == artifact.entry)
        .expect("entry presence was validated");
    if !entry.params.is_empty() {
        return Err(BytecodeArtifactError::new(format!(
            "bytecode artifact entry `{}` must not require parameters",
            artifact.entry
        )));
    }

    Ok(())
}

fn validate_bytecode_instructions(
    function_name: &str,
    instructions: &[BytecodeInstruction],
    loop_depth: usize,
) -> Result<(), BytecodeArtifactError> {
    for instruction in instructions {
        match instruction {
            BytecodeInstruction::Break(_) => {
                if loop_depth == 0 {
                    return Err(BytecodeArtifactError::new(format!(
                        "bytecode instruction `break` outside loop in function `{function_name}`"
                    )));
                }
            }
            BytecodeInstruction::Continue(_) => {
                if loop_depth == 0 {
                    return Err(BytecodeArtifactError::new(format!(
                        "bytecode instruction `continue` outside loop in function `{function_name}`"
                    )));
                }
            }
            BytecodeInstruction::If {
                branches,
                else_body,
                ..
            } => {
                for branch in branches {
                    validate_bytecode_instructions(function_name, &branch.body, loop_depth)?;
                }
                validate_bytecode_instructions(function_name, else_body, loop_depth)?;
            }
            BytecodeInstruction::While { body, .. }
            | BytecodeInstruction::For { body, .. }
            | BytecodeInstruction::Loop { body, .. } => {
                validate_bytecode_instructions(function_name, body, loop_depth + 1)?;
            }
            BytecodeInstruction::Try {
                body, catch_body, ..
            } => {
                // `try`/`catch` is not a loop: keep the same loop depth.
                validate_bytecode_instructions(function_name, body, loop_depth)?;
                validate_bytecode_instructions(function_name, catch_body, loop_depth)?;
            }
            BytecodeInstruction::Match { arms, .. } => {
                // `match` is not a loop: keep the same loop depth for each arm.
                for arm in arms {
                    validate_bytecode_instructions(function_name, &arm.body, loop_depth)?;
                }
            }
            BytecodeInstruction::Let { .. }
            | BytecodeInstruction::Assign { .. }
            | BytecodeInstruction::Return(_)
            | BytecodeInstruction::Throw { .. }
            | BytecodeInstruction::Expr(_) => {}
        }
    }

    Ok(())
}

fn ir_function_to_bytecode(function: &IrFunction) -> BytecodeFunction {
    BytecodeFunction {
        name: function.name.clone(),
        params: function.params.clone(),
        return_type: function.return_type.clone(),
        instructions: lower_bytecode_block(&function.body),
        span: function.span,
    }
}

pub fn lower_to_bytecode(module: &IrModule) -> BytecodeModule {
    BytecodeModule {
        functions: module
            .functions
            .iter()
            .map(ir_function_to_bytecode)
            .collect(),
        structs: module.structs.clone(),
        enums: module.enums.clone(),
        impls: module
            .impls
            .iter()
            .map(|impl_method| BytecodeImplMethod {
                type_name: impl_method.type_name.clone(),
                method_name: impl_method.method_name.clone(),
                function: ir_function_to_bytecode(&impl_method.function),
            })
            .collect(),
        trait_methods: module.trait_methods.clone(),
    }
}

pub fn run_bytecode_main(module: &BytecodeModule) -> Result<Value, RuntimeError> {
    run_bytecode_main_with_args(module, Vec::new())
}

/// Run `main` on the bytecode VM (via the shared IR interpreter) with the
/// running program's CLI arguments, which the `args()` builtin exposes.
/// `run_bytecode_main` is the zero-argument wrapper.
pub fn run_bytecode_main_with_args(
    module: &BytecodeModule,
    args: Vec<String>,
) -> Result<Value, RuntimeError> {
    let ir = IrModule {
        structs: module.structs.clone(),
        enums: module.enums.clone(),
        functions: module
            .functions
            .iter()
            .map(bytecode_function_to_ir)
            .collect(),
        impls: module
            .impls
            .iter()
            .map(|impl_method| IrImplMethod {
                type_name: impl_method.type_name.clone(),
                method_name: impl_method.method_name.clone(),
                function: bytecode_function_to_ir(&impl_method.function),
            })
            .collect(),
        trait_methods: module.trait_methods.clone(),
    };
    run_main_with_args(&ir, args)
}

fn lower_bytecode_block(statements: &[IrStmt]) -> Vec<BytecodeInstruction> {
    statements.iter().map(lower_bytecode_instruction).collect()
}

fn lower_bytecode_instruction(statement: &IrStmt) -> BytecodeInstruction {
    match statement {
        IrStmt::Let {
            name,
            ty,
            value,
            span,
        } => BytecodeInstruction::Let {
            name: name.clone(),
            ty: ty.clone(),
            value: lower_bytecode_expr(value),
            span: *span,
        },
        IrStmt::Assign {
            name,
            path,
            op,
            value,
            span,
        } => BytecodeInstruction::Assign {
            name: name.clone(),
            path: path.iter().map(ir_place_to_bytecode).collect(),
            op: *op,
            value: lower_bytecode_expr(value),
            span: *span,
        },
        IrStmt::Return(expr) => BytecodeInstruction::Return(expr.as_ref().map(lower_bytecode_expr)),
        IrStmt::Break(span) => BytecodeInstruction::Break(*span),
        IrStmt::Continue(span) => BytecodeInstruction::Continue(*span),
        IrStmt::Expr(expr) => BytecodeInstruction::Expr(lower_bytecode_expr(expr)),
        IrStmt::If {
            branches,
            else_body,
            span,
        } => BytecodeInstruction::If {
            branches: branches
                .iter()
                .map(|branch| BytecodeIfBranch {
                    condition: lower_bytecode_expr(&branch.condition),
                    body: lower_bytecode_block(&branch.body),
                })
                .collect(),
            else_body: lower_bytecode_block(else_body),
            span: *span,
        },
        IrStmt::While {
            condition,
            body,
            span,
        } => BytecodeInstruction::While {
            condition: lower_bytecode_expr(condition),
            body: lower_bytecode_block(body),
            span: *span,
        },
        IrStmt::For {
            name,
            start,
            end,
            step,
            body,
            span,
        } => BytecodeInstruction::For {
            name: name.clone(),
            start: lower_bytecode_expr(start),
            end: lower_bytecode_expr(end),
            step: step.as_ref().map(lower_bytecode_expr),
            body: lower_bytecode_block(body),
            span: *span,
        },
        IrStmt::Loop { body, span } => BytecodeInstruction::Loop {
            body: lower_bytecode_block(body),
            span: *span,
        },
        IrStmt::Throw { value, span } => BytecodeInstruction::Throw {
            value: lower_bytecode_expr(value),
            span: *span,
        },
        IrStmt::Try {
            body,
            catch_name,
            catch_body,
            span,
        } => BytecodeInstruction::Try {
            body: lower_bytecode_block(body),
            catch_name: catch_name.clone(),
            catch_body: lower_bytecode_block(catch_body),
            span: *span,
        },
        IrStmt::Match {
            scrutinee,
            arms,
            span,
        } => BytecodeInstruction::Match {
            scrutinee: lower_bytecode_expr(scrutinee),
            arms: arms
                .iter()
                .map(|arm| BytecodeMatchArm {
                    pattern: ir_match_pattern_to_bytecode(&arm.pattern),
                    body: lower_bytecode_block(&arm.body),
                })
                .collect(),
            span: *span,
        },
    }
}

fn ir_match_pattern_to_bytecode(pattern: &IrMatchPattern) -> BytecodeMatchPattern {
    match pattern {
        IrMatchPattern::Variant { name, bindings } => BytecodeMatchPattern::Variant {
            name: name.clone(),
            bindings: bindings.clone(),
        },
        IrMatchPattern::Wildcard => BytecodeMatchPattern::Wildcard,
    }
}

fn ir_place_to_bytecode(place: &IrPlace) -> BytecodePlace {
    match place {
        IrPlace::Field(field) => BytecodePlace::Field(field.clone()),
        IrPlace::Index(expr) => BytecodePlace::Index(lower_bytecode_expr(expr)),
    }
}

fn lower_bytecode_expr(expr: &IrExpr) -> BytecodeExpr {
    let kind = match &expr.kind {
        IrExprKind::Integer(value) => BytecodeExprKind::Integer(*value),
        IrExprKind::Float(value) => BytecodeExprKind::Float(*value),
        IrExprKind::Bool(value) => BytecodeExprKind::Bool(*value),
        IrExprKind::String(value) => BytecodeExprKind::String(value.clone()),
        IrExprKind::Char(value) => BytecodeExprKind::Char(*value),
        IrExprKind::Array(values) => {
            BytecodeExprKind::Array(values.iter().map(lower_bytecode_expr).collect())
        }
        IrExprKind::Variable(name) => BytecodeExprKind::Variable(name.clone()),
        IrExprKind::Index { target, index } => BytecodeExprKind::Index {
            target: Box::new(lower_bytecode_expr(target)),
            index: Box::new(lower_bytecode_expr(index)),
        },
        IrExprKind::Field { target, field } => BytecodeExprKind::Field {
            target: Box::new(lower_bytecode_expr(target)),
            field: field.clone(),
        },
        IrExprKind::Unary { op, expr } => BytecodeExprKind::Unary {
            op: *op,
            expr: Box::new(lower_bytecode_expr(expr)),
        },
        IrExprKind::Binary { left, op, right } => BytecodeExprKind::Binary {
            left: Box::new(lower_bytecode_expr(left)),
            op: *op,
            right: Box::new(lower_bytecode_expr(right)),
        },
        IrExprKind::Call { name, args } => BytecodeExprKind::Call {
            name: name.clone(),
            args: args.iter().map(lower_bytecode_expr).collect(),
        },
    };

    BytecodeExpr {
        kind,
        ty: expr.ty.clone(),
        span: expr.span,
    }
}

fn bytecode_function_to_ir(function: &BytecodeFunction) -> IrFunction {
    IrFunction {
        name: function.name.clone(),
        params: function.params.clone(),
        return_type: function.return_type.clone(),
        body: bytecode_block_to_ir(&function.instructions),
        span: function.span,
    }
}

fn bytecode_block_to_ir(instructions: &[BytecodeInstruction]) -> Vec<IrStmt> {
    instructions
        .iter()
        .map(bytecode_instruction_to_ir)
        .collect()
}

fn bytecode_instruction_to_ir(instruction: &BytecodeInstruction) -> IrStmt {
    match instruction {
        BytecodeInstruction::Let {
            name,
            ty,
            value,
            span,
        } => IrStmt::Let {
            name: name.clone(),
            ty: ty.clone(),
            value: bytecode_expr_to_ir(value),
            span: *span,
        },
        BytecodeInstruction::Assign {
            name,
            path,
            op,
            value,
            span,
        } => IrStmt::Assign {
            name: name.clone(),
            path: path.iter().map(bytecode_place_to_ir).collect(),
            op: *op,
            value: bytecode_expr_to_ir(value),
            span: *span,
        },
        BytecodeInstruction::Return(expr) => IrStmt::Return(expr.as_ref().map(bytecode_expr_to_ir)),
        BytecodeInstruction::Break(span) => IrStmt::Break(*span),
        BytecodeInstruction::Continue(span) => IrStmt::Continue(*span),
        BytecodeInstruction::Expr(expr) => IrStmt::Expr(bytecode_expr_to_ir(expr)),
        BytecodeInstruction::If {
            branches,
            else_body,
            span,
        } => IrStmt::If {
            branches: branches
                .iter()
                .map(|branch| IrIfBranch {
                    condition: bytecode_expr_to_ir(&branch.condition),
                    body: bytecode_block_to_ir(&branch.body),
                })
                .collect(),
            else_body: bytecode_block_to_ir(else_body),
            span: *span,
        },
        BytecodeInstruction::While {
            condition,
            body,
            span,
        } => IrStmt::While {
            condition: bytecode_expr_to_ir(condition),
            body: bytecode_block_to_ir(body),
            span: *span,
        },
        BytecodeInstruction::For {
            name,
            start,
            end,
            step,
            body,
            span,
        } => IrStmt::For {
            name: name.clone(),
            start: bytecode_expr_to_ir(start),
            end: bytecode_expr_to_ir(end),
            step: step.as_ref().map(bytecode_expr_to_ir),
            body: bytecode_block_to_ir(body),
            span: *span,
        },
        BytecodeInstruction::Loop { body, span } => IrStmt::Loop {
            body: bytecode_block_to_ir(body),
            span: *span,
        },
        BytecodeInstruction::Throw { value, span } => IrStmt::Throw {
            value: bytecode_expr_to_ir(value),
            span: *span,
        },
        BytecodeInstruction::Try {
            body,
            catch_name,
            catch_body,
            span,
        } => IrStmt::Try {
            body: bytecode_block_to_ir(body),
            catch_name: catch_name.clone(),
            catch_body: bytecode_block_to_ir(catch_body),
            span: *span,
        },
        BytecodeInstruction::Match {
            scrutinee,
            arms,
            span,
        } => IrStmt::Match {
            scrutinee: bytecode_expr_to_ir(scrutinee),
            arms: arms
                .iter()
                .map(|arm| IrMatchArm {
                    pattern: bytecode_match_pattern_to_ir(&arm.pattern),
                    body: bytecode_block_to_ir(&arm.body),
                })
                .collect(),
            span: *span,
        },
    }
}

fn bytecode_match_pattern_to_ir(pattern: &BytecodeMatchPattern) -> IrMatchPattern {
    match pattern {
        BytecodeMatchPattern::Variant { name, bindings } => IrMatchPattern::Variant {
            name: name.clone(),
            bindings: bindings.clone(),
        },
        BytecodeMatchPattern::Wildcard => IrMatchPattern::Wildcard,
    }
}

fn bytecode_place_to_ir(place: &BytecodePlace) -> IrPlace {
    match place {
        BytecodePlace::Field(field) => IrPlace::Field(field.clone()),
        BytecodePlace::Index(expr) => IrPlace::Index(bytecode_expr_to_ir(expr)),
    }
}

fn bytecode_expr_to_ir(expr: &BytecodeExpr) -> IrExpr {
    let kind = match &expr.kind {
        BytecodeExprKind::Integer(value) => IrExprKind::Integer(*value),
        BytecodeExprKind::Float(value) => IrExprKind::Float(*value),
        BytecodeExprKind::Bool(value) => IrExprKind::Bool(*value),
        BytecodeExprKind::String(value) => IrExprKind::String(value.clone()),
        BytecodeExprKind::Char(value) => IrExprKind::Char(*value),
        BytecodeExprKind::Array(values) => {
            IrExprKind::Array(values.iter().map(bytecode_expr_to_ir).collect())
        }
        BytecodeExprKind::Variable(name) => IrExprKind::Variable(name.clone()),
        BytecodeExprKind::Index { target, index } => IrExprKind::Index {
            target: Box::new(bytecode_expr_to_ir(target)),
            index: Box::new(bytecode_expr_to_ir(index)),
        },
        BytecodeExprKind::Field { target, field } => IrExprKind::Field {
            target: Box::new(bytecode_expr_to_ir(target)),
            field: field.clone(),
        },
        BytecodeExprKind::Unary { op, expr } => IrExprKind::Unary {
            op: *op,
            expr: Box::new(bytecode_expr_to_ir(expr)),
        },
        BytecodeExprKind::Binary { left, op, right } => IrExprKind::Binary {
            left: Box::new(bytecode_expr_to_ir(left)),
            op: *op,
            right: Box::new(bytecode_expr_to_ir(right)),
        },
        BytecodeExprKind::Call { name, args } => IrExprKind::Call {
            name: name.clone(),
            args: args.iter().map(bytecode_expr_to_ir).collect(),
        },
    };

    IrExpr {
        kind,
        ty: expr.ty.clone(),
        span: expr.span,
    }
}

#[derive(Default)]
struct ConstantFolder {
    folded_expressions: usize,
}

impl ConstantFolder {
    fn fold_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
            structs: module.structs.clone(),
            enums: module.enums.clone(),
            impls: module.impls.clone(),
            trait_methods: module.trait_methods.clone(),
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
            | IrExprKind::Float(_)
            | IrExprKind::Bool(_)
            | IrExprKind::String(_)
            | IrExprKind::Char(_)
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
            BinaryOp::Divide,
            IrExprKind::Integer(0),
        ) => None,
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
struct CommonSubexpressionEliminator {
    eliminated_common_subexpressions: usize,
}

#[derive(Debug, Clone)]
struct AvailableExpr {
    variable: String,
    dependencies: HashSet<String>,
}

#[derive(Debug, Clone)]
struct ExprSignature {
    key: String,
    dependencies: HashSet<String>,
}

impl CommonSubexpressionEliminator {
    fn eliminate_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
            structs: module.structs.clone(),
            enums: module.enums.clone(),
            impls: module.impls.clone(),
            trait_methods: module.trait_methods.clone(),
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
            IrExprKind::Integer(_)
            | IrExprKind::Float(_)
            | IrExprKind::Bool(_)
            | IrExprKind::String(_)
            | IrExprKind::Char(_)
            | IrExprKind::Variable(_) => expr.clone(),
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
        IrExprKind::Variable(name) => {
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
        IrExprKind::Call { .. } => return None,
    };

    Some(ExprSignature { key, dependencies })
}

fn combine_signatures(
    prefix: &str,
    ty: &str,
    signatures: Vec<ExprSignature>,
) -> (String, HashSet<String>) {
    let mut dependencies = HashSet::new();
    let mut parts = Vec::new();
    for signature in signatures {
        dependencies.extend(signature.dependencies);
        parts.push(signature.key);
    }
    (format!("{prefix}:{ty}({})", parts.join(",")), dependencies)
}

#[derive(Default)]
struct LoopInvariantMover {
    hoisted_loop_invariants: usize,
    next_temp: usize,
    reserved_names: HashSet<String>,
}

impl LoopInvariantMover {
    fn move_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
            structs: module.structs.clone(),
            enums: module.enums.clone(),
            impls: module.impls.clone(),
            trait_methods: module.trait_methods.clone(),
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
            IrStmt::Let { .. }
            | IrStmt::Assign { .. }
            | IrStmt::Return(_)
            | IrStmt::Break(_)
            | IrStmt::Continue(_)
            | IrStmt::Throw { .. }
            | IrStmt::Try { .. }
            | IrStmt::Match { .. }
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
            IrStmt::While { body, .. } | IrStmt::Loop { body, .. } => {
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
            IrStmt::While { body, .. } | IrStmt::Loop { body, .. } => {
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
        IrExprKind::Variable(name) => {
            let mut dependencies = HashSet::new();
            dependencies.insert(name.clone());
            (format!("var:{name}:{}", expr.ty.name), dependencies)
        }
        IrExprKind::Unary { op, expr: inner } => {
            let inner = loop_invariant_expr_signature(inner)?;
            combine_signatures(&format!("unary:{op:?}"), &expr.ty.name, vec![inner])
        }
        IrExprKind::Binary { left, op, right } => {
            if matches!(op, BinaryOp::Divide) {
                return None;
            }
            let left = loop_invariant_expr_signature(left)?;
            let right = loop_invariant_expr_signature(right)?;
            combine_signatures(&format!("binary:{op:?}"), &expr.ty.name, vec![left, right])
        }
        IrExprKind::Array(_)
        | IrExprKind::Index { .. }
        | IrExprKind::Field { .. }
        | IrExprKind::Call { .. } => return None,
    };

    Some(ExprSignature { key, dependencies })
}

fn is_hoist_worthwhile(expr: &IrExpr) -> bool {
    matches!(
        expr.kind,
        IrExprKind::Unary { .. } | IrExprKind::Binary { .. }
    )
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

#[derive(Default)]
struct CopyPropagator {
    propagated_copies: usize,
}

impl CopyPropagator {
    fn propagate_module(&mut self, module: &IrModule) -> IrModule {
        IrModule {
            structs: module.structs.clone(),
            enums: module.enums.clone(),
            impls: module.impls.clone(),
            trait_methods: module.trait_methods.clone(),
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
            IrExprKind::Integer(_)
            | IrExprKind::Float(_)
            | IrExprKind::Bool(_)
            | IrExprKind::String(_)
            | IrExprKind::Char(_) => expr.clone(),
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

fn expr_requires_optimizer_barrier(expr: &IrExpr) -> bool {
    match &expr.kind {
        IrExprKind::Call { .. } => true,
        IrExprKind::Array(values) => values.iter().any(expr_requires_optimizer_barrier),
        IrExprKind::Index { .. } => true,
        // Field access is pure; only its target can require a barrier.
        IrExprKind::Field { target, .. } => expr_requires_optimizer_barrier(target),
        IrExprKind::Unary { expr, .. } => expr_requires_optimizer_barrier(expr),
        IrExprKind::Binary { left, right, .. } => {
            expr_requires_optimizer_barrier(left) || expr_requires_optimizer_barrier(right)
        }
        IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
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
            structs: module.structs.clone(),
            enums: module.enums.clone(),
            impls: module.impls.clone(),
            trait_methods: module.trait_methods.clone(),
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

fn is_unconditional_terminator(statement: &IrStmt) -> bool {
    matches!(
        statement,
        IrStmt::Return(_) | IrStmt::Break(_) | IrStmt::Continue(_)
    )
}

struct IrRuntime<'a> {
    /// The whole IR module, borrowed so a builtin can spawn sibling interpreters
    /// over the same shared `&IrModule` (used by `parallel_map`'s scoped threads).
    module: &'a IrModule,
    /// An owned share of the same module, handed by `.clone()` to detached
    /// threads created by `spawn` so they can build their own interpreter over
    /// `&*arc` and outlive the `spawn` call. Separate handle, not self-referential.
    module_arc: Arc<IrModule>,
    functions: HashMap<&'a str, &'a IrFunction>,
    /// The running program's CLI arguments, exposed by the `args()` builtin.
    program_args: Vec<String>,
    structs: HashMap<&'a str, Vec<String>>,
    /// Enum variant name -> owning enum name. Variant names are globally unique.
    variants: HashMap<&'a str, &'a str>,
    heap: Vec<Option<Value>>,
    refcounts: HashMap<usize, usize>,
    /// Per-runtime table of open network sockets, mirroring the AST interpreter.
    /// A `Value::Socket(i)` indexes this vector; closing a socket clears its slot.
    sockets: Vec<Option<SocketResource>>,
    call_stack: Vec<TraceFrame>,
    /// Trait-method dispatch table: `(receiver type name, method name)` -> impl
    /// function. Built once from every `impl` in the module.
    impl_methods: HashMap<(String, String), &'a IrFunction>,
    /// Names that are trait methods; a call to one dispatches via `impl_methods`.
    trait_method_names: std::collections::HashSet<String>,
}

impl<'a> IrRuntime<'a> {
    /// Build an interpreter over the borrowed module `module` while retaining an
    /// owned `Arc<IrModule>` (`module_arc`) that points at the same data, used
    /// only to hand a share to detached `spawn`ed threads. The caller passes both
    /// handles (e.g. `IrRuntime::new(&arc, Arc::clone(&arc))`).
    fn new(module: &'a IrModule, module_arc: Arc<IrModule>) -> Result<Self, RuntimeError> {
        let functions = module
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect::<HashMap<_, _>>();

        if !functions.contains_key("main") {
            return Err(RuntimeError::new("L0422", "missing `main` function"));
        }

        let structs = module
            .structs
            .iter()
            .map(|declaration| {
                (
                    declaration.name.as_str(),
                    declaration
                        .fields
                        .iter()
                        .map(|(field, _)| field.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        let mut variants = HashMap::new();
        // Built-in `option`/`result` generic-enum variants, registered like user
        // variants so construction and `match` reuse the same `Value::Enum` path.
        variants.insert("some", "option");
        variants.insert("none", "option");
        variants.insert("ok", "result");
        variants.insert("err", "result");
        for declaration in &module.enums {
            for variant in &declaration.variants {
                variants.insert(variant.name.as_str(), declaration.name.as_str());
            }
        }

        // Build the trait-method dispatch table from all impls in the module.
        let mut impl_methods = HashMap::new();
        for impl_method in &module.impls {
            impl_methods.insert(
                (
                    impl_method.type_name.clone(),
                    impl_method.method_name.clone(),
                ),
                &impl_method.function,
            );
        }
        let trait_method_names = module.trait_methods.iter().cloned().collect();

        Ok(Self {
            module,
            module_arc,
            functions,
            program_args: Vec::new(),
            structs,
            variants,
            heap: Vec::new(),
            refcounts: HashMap::new(),
            sockets: Vec::new(),
            call_stack: Vec::new(),
            impl_methods,
            trait_method_names,
        })
    }

    fn call_function(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // Trait-method dispatch: select the impl by the receiver's runtime type.
        if self.trait_method_names.contains(name) {
            let receiver_type = args.first().map(value_type_name).ok_or_else(|| {
                RuntimeError::new(
                    "L0401",
                    format!("trait method `{name}` called without a receiver"),
                )
            })?;
            let method = *self
                .impl_methods
                .get(&(receiver_type.clone(), name.to_string()))
                .ok_or_else(|| {
                    RuntimeError::new(
                        "L0401",
                        format!("type `{receiver_type}` does not implement trait method `{name}`"),
                    )
                })?;
            return self.invoke_function(method, args);
        }
        if let Some(enum_name) = self.variants.get(name) {
            return Ok(Value::Enum {
                enum_name: enum_name.to_string(),
                variant: name.to_string(),
                payload: args,
            });
        }
        if let Some(field_names) = self.structs.get(name) {
            return Ok(Value::Struct {
                name: name.to_string(),
                fields: field_names.iter().cloned().zip(args).collect(),
            });
        }
        match name {
            "alloc" => self.builtin_alloc(args),
            "load" => self.builtin_load(args),
            "store" => self.builtin_store(args),
            "dealloc" => self.builtin_dealloc(args),
            "read_file" => self.builtin_read_file(args),
            "write_file" => self.builtin_write_file(args),
            "append_file" => self.builtin_append_file(args),
            "file_exists" => self.builtin_file_exists(args),
            "read_lines" => self.builtin_read_lines(args),
            "read_bytes" => self.builtin_read_bytes(args),
            "write_bytes" => self.builtin_write_bytes(args),
            "file_size" => self.builtin_file_size(args),
            "is_file" => self.builtin_is_file(args),
            "is_dir" => self.builtin_is_dir(args),
            "list_dir" => self.builtin_list_dir(args),
            "make_dir" => self.builtin_make_dir(args),
            "remove_file" => self.builtin_remove_file(args),
            "remove_dir" => self.builtin_remove_dir(args),
            "sys_status" => self.builtin_sys_status(args),
            "sys_output" => self.builtin_sys_output(args),
            "print" => self.builtin_print("print", args, false),
            "println" => self.builtin_print("println", args, true),
            "warn" => self.builtin_warn(args),
            "wasm_log" => self.builtin_wasm_log(args),
            "flush" => self.builtin_flush(args),
            "assert" => Self::builtin_assert(args),
            "to_string" => Self::builtin_to_string(args),
            "char_code" => Self::builtin_char_code(args),
            "char_from" => Self::builtin_char_from(args),
            "byte" => Self::builtin_byte(args),
            "byte_val" => Self::builtin_byte_val(args),
            "len" => Self::builtin_len(args),
            "list_new" => Self::builtin_list_new(args),
            "push" => Self::builtin_push(args),
            "get" => Self::builtin_get(args),
            "set" => Self::builtin_set(args),
            "pop" => Self::builtin_pop(args),
            "map_new" => Self::builtin_map_new(args),
            "map_set" => Self::builtin_map_set(args),
            "map_get" => Self::builtin_map_get(args),
            "map_has" => Self::builtin_map_has(args),
            "map_len" => Self::builtin_map_len(args),
            "map_del" => Self::builtin_map_del(args),
            "substring" => Self::builtin_substring(args),
            "find" => Self::builtin_find(args),
            "contains" => Self::builtin_contains(args),
            "starts_with" => Self::builtin_starts_with(args),
            "ends_with" => Self::builtin_ends_with(args),
            "repeat" => Self::builtin_repeat(args),
            "split" => Self::builtin_split(args),
            "join" => Self::builtin_join(args),
            "trim" => Self::builtin_trim(args),
            "replace" => Self::builtin_replace(args),
            "upper" => Self::builtin_upper(args),
            "lower" => Self::builtin_lower(args),
            "abs" => Self::builtin_abs(args),
            "min" => Self::builtin_min(args),
            "max" => Self::builtin_max(args),
            "pow" => Self::builtin_pow(args),
            "sqrt" => Self::builtin_sqrt(args),
            "sin" => Self::builtin_unary_f64("sin", args, f64::sin),
            "cos" => Self::builtin_unary_f64("cos", args, f64::cos),
            "tan" => Self::builtin_unary_f64("tan", args, f64::tan),
            "atan" => Self::builtin_unary_f64("atan", args, f64::atan),
            "exp" => Self::builtin_unary_f64("exp", args, f64::exp),
            "ln" => Self::builtin_unary_f64("ln", args, f64::ln),
            "log10" => Self::builtin_unary_f64("log10", args, f64::log10),
            "atan2" => Self::builtin_atan2(args),
            "floor" => Self::builtin_floor(args),
            "ceil" => Self::builtin_ceil(args),
            "round" => Self::builtin_round(args),
            "rc_new" => self.builtin_rc_new(args),
            "rc_clone" => self.builtin_rc_clone(args),
            "rc_release" => self.builtin_rc_release(args),
            "rc_get" | "ref_get" | "ptr_read" => self.builtin_ref_get(name, args),
            "rc_borrow" => self.builtin_rc_borrow(args),
            "ptr_write" => self.builtin_store(args),
            "env" => Self::builtin_env(args),
            "args" => self.builtin_args(args),
            "parallel_map" => self.builtin_parallel_map(args),
            "chan_new" => Self::builtin_chan_new(args),
            "send" => Self::builtin_send(args),
            "recv" => Self::builtin_recv(args),
            "try_recv" => Self::builtin_try_recv(args),
            "spawn" => self.builtin_spawn(args),
            "task_join" => Self::builtin_task_join(args),
            "mutex_new" => Self::builtin_mutex_new(args),
            "mutex_get" => Self::builtin_mutex_get(args),
            "mutex_set" => Self::builtin_mutex_set(args),
            "mutex_add" => Self::builtin_mutex_add(args),
            "tcp_connect" => self.builtin_tcp_connect(args),
            "tcp_listen" => self.builtin_tcp_listen(args),
            "tcp_accept" => self.builtin_tcp_accept(args),
            "tcp_read" => self.builtin_tcp_read(args),
            "tcp_write" => self.builtin_tcp_write(args),
            "tcp_shutdown" => self.builtin_tcp_shutdown(args),
            "tcp_close" => self.builtin_socket_close(args),
            "udp_bind" => self.builtin_udp_bind(args),
            "udp_send_to" => self.builtin_udp_send_to(args),
            "udp_recv" => self.builtin_udp_recv(args),
            "http_get" => Self::builtin_http_get(args),
            "http_post" => Self::builtin_http_post(args),
            // A region-creation marker has no runtime effect in the current
            // analysis-only region model.
            "region_create" => Ok(Value::Void),
            _ => {
                let function = *self.functions.get(name).ok_or_else(|| {
                    RuntimeError::new("L0401", format!("unknown function `{name}`"))
                })?;
                self.invoke_function(function, args)
            }
        }
    }

    /// Execute a user function (or trait impl method) with the given argument
    /// values, threading the traceback and translating loop-control escape.
    fn invoke_function(
        &mut self,
        function: &'a IrFunction,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        if function.params.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "function `{}` expects {} arguments but got {}",
                    function.name,
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
                "L0410",
                "loop control escaped function body",
            )),
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
                name,
                path,
                op,
                value,
                ..
            } => {
                let rhs = self.eval_expr(value, env)?;
                if path.is_empty() {
                    let new = match op {
                        AssignOp::Replace => rhs,
                        _ => apply_compound(env.get(name)?, op, rhs)?,
                    };
                    env.assign(name, new)?;
                } else {
                    let resolved = self.resolve_places(path, env)?;
                    let mut root = env.get(name)?;
                    let new = match op {
                        AssignOp::Replace => rhs,
                        _ => apply_compound(get_place(&root, &resolved)?, op, rhs)?,
                    };
                    set_place(&mut root, &resolved, new)?;
                    env.assign(name, root)?;
                }
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
                    return Err(RuntimeError::new("L0411", "for loop step cannot be zero"));
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
            IrStmt::Throw { value, .. } => {
                let message = self.eval_expr(value, env)?.as_string()?;
                Err(RuntimeError::new("L0420", message))
            }
            IrStmt::Try {
                body,
                catch_name,
                catch_body,
                ..
            } => match self.eval_scoped_block(body, env) {
                Err(error) if error.code == "L0420" => {
                    env.push_scope();
                    env.define(catch_name.clone(), Value::String(error.message.clone()));
                    let result = self.eval_block(catch_body, env);
                    env.pop_scope();
                    result
                }
                other => other,
            },
            IrStmt::Match {
                scrutinee, arms, ..
            } => self.eval_match(scrutinee, arms, env),
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

    /// Evaluate an IR `match` identically to the AST runtime: select the arm
    /// whose variant matches the scrutinee's enum value (or the `_` wildcard),
    /// bind payloads to arm-scoped locals, and evaluate the arm block.
    fn eval_match(
        &mut self,
        scrutinee: &IrExpr,
        arms: &[IrMatchArm],
        env: &mut Env,
    ) -> Result<Control, RuntimeError> {
        let value = self.eval_expr(scrutinee, env)?;
        let Value::Enum {
            variant, payload, ..
        } = value
        else {
            return Err(RuntimeError::new(
                "L0383",
                "match scrutinee did not evaluate to an enum value",
            ));
        };
        for arm in arms {
            match &arm.pattern {
                IrMatchPattern::Wildcard => {
                    return self.eval_scoped_block(&arm.body, env);
                }
                IrMatchPattern::Variant { name, bindings } if name == &variant => {
                    env.push_scope();
                    for (binding, value) in bindings.iter().zip(payload.iter()) {
                        env.define(binding.clone(), value.clone());
                    }
                    let result = self.eval_block(&arm.body, env);
                    env.pop_scope();
                    return result;
                }
                IrMatchPattern::Variant { .. } => {}
            }
        }
        Err(RuntimeError::new(
            "L0384",
            format!("no match arm covered variant `{variant}`"),
        ))
    }

    fn resolve_places(
        &mut self,
        path: &[IrPlace],
        env: &Env,
    ) -> Result<Vec<ResolvedPlace>, RuntimeError> {
        path.iter()
            .map(|place| match place {
                IrPlace::Field(field) => Ok(ResolvedPlace::Field(field.clone())),
                IrPlace::Index(expr) => {
                    Ok(ResolvedPlace::Index(self.eval_expr(expr, env)?.as_i64()?))
                }
            })
            .collect()
    }

    fn eval_expr(&mut self, expr: &IrExpr, env: &Env) -> Result<Value, RuntimeError> {
        let result = match &expr.kind {
            IrExprKind::Integer(value) => Ok(Value::I64(*value)),
            IrExprKind::Float(value) => Ok(Value::F64(*value)),
            IrExprKind::Bool(value) => Ok(Value::Bool(*value)),
            IrExprKind::String(value) => Ok(Value::String(value.clone())),
            IrExprKind::Char(value) => Ok(Value::Char(*value)),
            IrExprKind::Array(values) => values
                .iter()
                .map(|value| self.eval_expr(value, env))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            IrExprKind::Variable(name) => match env.get(name) {
                Ok(value) => Ok(value),
                Err(error) => {
                    // A bare name that is not a local but is a known enum variant
                    // constructs a unit variant.
                    if let Some(enum_name) = self.variants.get(name.as_str()) {
                        Ok(Value::Enum {
                            enum_name: enum_name.to_string(),
                            variant: name.clone(),
                            payload: Vec::new(),
                        })
                    } else if self.functions.contains_key(name.as_str()) {
                        // A bare name that is a known top-level function evaluates
                        // to a first-class function value.
                        Ok(Value::Func(name.clone()))
                    } else {
                        Err(error)
                    }
                }
            },
            IrExprKind::Index { target, index } => {
                let target = self.eval_expr(target, env)?;
                let index = self.eval_expr(index, env)?.as_i64()?;
                let Value::Array(values) = target else {
                    return Err(RuntimeError::new("L0412", "index target is not an array"));
                };
                if index < 0 {
                    return Err(RuntimeError::new(
                        "L0413",
                        format!("array index `{index}` is out of bounds"),
                    ));
                }
                values.get(index as usize).cloned().ok_or_else(|| {
                    RuntimeError::new("L0413", format!("array index `{index}` is out of bounds"))
                })
            }
            IrExprKind::Field { target, field } => match self.eval_expr(target, env)? {
                Value::Struct { fields, .. } => fields
                    .into_iter()
                    .find(|(name, _)| name == field)
                    .map(|(_, value)| value)
                    .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`"))),
                _ => Err(RuntimeError::new(
                    "L0371",
                    format!("cannot access field `{field}` on non-struct value"),
                )),
            },
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
                // A call name that is a local holding a function value dispatches
                // through that value: invoke the referenced top-level function.
                if let Ok(Value::Func(target)) = env.get(name) {
                    self.call_function(&target, values)
                } else {
                    self.call_function(name, values)
                }
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
        if let (Value::F64(l), Value::F64(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F64(l + r),
                BinaryOp::Subtract => Value::F64(l - r),
                BinaryOp::Multiply => Value::F64(l * r),
                BinaryOp::Divide => Value::F64(l / r),
                BinaryOp::Equal => Value::Bool(l == r),
                BinaryOp::NotEqual => Value::Bool(l != r),
                BinaryOp::Less => Value::Bool(l < r),
                BinaryOp::LessEqual => Value::Bool(l <= r),
                BinaryOp::Greater => Value::Bool(l > r),
                BinaryOp::GreaterEqual => Value::Bool(l >= r),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
            });
        }
        match op {
            BinaryOp::Add if matches!((&left, &right), (Value::String(_), Value::String(_))) => {
                Ok(Value::String(left.as_string()? + &right.as_string()?))
            }
            BinaryOp::Add => Ok(Value::I64(left.as_i64()? + right.as_i64()?)),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()? - right.as_i64()?)),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()? * right.as_i64()?)),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "division by zero"))
                } else {
                    Ok(Value::I64(left.as_i64()? / divisor))
                }
            }
            BinaryOp::Equal => Ok(Value::Bool(left == right)),
            BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
            // Char ordering compares by Unicode code point; byte ordering is
            // numeric. Both fall through to i64 ordering otherwise.
            BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
                if scalar_order_keys(&left, &right).is_some() =>
            {
                let (l, r) = scalar_order_keys(&left, &right)
                    .expect("guarded by the match arm condition above");
                Ok(Value::Bool(match op {
                    BinaryOp::Less => l < r,
                    BinaryOp::LessEqual => l <= r,
                    BinaryOp::Greater => l > r,
                    BinaryOp::GreaterEqual => l >= r,
                    _ => unreachable!("guarded to ordering operators"),
                }))
            }
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
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    fn builtin_store(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("store", 2, args.len()))?;
        let slot = ptr.as_ptr()?;
        let Some(target) = self.heap.get_mut(slot) else {
            return Err(RuntimeError::new(
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if target.is_none() {
            return Err(RuntimeError::new(
                "L0406",
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
                "L0406",
                format!("invalid pointer `{slot}`"),
            ));
        };
        if value.take().is_none() {
            return Err(RuntimeError::new(
                "L0406",
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
                RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
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
                RuntimeError::resource("L0415", format!("failed to write `{path}`: {error}"))
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
                RuntimeError::resource("L0415", format!("failed to append `{path}`: {error}"))
            })
    }

    fn builtin_file_exists(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_exists", 1, args.len()))?;
        Ok(Value::Bool(fs::metadata(path.as_string()?).is_ok()))
    }

    fn builtin_read_lines(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_lines", 1, args.len()))?;
        let path = path.as_string()?;
        let contents = fs::read_to_string(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::Array(
            contents
                .lines()
                .map(|line| Value::String(line.to_string()))
                .collect(),
        ))
    }

    fn builtin_read_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("read_bytes", 1, args.len()))?;
        let path = path.as_string()?;
        let bytes = fs::read(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::Array(bytes.into_iter().map(Value::Byte).collect()))
    }

    fn builtin_write_bytes(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path, data]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("write_bytes", 2, args.len()))?;
        let path = path.as_string()?;
        let bytes = Self::value_to_bytes("write_bytes", data)?;
        fs::write(&path, bytes)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to write `{path}`: {error}"))
            })
    }

    /// Convert a `list<byte>` (`Value::Array` of `Value::Byte`) to raw bytes,
    /// erroring on a non-array or a non-byte element.
    fn value_to_bytes(name: &str, value: Value) -> Result<Vec<u8>, RuntimeError> {
        let Value::Array(values) = value else {
            return Err(RuntimeError::new(
                "L0418",
                format!("{name} expects a `list<byte>` value"),
            ));
        };
        values
            .into_iter()
            .map(|element| match element {
                Value::Byte(b) => Ok(b),
                other => Err(RuntimeError::new(
                    "L0418",
                    format!("{name} expects `list<byte>` but found `{other}`"),
                )),
            })
            .collect()
    }

    fn builtin_file_size(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("file_size", 1, args.len()))?;
        let path = path.as_string()?;
        let metadata = fs::metadata(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        Ok(Value::I64(metadata.len() as i64))
    }

    fn builtin_is_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_file", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_file())
                .unwrap_or(false),
        ))
    }

    fn builtin_is_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("is_dir", 1, args.len()))?;
        Ok(Value::Bool(
            fs::metadata(path.as_string()?)
                .map(|m| m.is_dir())
                .unwrap_or(false),
        ))
    }

    fn builtin_list_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_dir", 1, args.len()))?;
        let path = path.as_string()?;
        let entries = fs::read_dir(&path).map_err(|error| {
            RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
        })?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                RuntimeError::resource("L0414", format!("failed to read `{path}`: {error}"))
            })?;
            names.push(Value::String(
                entry.file_name().to_string_lossy().to_string(),
            ));
        }
        Ok(Value::Array(names))
    }

    fn builtin_make_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("make_dir", 1, args.len()))?;
        let path = path.as_string()?;
        fs::create_dir_all(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to create `{path}`: {error}"))
            })
    }

    fn builtin_remove_file(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("remove_file", 1, args.len()))?;
        let path = path.as_string()?;
        fs::remove_file(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to remove `{path}`: {error}"))
            })
    }

    fn builtin_remove_dir(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [path]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("remove_dir", 1, args.len()))?;
        let path = path.as_string()?;
        fs::remove_dir(&path)
            .map(|()| Value::Void)
            .map_err(|error| {
                RuntimeError::resource("L0415", format!("failed to remove `{path}`: {error}"))
            })
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
                RuntimeError::resource("L0416", format!("failed to run `{program}`: {error}"))
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
                RuntimeError::resource("L0416", format!("failed to run `{program}`: {error}"))
            })?;
        Ok(Value::String(
            String::from_utf8_lossy(&output.stdout).to_string(),
        ))
    }

    fn builtin_print(
        &self,
        name: &'static str,
        args: Vec<Value>,
        newline: bool,
    ) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let result = if newline {
            writeln!(handle, "{text}")
        } else {
            write!(handle, "{text}")
        };
        result.map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    fn builtin_warn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("warn", 1, args.len()))?;
        let text = text.as_string()?;
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        writeln!(handle, "{text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stderr: {error}"))
        })?;
        Ok(Value::Void)
    }

    fn builtin_flush(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        if !args.is_empty() {
            return Err(Self::wrong_arity("flush", 0, args.len()));
        }
        std::io::stdout().flush().map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to flush stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `wasm_log(x i64) -> void`: the host log builtin. On the WASM backend it
    /// lowers to a `call` of the imported `env.log_i64`; on the interpreters it
    /// prints the value as a stdout line, kept at parity with the AST runtime so
    /// all backends observe the same side effect.
    fn builtin_wasm_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wasm_log", 1, args.len()))?;
        let value = value.as_i64()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{value}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `assert(cond bool) -> void`: raises the same catchable user-error (code
    /// `L0420`) a `throw` produces when `cond` is false; returns void otherwise.
    /// Kept at parity with the AST runtime's `builtin_assert`.
    fn builtin_assert(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("assert", 1, args.len()))?;
        if value.as_bool()? {
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new("L0420", "assertion failed"))
        }
    }

    fn builtin_to_string(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_string", 1, args.len()))?;
        match value {
            Value::I64(_)
            | Value::F64(_)
            | Value::Bool(_)
            | Value::String(_)
            | Value::Char(_)
            | Value::Byte(_) => Ok(Value::String(value.to_string())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("to_string cannot convert `{other}`"),
            )),
        }
    }

    /// `char_code(c char) -> i64`: the char's Unicode scalar value.
    fn builtin_char_code(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("char_code", 1, args.len()))?;
        match value {
            Value::Char(c) => Ok(Value::I64(c as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("char_code expects a char but got `{other}`"),
            )),
        }
    }

    /// `char_from(i i64) -> char`: the char for a Unicode scalar value; a runtime
    /// error when `i` is not a valid Unicode scalar.
    fn builtin_char_from(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("char_from", 1, args.len()))?;
        let code = expect_i64("char_from", value)?;
        u32::try_from(code)
            .ok()
            .and_then(char::from_u32)
            .map(Value::Char)
            .ok_or_else(|| {
                RuntimeError::new(
                    "L0417",
                    format!("char_from got `{code}`, which is not a valid Unicode scalar value"),
                )
            })
    }

    /// `byte(i i64) -> byte`: an 8-bit unsigned value; a runtime error outside 0-255.
    fn builtin_byte(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte", 1, args.len()))?;
        let number = expect_i64("byte", value)?;
        u8::try_from(number).map(Value::Byte).map_err(|_| {
            RuntimeError::new(
                "L0417",
                format!("byte got `{number}`, which is outside the 0-255 range"),
            )
        })
    }

    /// `byte_val(b byte) -> i64`: the numeric value of a byte.
    fn builtin_byte_val(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte_val", 1, args.len()))?;
        match value {
            Value::Byte(b) => Ok(Value::I64(b as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("byte_val expects a byte but got `{other}`"),
            )),
        }
    }

    fn builtin_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("len", 1, args.len()))?;
        match value {
            Value::Array(values) => Ok(Value::I64(values.len() as i64)),
            Value::String(text) => Ok(Value::I64(text.chars().count() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("len expects a string or array but got `{other}`"),
            )),
        }
    }

    /// `list_new() -> list<T>`: a fresh empty list, represented as an array.
    fn builtin_list_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_new", 0, args.len()))?;
        Ok(Value::Array(Vec::new()))
    }

    /// `env(name string) -> option<string>`: `some(value)` when the environment
    /// variable is set, `none` otherwise.
    fn builtin_env(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [name]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("env", 1, args.len()))?;
        let name = expect_string("env", name)?;
        Ok(option_value(std::env::var(&name).ok().map(Value::String)))
    }

    /// `args() -> list<string>`: the running program's CLI arguments (an empty
    /// list when none were passed), represented as an array of strings.
    fn builtin_args(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("args", 0, args.len()))?;
        Ok(Value::Array(
            self.program_args
                .iter()
                .cloned()
                .map(Value::String)
                .collect(),
        ))
    }

    /// `parallel_map(f fn(i64) -> i64, args list<i64>) -> list<i64>`: evaluate
    /// `f(arg)` for every element of `args` concurrently on separate OS threads,
    /// returning the results in the SAME order as `args`. Each thread builds a
    /// fresh sibling interpreter over the shared `&IrModule` (heaps are
    /// per-thread, so there is no shared mutable state and no locking). Output
    /// order follows input order, so results are fully deterministic.
    fn builtin_parallel_map(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [callee, elements]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parallel_map", 2, args.len()))?;
        let func_name = match callee {
            Value::Func(name) => name,
            other => {
                return Err(RuntimeError::new(
                    "L0417",
                    format!("parallel_map expects a function but got `{other}`"),
                ));
            }
        };
        let arg_values = expect_list("parallel_map", elements)?;

        let module = self.module;
        let module_arc = &self.module_arc;
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = arg_values
                .iter()
                .map(|value| {
                    let name = func_name.clone();
                    let value = value.clone();
                    let arc = Arc::clone(module_arc);
                    scope.spawn(move || {
                        let mut runtime = IrRuntime::new(module, arc)?;
                        runtime.call_function(&name, vec![value])
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(RuntimeError::new(
                            "L0401",
                            "parallel_map worker thread panicked",
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })?;

        Ok(Value::Array(results))
    }

    /// `chan_new() -> Chan`: create an unbounded `i64` message-passing channel.
    fn builtin_chan_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chan_new", 0, args.len()))?;
        Ok(new_chan())
    }

    /// `send(ch Chan, v i64) -> void`: enqueue `v` (never blocks; unbounded).
    fn builtin_send(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("send", 2, args.len()))?;
        let chan = expect_chan("send", chan)?;
        let value = expect_i64("send", value)?;
        chan.sender
            .send(Value::I64(value))
            .map_err(|_| RuntimeError::new("L0401", "send on a channel with no live receiver"))?;
        Ok(Value::Void)
    }

    /// `recv(ch Chan) -> i64`: dequeue, blocking until a value is available.
    fn builtin_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("recv", 1, args.len()))?;
        let chan = expect_chan("recv", chan)?;
        let receiver = chan
            .receiver
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "recv on a poisoned channel"))?;
        receiver
            .recv()
            .map_err(|_| RuntimeError::new("L0401", "recv on a closed, empty channel"))
    }

    /// `try_recv(ch Chan) -> option<i64>`: non-blocking; `some(v)` or `none`.
    fn builtin_try_recv(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [chan]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("try_recv", 1, args.len()))?;
        let chan = expect_chan("try_recv", chan)?;
        let receiver = chan
            .receiver
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "try_recv on a poisoned channel"))?;
        Ok(option_value(receiver.try_recv().ok()))
    }

    /// `spawn(f fn(Chan, i64) -> void, ch Chan, v i64) -> Task`: run `f(ch, v)` on
    /// a detached OS thread that owns a share of the module (an `Arc<IrModule>`
    /// clone) and builds its own interpreter over `&*arc`, then returns a one-shot
    /// `Task` handle so the thread is `task_join`ed exactly once.
    fn builtin_spawn(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [callee, chan, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("spawn", 3, args.len()))?;
        let func_name = match callee {
            Value::Func(name) => name,
            other => {
                return Err(RuntimeError::new(
                    "L0417",
                    format!("spawn expects a function but got `{other}`"),
                ));
            }
        };
        let chan = expect_chan("spawn", chan)?;
        let value = expect_i64("spawn", value)?;
        let arc = Arc::clone(&self.module_arc);
        let handle = std::thread::spawn(move || {
            let mut runtime = IrRuntime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, vec![Value::Chan(chan), Value::I64(value)])
        });
        Ok(Value::Task(Task {
            handle: Arc::new(std::sync::Mutex::new(Some(handle))),
        }))
    }

    /// `task_join(t Task) -> void`: wait for the spawned thread; a second
    /// `task_join` on an already-joined handle is a harmless no-op.
    fn builtin_task_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [task]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("task_join", 1, args.len()))?;
        let task = expect_task("task_join", task)?;
        join_task(&task)
    }

    /// `mutex_new(v i64) -> Mutex`: a shared mutex over one `i64`.
    fn builtin_mutex_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_new", 1, args.len()))?;
        let value = expect_i64("mutex_new", value)?;
        Ok(Value::Mutex(SharedMutex {
            cell: Arc::new(std::sync::Mutex::new(value)),
        }))
    }

    /// `mutex_get(m Mutex) -> i64`: lock, read, unlock.
    fn builtin_mutex_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_get", 1, args.len()))?;
        let mutex = expect_mutex("mutex_get", mutex)?;
        let guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_get on a poisoned mutex"))?;
        Ok(Value::I64(*guard))
    }

    /// `mutex_set(m Mutex, v i64) -> void`: lock, write, unlock.
    fn builtin_mutex_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_set", 2, args.len()))?;
        let mutex = expect_mutex("mutex_set", mutex)?;
        let value = expect_i64("mutex_set", value)?;
        let mut guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_set on a poisoned mutex"))?;
        *guard = value;
        Ok(Value::Void)
    }

    /// `mutex_add(m Mutex, delta i64) -> i64`: lock, `v += delta`, return the new
    /// value, unlock — an atomic read-modify-write so worker threads accumulate
    /// safely.
    fn builtin_mutex_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [mutex, delta]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mutex_add", 2, args.len()))?;
        let mutex = expect_mutex("mutex_add", mutex)?;
        let delta = expect_i64("mutex_add", delta)?;
        let mut guard = mutex
            .cell
            .lock()
            .map_err(|_| RuntimeError::new("L0401", "mutex_add on a poisoned mutex"))?;
        *guard = guard.wrapping_add(delta);
        Ok(Value::I64(*guard))
    }

    /// Push a freshly opened socket resource into the handle table, returning its
    /// index wrapped as a `Value::Socket`.
    fn register_socket(&mut self, resource: SocketResource) -> Value {
        self.sockets.push(Some(resource));
        Value::Socket(self.sockets.len() - 1)
    }

    /// Resolve a socket handle argument to its live slot index, reporting a
    /// wrong-argument-type error for a non-socket value and a stale-handle error
    /// for a closed or invalid slot.
    fn socket_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
        let Value::Socket(handle) = value else {
            return Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a Socket but got `{value}`"),
            ));
        };
        match self.sockets.get(*handle) {
            Some(Some(_)) => Ok(*handle),
            _ => Err(RuntimeError::new(
                "L0406",
                format!("{name} received a closed or invalid socket `{handle}`"),
            )),
        }
    }

    /// `tcp_connect(host string, port i64) -> result<Socket, string>`.
    fn builtin_tcp_connect(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_connect", 2, args.len()))?;
        let host = expect_string("tcp_connect", host)?;
        let port = expect_i64("tcp_connect", port)?;
        match TcpStream::connect((host.as_str(), port as u16)) {
            Ok(stream) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_listen(host string, port i64) -> result<Socket, string>`.
    fn builtin_tcp_listen(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_listen", 2, args.len()))?;
        let host = expect_string("tcp_listen", host)?;
        let port = expect_i64("tcp_listen", port)?;
        match TcpListener::bind((host.as_str(), port as u16)) {
            Ok(listener) => {
                let socket = self.register_socket(SocketResource::Listener(listener));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_accept(listener Socket) -> result<Socket, string>`: block for a
    /// connection and register the accepted stream as a new handle.
    fn builtin_tcp_accept(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [listener]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_accept", 1, args.len()))?;
        let slot = self.socket_slot("tcp_accept", &listener)?;
        let accepted = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.accept(),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_accept requires a listening socket".to_string(),
                ))));
            }
        };
        match accepted {
            Ok((stream, _addr)) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(socket)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_read(conn Socket) -> result<string, string>`: read up to 4096 bytes
    /// and return them as a lossy UTF-8 string (empty on clean EOF).
    fn builtin_tcp_read(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [conn]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_read", 1, args.len()))?;
        let slot = self.socket_slot("tcp_read", &conn)?;
        let mut buffer = [0u8; 4096];
        let read = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => stream.read(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_read requires a connected stream socket".to_string(),
                ))));
            }
        };
        match read {
            Ok(count) => Ok(result_value(Ok(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_write(conn Socket, data string) -> result<i64, string>`: write the
    /// string's bytes and return the number of bytes written.
    fn builtin_tcp_write(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [conn, data]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_write", 2, args.len()))?;
        let slot = self.socket_slot("tcp_write", &conn)?;
        let data = expect_string("tcp_write", data)?;
        let bytes = data.as_bytes();
        let written = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => {
                // Write the FULL buffer (short writes are possible) and flush.
                stream.write_all(bytes).and_then(|()| stream.flush())
            }
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_write requires a connected stream socket".to_string(),
                ))));
            }
        };
        match written {
            Ok(()) => Ok(result_value(Ok(Value::I64(bytes.len() as i64)))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `tcp_shutdown(conn Socket) -> void`: gracefully shut down the write half
    /// of the connection (`Shutdown::Write`), signaling EOF to the peer so any
    /// buffered response is delivered before the socket is dropped.
    fn builtin_tcp_shutdown(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::net::Shutdown;
        let [socket]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_shutdown", 1, args.len()))?;
        if let Value::Socket(handle) = socket {
            if let Some(Some(SocketResource::Stream(stream))) = self.sockets.get(handle) {
                let _ = stream.shutdown(Shutdown::Write);
            }
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new(
                "L0417",
                format!("tcp_shutdown expects a Socket but got `{socket}`"),
            ))
        }
    }

    /// `tcp_close(conn Socket) -> void`: drop the handle, freeing its table slot.
    /// Closing an already-closed handle is a no-op.
    fn builtin_socket_close(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [socket]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_close", 1, args.len()))?;
        if let Value::Socket(handle) = socket {
            if let Some(slot) = self.sockets.get_mut(handle) {
                *slot = None;
            }
            Ok(Value::Void)
        } else {
            Err(RuntimeError::new(
                "L0417",
                format!("tcp_close expects a Socket but got `{socket}`"),
            ))
        }
    }

    /// `udp_bind(host string, port i64) -> result<Socket, string>`.
    fn builtin_udp_bind(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [host, port]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_bind", 2, args.len()))?;
        let host = expect_string("udp_bind", host)?;
        let port = expect_i64("udp_bind", port)?;
        match UdpSocket::bind((host.as_str(), port as u16)) {
            Ok(socket) => {
                let handle = self.register_socket(SocketResource::Udp(socket));
                Ok(result_value(Ok(handle)))
            }
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_send_to(sock Socket, data string, host string, port i64)
    /// -> result<i64, string>`: send one datagram, returning the byte count.
    fn builtin_udp_send_to(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock, data, host, port]: [Value; 4] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_send_to", 4, args.len()))?;
        let slot = self.socket_slot("udp_send_to", &sock)?;
        let data = expect_string("udp_send_to", data)?;
        let host = expect_string("udp_send_to", host)?;
        let port = expect_i64("udp_send_to", port)?;
        let sent = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => {
                socket.send_to(data.as_bytes(), (host.as_str(), port as u16))
            }
            _ => {
                return Ok(result_value(Err(Value::String(
                    "udp_send_to requires a UDP socket".to_string(),
                ))));
            }
        };
        match sent {
            Ok(count) => Ok(result_value(Ok(Value::I64(count as i64)))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `udp_recv(sock Socket) -> result<string, string>`: receive one datagram,
    /// dropping the sender address, and return it as a lossy UTF-8 string.
    fn builtin_udp_recv(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_recv", 1, args.len()))?;
        let slot = self.socket_slot("udp_recv", &sock)?;
        let mut buffer = [0u8; 4096];
        let received = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => socket.recv_from(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "udp_recv requires a UDP socket".to_string(),
                ))));
            }
        };
        match received {
            Ok((count, _addr)) => Ok(result_value(Ok(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))),
            Err(error) => Ok(net_err(&error)),
        }
    }

    /// `http_get(url string) -> result<string, string>`: perform an HTTP/1.1
    /// GET and return the response body on a 2xx/3xx response, or `err(message)`
    /// on a connection/parse/HTTP error.
    fn builtin_http_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_get", 1, args.len()))?;
        let url = expect_string("http_get", url)?;
        Ok(http_exchange("GET", &url, None))
    }

    /// `http_post(url string, body string) -> result<string, string>`: perform
    /// an HTTP/1.1 POST with a `text/plain` body and return the response body on
    /// a 2xx/3xx response, or `err(message)` on a connection/parse/HTTP error.
    fn builtin_http_post(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [url, body]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("http_post", 2, args.len()))?;
        let url = expect_string("http_post", url)?;
        let body = expect_string("http_post", body)?;
        Ok(http_exchange("POST", &url, Some(&body)))
    }

    /// `push(l, x) -> list<T>`: a new list with `x` appended.
    fn builtin_push(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("push", 2, args.len()))?;
        let mut values = expect_list("push", list)?;
        values.push(value);
        Ok(Value::Array(values))
    }

    /// `get(l, i) -> T`: bounds-checked element read.
    fn builtin_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, index]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("get", 2, args.len()))?;
        let values = expect_list("get", list)?;
        let index = expect_i64("get", index)?;
        if index < 0 || index as usize >= values.len() {
            return Err(RuntimeError::new(
                "L0413",
                format!("list index `{index}` is out of bounds"),
            ));
        }
        Ok(values[index as usize].clone())
    }

    /// `set(l, i, x) -> list<T>`: a new list with index `i` replaced by `x`.
    fn builtin_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, index, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("set", 3, args.len()))?;
        let mut values = expect_list("set", list)?;
        let index = expect_i64("set", index)?;
        if index < 0 || index as usize >= values.len() {
            return Err(RuntimeError::new(
                "L0413",
                format!("list index `{index}` is out of bounds"),
            ));
        }
        values[index as usize] = value;
        Ok(Value::Array(values))
    }

    /// `pop(l) -> list<T>`: a new list without the last element.
    fn builtin_pop(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("pop", 1, args.len()))?;
        let mut values = expect_list("pop", list)?;
        if values.pop().is_none() {
            return Err(RuntimeError::new("L0413", "cannot pop from an empty list"));
        }
        Ok(Value::Array(values))
    }

    /// `map_new() -> map<K, V>`: a fresh empty map.
    fn builtin_map_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_new", 0, args.len()))?;
        Ok(Value::Map(Vec::new()))
    }

    /// `map_set(m, k, v) -> map<K, V>`: a new map with `k` mapped to `v`.
    fn builtin_map_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_set", 3, args.len()))?;
        let mut entries = expect_map("map_set", map)?;
        match entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key, value)),
        }
        Ok(Value::Map(entries))
    }

    /// `map_get(m, k) -> option<V>`: `some(v)` if present, else `none`.
    fn builtin_map_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_get", 2, args.len()))?;
        let entries = expect_map("map_get", map)?;
        let found = entries.into_iter().find(|(k, _)| *k == key).map(|(_, v)| v);
        Ok(option_value(found))
    }

    /// `map_has(m, k) -> bool`.
    fn builtin_map_has(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_has", 2, args.len()))?;
        let entries = expect_map("map_has", map)?;
        Ok(Value::Bool(entries.iter().any(|(k, _)| *k == key)))
    }

    /// `map_len(m) -> i64`.
    fn builtin_map_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_len", 1, args.len()))?;
        let entries = expect_map("map_len", map)?;
        Ok(Value::I64(entries.len() as i64))
    }

    /// `map_del(m, k) -> map<K, V>`: a new map without key `k`.
    fn builtin_map_del(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_del", 2, args.len()))?;
        let mut entries = expect_map("map_del", map)?;
        entries.retain(|(k, _)| *k != key);
        Ok(Value::Map(entries))
    }

    fn builtin_substring(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, start, end]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("substring", 3, args.len()))?;
        let text = expect_string("substring", text)?;
        let start = expect_i64("substring", start)?;
        let end = expect_i64("substring", end)?;
        let chars: Vec<char> = text.chars().collect();
        let count = chars.len() as i64;
        if start < 0 || end < 0 || start > end || end > count {
            return Err(RuntimeError::new(
                "L0413",
                format!(
                    "substring range [{start}, {end}) is out of bounds for a string of length {count}"
                ),
            ));
        }
        let slice: String = chars[start as usize..end as usize].iter().collect();
        Ok(Value::String(slice))
    }

    fn builtin_find(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("find", 2, args.len()))?;
        let text = expect_string("find", text)?;
        let needle = expect_string("find", needle)?;
        Ok(Value::I64(char_find(&text, &needle)))
    }

    fn builtin_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, needle]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("contains", 2, args.len()))?;
        let text = expect_string("contains", text)?;
        let needle = expect_string("contains", needle)?;
        Ok(Value::Bool(text.contains(&needle)))
    }

    fn builtin_starts_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, prefix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("starts_with", 2, args.len()))?;
        let text = expect_string("starts_with", text)?;
        let prefix = expect_string("starts_with", prefix)?;
        Ok(Value::Bool(text.starts_with(&prefix)))
    }

    fn builtin_ends_with(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, suffix]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ends_with", 2, args.len()))?;
        let text = expect_string("ends_with", text)?;
        let suffix = expect_string("ends_with", suffix)?;
        Ok(Value::Bool(text.ends_with(&suffix)))
    }

    fn builtin_repeat(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, count]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("repeat", 2, args.len()))?;
        let text = expect_string("repeat", text)?;
        let count = expect_i64("repeat", count)?;
        let result = if count <= 0 {
            String::new()
        } else {
            text.repeat(count as usize)
        };
        Ok(Value::String(result))
    }

    fn builtin_split(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, sep]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("split", 2, args.len()))?;
        let text = expect_string("split", text)?;
        let sep = expect_string("split", sep)?;
        if sep.is_empty() {
            return Err(RuntimeError::new(
                "L0417",
                "split requires a non-empty separator".to_string(),
            ));
        }
        let parts = text
            .split(sep.as_str())
            .map(|part| Value::String(part.to_string()))
            .collect();
        Ok(Value::Array(parts))
    }

    fn builtin_join(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [parts, sep]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("join", 2, args.len()))?;
        let Value::Array(parts) = parts else {
            return Err(RuntimeError::new(
                "L0417",
                format!("join expects an array of strings but got `{parts}`"),
            ));
        };
        let sep = expect_string("join", sep)?;
        let mut pieces = Vec::with_capacity(parts.len());
        for part in parts {
            pieces.push(expect_string("join", part)?);
        }
        Ok(Value::String(pieces.join(sep.as_str())))
    }

    fn builtin_trim(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("trim", 1, args.len()))?;
        let text = expect_string("trim", text)?;
        Ok(Value::String(
            text.trim_matches(|c: char| c.is_ascii_whitespace())
                .to_string(),
        ))
    }

    fn builtin_replace(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, from, to]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("replace", 3, args.len()))?;
        let text = expect_string("replace", text)?;
        let from = expect_string("replace", from)?;
        let to = expect_string("replace", to)?;
        if from.is_empty() {
            return Err(RuntimeError::new(
                "L0417",
                "replace requires a non-empty `from` pattern".to_string(),
            ));
        }
        Ok(Value::String(text.replace(from.as_str(), to.as_str())))
    }

    fn builtin_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("upper", 1, args.len()))?;
        let text = expect_string("upper", text)?;
        Ok(Value::String(text.to_uppercase()))
    }

    fn builtin_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("lower", 1, args.len()))?;
        let text = expect_string("lower", text)?;
        Ok(Value::String(text.to_lowercase()))
    }

    fn builtin_abs(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("abs", 1, args.len()))?;
        match value {
            Value::I64(n) => Ok(Value::I64(n.abs())),
            Value::F64(n) => Ok(Value::F64(n.abs())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("abs expects an i64 or f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [left, right]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("min", 2, args.len()))?;
        match (left, right) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(a.min(b))),
            (Value::F64(a), Value::F64(b)) => Ok(Value::F64(a.min(b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("min expects two matching i64 or f64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    fn builtin_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [left, right]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("max", 2, args.len()))?;
        match (left, right) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(a.max(b))),
            (Value::F64(a), Value::F64(b)) => Ok(Value::F64(a.max(b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("max expects two matching i64 or f64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    fn builtin_pow(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [base, exp]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("pow", 2, args.len()))?;
        match (base, exp) {
            (Value::I64(b), Value::I64(e)) => {
                if e < 0 {
                    return Err(RuntimeError::new(
                        "L0417",
                        format!("pow expects a non-negative integer exponent but got `{e}`"),
                    ));
                }
                Ok(Value::I64(b.pow(e as u32)))
            }
            (Value::F64(b), Value::F64(e)) => Ok(Value::F64(b.powf(e))),
            (b, e) => Err(RuntimeError::new(
                "L0417",
                format!("pow expects two matching i64 or f64 values but got `{b}` and `{e}`"),
            )),
        }
    }

    fn builtin_sqrt(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sqrt", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.sqrt())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("sqrt expects an f64 but got `{other}`"),
            )),
        }
    }

    /// Shared implementation for the unary `f64 -> f64` math builtins, matching
    /// the AST runtime so every backend produces bit-identical results.
    fn builtin_unary_f64(
        name: &str,
        args: Vec<Value>,
        op: fn(f64) -> f64,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(op(n))),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{name} expects an f64 but got `{other}`"),
            )),
        }
    }

    /// `atan2(y, x)`: the angle of the vector `(x, y)` in radians.
    fn builtin_atan2(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [y, x]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atan2", 2, args.len()))?;
        match (y, x) {
            (Value::F64(y), Value::F64(x)) => Ok(Value::F64(y.atan2(x))),
            (y, x) => Err(RuntimeError::new(
                "L0417",
                format!("atan2 expects two f64 values but got `{y}` and `{x}`"),
            )),
        }
    }

    fn builtin_floor(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("floor", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.floor())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("floor expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_ceil(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ceil", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.ceil())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("ceil expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_round(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("round", 1, args.len()))?;
        match value {
            Value::F64(n) => Ok(Value::F64(n.round())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("round expects an f64 but got `{other}`"),
            )),
        }
    }

    fn builtin_rc_new(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_new", 1, args.len()))?;
        self.heap.push(Some(value));
        let slot = self.heap.len() - 1;
        self.refcounts.insert(slot, 1);
        Ok(Value::Ptr(slot))
    }

    fn builtin_rc_clone(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_clone", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        match self.refcounts.get_mut(&slot) {
            Some(count) => {
                *count += 1;
                Ok(Value::Ptr(slot))
            }
            None => Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            )),
        }
    }

    fn builtin_rc_release(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_release", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        match self.refcounts.get_mut(&slot) {
            Some(count) => {
                *count -= 1;
                if *count == 0 {
                    self.refcounts.remove(&slot);
                    if let Some(target) = self.heap.get_mut(slot) {
                        *target = None;
                    }
                }
                Ok(Value::Void)
            }
            None => Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            )),
        }
    }

    fn builtin_rc_borrow(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rc_borrow", 1, args.len()))?;
        let slot = handle.as_ptr()?;
        if self.refcounts.contains_key(&slot) {
            Ok(Value::Ptr(slot))
        } else {
            Err(RuntimeError::new(
                "L0406",
                format!("invalid reference-counted handle `{slot}`"),
            ))
        }
    }

    fn builtin_ref_get(&self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        let slot = handle.as_ptr()?;
        self.heap
            .get(slot)
            .and_then(|value| value.clone())
            .ok_or_else(|| RuntimeError::new("L0406", format!("invalid pointer `{slot}`")))
    }

    fn wrong_arity(name: &str, expected: usize, actual: usize) -> RuntimeError {
        RuntimeError::new(
            "L0405",
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
        | IrStmt::Loop { span, .. }
        | IrStmt::Throw { span, .. }
        | IrStmt::Try { span, .. }
        | IrStmt::Match { span, .. } => *span,
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
            "L0403",
            format!("unknown variable `{name}`"),
        ))
    }

    fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .cloned()
            .ok_or_else(|| RuntimeError::new("L0403", format!("unknown variable `{name}`")))
    }
}

struct Lowerer<'a> {
    program: &'a Program,
    signatures: &'a HashMap<String, Signature>,
    /// Declared return type of the function currently being lowered. Threaded so
    /// `return EXPR` and a function's final expression can supply the expected
    /// type that `none`/`ok`/`err` need. Set at the start of each function.
    current_return_type: std::cell::RefCell<TypeRef>,
}

impl<'a> Lowerer<'a> {
    fn new(program: &'a Program, signatures: &'a HashMap<String, Signature>) -> Self {
        Self {
            program,
            signatures,
            current_return_type: std::cell::RefCell::new(TypeRef::new("void")),
        }
    }

    fn lower_program(&self) -> Result<IrModule, IrLoweringError> {
        let functions = self
            .program
            .functions
            .iter()
            .map(|function| self.lower_function(function))
            .collect::<Result<Vec<_>, _>>()?;
        let structs = self
            .program
            .structs
            .iter()
            .map(|declaration| IrStructDef {
                name: declaration.name.clone(),
                fields: declaration
                    .fields
                    .iter()
                    .map(|field| (field.name.clone(), field.ty.clone()))
                    .collect(),
            })
            .collect();
        let enums = self
            .program
            .enums
            .iter()
            .map(|declaration| IrEnumDef {
                name: declaration.name.clone(),
                variants: declaration
                    .variants
                    .iter()
                    .map(|variant| IrEnumVariant {
                        name: variant.name.clone(),
                        payload: variant.payload.clone(),
                    })
                    .collect(),
            })
            .collect();
        // Lower every trait impl method to an IR function, keyed by
        // `(type_name, method_name)` for runtime dispatch.
        let mut impls = Vec::new();
        for decl in &self.program.impls {
            for method in &decl.methods {
                impls.push(IrImplMethod {
                    type_name: decl.type_name.clone(),
                    method_name: method.name.clone(),
                    function: self.lower_function(method)?,
                });
            }
        }
        let trait_methods = self
            .program
            .traits
            .iter()
            .flat_map(|decl| decl.methods.iter().map(|method| method.name.clone()))
            .collect();
        Ok(IrModule {
            functions,
            structs,
            enums,
            impls,
            trait_methods,
        })
    }

    /// The declared type of `field` on struct `struct_name`, if any.
    fn struct_field_type(&self, struct_name: &str, field: &str) -> Option<TypeRef> {
        self.program
            .structs
            .iter()
            .find(|declaration| declaration.name == struct_name)
            .and_then(|declaration| declaration.fields.iter().find(|f| f.name == field))
            .map(|f| f.ty.clone())
    }

    fn is_struct(&self, name: &str) -> bool {
        self.program.structs.iter().any(|s| s.name == name)
    }

    /// If `name` is a trait method, its declared signature (from the trait).
    fn trait_method_sig(&self, name: &str) -> Option<&'a lullaby_parser::MethodSig> {
        self.program
            .traits
            .iter()
            .flat_map(|decl| decl.methods.iter())
            .find(|method| method.name == name)
    }

    /// If `name` is a known enum variant, the owning enum's name.
    fn enum_of_variant(&self, name: &str) -> Option<String> {
        self.program.enums.iter().find_map(|declaration| {
            declaration
                .variants
                .iter()
                .any(|variant| variant.name == name)
                .then(|| declaration.name.clone())
        })
    }

    fn lower_function(&self, function: &Function) -> Result<IrFunction, IrLoweringError> {
        let mut scope = function
            .params
            .iter()
            .map(|param| (param.name.clone(), param.ty.clone()))
            .collect::<HashMap<_, _>>();
        // Record the return type so `return` and the final expression can supply
        // the expected type to `none`/`ok`/`err`.
        *self.current_return_type.borrow_mut() = function.return_type.clone();
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
            body: self.lower_function_body(&function.body, &mut scope)?,
            span: function.span,
        })
    }

    fn lower_block(
        &self,
        statements: &[Stmt],
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<Vec<IrStmt>, IrLoweringError> {
        let mut lowered = Vec::with_capacity(statements.len());
        for statement in statements {
            match statement {
                // `unsafe` is a transparent compile-time gate; inline its body
                // into the enclosing block so no IR node is needed for it.
                Stmt::Unsafe { body, .. } => {
                    lowered.extend(self.lower_block(body, scope)?);
                }
                other => lowered.push(self.lower_statement(other, scope)?),
            }
        }
        Ok(lowered)
    }

    /// Lower a function body. A trailing bare expression statement is lowered
    /// against the function's return type so a final `some/none/ok/err` gets its
    /// context-directed type, mirroring the semantic final-expression rule.
    fn lower_function_body(
        &self,
        statements: &[Stmt],
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<Vec<IrStmt>, IrLoweringError> {
        let last_index = statements.len().checked_sub(1);
        let return_type = self.current_return_type.borrow().clone();
        let mut lowered = Vec::with_capacity(statements.len());
        for (index, statement) in statements.iter().enumerate() {
            match statement {
                Stmt::Unsafe { body, .. } => {
                    lowered.extend(self.lower_block(body, scope)?);
                }
                Stmt::Expr(expr)
                    if Some(index) == last_index
                        && !return_type.is_void()
                        && !matches!(expr.kind, ExprKind::Match { .. }) =>
                {
                    let lowered_expr = self.lower_expr_expected(expr, Some(&return_type), scope)?;
                    lowered.push(IrStmt::Expr(lowered_expr));
                }
                other => lowered.push(self.lower_statement(other, scope)?),
            }
        }
        Ok(lowered)
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
                let value = self.lower_expr_expected(value, ty.as_ref(), scope)?;
                let binding_type = ty.clone().unwrap_or_else(|| value.ty.clone());
                scope.insert(name.clone(), binding_type.clone());
                Ok(IrStmt::Let {
                    name: name.clone(),
                    ty: binding_type,
                    value,
                    span: *span,
                })
            }
            Stmt::Assign {
                name,
                path,
                op,
                value,
                span,
            } => {
                let path = path
                    .iter()
                    .map(|place| self.lower_place(place, scope))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrStmt::Assign {
                    name: name.clone(),
                    path,
                    op: *op,
                    value: self.lower_expr(value, scope)?,
                    span: *span,
                })
            }
            Stmt::Return(expr) => {
                let return_type = self.current_return_type.borrow().clone();
                Ok(IrStmt::Return(
                    expr.as_ref()
                        .map(|expr| self.lower_expr_expected(expr, Some(&return_type), scope))
                        .transpose()?,
                ))
            }
            Stmt::Break(span) => Ok(IrStmt::Break(*span)),
            Stmt::Continue(span) => Ok(IrStmt::Continue(*span)),
            // A `match` reaches lowering wrapped in a `Stmt::Expr`; lower it to a
            // dedicated `IrStmt::Match` so it threads through the IR and bytecode
            // backends and optimizers exactly like `try`.
            Stmt::Expr(Expr {
                kind: ExprKind::Match { scrutinee, arms },
                span,
            }) => self.lower_match(scrutinee, arms, *span, scope),
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
            // A region declaration lowers to a `region_create` marker call so
            // its metadata flows through memory analysis as a RegionCreate op.
            Stmt::Region(decl) => {
                let mut args = vec![
                    IrExpr {
                        kind: IrExprKind::String(decl.name.clone()),
                        ty: TypeRef::new("string"),
                        span: decl.span,
                    },
                    IrExpr {
                        kind: IrExprKind::Integer(decl.size),
                        ty: TypeRef::new("i64"),
                        span: decl.span,
                    },
                ];
                if let Some(align) = decl.align {
                    args.push(IrExpr {
                        kind: IrExprKind::Integer(align),
                        ty: TypeRef::new("i64"),
                        span: decl.span,
                    });
                }
                Ok(IrStmt::Expr(IrExpr {
                    kind: IrExprKind::Call {
                        name: "region_create".to_string(),
                        args,
                    },
                    ty: TypeRef::new("void"),
                    span: decl.span,
                }))
            }
            Stmt::Throw { value, span } => Ok(IrStmt::Throw {
                value: self.lower_expr(value, scope)?,
                span: *span,
            }),
            Stmt::Try {
                body,
                catch_name,
                catch_body,
                span,
            } => {
                let mut try_scope = scope.clone();
                let body = self.lower_block(body, &mut try_scope)?;
                let mut catch_scope = scope.clone();
                catch_scope.insert(catch_name.clone(), TypeRef::new("string"));
                let catch_body = self.lower_block(catch_body, &mut catch_scope)?;
                Ok(IrStmt::Try {
                    body,
                    catch_name: catch_name.clone(),
                    catch_body,
                    span: *span,
                })
            }
            // `unsafe` blocks are flattened in `lower_block`; reaching here means
            // a lone unsafe statement, which we lower transparently by inlining.
            Stmt::Unsafe { body, span } => {
                let mut lowered = self.lower_block(body, scope)?;
                match lowered.len() {
                    1 => Ok(lowered.remove(0)),
                    // An empty or multi-statement unsafe body cannot collapse to
                    // one IR statement; represent it as a always-false guard-free
                    // block via an `if false` is overkill, so surface it as a
                    // lowering error to be handled by the flattening path.
                    _ => Err(IrLoweringError::new(
                        "unsafe block must be lowered by lower_block".to_string(),
                        Some(*span),
                    )),
                }
            }
        }
    }

    /// Lower a `match` to an `IrStmt::Match`. Each arm's payload bindings are
    /// typed by the owning variant's declared payload types and inserted into a
    /// per-arm scope, so arm bodies lower against the right binding types.
    fn lower_match(
        &self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        scope: &mut HashMap<String, TypeRef>,
    ) -> Result<IrStmt, IrLoweringError> {
        let scrutinee = self.lower_expr(scrutinee, scope)?;
        let scrutinee_ty = scrutinee.ty.clone();
        let mut lowered_arms = Vec::with_capacity(arms.len());
        for arm in arms {
            let mut arm_scope = scope.clone();
            let pattern = match &arm.pattern {
                MatchPattern::Wildcard => IrMatchPattern::Wildcard,
                MatchPattern::Variant { name, bindings } => {
                    let payload = self.variant_binding_types(&scrutinee_ty, name);
                    for (binding, ty) in bindings.iter().zip(payload.iter()) {
                        arm_scope.insert(binding.clone(), ty.clone());
                    }
                    IrMatchPattern::Variant {
                        name: name.clone(),
                        bindings: bindings.clone(),
                    }
                }
            };
            let body = self.lower_block(&arm.body, &mut arm_scope)?;
            lowered_arms.push(IrMatchArm { pattern, body });
        }
        Ok(IrStmt::Match {
            scrutinee,
            arms: lowered_arms,
            span,
        })
    }

    /// The payload binding types of `variant` for a scrutinee of type
    /// `scrutinee_ty`. Handles user enums (nominal name) plus the built-in
    /// `option<U>` (`some(U)`) and `result<T, E>` (`ok(T)`/`err(E)`) generics,
    /// whose payloads are read from the scrutinee's type arguments.
    fn variant_binding_types(&self, scrutinee_ty: &TypeRef, variant: &str) -> Vec<TypeRef> {
        if let Some(payload) = scrutinee_ty.option_element() {
            return match variant {
                "some" => vec![payload],
                _ => Vec::new(),
            };
        }
        if let Some((ok_ty, err_ty)) = scrutinee_ty.result_args() {
            return match variant {
                "ok" => vec![ok_ty],
                "err" => vec![err_ty],
                _ => Vec::new(),
            };
        }
        self.program
            .enums
            .iter()
            .find(|declaration| declaration.name == scrutinee_ty.name)
            .and_then(|declaration| declaration.variants.iter().find(|v| v.name == variant))
            .map(|v| v.payload.clone())
            .unwrap_or_default()
    }

    fn lower_place(
        &self,
        place: &Place,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrPlace, IrLoweringError> {
        match place {
            Place::Field(field) => Ok(IrPlace::Field(field.clone())),
            Place::Index(expr) => Ok(IrPlace::Index(self.lower_expr(expr, scope)?)),
        }
    }

    fn lower_expr(
        &self,
        expr: &Expr,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        self.lower_expr_expected(expr, None, scope)
    }

    /// Lower an expression, optionally carrying a contextual expected type. The
    /// expected type flows from `let` annotations and `return`/final-expression
    /// sites so `none`/`ok`/`err` — which cannot be typed from their payload
    /// alone — lower to the correct `option`/`result` type. Every other
    /// expression ignores `expected` and lowers exactly as before.
    fn lower_expr_expected(
        &self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        // Built-in `option`/`result` construction is context-directed; resolve it
        // before the generic expression rules.
        if let Some(result) = self.lower_builtin_construction(expr, expected, scope) {
            return result;
        }
        let (kind, ty) = match &expr.kind {
            ExprKind::Integer(value) => (IrExprKind::Integer(*value), TypeRef::new("i64")),
            ExprKind::Float(value) => (IrExprKind::Float(*value), TypeRef::new("f64")),
            ExprKind::Bool(value) => (IrExprKind::Bool(*value), TypeRef::new("bool")),
            ExprKind::String(value) => (IrExprKind::String(value.clone()), TypeRef::new("string")),
            ExprKind::Char(value) => (IrExprKind::Char(*value), TypeRef::new("char")),
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
                if let Some(ty) = scope.get(name).cloned() {
                    (IrExprKind::Variable(name.clone()), ty)
                } else if let Some(enum_name) = self.enum_of_variant(name) {
                    // A bare name that is not a local but is a known unit variant
                    // is enum construction. Lower it to a variant `Call` (no args)
                    // so the interpreter and VM build the enum value uniformly.
                    (
                        IrExprKind::Call {
                            name: name.clone(),
                            args: Vec::new(),
                        },
                        TypeRef::new(enum_name),
                    )
                } else if let Some(signature) = self.signatures.get(name) {
                    // A bare name that is a declared top-level function lowers to a
                    // first-class function value of type `fn(params) -> ret`. It
                    // stays a `Variable`, so the interpreter and VM turn it into a
                    // `Value::Func`.
                    (
                        IrExprKind::Variable(name.clone()),
                        function_type(&signature.params, &signature.return_type),
                    )
                } else {
                    return Err(IrLoweringError::new(
                        format!("unknown variable `{name}`"),
                        Some(expr.span),
                    ));
                }
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
                    // `+` on two strings concatenates and yields a string.
                    BinaryOp::Add
                        if left.ty == TypeRef::new("string")
                            && right.ty == TypeRef::new("string") =>
                    {
                        TypeRef::new("string")
                    }
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
                let args = self.lower_call_args(name, args, expected, scope)?;
                // A call whose name is a function-typed local dispatches through
                // that function value; its result type is the function type's
                // return type. The `Call` stays name-based so the interpreter and
                // VM resolve the held `Value::Func` at runtime.
                let ty = match scope.get(name).and_then(TypeRef::function_signature) {
                    Some((_, return_type)) => return_type,
                    None => self.call_return_type(name, &args, expr.span)?,
                };
                (
                    IrExprKind::Call {
                        name: name.clone(),
                        args,
                    },
                    ty,
                )
            }
            ExprKind::StructLiteral { name, fields } => {
                // Reorder named fields into declared order and emit the same
                // positional construction the runtime already knows how to build.
                let order = self
                    .program
                    .structs
                    .iter()
                    .find(|declaration| &declaration.name == name)
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("`{name}` is not a struct type"),
                            Some(expr.span),
                        )
                    })?
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
                let mut lowered = Vec::with_capacity(fields.len());
                for (field_name, value) in fields {
                    lowered.push((field_name.clone(), self.lower_expr(value, scope)?));
                }
                let args = order
                    .iter()
                    .map(|declared| {
                        lowered
                            .iter()
                            .find(|(n, _)| n == declared)
                            .map(|(_, value)| value.clone())
                            .ok_or_else(|| {
                                IrLoweringError::new(
                                    format!("missing field `{declared}` for `{name}`"),
                                    Some(expr.span),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                (
                    IrExprKind::Call {
                        name: name.clone(),
                        args,
                    },
                    TypeRef::new(name.clone()),
                )
            }
            ExprKind::Field { target, field } => {
                let target = self.lower_expr(target, scope)?;
                let ty = self
                    .struct_field_type(&target.ty.name, field)
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("unknown field `{field}` on `{}`", target.ty.name),
                            Some(expr.span),
                        )
                    })?;
                (
                    IrExprKind::Field {
                        target: Box::new(target),
                        field: field.clone(),
                    },
                    ty,
                )
            }
            // A `match` is lowered as a statement (`IrStmt::Match`) at its
            // statement position; the indentation-only surface never nests it
            // inside another expression, so reaching here is a lowering bug.
            ExprKind::Match { .. } => {
                return Err(IrLoweringError::new(
                    "`match` can only appear as a statement expression",
                    Some(expr.span),
                ));
            }
        };

        Ok(IrExpr {
            kind,
            ty,
            span: expr.span,
        })
    }

    /// Lower a built-in `option`/`result` constructor to a variant `Call` IR
    /// node whose type is resolved from the payload and/or the contextual
    /// expected type. Returns `None` when `expr` is not such a constructor so the
    /// caller falls through to the generic lowering rules. Semantics has already
    /// validated these sites, so the expected type is trusted here.
    fn lower_builtin_construction(
        &self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Option<Result<IrExpr, IrLoweringError>> {
        // `list_new()` has no payload; its element type comes solely from the
        // expected `list<...>` type, exactly like `none`. Semantics has already
        // validated that an expected `list<...>` type is present here.
        if let ExprKind::Call { name, args } = &expr.kind
            && name == "list_new"
            && args.is_empty()
        {
            let ty = match expected.cloned() {
                Some(ty) if ty.generic_args("list").is_some() => ty,
                _ => {
                    return Some(Err(IrLoweringError::new(
                        "cannot infer the element type of `list_new` without an expected `list<...>` type",
                        Some(expr.span),
                    )));
                }
            };
            return Some(Ok(IrExpr {
                kind: IrExprKind::Call {
                    name: name.clone(),
                    args: Vec::new(),
                },
                ty,
                span: expr.span,
            }));
        }

        // `map_new()` mirrors `list_new()`: its key/value types come solely from
        // the expected `map<...>` type. Semantics has already validated it.
        if let ExprKind::Call { name, args } = &expr.kind
            && name == "map_new"
            && args.is_empty()
        {
            let ty = match expected.cloned() {
                Some(ty) if ty.generic_args("map").is_some() => ty,
                _ => {
                    return Some(Err(IrLoweringError::new(
                        "cannot infer the key/value types of `map_new` without an expected `map<...>` type",
                        Some(expr.span),
                    )));
                }
            };
            return Some(Ok(IrExpr {
                kind: IrExprKind::Call {
                    name: name.clone(),
                    args: Vec::new(),
                },
                ty,
                span: expr.span,
            }));
        }

        let (name, payload_expr) = match &expr.kind {
            // Bare `none` (not shadowed by a local) is unit-variant construction.
            ExprKind::Variable(name) if name == "none" && !scope.contains_key(name) => {
                (name.as_str(), None)
            }
            ExprKind::Call { name, args } if name == "some" || name == "ok" || name == "err" => {
                (name.as_str(), args.first())
            }
            _ => return None,
        };

        // Lower the payload (if any), guided by the expected type so nested
        // `option`/`result` payloads type correctly.
        let payload_expected = match name {
            "some" => expected.and_then(|ty| ty.option_element()),
            "ok" => expected.and_then(|ty| ty.result_args()).map(|(ok, _)| ok),
            "err" => expected.and_then(|ty| ty.result_args()).map(|(_, err)| err),
            _ => None,
        };
        let lowered_payload = match payload_expr {
            Some(payload) => Some(
                match self.lower_expr_expected(payload, payload_expected.as_ref(), scope) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                },
            ),
            None => None,
        };

        // Resolve the constructed type. `some(v)` synthesizes `option<typeof v>`
        // when no expected type pins it; `none`/`ok`/`err` require the expected
        // type (guaranteed present by semantics).
        let ty = match name {
            "some" => expected.cloned().unwrap_or_else(|| {
                option_type(
                    lowered_payload
                        .as_ref()
                        .map(|value| &value.ty)
                        .unwrap_or(&TypeRef::new("void")),
                )
            }),
            "none" | "ok" | "err" => match expected.cloned() {
                Some(ty) => ty,
                None => {
                    return Some(Err(IrLoweringError::new(
                        format!("cannot infer the type of `{name}` without an expected type"),
                        Some(expr.span),
                    )));
                }
            },
            _ => unreachable!(),
        };

        let args = lowered_payload.into_iter().collect();
        Some(Ok(IrExpr {
            kind: IrExprKind::Call {
                name: name.to_string(),
                args,
            },
            ty,
            span: expr.span,
        }))
    }

    /// Lower a call's arguments, propagating context-directed expected types into
    /// argument position so a nested `list_new`/`map_new`/`none`/`ok`/`err` (or a
    /// value flowing into a collection element/key/value slot) re-derives the
    /// same type semantics assigned. Mirrors the argument-position inference in
    /// `lullaby_semantics::Analyzer::check_call`.
    fn lower_call_args(
        &self,
        name: &str,
        args: &[Expr],
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<Vec<IrExpr>, IrLoweringError> {
        // Collection-growing builtins return the container type, so the outer
        // expected container type flows into the container argument and the
        // resolved element/key/value types flow into the value arguments.
        match name {
            "push" if args.len() == 2 => {
                let list = self.lower_expr_expected(&args[0], expected, scope)?;
                let element = list_element_type(&list.ty);
                let value = self.lower_expr_expected(&args[1], element.as_ref(), scope)?;
                return Ok(vec![list, value]);
            }
            "set" if args.len() == 3 => {
                let list = self.lower_expr_expected(&args[0], expected, scope)?;
                let index = self.lower_expr(&args[1], scope)?;
                let element = list_element_type(&list.ty);
                let value = self.lower_expr_expected(&args[2], element.as_ref(), scope)?;
                return Ok(vec![list, index, value]);
            }
            "pop" if args.len() == 1 => {
                return Ok(vec![self.lower_expr_expected(&args[0], expected, scope)?]);
            }
            "map_set" if args.len() == 3 => {
                let map = self.lower_expr_expected(&args[0], expected, scope)?;
                let (key_ty, value_ty) = map_kv_types(&map.ty);
                let key = self.lower_expr_expected(&args[1], key_ty.as_ref(), scope)?;
                let value = self.lower_expr_expected(&args[2], value_ty.as_ref(), scope)?;
                return Ok(vec![map, key, value]);
            }
            "map_del" if args.len() == 2 => {
                let map = self.lower_expr_expected(&args[0], expected, scope)?;
                let key = self.lower_expr(&args[1], scope)?;
                return Ok(vec![map, key]);
            }
            _ => {}
        }
        // A function-typed local: propagate its declared parameter types.
        if let Some((params, _)) = scope.get(name).and_then(TypeRef::function_signature) {
            if params.len() == args.len() {
                return args
                    .iter()
                    .zip(params.iter())
                    .map(|(arg, param)| self.lower_expr_expected(arg, Some(param), scope))
                    .collect();
            }
        } else if let Some(signature) = self.signatures.get(name) {
            // A user function: propagate each concrete (non-type-variable)
            // parameter type. A parameter that mentions a type variable is left
            // uncontextualized, matching `check_generic_call`.
            if signature.params.len() == args.len() {
                let empty = HashMap::new();
                return args
                    .iter()
                    .zip(signature.params.iter())
                    .map(|(arg, param)| {
                        let has_var = lullaby_semantics::first_unresolved_type_var(
                            param,
                            &signature.type_params,
                            &empty,
                        )
                        .is_some();
                        let expected = if has_var { None } else { Some(param) };
                        self.lower_expr_expected(arg, expected, scope)
                    })
                    .collect();
            }
        }
        // Default: no contextual expected type for any argument.
        args.iter().map(|arg| self.lower_expr(arg, scope)).collect()
    }

    fn call_return_type(
        &self,
        name: &str,
        args: &[IrExpr],
        span: Span,
    ) -> Result<TypeRef, IrLoweringError> {
        // A trait-method call: its result type is the trait method's return type
        // with `Self` = the receiver's type. Generics are erased, so a bounded
        // `v.show()` resolves the same way on the concrete or type-variable type.
        if let Some(method_sig) = self.trait_method_sig(name) {
            let receiver = args.first().ok_or_else(|| {
                IrLoweringError::new(
                    format!("trait method `{name}` call missing receiver"),
                    Some(span),
                )
            })?;
            return Ok(substitute_self_type(&method_sig.return_type, &receiver.ty));
        }
        // A call whose name is a known enum variant is enum construction; its
        // type is the owning enum's nominal type.
        if let Some(enum_name) = self.enum_of_variant(name) {
            return Ok(TypeRef::new(enum_name));
        }
        // A call whose name is a declared struct is a struct construction.
        if self.is_struct(name) {
            return Ok(TypeRef::new(name));
        }
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
            "store" | "dealloc" | "write_file" | "append_file" | "write_bytes" | "make_dir"
            | "remove_file" | "remove_dir" | "print" | "println" | "warn" | "wasm_log"
            | "flush" | "assert" | "rc_release" | "ptr_write" | "region_create" | "tcp_close"
            | "tcp_shutdown" => TypeRef::new("void"),
            // Network builtins report failures as runtime `result` values.
            "tcp_connect" | "tcp_listen" | "tcp_accept" | "udp_bind" => {
                generic_type("result", &[TypeRef::new("Socket"), TypeRef::new("string")])
            }
            "tcp_read" | "udp_recv" | "http_get" | "http_post" => {
                generic_type("result", &[TypeRef::new("string"), TypeRef::new("string")])
            }
            "tcp_write" | "udp_send_to" => {
                generic_type("result", &[TypeRef::new("i64"), TypeRef::new("string")])
            }
            "read_file" | "sys_output" | "to_string" | "substring" | "join" | "trim"
            | "replace" | "upper" | "lower" | "repeat" => TypeRef::new("string"),
            "read_lines" | "list_dir" => {
                generic_type("list", std::slice::from_ref(&TypeRef::new("string")))
            }
            "read_bytes" => generic_type("list", std::slice::from_ref(&TypeRef::new("byte"))),
            "file_exists" | "is_file" | "is_dir" | "contains" | "starts_with" | "ends_with"
            | "map_has" => TypeRef::new("bool"),
            "sys_status" | "file_size" | "len" | "find" | "map_len" | "char_code" | "byte_val" => {
                TypeRef::new("i64")
            }
            "char_from" => TypeRef::new("char"),
            "byte" => TypeRef::new("byte"),
            // `push`/`set`/`pop` return a new `list<T>` of the same type as their
            // list argument (already spelled `list<T>`).
            "push" | "set" | "pop" => {
                args.first().map(|list| list.ty.clone()).ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing list argument"), Some(span))
                })?
            }
            // `get(l, i)` returns the element type `T` of its `list<T>` argument.
            "get" => {
                let list = args.first().ok_or_else(|| {
                    IrLoweringError::new("get call missing list argument", Some(span))
                })?;
                list.ty
                    .generic_args("list")
                    .filter(|args| args.len() == 1)
                    .map(|mut args| args.remove(0))
                    .ok_or_else(|| {
                        IrLoweringError::new("get call argument is not a list", Some(span))
                    })?
            }
            // `map_set`/`map_del` return a new `map<K, V>` of the same type as
            // their map argument (already spelled `map<K, V>`).
            "map_set" | "map_del" => args.first().map(|map| map.ty.clone()).ok_or_else(|| {
                IrLoweringError::new(format!("{name} call missing map argument"), Some(span))
            })?,
            // `map_get(m, k)` returns `option<V>` where `V` is the value type of
            // its `map<K, V>` argument.
            "map_get" => {
                let map = args.first().ok_or_else(|| {
                    IrLoweringError::new("map_get call missing map argument", Some(span))
                })?;
                let value = map
                    .ty
                    .generic_args("map")
                    .filter(|args| args.len() == 2)
                    .map(|mut args| args.remove(1))
                    .ok_or_else(|| {
                        IrLoweringError::new("map_get call argument is not a map", Some(span))
                    })?;
                generic_type("option", std::slice::from_ref(&value))
            }
            "split" => TypeRef::new("array<string>"),
            // `env(name)` yields `option<string>`; `args()` yields `list<string>`.
            "env" => generic_type("option", std::slice::from_ref(&TypeRef::new("string"))),
            "args" => generic_type("list", std::slice::from_ref(&TypeRef::new("string"))),
            // `parallel_map(f, list<i64>)` maps `fn(i64) -> i64` over the list,
            // yielding a `list<i64>` in input order.
            "parallel_map" => generic_type("list", std::slice::from_ref(&TypeRef::new("i64"))),
            // Concurrency builtins: opaque handle producers and readers.
            "chan_new" => TypeRef::new("Chan"),
            "spawn" => TypeRef::new("Task"),
            "mutex_new" => TypeRef::new("Mutex"),
            "recv" | "mutex_get" | "mutex_add" => TypeRef::new("i64"),
            "try_recv" => generic_type("option", std::slice::from_ref(&TypeRef::new("i64"))),
            "send" | "task_join" | "mutex_set" => TypeRef::new("void"),
            "sqrt" | "floor" | "ceil" | "round" | "sin" | "cos" | "tan" | "atan" | "exp" | "ln"
            | "log10" | "atan2" => TypeRef::new("f64"),
            "abs" | "min" | "max" | "pow" => {
                let value = args.first().ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing argument"), Some(span))
                })?;
                TypeRef::new(value.ty.name.clone())
            }
            "rc_new" => {
                let value = args.first().ok_or_else(|| {
                    IrLoweringError::new("rc_new call missing value argument", Some(span))
                })?;
                TypeRef::new(format!("rc<{}>", value.ty.name))
            }
            "rc_clone" => args
                .first()
                .map(|handle| handle.ty.clone())
                .ok_or_else(|| {
                    IrLoweringError::new("rc_clone call missing handle argument", Some(span))
                })?,
            "rc_get" => reference_inner(args, "rc", span)?,
            "ref_get" => reference_inner(args, "ref", span)?,
            "rc_borrow" => {
                TypeRef::new(format!("ref<{}>", reference_inner(args, "rc", span)?.name))
            }
            "ptr_read" => {
                let ptr = args.first().ok_or_else(|| {
                    IrLoweringError::new("ptr_read call missing pointer argument", Some(span))
                })?;
                ptr.ty.pointer_target().ok_or_else(|| {
                    IrLoweringError::new("ptr_read call argument is not a pointer", Some(span))
                })?
            }
            _ => {
                let signature = self.signatures.get(name).ok_or_else(|| {
                    IrLoweringError::new(format!("unknown function `{name}`"), Some(span))
                })?;
                if signature.type_params.is_empty() {
                    signature.return_type.clone()
                } else {
                    // Generic function: re-run the same call-site inference as
                    // semantics against the lowered argument types so the IR
                    // result type matches. Generics are erased, so this only
                    // determines the static result type; the emitted call is an
                    // ordinary call by name.
                    let arg_types: Vec<TypeRef> = args.iter().map(|arg| arg.ty.clone()).collect();
                    lullaby_semantics::infer_generic_return(signature, &arg_types).map_err(
                        |error| {
                            IrLoweringError::new(
                                format!("generic call `{name}` inference failed: {error:?}"),
                                Some(span),
                            )
                        },
                    )?
                }
            }
        })
    }
}

/// Canonical `option<T>` type spelling.
fn option_type(payload: &TypeRef) -> TypeRef {
    generic_type("option", std::slice::from_ref(payload))
}

/// The element type `T` of a `list<T>` spelling, if any.
fn list_element_type(ty: &TypeRef) -> Option<TypeRef> {
    ty.generic_args("list")
        .filter(|args| args.len() == 1)
        .map(|mut args| args.remove(0))
}

/// The `(K, V)` type pair of a `map<K, V>` spelling, split into optional parts.
fn map_kv_types(ty: &TypeRef) -> (Option<TypeRef>, Option<TypeRef>) {
    match ty.generic_args("map").filter(|args| args.len() == 2) {
        Some(mut args) => {
            let value = args.remove(1);
            let key = args.remove(0);
            (Some(key), Some(value))
        }
        None => (None, None),
    }
}

/// Extract the inner type `T` of the first argument's `<ctor><T>` reference type.
fn reference_inner(args: &[IrExpr], ctor: &str, span: Span) -> Result<TypeRef, IrLoweringError> {
    args.first()
        .and_then(|arg| arg.ty.generic_arg(ctor))
        .ok_or_else(|| {
            IrLoweringError::new(
                format!("{ctor} reference call argument is invalid"),
                Some(span),
            )
        })
}

/// Replace `Self` in a type with the receiver's concrete type, recursing into
/// compound generic types. Used to compute a trait method's IR result type.
fn substitute_self_type(ty: &TypeRef, self_ty: &TypeRef) -> TypeRef {
    if ty.name == "Self" {
        return self_ty.clone();
    }
    for ctor in [
        "array", "list", "option", "result", "map", "ptr", "ref", "rc",
    ] {
        if let Some(args) = ty.generic_args(ctor) {
            let mapped: Vec<TypeRef> = args
                .iter()
                .map(|arg| substitute_self_type(arg, self_ty))
                .collect();
            return generic_type(ctor, &mapped);
        }
    }
    ty.clone()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use lullaby_lexer::lex;
    use lullaby_parser::parse;
    use lullaby_runtime::run_main as run_ast_main;
    use lullaby_semantics::{validate, validate_executable};

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
                "target/lullaby_fixture_io.txt",
                "target/lullaby_ir_fixture_io.txt",
            )
        })
    }

    fn cleanup_parity_files() {
        fs::create_dir_all("target").expect("target directory");
        let _ = fs::remove_file("target/lullaby_ir_fixture_io.txt");
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
    fn memory_analysis_reports_alpha_memory_operations_and_safety_metadata() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let values array<i64> = [1, 2, 3]\n    let selected i64 = values[1]\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + selected\n";
        let module = lower_source(source);
        let operations = analyze_memory_operations(&module);

        assert_eq!(operations.len(), 5);
        assert_eq!(
            operations
                .iter()
                .map(|operation| operation.sequence)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(operations[0].function, "main");
        assert!(matches!(
            operations[0].kind,
            IrMemoryOperationKind::Allocate { .. }
        ));
        assert_eq!(
            operations[0].safety.cleanup_role,
            IrCleanupRole::CreatesResource
        );
        assert!(operations[0].safety.mutates_memory);

        assert!(matches!(
            operations[1].kind,
            IrMemoryOperationKind::Store { .. }
        ));
        assert_eq!(
            operations[1].safety.cleanup_role,
            IrCleanupRole::UsesResource
        );
        assert!(operations[1].safety.requires_live_resource);
        assert!(operations[1].safety.unsafe_boundary);

        assert!(matches!(
            operations[2].kind,
            IrMemoryOperationKind::BoundsCheck { .. }
        ));
        assert_eq!(
            operations[2].safety.cleanup_role,
            IrCleanupRole::CheckedAccess
        );
        assert!(operations[2].safety.requires_bounds_check);
        assert!(!operations[2].safety.mutates_memory);

        assert!(matches!(
            operations[3].kind,
            IrMemoryOperationKind::Load { .. }
        ));
        assert!(operations[3].safety.requires_live_resource);
        assert!(!operations[3].safety.mutates_memory);

        assert!(matches!(
            operations[4].kind,
            IrMemoryOperationKind::Deallocate { .. }
        ));
        assert_eq!(
            operations[4].safety.cleanup_role,
            IrCleanupRole::ReleasesResource
        );
        assert!(operations[4].safety.mutates_memory);
    }

    #[test]
    fn memory_analysis_covers_region_copy_and_cleanup_end_to_end() {
        // A single program exercising region creation, reference copy
        // (rc_clone), and compiler-visible cleanup (rc_release) produces all
        // three memory-operation kinds from parseable source, lowered end to end.
        let module = lower_source(
            "fn main -> i64\n    region pool: size=64, align=8\n    let h rc<i64> = rc_new(7)\n    let s rc<i64> = rc_clone(h)\n    rc_release(s)\n    rc_release(h)\n    0\n",
        );
        let operations = analyze_memory_operations(&module);
        use IrMemoryOperationKind::*;
        assert!(
            operations
                .iter()
                .any(|op| matches!(op.kind, RegionCreate { .. }))
        );
        assert!(operations.iter().any(|op| matches!(op.kind, Copy { .. })));
        assert!(
            operations
                .iter()
                .any(|op| matches!(op.kind, Cleanup { .. }))
        );
        // Every reported operation carries safety metadata for optimizer/codegen.
        assert!(operations.iter().all(|op| matches!(
            op.safety.cleanup_role,
            IrCleanupRole::CreatesResource
                | IrCleanupRole::UsesResource
                | IrCleanupRole::ReleasesResource
                | IrCleanupRole::CheckedAccess
        )));
    }

    #[test]
    fn memory_analysis_reports_region_creation() {
        let module = lower_source("fn main -> i64\n    region pool: size=4096, align=16\n    0\n");
        let operations = analyze_memory_operations(&module);
        let region = operations
            .iter()
            .find(|op| matches!(op.kind, IrMemoryOperationKind::RegionCreate { .. }))
            .expect("region create op");
        let IrMemoryOperationKind::RegionCreate { region_type } = &region.kind else {
            unreachable!()
        };
        assert_eq!(region_type.name, "region<pool>");
    }

    #[test]
    fn memory_analysis_reports_reference_operations() {
        let source = "fn main -> i64\n    let h rc<i64> = rc_new(1)\n    let s rc<i64> = rc_clone(h)\n    let v i64 = rc_get(h)\n    rc_release(s)\n    rc_release(h)\n    v\n";
        let module = lower_source(source);
        let operations = analyze_memory_operations(&module);

        assert!(
            operations
                .iter()
                .any(|op| matches!(op.kind, IrMemoryOperationKind::Allocate { .. })),
            "rc_new should be an allocation"
        );
        assert!(
            operations
                .iter()
                .any(|op| matches!(op.kind, IrMemoryOperationKind::Copy { .. })),
            "rc_clone should be a copy/share"
        );
        assert!(
            operations
                .iter()
                .any(|op| matches!(op.kind, IrMemoryOperationKind::Load { .. })),
            "rc_get should be a load"
        );
        let cleanups = operations
            .iter()
            .filter(|op| matches!(op.kind, IrMemoryOperationKind::Cleanup { .. }))
            .count();
        assert_eq!(cleanups, 2, "two rc_release calls should be cleanups");
    }

    #[test]
    fn planned_memory_operation_kinds_have_safety_metadata() {
        let cases = [
            (
                IrMemoryOperationKind::RegionCreate {
                    region_type: TypeRef::new("region"),
                },
                IrCleanupRole::CreatesResource,
                false,
                true,
                false,
            ),
            (
                IrMemoryOperationKind::RegionResize {
                    region_type: TypeRef::new("region"),
                },
                IrCleanupRole::UsesResource,
                true,
                true,
                true,
            ),
            (
                IrMemoryOperationKind::Copy {
                    source_type: TypeRef::new("ptr_i64"),
                    target_type: TypeRef::new("ptr_i64"),
                },
                IrCleanupRole::UsesResource,
                true,
                true,
                true,
            ),
            (
                IrMemoryOperationKind::Cleanup {
                    resource_type: TypeRef::new("ptr_i64"),
                },
                IrCleanupRole::ReleasesResource,
                true,
                true,
                false,
            ),
        ];

        for (kind, role, requires_live_resource, mutates_memory, unsafe_boundary) in cases {
            let safety = memory_safety_for_kind(&kind).expect("planned memory safety");
            assert_eq!(safety.cleanup_role, role);
            assert_eq!(safety.requires_live_resource, requires_live_resource);
            assert_eq!(safety.mutates_memory, mutates_memory);
            assert_eq!(safety.unsafe_boundary, unsafe_boundary);
            assert!(!safety.requires_bounds_check);
        }
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
            "L0404"
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
            "fn main -> i64\n    let base i64 = 40\n    let copy i64 = base\n    let second i64 = copy\n    second + 2\n",
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
        let source = "fn main -> i64\n    let source i64 = 1\n    let copy i64 = source\n    source = 2\n    copy\n";
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
        assert_eq!(expr.kind, IrExprKind::Variable("copy".to_string()));
    }

    #[test]
    fn common_subexpression_elimination_reuses_prior_pure_binding() {
        let source = "fn main -> i64\n    let base i64 = 4\n    let first i64 = (base + 1) * (base + 2)\n    let second i64 = (base + 1) * (base + 2)\n    first + second\n";
        let module = lower_source(source);
        let (optimized, report) = optimize(
            &module,
            &OptimizationConfig::common_subexpression_elimination(),
        );

        assert_eq!(
            report.applied_passes,
            vec![OptimizationPass::CommonSubexpressionElimination]
        );
        assert_eq!(report.eliminated_common_subexpressions, 1);
        assert_eq!(
            run_main(&optimized).expect("optimized run"),
            run_all_backends(source).0
        );

        let IrStmt::Let { value, .. } = &optimized.functions[0].body[2] else {
            panic!("expected second binding");
        };
        assert_eq!(value.kind, IrExprKind::Variable("first".to_string()));
    }

    #[test]
    fn common_subexpression_elimination_invalidates_after_assignment() {
        let source = "fn main -> i64\n    let source i64 = 1\n    let first i64 = source + 1\n    source = 2\n    let second i64 = source + 1\n    first + second\n";
        let module = lower_source(source);
        let (optimized, report) = optimize(
            &module,
            &OptimizationConfig::common_subexpression_elimination(),
        );

        assert_eq!(report.eliminated_common_subexpressions, 0);
        assert_eq!(
            run_main(&optimized).expect("optimized run"),
            run_all_backends(source).0
        );

        let IrStmt::Let { value, .. } = &optimized.functions[0].body[3] else {
            panic!("expected second binding");
        };
        assert!(matches!(value.kind, IrExprKind::Binary { .. }));
    }

    #[test]
    fn loop_invariant_motion_hoists_safe_binding_from_for_body() {
        let source = "fn main -> i64\n    let base i64 = 3\n    let total i64 = 0\n    for i from 1 to 3\n        let invariant i64 = (base + 1) * 2\n        total += invariant + i\n    total\n";
        let module = lower_source(source);
        let (optimized, report) = optimize(&module, &OptimizationConfig::loop_invariant_motion());

        assert_eq!(
            report.applied_passes,
            vec![OptimizationPass::LoopInvariantMotion]
        );
        assert_eq!(report.hoisted_loop_invariants, 1);
        assert_eq!(
            run_main(&optimized).expect("optimized run"),
            run_all_backends(source).0
        );

        let function = &optimized.functions[0];
        let IrStmt::Let {
            name: temp_name,
            value: temp_value,
            ..
        } = &function.body[2]
        else {
            panic!("expected hoisted temp binding");
        };
        assert!(temp_name.starts_with("__lullaby_loop_invariant_"));
        assert!(matches!(temp_value.kind, IrExprKind::Binary { .. }));

        let IrStmt::For { body, .. } = &function.body[3] else {
            panic!("expected for loop after hoisted binding");
        };
        let IrStmt::Let { value, .. } = &body[0] else {
            panic!("expected rewritten loop binding");
        };
        assert_eq!(value.kind, IrExprKind::Variable(temp_name.clone()));
    }

    #[test]
    fn loop_invariant_motion_keeps_loop_variable_dependency_in_place() {
        let source = "fn main -> i64\n    let total i64 = 0\n    for i from 1 to 3\n        let value i64 = i + 1\n        total += value\n    total\n";
        let module = lower_source(source);
        let (optimized, report) = optimize(&module, &OptimizationConfig::loop_invariant_motion());

        assert_eq!(report.hoisted_loop_invariants, 0);
        assert_eq!(
            run_main(&optimized).expect("optimized run"),
            run_all_backends(source).0
        );

        let IrStmt::For { body, .. } = &optimized.functions[0].body[1] else {
            panic!("expected for loop");
        };
        let IrStmt::Let { value, .. } = &body[0] else {
            panic!("expected loop-local binding");
        };
        assert!(matches!(value.kind, IrExprKind::Binary { .. }));
    }

    #[test]
    fn loop_invariant_motion_does_not_hoist_potential_runtime_failure() {
        let source = "fn main -> i64\n    while false\n        let value i64 = 1 / 0\n    42\n";
        let module = lower_source(source);
        let (optimized, report) = optimize(&module, &OptimizationConfig::loop_invariant_motion());

        assert_eq!(report.hoisted_loop_invariants, 0);
        assert_eq!(run_main(&optimized).expect("optimized run"), Value::I64(42));

        let IrStmt::While { body, .. } = &optimized.functions[0].body[0] else {
            panic!("expected while loop");
        };
        let IrStmt::Let { value, .. } = &body[0] else {
            panic!("expected loop-local binding");
        };
        assert!(matches!(
            value.kind,
            IrExprKind::Binary {
                op: BinaryOp::Divide,
                ..
            }
        ));
    }

    #[test]
    fn alpha_optimizer_runs_alpha_pass_pipeline() {
        let module =
            lower_source("fn main -> i64\n    let value i64 = 40 + 2\n    return value\n    0\n");
        let (optimized, report) = optimize(&module, &OptimizationConfig::alpha_default());

        assert_eq!(
            report.applied_passes,
            vec![
                OptimizationPass::ConstantFolding,
                OptimizationPass::CommonSubexpressionElimination,
                OptimizationPass::LoopInvariantMotion,
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
        assert_eq!(artifact.metadata.payload, "instruction-bytecode");
        assert_eq!(artifact.entry, "main");
        assert_eq!(artifact.function_table.len(), 1);
        assert_eq!(artifact.function_table[0].name, "main");
        assert!(matches!(
            artifact.module.functions[0].instructions[0],
            BytecodeInstruction::Expr(_)
        ));
        assert_eq!(
            run_bytecode_main(&artifact.module).expect("run artifact bytecode"),
            Value::I64(42)
        );
    }

    #[test]
    fn bytecode_artifact_preserves_memory_operation_metadata() {
        let ir = lower_source(
            "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let values array<i64> = [1, 2, 3]\n    let selected i64 = values[1]\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + selected\n",
        );
        let bytecode = lower_to_bytecode(&ir);
        let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
        let artifact = decode_bytecode_artifact(&encoded).expect("decode artifact");

        assert!(encoded.contains("\"memory_operations\""));
        assert_eq!(
            artifact.memory_operations,
            analyze_bytecode_memory_operations(&artifact.module)
        );
        assert_eq!(artifact.memory_operations.len(), 5);
        assert!(matches!(
            artifact.memory_operations[0].kind,
            IrMemoryOperationKind::Allocate { .. }
        ));
        assert!(matches!(
            artifact.memory_operations[1].kind,
            IrMemoryOperationKind::Store { .. }
        ));
        assert!(matches!(
            artifact.memory_operations[2].kind,
            IrMemoryOperationKind::BoundsCheck { .. }
        ));
        assert!(matches!(
            artifact.memory_operations[3].kind,
            IrMemoryOperationKind::Load { .. }
        ));
        assert!(matches!(
            artifact.memory_operations[4].kind,
            IrMemoryOperationKind::Deallocate { .. }
        ));
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
    fn bytecode_artifact_rejects_old_structured_payload_version() {
        let invalid = format!(
            "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":2,\"entry\":\"main\",\"metadata\":{{\"producer\":\"test\",\"target\":\"alpha1\",\"payload\":\"structured-bytecode\"}},\"function_table\":[],\"module\":{{\"functions\":[]}}}}"
        );
        let error = decode_bytecode_artifact(&invalid).expect_err("old artifact");

        assert!(
            error
                .message
                .contains("unsupported bytecode artifact version `2`")
        );
    }

    #[test]
    fn bytecode_artifact_rejects_missing_entry_function() {
        let invalid = format!(
            "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":{BYTECODE_ARTIFACT_VERSION},\"entry\":\"main\",\"metadata\":{{\"producer\":\"test\",\"target\":\"alpha1\",\"payload\":\"instruction-bytecode\"}},\"function_table\":[],\"module\":{{\"functions\":[]}}}}"
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
    fn bytecode_artifact_rejects_memory_operation_mismatch() {
        let ir = lower_source(
            "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value\n",
        );
        let bytecode = lower_to_bytecode(&ir);
        let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
        let mut value: serde_json::Value = serde_json::from_str(&encoded).expect("json artifact");
        value["memory_operations"] = serde_json::json!([]);
        let invalid = serde_json::to_string(&value).expect("invalid json artifact");

        let error = decode_bytecode_artifact(&invalid).expect_err("memory mismatch");

        assert!(error.message.contains("memory_operations does not match"));
    }

    #[test]
    fn bytecode_artifact_rejects_parameterized_entrypoint() {
        let span = Span::new(1, 1);
        let module = BytecodeModule {
            structs: Vec::new(),
            enums: Vec::new(),
            impls: Vec::new(),
            trait_methods: Vec::new(),
            functions: vec![BytecodeFunction {
                name: "main".to_string(),
                params: vec![IrParam {
                    name: "argc".to_string(),
                    ty: TypeRef::new("i64"),
                }],
                return_type: TypeRef::new("i64"),
                instructions: vec![BytecodeInstruction::Return(Some(BytecodeExpr {
                    kind: BytecodeExprKind::Variable("argc".to_string()),
                    ty: TypeRef::new("i64"),
                    span,
                }))],
                span,
            }],
        };
        let encoded = serde_json::to_string(&BytecodeArtifact::new(module)).expect("encode");
        let error = decode_bytecode_artifact(&encoded).expect_err("parameterized entry");

        assert!(
            error
                .message
                .contains("entry `main` must not require parameters")
        );
    }

    #[test]
    fn bytecode_artifact_rejects_break_outside_loop_instruction() {
        let span = Span::new(1, 1);
        let module = BytecodeModule {
            structs: Vec::new(),
            enums: Vec::new(),
            impls: Vec::new(),
            trait_methods: Vec::new(),
            functions: vec![BytecodeFunction {
                name: "main".to_string(),
                params: Vec::new(),
                return_type: TypeRef::new("i64"),
                instructions: vec![BytecodeInstruction::Break(span)],
                span,
            }],
        };
        let encoded = serde_json::to_string(&BytecodeArtifact::new(module)).expect("encode");
        let error = decode_bytecode_artifact(&encoded).expect_err("invalid break");

        assert!(
            error
                .message
                .contains("instruction `break` outside loop in function `main`")
        );
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
    fn ir_and_bytecode_match_ast_for_error_handling() {
        let source = "fn checked n i64 -> string\n    try\n        if n < 0\n            throw \"neg\"\n        \"ok:\" + to_string(n)\n    catch message\n        \"err:\" + message\n\nfn main -> string\n    checked(5) + \" \" + checked(0 - 1)\n";
        let (ast, ir, bytecode) = run_all_backends(source);
        assert_eq!(ast, Value::String("ok:5 err:neg".to_string()));
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
    }

    #[test]
    fn ir_and_bytecode_match_ast_for_pattern_matching() {
        let source = concat!(
            "enum Shape\n    Circle i64\n    Rect i64 i64\n    Empty\n\n",
            "enum Color\n    Red\n    Green\n    Blue\n\n",
            "fn area s Shape -> i64\n",
            "    match s\n",
            "        Circle(r) -> r * r\n",
            "        Rect(w, h) -> w * h\n",
            "        Empty -> 0\n\n",
            "fn rank c Color -> i64\n",
            "    match c\n",
            "        Green -> 10\n",
            "        _ -> 1\n\n",
            "fn main -> i64\n",
            "    area(Circle(3)) + area(Rect(4, 5)) + area(Empty) + rank(Green) + rank(Red)\n",
        );
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ast, Value::I64(40));
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
        assert_eq!(optimized_ir, ast);
        assert_eq!(optimized_bytecode, ast);
    }

    #[test]
    fn ir_and_bytecode_match_ast_for_memory_builtins() {
        let source = "fn main -> i64\n    let ptr ptr_i64 = alloc(0)\n    store(ptr, 41)\n    let value i64 = load(ptr)\n    dealloc(ptr)\n    value + 1\n";
        let (ast, ir, bytecode) = run_all_backends(source);
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
    }

    #[test]
    fn write_bytes_read_bytes_round_trip_matches_across_backends() {
        // Each backend writes and reads back the same file sequentially, so the
        // fixed path is deterministic. The program reconstructs the byte sum.
        let path = "target/lullaby_ir_bytes_roundtrip.bin";
        let _ = fs::create_dir_all("target");
        let _ = fs::remove_file(path);
        let source = format!(
            "fn main -> i64\n    \
             let data list<byte> = list_new()\n    \
             data = push(data, byte(7))\n    \
             data = push(data, byte(11))\n    \
             write_bytes(\"{path}\", data)\n    \
             let back list<byte> = read_bytes(\"{path}\")\n    \
             byte_val(get(back, 0)) + byte_val(get(back, 1)) + len(back)\n"
        );
        let (ast, ir, bytecode) = run_all_backends(&source);
        // 7 + 11 + 2 == 20
        assert_eq!(ast, Value::I64(20));
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn executable_valid_fixtures_match_across_backend_variants() {
        cleanup_parity_files();
        let fixture_dir = workspace_root().join("tests/fixtures/valid");
        let mut fixtures = fs::read_dir(&fixture_dir)
            .expect("valid fixture directory")
            .map(|entry| entry.expect("fixture entry").path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("lby"))
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
        assert!(covered.contains(&"run_file_io.lby".to_string()));
        assert!(covered.contains(&"run_store.lby".to_string()));
        assert!(covered.contains(&"run_for_step.lby".to_string()));
        assert!(covered.contains(&"run_option_result.lby".to_string()));
    }

    #[test]
    fn lowers_nested_constructor_in_call_argument_position() {
        // The IR lowerer re-derives types, so a nested `list_new()` in argument
        // position must take its element type from the surrounding context (the
        // outer `list<byte>` flowing through `push`), and all backends agree.
        let source = concat!(
            "fn count o option<i64> -> i64\n",
            "    match o\n",
            "        some(v) -> v\n",
            "        none -> 0\n\n",
            "fn main -> i64\n",
            "    let data list<byte> = push(list_new(), byte(65))\n",
            "    let a i64 = count(none)\n",
            "    byte_val(get(data, 0)) + a\n",
        );
        let module = lower_source(source);
        let main = module
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main function");
        let IrStmt::Let { value, .. } = &main.body[0] else {
            panic!("expected list binding");
        };
        // The nested `list_new()` inferred `list<byte>`, so `push` returns it.
        assert_eq!(value.ty, TypeRef::new("list<byte>"));

        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
        assert_eq!(optimized_ir, ast);
        assert_eq!(optimized_bytecode, ast);
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
