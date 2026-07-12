use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use lullaby_diagnostics::{Span, TraceFrame};
use lullaby_parser::{
    AssignOp, BinaryOp, Expr, ExprKind, Function, MatchArm, MatchPattern, Place, Program, Stmt,
    TypeRef, UnaryOp, function_type, generic_type,
};
use lullaby_runtime::{
    ArithOp, Closure, EnumValue, Future, IntKind, MEMORY_ORDER_VARIANTS, OverflowMode,
    ProcessResource, ResolvedPlace, RuntimeError, SharedAtomic, SharedMutex, SocketResource,
    StructValue, Task, Value, apply_compound, asm_interpreter_error, await_future,
    builtin_atomic_add_ordered, builtin_atomic_and_ordered, builtin_atomic_cas_ordered,
    builtin_atomic_load_ordered, builtin_atomic_or_ordered, builtin_atomic_store_ordered,
    builtin_atomic_sub_ordered, builtin_atomic_swap_ordered, builtin_atomic_xor_ordered,
    builtin_fence, char_find, expect_atomic, expect_bool, expect_chan, expect_future, expect_i64,
    expect_list, expect_map, expect_mutex, expect_string, expect_task, extern_call_error, gcd_i64,
    get_place, http_exchange, int_cmp, int_div, int_rem, int_shl, int_shr, join_task, list_extreme,
    list_sum_values, monotonic_now_nanos, net_err, new_chan, option_value, os_random_bytes,
    overflow_arith, process_exit_code, result_value, scalar_order_keys, set_place, shift_left,
    shift_right, sleep_millis, sort_scalar_list, value_type_name, wall_now_millis,
};
use lullaby_semantics::{CheckedProgram, Signature};
use serde::{Deserialize, Serialize};

pub mod aarch64;
pub mod elf_object;
pub mod frame_layout;
pub mod macho_object;
pub mod native_contract;
pub mod native_object;
pub mod object_model;
pub mod wasm;

pub use native_object::{
    DebugOptions, NATIVE_ENTRY_SYMBOL, NATIVE_NO_ELIGIBLE_CODE, NativeProgram, NativeProgramError,
    NativeSkippedFunction, emit_native_program, emit_native_program_for_target,
    emit_native_program_with_debug,
};
pub use wasm::{SkippedFunction, WasmArtifact, WasmError, emit_wasm_module};

pub const BYTECODE_ARTIFACT_FORMAT: &str = "lullaby-bytecode";
pub const BYTECODE_ARTIFACT_EXTENSION: &str = "lbc";
pub const BYTECODE_ARTIFACT_VERSION: u32 = 5;
const BYTECODE_ARTIFACT_PAYLOAD: &str = "instruction-bytecode";
const BYTECODE_ARTIFACT_TARGET: &str = "lullaby-vm";

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
    /// Names of `async fn` functions. Calling one spawns an OS thread running its
    /// body and yields a `Value::Future` that `await` resolves. Serde-defaulted
    /// so existing artifacts and snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub async_functions: Vec<String>,
    /// Names of `extern fn` (C-ABI) functions. These have no lowered body: a call
    /// to one emits a `call` of the external symbol on the native backend, and is
    /// rejected with `L0423` on the interpreters (which cannot execute C).
    /// Serde-defaulted so existing artifacts and snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extern_functions: Vec<String>,
    /// C-ABI signatures for the `extern_functions`, in the same declaration
    /// order. The native backend uses each signature's parameter/return scalar
    /// widths to marshal an extern call correctly (Win64 integer registers, with
    /// a narrow C return normalized in `rax`). Serde-defaulted so existing
    /// artifacts and snapshots (which lack this field) stay valid; when absent the
    /// native backend falls back to treating extern params/returns as `i64`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extern_signatures: Vec<IrExternSignature>,
    /// Names of `export fn` functions — normal Lullaby functions additionally
    /// exposed under their plain C name as externally visible, defined native
    /// symbols so C can call into them. `export` is meaningful only to native
    /// codegen; on the interpreters an export runs like an ordinary function.
    /// Serde-defaulted so existing artifacts and snapshots stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub export_functions: Vec<String>,
    /// Lowered closure bodies, keyed by the parse-order `id` an
    /// [`IrExprKind::Closure`] node carries. Each entry pairs the closure's
    /// parameter names with its lowered single-expression body, so the
    /// interpreter/VM can invoke a closure value (which stores only its id plus
    /// captured snapshot) by looking its body up here. Serialized in the `.lbc`
    /// artifact like every other lowered code fragment; the runtime `Value` is
    /// never serialized. Serde-defaulted so existing artifacts stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closures: Vec<IrClosureDef>,
}

/// A lowered closure body in the IR module, keyed by the parse-order `id` the
/// [`IrExprKind::Closure`] node carries. `params` are the closure's parameter
/// names (types are erased at runtime, exactly like a function's), and `body` is
/// the lowered single expression evaluated on invocation with the captured
/// snapshot and parameters bound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrClosureDef {
    pub id: usize,
    pub params: Vec<String>,
    pub body: IrExpr,
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

/// The C-ABI signature of an `extern fn`: its symbol name, ordered parameter
/// types, and return type. Carried alongside `extern_functions` (the names view)
/// so the native backend can marshal each argument/return to the correct C
/// scalar width and normalize a narrow C return per the Win64 ABI. The
/// interpreters ignore it (they reject extern calls with `L0423`). Serde-defaulted
/// so existing `.lbc` artifacts and JSON snapshots without this field stay valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrExternSignature {
    pub name: String,
    pub params: Vec<TypeRef>,
    pub return_type: TypeRef,
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
    /// Inline assembly: raw x86-64 machine-code bytes emitted verbatim by the
    /// native backend. Native-only; the IR and bytecode interpreters reject it
    /// at runtime with `L0425`. Each byte is validated in `0..=255` by semantics.
    Asm {
        bytes: Vec<u8>,
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
    /// A slot-resolved local read produced by [`resolve_slots`] at interpretation
    /// time: `Local(packed)` where `packed = ((depth << 16) | slot) + 1` names the
    /// binding by its `(scopes-from-innermost depth, index-within-scope slot)`
    /// instead of by name, so [`Env::get_slot`] indexes it directly with no
    /// string scan. It only ever appears in the interpreter's resolved copy of a
    /// function body — never in lowered/optimized/serialized IR, and never in the
    /// WASM or native paths — because resolution runs after all of those and only
    /// for the tree-walking evaluator. The binding's name is still retained in the
    /// `Env` scope entry, so any code path that meets a `Local` it cannot use
    /// falls back to the equivalent `Variable` behavior via the retained name.
    Local {
        name: String,
        packed: u32,
    },
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
    /// `await EXPR`: block until the awaited `Future<T>` completes and yield its
    /// `T`. The operand has type `Future<T>` (typically a call to an `async fn`);
    /// this node's `ty` is `T`.
    Await {
        expr: Box<IrExpr>,
    },
    /// An inline closure literal, carrying only its parse-order `id`. The body and
    /// parameter names live in [`IrModule::closures`], keyed by this id; the node
    /// itself stores no body. Its `ty` is the closure's `fn(...) -> R` type.
    Closure {
        id: usize,
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

    pub fn inlining() -> Self {
        Self {
            passes: vec![OptimizationPass::Inlining],
        }
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

    pub fn full() -> Self {
        Self {
            passes: vec![
                OptimizationPass::Inlining,
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
        Self::full()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizationPass {
    Inlining,
    ConstantFolding,
    CommonSubexpressionElimination,
    LoopInvariantMotion,
    CopyPropagation,
    DeadCodeElimination,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OptimizationReport {
    pub applied_passes: Vec<OptimizationPass>,
    pub inlined_calls: usize,
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
            OptimizationPass::Inlining => {
                let mut inliner = Inliner::new(&optimized);
                optimized = inliner.inline_module(&optimized);
                report.inlined_calls += inliner.inlined_calls;
                report.applied_passes.push(*pass);
            }
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
    let mut owned = module.clone();
    resolve_module_slots(&mut owned);
    let arc = Arc::new(owned);
    run_main_shared(arc, args)
}

// -- Slot-based variable resolution -------------------------------------------
//
// Every variable read in the tree-walking interpreter used to do a linear
// name scan: walk the scope stack from the innermost scope outward, comparing
// the target name against each binding's `String` key. Because the same IR
// nodes are re-walked on every loop iteration, that scan is paid per access on
// the hottest path.
//
// `resolve_module_slots` runs once, at interpretation entry, over an owned copy
// of the module. It walks each function body with a scope model that mirrors the
// evaluator's `push_scope`/`pop_scope`/`define` behavior exactly, and rewrites
// each name-resolvable local *read* (`Variable`) into a `Local { name, packed }`
// carrying the binding's `(depth, slot)` position. The evaluator then indexes it
// directly via `Env::get_slot` — no scan.
//
// Safety is structural, not by careful analysis: `Env::get_slot` re-checks the
// binding name at the resolved position and falls back to the name scan on any
// mismatch, so a wrong `(depth, slot)` can only miss (and be slower), never read
// the wrong binding. Closures are left unresolved (their bodies run against a
// captured-snapshot environment whose scope shape differs from lexical nesting),
// and any reference whose `(depth, slot)` would overflow the packed encoding
// simply stays a `Variable`. The rewrite is confined to the interpreter's owned
// copy: lowered/optimized/serialized IR and the WASM/native paths never see a
// `Local`.

/// Pack a `(depth, slot)` local reference into a `u32` (`depth` in the high 16
/// bits, `slot` in the low 16). Returns `None` when either field would overflow,
/// so the caller keeps the name-scanned `Variable` form for that reference.
fn pack_slot(depth: usize, slot: usize) -> Option<u32> {
    if depth > 0xffff || slot > 0xffff {
        return None;
    }
    Some(((depth as u32) << 16) | (slot as u32))
}

/// Inverse of [`pack_slot`]: `(depth, slot)`.
fn unpack_slot(packed: u32) -> (usize, usize) {
    ((packed >> 16) as usize, (packed & 0xffff) as usize)
}

/// Resolve `name` to a packed `(depth, slot)` against the compile-time scope
/// stack, searching innermost-first exactly like [`Env::get_ref`].
fn resolve_var_slot(name: &str, scopes: &[Vec<String>]) -> Option<u32> {
    for (index, scope) in scopes.iter().enumerate().rev() {
        if let Some(slot) = scope.iter().position(|existing| existing == name) {
            let depth = scopes.len() - 1 - index;
            return pack_slot(depth, slot);
        }
    }
    None
}

fn resolve_module_slots(module: &mut IrModule) {
    for function in &mut module.functions {
        resolve_function_slots(&mut function.params, &mut function.body);
    }
    for impl_method in &mut module.impls {
        let function = &mut impl_method.function;
        resolve_function_slots(&mut function.params, &mut function.body);
    }
    // Closures (`module.closures`) are intentionally not resolved: a closure body
    // executes against a captured-snapshot environment, so its lexical nesting
    // does not match the runtime scope stack the `(depth, slot)` model assumes.
}

fn resolve_function_slots(params: &mut [IrParam], body: &mut [IrStmt]) {
    let mut scopes: Vec<Vec<String>> = vec![params.iter().map(|p| p.name.clone()).collect()];
    resolve_block_slots(body, &mut scopes);
}

fn resolve_block_slots(body: &mut [IrStmt], scopes: &mut Vec<Vec<String>>) {
    for stmt in body {
        resolve_stmt_slots(stmt, scopes);
    }
}

/// Add `name` to the innermost scope if it is not already bound there, matching
/// `Env::define`'s replace-in-place-or-push behavior (a re-`let` of the same name
/// keeps its existing slot).
fn declare_in_scope(name: &str, scopes: &mut [Vec<String>]) {
    if let Some(scope) = scopes.last_mut()
        && !scope.iter().any(|existing| existing == name)
    {
        scope.push(name.to_string());
    }
}

fn resolve_stmt_slots(stmt: &mut IrStmt, scopes: &mut Vec<Vec<String>>) {
    match stmt {
        IrStmt::Let { name, value, .. } => {
            // The initializer is evaluated before the binding is introduced (or,
            // for a re-`let`, while its previous value is still bound), so resolve
            // it against the current scopes first, then declare the name.
            resolve_expr_slots(value, scopes);
            declare_in_scope(name, scopes);
        }
        IrStmt::Assign {
            path, value, ..
        } => {
            // The target name keeps the name-scan `assign` path; only the RHS and
            // any index expressions in the path are read positions.
            for place in path.iter_mut() {
                if let IrPlace::Index(index) = place {
                    resolve_expr_slots(index, scopes);
                }
            }
            resolve_expr_slots(value, scopes);
        }
        IrStmt::Return(expr) => {
            if let Some(expr) = expr {
                resolve_expr_slots(expr, scopes);
            }
        }
        IrStmt::Expr(expr) | IrStmt::Throw { value: expr, .. } => {
            resolve_expr_slots(expr, scopes);
        }
        IrStmt::If {
            branches,
            else_body,
            ..
        } => {
            for branch in branches {
                resolve_expr_slots(&mut branch.condition, scopes);
                scopes.push(Vec::new());
                resolve_block_slots(&mut branch.body, scopes);
                scopes.pop();
            }
            scopes.push(Vec::new());
            resolve_block_slots(else_body, scopes);
            scopes.pop();
        }
        IrStmt::While {
            condition, body, ..
        } => {
            resolve_expr_slots(condition, scopes);
            scopes.push(Vec::new());
            resolve_block_slots(body, scopes);
            scopes.pop();
        }
        IrStmt::For {
            name,
            start,
            end,
            step,
            body,
            ..
        } => {
            resolve_expr_slots(start, scopes);
            resolve_expr_slots(end, scopes);
            if let Some(step) = step {
                resolve_expr_slots(step, scopes);
            }
            // The loop variable lives in its own scope; the body opens a further
            // child scope each iteration (mirroring the evaluator's two pushes).
            scopes.push(vec![name.clone()]);
            scopes.push(Vec::new());
            resolve_block_slots(body, scopes);
            scopes.pop();
            scopes.pop();
        }
        IrStmt::Loop { body, .. } => {
            scopes.push(Vec::new());
            resolve_block_slots(body, scopes);
            scopes.pop();
        }
        IrStmt::Try {
            body,
            catch_name,
            catch_body,
            ..
        } => {
            scopes.push(Vec::new());
            resolve_block_slots(body, scopes);
            scopes.pop();
            scopes.push(vec![catch_name.clone()]);
            resolve_block_slots(catch_body, scopes);
            scopes.pop();
        }
        IrStmt::Match {
            scrutinee, arms, ..
        } => {
            resolve_expr_slots(scrutinee, scopes);
            for arm in arms {
                let seeds = match &arm.pattern {
                    IrMatchPattern::Variant { bindings, .. } => bindings.clone(),
                    IrMatchPattern::Wildcard => Vec::new(),
                };
                scopes.push(seeds);
                resolve_block_slots(&mut arm.body, scopes);
                scopes.pop();
            }
        }
        IrStmt::Break(_) | IrStmt::Continue(_) | IrStmt::Asm { .. } => {}
    }
}

fn resolve_expr_slots(expr: &mut IrExpr, scopes: &[Vec<String>]) {
    match &mut expr.kind {
        IrExprKind::Variable(name) => {
            if let Some(packed) = resolve_var_slot(name, scopes) {
                let name = std::mem::take(name);
                expr.kind = IrExprKind::Local { name, packed };
            }
        }
        IrExprKind::Array(elements) => {
            for element in elements {
                resolve_expr_slots(element, scopes);
            }
        }
        IrExprKind::Index { target, index } => {
            resolve_expr_slots(target, scopes);
            resolve_expr_slots(index, scopes);
        }
        IrExprKind::Unary { expr: inner, .. } | IrExprKind::Await { expr: inner } => {
            resolve_expr_slots(inner, scopes);
        }
        IrExprKind::Binary { left, right, .. } => {
            resolve_expr_slots(left, scopes);
            resolve_expr_slots(right, scopes);
        }
        IrExprKind::Call { args, .. } => {
            for arg in args {
                resolve_expr_slots(arg, scopes);
            }
        }
        IrExprKind::Field { target, .. } => {
            resolve_expr_slots(target, scopes);
        }
        // Leaves and already-resolved / opaque nodes: literals carry no reads, a
        // `Closure` stores only an id (its body is resolved never), and a `Local`
        // is already resolved.
        IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
        | IrExprKind::Local { .. }
        | IrExprKind::Closure { .. } => {}
    }
}

/// Shared-module entry: build an interpreter borrowing `&*arc` while retaining an
/// owned `Arc<IrModule>` clone for detached-thread spawning.
/// Read one element from an indexable value (`string` char or `array` element) by
/// **borrowing** the container and cloning only the element, so a bare-variable
/// `a[i]` does not clone the whole container on every access.
/// The binding name of a bare local read — a `Variable` or a slot-resolved
/// `Local` — or `None` for any other expression. Lets the borrow fast paths
/// (`a[i]`, `s.field`) treat both read forms identically.
fn bare_local_name(kind: &IrExprKind) -> Option<&str> {
    match kind {
        IrExprKind::Variable(name) | IrExprKind::Local { name, .. } => Some(name),
        _ => None,
    }
}

/// Borrow the value of a bare local read without cloning: a slot-resolved
/// `Local` indexes directly (validated, falling back to the name scan on a
/// miss), a `Variable` uses the name scan. `None` for a non-local expression.
fn bare_local_ref<'e>(kind: &IrExprKind, env: &'e Env) -> Option<&'e Value> {
    match kind {
        IrExprKind::Variable(name) => env.get_ref(name),
        IrExprKind::Local { name, packed } => {
            env.get_slot(*packed, name).or_else(|| env.get_ref(name))
        }
        _ => None,
    }
}

fn index_into(container: &Value, index: i64) -> Result<Value, RuntimeError> {
    match container {
        Value::String(text) => {
            if index < 0 {
                return Err(RuntimeError::new(
                    "L0413",
                    format!("string index `{index}` is out of bounds"),
                ));
            }
            text.chars()
                .nth(index as usize)
                .map(Value::Char)
                .ok_or_else(|| {
                    RuntimeError::new("L0413", format!("string index `{index}` is out of bounds"))
                })
        }
        Value::Array(values) => {
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
        _ => Err(RuntimeError::new("L0412", "index target is not an array")),
    }
}

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
            IrStmt::Return(None) | IrStmt::Break(_) | IrStmt::Continue(_) | IrStmt::Asm { .. } => {}
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
        IrExprKind::Await { expr } => {
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
        // A closure literal node carries no body here (the body lives in the
        // module's closure table). Its own construction performs no direct memory
        // operation, so nothing is recorded at this node.
        IrExprKind::Closure { .. }
        | IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
        | IrExprKind::Variable(_)
        | IrExprKind::Local { .. } => {}
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
    /// Names of `async fn` functions, carried through to the bytecode VM so an
    /// `async fn` call spawns a thread. Serde-defaulted for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub async_functions: Vec<String>,
    /// Names of `extern fn` (C-ABI) functions, carried through so the native
    /// backend emits an external-symbol call and the bytecode VM rejects a call
    /// with `L0423`. Serde-defaulted for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extern_functions: Vec<String>,
    /// C-ABI signatures for the `extern_functions`, carried through so the native
    /// backend can marshal each extern call to the correct C scalar widths.
    /// Serde-defaulted for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extern_signatures: Vec<IrExternSignature>,
    /// Names of `export fn` functions, carried through so the native backend
    /// emits an externally visible, defined symbol under the plain C name. Purely
    /// a native-codegen concern; the bytecode VM runs an export like any function.
    /// Serde-defaulted for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub export_functions: Vec<String>,
    /// Lowered closure bodies, keyed by the parse-order id a
    /// [`BytecodeExprKind::Closure`] node carries. Round-tripped through the IR so
    /// the VM (which runs on the IR interpreter) can invoke a closure value.
    /// Serde-defaulted for compatibility.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closures: Vec<BytecodeClosureDef>,
}

/// A lowered closure body in the bytecode module: the closure's parse-order id,
/// its parameter names, and its instruction-body expression. Mirrors
/// [`IrClosureDef`], round-tripped when the bytecode module is built from / lowered
/// back to the IR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeClosureDef {
    pub id: usize,
    pub params: Vec<String>,
    pub body: BytecodeExpr,
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
    /// Inline assembly: raw x86-64 machine-code bytes emitted verbatim by the
    /// native backend. Native-only; the bytecode VM rejects it at runtime with
    /// `L0425`.
    Asm {
        bytes: Vec<u8>,
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
    /// `await EXPR`: block until the awaited `Future<T>` completes and yield its
    /// `T`. Mirrors `IrExprKind::Await`.
    Await {
        expr: Box<BytecodeExpr>,
    },
    /// An inline closure literal, carrying only its parse-order `id`. Mirrors
    /// `IrExprKind::Closure`; the body lives in `BytecodeModule::closures`.
    Closure {
        id: usize,
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
            | BytecodeInstruction::Continue(_)
            | BytecodeInstruction::Asm { .. } => {}
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
        BytecodeExprKind::Await { expr } => {
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
        // A closure literal node performs no direct memory operation.
        BytecodeExprKind::Closure { .. }
        | BytecodeExprKind::Integer(_)
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
            | BytecodeInstruction::Asm { .. }
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
        async_functions: module.async_functions.clone(),
        extern_functions: module.extern_functions.clone(),
        extern_signatures: module.extern_signatures.clone(),
        export_functions: module.export_functions.clone(),
        closures: module
            .closures
            .iter()
            .map(|def| BytecodeClosureDef {
                id: def.id,
                params: def.params.clone(),
                body: lower_bytecode_expr(&def.body),
            })
            .collect(),
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
        async_functions: module.async_functions.clone(),
        extern_functions: module.extern_functions.clone(),
        extern_signatures: module.extern_signatures.clone(),
        export_functions: module.export_functions.clone(),
        closures: module
            .closures
            .iter()
            .map(|def| IrClosureDef {
                id: def.id,
                params: def.params.clone(),
                body: bytecode_expr_to_ir(&def.body),
            })
            .collect(),
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
        IrStmt::Asm { bytes, span } => BytecodeInstruction::Asm {
            bytes: bytes.clone(),
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
        // A `Local` only exists in the interpreter's resolved copy; if one is
        // lowered to bytecode it collapses back to its name-scanned form.
        IrExprKind::Local { name, .. } => BytecodeExprKind::Variable(name.clone()),
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
        IrExprKind::Await { expr } => BytecodeExprKind::Await {
            expr: Box::new(lower_bytecode_expr(expr)),
        },
        IrExprKind::Closure { id } => BytecodeExprKind::Closure { id: *id },
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
        BytecodeInstruction::Asm { bytes, span } => IrStmt::Asm {
            bytes: bytes.clone(),
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
        BytecodeExprKind::Await { expr } => IrExprKind::Await {
            expr: Box::new(bytecode_expr_to_ir(expr)),
        },
        BytecodeExprKind::Closure { id } => IrExprKind::Closure { id: *id },
    };

    IrExpr {
        kind,
        ty: expr.ty.clone(),
        span: expr.span,
    }
}

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
struct Inliner {
    /// name -> (parameter names, body expression) for each inlinable helper.
    inlinable: HashMap<String, (Vec<String>, IrExpr)>,
    inlined_calls: usize,
}

impl Inliner {
    fn new(module: &IrModule) -> Self {
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

    fn inline_module(&mut self, module: &IrModule) -> IrModule {
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
            IrStmt::Let { .. }
            | IrStmt::Assign { .. }
            | IrStmt::Return(_)
            | IrStmt::Break(_)
            | IrStmt::Continue(_)
            | IrStmt::Throw { .. }
            | IrStmt::Try { .. }
            | IrStmt::Match { .. }
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
            // Inline assembly is opaque: clear aliases and pass the bytes through.
            IrStmt::Asm { bytes, span } => {
                aliases.clear();
                IrStmt::Asm {
                    bytes: bytes.clone(),
                    span: *span,
                }
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
            IrExprKind::Await { expr: inner } => IrExpr {
                kind: IrExprKind::Await {
                    expr: Box::new(self.propagate_expr(inner, aliases)),
                },
                ty: expr.ty.clone(),
                span: expr.span,
            },
            // A closure literal node carries only an id; its captured values are
            // materialized at runtime and its body lives in the module table, so
            // copy propagation has nothing to rewrite here.
            // A `Local` is only introduced after every optimization pass (at
            // interpretation time), so it never reaches copy propagation; copy it
            // through unchanged for match completeness.
            IrExprKind::Closure { .. }
            | IrExprKind::Integer(_)
            | IrExprKind::Float(_)
            | IrExprKind::Bool(_)
            | IrExprKind::String(_)
            | IrExprKind::Char(_)
            | IrExprKind::Local { .. } => expr.clone(),
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
        // `await` spawns/joins a thread, so it is never removable dead code.
        IrExprKind::Await { .. } => true,
        IrExprKind::Array(values) => values.iter().any(expr_requires_optimizer_barrier),
        IrExprKind::Index { .. } => true,
        // Field access is pure; only its target can require a barrier.
        IrExprKind::Field { target, .. } => expr_requires_optimizer_barrier(target),
        IrExprKind::Unary { expr, .. } => expr_requires_optimizer_barrier(expr),
        IrExprKind::Binary { left, right, .. } => {
            expr_requires_optimizer_barrier(left) || expr_requires_optimizer_barrier(right)
        }
        // Constructing a closure value only snapshots locals (no side effect), so
        // it is not an optimizer barrier — an unused closure binding is removable.
        IrExprKind::Closure { .. }
        | IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_)
        | IrExprKind::Variable(_)
        | IrExprKind::Local { .. } => false,
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

fn is_unconditional_terminator(statement: &IrStmt) -> bool {
    matches!(
        statement,
        IrStmt::Return(_) | IrStmt::Break(_) | IrStmt::Continue(_)
    )
}

/// The function value an IR `parallel_map` runs on each worker thread: either a
/// named top-level function or a self-contained capturing closure. Both are
/// `Send`, so they cross the scoped-thread boundary safely.
#[derive(Debug, Clone)]
enum IrParallelCallable {
    Func(String),
    Closure(Closure),
}

/// One entry in the interpreter's active call stack, mirroring the AST
/// runtime's `CallFrame`: the function name is *borrowed* from the program
/// (`&'a str`) so pushing a frame per call is allocation-free; owned
/// [`TraceFrame`]s are materialized only when a traceback is attached on error.
struct CallFrame<'a> {
    function: &'a str,
    span: Option<Span>,
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
    /// Per-runtime table of live external processes, mirroring the AST interpreter.
    /// A `Value::Process(i)` indexes this vector.
    processes: Vec<Option<ProcessResource>>,
    call_stack: Vec<CallFrame<'a>>,
    /// Trait-method dispatch table: `(receiver type name, method name)` -> impl
    /// function. Built once from every `impl` in the module.
    impl_methods: HashMap<(String, String), &'a IrFunction>,
    /// Names that are trait methods; a call to one dispatches via `impl_methods`.
    trait_method_names: std::collections::HashSet<String>,
    /// Names of `async fn` functions. Calling one spawns an OS thread running its
    /// body and yields a `Value::Future` that `await` resolves.
    async_functions: std::collections::HashSet<String>,
    /// Names of `extern fn` (C-ABI) functions. The interpreter cannot execute C,
    /// so a call to one raises `L0423` rather than dispatching a body.
    extern_functions: std::collections::HashSet<String>,
    /// Closure-body table: `closure id -> lowered closure def`. Built once from
    /// `module.closures`. A `Value::Closure` carries only its id, so an invocation
    /// looks its body up here. Bodies borrow the module with lifetime `'a`.
    closures: HashMap<usize, &'a IrClosureDef>,
    /// A free-list of reusable per-call environments. Each call needs a fresh
    /// `Env`; borrowing a reset one from here and returning it on the normal exit
    /// path lets deep/repeated calls reuse the scope buffers instead of
    /// reallocating. Only returned on success; error paths drop theirs.
    env_pool: Vec<Env>,
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
        // Compiler-provided `MemoryOrder` enum, registered like `option`/`result`
        // so bare `acquire`/`seq_cst`/… build the ordering `Value::Enum` consumed
        // by the ordering-taking atomic builtins and `fence`.
        for variant in MEMORY_ORDER_VARIANTS {
            variants.insert(variant, "MemoryOrder");
        }
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
        let async_functions = module.async_functions.iter().cloned().collect();
        let extern_functions = module.extern_functions.iter().cloned().collect();

        let closures = module
            .closures
            .iter()
            .map(|def| (def.id, def))
            .collect::<HashMap<_, _>>();

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
            processes: Vec::new(),
            call_stack: Vec::new(),
            impl_methods,
            trait_method_names,
            async_functions,
            extern_functions,
            closures,
            env_pool: Vec::new(),
        })
    }

    /// Spawn an `async fn` call on a new OS thread that owns a share of the
    /// module (an `Arc<IrModule>` clone) and builds its own interpreter, then
    /// return a `Value::Future` handle so `await` retrieves the produced value.
    /// The already-evaluated argument values are `Send`; heaps are per-thread.
    fn spawn_async(&self, name: &str, args: Vec<Value>) -> Value {
        let arc = Arc::clone(&self.module_arc);
        let func_name = name.to_string();
        let handle = std::thread::spawn(move || {
            let mut runtime = IrRuntime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, args)
        });
        Value::Future(Future {
            handle: Arc::new(std::sync::Mutex::new(Some(handle))),
        })
    }

    /// Safety gate for the move-on-functional-update fast path (IR twin of the
    /// AST runtime's method): true when a call to `name` is a plain builtin (or
    /// infallible enum/struct constructor) that cannot raise a *catchable*
    /// `L0420` user error, so moving the consumed argument out can never leave a
    /// moved-out placeholder observable by a surrounding `catch`. Excludes
    /// closure/func-valued variables, `extern`/`async` functions, trait methods,
    /// user functions, and `assert` (the one builtin that raises `L0420`).
    fn is_move_safe_builtin(&self, name: &str, env: &Env) -> bool {
        if matches!(
            env.get_ref(name),
            Some(Value::Closure(_)) | Some(Value::Func(_))
        ) {
            return false;
        }
        name != "assert"
            && !self.extern_functions.contains(name)
            && !self.async_functions.contains(name)
            && !self.trait_method_names.contains(name)
            && !self.functions.contains_key(name)
    }

    /// The move-on-functional-update fast path for the `x = f(x, …)` (CALL) and
    /// `x = x <binop> e` / `x = e <binop> x` (BINARY) accumulation idioms (IR twin
    /// of the AST runtime's method). When the target `name` appears exactly once —
    /// as a bare call argument, or as exactly one bare operand of a binary op —
    /// and nowhere else in the RHS, and `name` is a local, this evaluates the RHS
    /// with that occurrence **moved** out of the environment instead of cloned and
    /// returns `Some(result)`. `None` means the caller must fall back to the
    /// ordinary clone path. The binary form makes `s = s + piece` loops O(n) by
    /// letting `eval_binary` reuse the moved left operand's buffer. See the AST
    /// runtime for the full safety argument; the two implementations are kept in
    /// lockstep.
    fn try_move_functional_update(
        &mut self,
        name: &str,
        rhs: &IrExpr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        match &rhs.kind {
            IrExprKind::Call { name: callee, args } => {
                self.try_move_call_update(name, callee, args, env, require_innermost)
            }
            IrExprKind::Binary { op, left, right } => {
                self.try_move_binary_update(name, *op, left, right, env, require_innermost)
            }
            _ => Ok(None),
        }
    }

    /// `x = f(x, …)` arm of [`Self::try_move_functional_update`].
    fn try_move_call_update(
        &mut self,
        name: &str,
        callee: &str,
        args: &[IrExpr],
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        if callee == name {
            return Ok(None);
        }
        if !self.is_move_safe_builtin(callee, env) {
            return Ok(None);
        }
        // For a `let` re-binding the binding must be innermost (`let` shadows into
        // the innermost scope); for a plain reassignment any-scope is fine (moved
        // from, and written back to, the nearest binding).
        let bound = if require_innermost {
            env.innermost_has(name)
        } else {
            env.is_bound(name)
        };
        if !bound {
            return Ok(None);
        }
        let mut target_idx: Option<usize> = None;
        for (i, arg) in args.iter().enumerate() {
            let is_bare = bare_local_name(&arg.kind) == Some(name);
            if is_bare && target_idx.is_none() {
                target_idx = Some(i);
            } else if expr_mentions_var(arg, name) {
                return Ok(None);
            }
        }
        let Some(target_idx) = target_idx else {
            return Ok(None);
        };
        // Evaluate every *other* argument first, in source order, so a failure
        // there leaves `name` intact and the env consistent.
        let mut evaluated: Vec<Option<Value>> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if i == target_idx {
                evaluated.push(None);
            } else {
                evaluated.push(Some(self.eval_expr(arg, env)?));
            }
        }
        let moved = env
            .move_out_nearest(name)
            .expect("target verified bound as a local");
        let mut moved = Some(moved);
        let values: Vec<Value> = evaluated
            .into_iter()
            .enumerate()
            .map(|(i, slot)| {
                if i == target_idx {
                    moved.take().expect("single target slot")
                } else {
                    slot.expect("non-target slots are evaluated")
                }
            })
            .collect();
        Ok(Some(self.call_function(callee, values)?))
    }

    /// `x = x <binop> e` / `x = e <binop> x` arm of
    /// [`Self::try_move_functional_update`]. Fires when exactly one operand is the
    /// bare variable `name` and `name` appears nowhere else in either operand.
    /// Short-circuit `and`/`or` are excluded (they do not route through
    /// `eval_binary` and their right operand is conditional).
    fn try_move_binary_update(
        &mut self,
        name: &str,
        op: BinaryOp,
        left: &IrExpr,
        right: &IrExpr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            return Ok(None);
        }
        let bound = if require_innermost {
            env.innermost_has(name)
        } else {
            env.is_bound(name)
        };
        if !bound {
            return Ok(None);
        }
        let left_bare = bare_local_name(&left.kind) == Some(name);
        let right_bare = bare_local_name(&right.kind) == Some(name);
        let target_is_left = if left_bare && !expr_mentions_var(right, name) {
            true
        } else if right_bare && !expr_mentions_var(left, name) {
            false
        } else {
            return Ok(None);
        };
        // Evaluate the non-target operand *before* moving the target.
        let other = if target_is_left {
            self.eval_expr(right, env)?
        } else {
            self.eval_expr(left, env)?
        };
        let moved = env
            .move_out_nearest(name)
            .expect("target verified bound as a local");
        let (l, r) = if target_is_left {
            (moved, other)
        } else {
            (other, moved)
        };
        Ok(Some(self.eval_binary(l, op, r)?))
    }

    /// Dispatch a call to an already-resolved top-level function name: reject an
    /// `extern fn` (C-ABI, native-only) with `L0423`, spawn an `async fn` on its
    /// own OS thread yielding a `Future`, or invoke the function / builtin /
    /// constructor synchronously through [`Self::call_function`].
    fn dispatch_named_call(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if self.extern_functions.contains(name) {
            return Err(extern_call_error(name));
        }
        if self.async_functions.contains(name) {
            Ok(self.spawn_async(name, args))
        } else {
            self.call_function(name, args)
        }
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
            return Ok(Value::Enum(Box::new(EnumValue {
                enum_name: enum_name.to_string(),
                variant: name.to_string(),
                payload: args,
            })));
        }
        if let Some(field_names) = self.structs.get(name) {
            return Ok(Value::Struct(Box::new(StructValue {
                name: name.to_string(),
                fields: field_names.iter().cloned().zip(args).collect(),
            })));
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
            "console_log" => self.builtin_console_log(args),
            "dom_set_text" => self.builtin_dom_set_text(args),
            "flush" => self.builtin_flush(args),
            "mono_now" => Self::builtin_mono_now(args),
            "wall_now" => Self::builtin_wall_now(args),
            "sleep_millis" => Self::builtin_sleep_millis(args),
            "assert" => Self::builtin_assert(args),
            "to_string" => Self::builtin_to_string(args),
            "char_code" => Self::builtin_char_code(args),
            "char_from" => Self::builtin_char_from(args),
            "is_digit" => Self::builtin_is_digit(args),
            "is_alpha" => Self::builtin_is_alpha(args),
            "is_alnum" => Self::builtin_is_alnum(args),
            "is_whitespace" => Self::builtin_is_whitespace(args),
            "is_upper" => Self::builtin_is_upper(args),
            "is_lower" => Self::builtin_is_lower(args),
            "byte" => Self::builtin_byte(args),
            "byte_val" => Self::builtin_byte_val(args),
            "to_i8" => Self::builtin_to_int("to_i8", args, IntKind::I8),
            "to_u8" => Self::builtin_to_int("to_u8", args, IntKind::U8),
            "to_i16" => Self::builtin_to_int("to_i16", args, IntKind::I16),
            "to_i32" => Self::builtin_to_int("to_i32", args, IntKind::I32),
            "to_u16" => Self::builtin_to_int("to_u16", args, IntKind::U16),
            "to_u32" => Self::builtin_to_int("to_u32", args, IntKind::U32),
            "to_u64" => Self::builtin_to_int("to_u64", args, IntKind::U64),
            "to_isize" => Self::builtin_to_int("to_isize", args, IntKind::Isize),
            "to_usize" => Self::builtin_to_int("to_usize", args, IntKind::Usize),
            "to_i64" => Self::builtin_to_i64(args),
            "to_f32" => Self::builtin_to_f32(args),
            "to_f64" => Self::builtin_to_f64(args),
            "checked_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Checked),
            "checked_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Checked),
            "checked_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Checked),
            "saturating_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Saturating),
            "saturating_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Saturating),
            "saturating_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Saturating),
            "wrapping_add" => overflow_arith(name, args, ArithOp::Add, OverflowMode::Wrapping),
            "wrapping_sub" => overflow_arith(name, args, ArithOp::Sub, OverflowMode::Wrapping),
            "wrapping_mul" => overflow_arith(name, args, ArithOp::Mul, OverflowMode::Wrapping),
            "len" => Self::builtin_len(args),
            "array_fill" => Self::builtin_array_fill(args),
            "list_new" => Self::builtin_list_new(args),
            "push" => Self::builtin_push(args),
            "get" => Self::builtin_get(args),
            "set" => Self::builtin_set(args),
            "pop" => Self::builtin_pop(args),
            "list_index_of" => Self::builtin_list_index_of(args),
            "list_contains" => Self::builtin_list_contains(args),
            "reverse" => Self::builtin_reverse(args),
            "sort" => Self::builtin_sort(args),
            "sort_by" => self.builtin_sort_by(args),
            "concat" => Self::builtin_concat(args),
            "slice" => Self::builtin_slice(args),
            "list_map" => self.builtin_list_map(args),
            "list_filter" => self.builtin_list_filter(args),
            "list_reduce" => self.builtin_list_reduce(args),
            "map_new" => Self::builtin_map_new(args),
            "map_set" => Self::builtin_map_set(args),
            "map_get" => Self::builtin_map_get(args),
            "map_has" => Self::builtin_map_has(args),
            "map_len" => Self::builtin_map_len(args),
            "map_keys" => Self::builtin_map_keys(args),
            "map_values" => Self::builtin_map_values(args),
            "map_del" => Self::builtin_map_del(args),
            "substring" => Self::builtin_substring(args),
            "find" => Self::builtin_find(args),
            "contains" => Self::builtin_contains(args),
            "starts_with" => Self::builtin_starts_with(args),
            "ends_with" => Self::builtin_ends_with(args),
            "repeat" => Self::builtin_repeat(args),
            "split" => Self::builtin_split(args),
            // `words`/`count` yield to a user-defined function of the same name, so
            // adding these common stdlib names never breaks existing user code.
            "words" if !self.functions.contains_key("words") => Self::builtin_words(args),
            "count" if !self.functions.contains_key("count") => Self::builtin_count(args),
            "join" => Self::builtin_join(args),
            "trim" => Self::builtin_trim(args),
            "replace" => Self::builtin_replace(args),
            "upper" => Self::builtin_upper(args),
            "chars" => Self::builtin_chars(args),
            "string_from_chars" => Self::builtin_string_from_chars(args),
            "lower" => Self::builtin_lower(args),
            "to_bytes" => Self::builtin_to_bytes(args),
            "from_bytes" => Self::builtin_from_bytes(args),
            "byte_len" => Self::builtin_byte_len(args),
            "parse_i64" => Self::builtin_parse_i64(args),
            "parse_f64" => Self::builtin_parse_f64(args),
            "abs" => Self::builtin_abs(args),
            "min" => Self::builtin_min(args),
            "max" => Self::builtin_max(args),
            "clamp" => Self::builtin_clamp(args),
            "sign" => Self::builtin_sign(args),
            "gcd" => Self::builtin_gcd(args),
            "list_sum" => Self::builtin_list_sum(args),
            "list_min" => Self::builtin_list_min(args),
            "list_max" => Self::builtin_list_max(args),
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
            "rotate_left" => Self::builtin_rotate_left(args),
            "rotate_right" => Self::builtin_rotate_right(args),
            "count_ones" => Self::builtin_count_ones(args),
            "leading_zeros" => Self::builtin_leading_zeros(args),
            "trailing_zeros" => Self::builtin_trailing_zeros(args),
            "reverse_bytes" => Self::builtin_reverse_bytes(args),
            "floor" => Self::builtin_floor(args),
            "ceil" => Self::builtin_ceil(args),
            "round" => Self::builtin_round(args),
            "rc_new" => self.builtin_rc_new(args),
            "rc_clone" => self.builtin_rc_clone(args),
            "rc_release" => self.builtin_rc_release(args),
            "rc_get" | "ref_get" | "ptr_read" => self.builtin_ref_get(name, args),
            "rc_borrow" => self.builtin_rc_borrow(args),
            "ptr_write" => self.builtin_store(args),
            "size_of" => Self::builtin_size_of(args),
            "align_of" => Self::builtin_align_of(args),
            "offset_of" => Self::builtin_offset_of(args),
            "ptr_to_int" => Self::builtin_ptr_to_int(args),
            "int_to_ptr" => Self::builtin_int_to_ptr(args),
            // Volatile raw-memory access behaves exactly like `load`/`store` on
            // the interpreters' single-threaded abstract heap; the no-elision /
            // no-reordering guarantee is a native-codegen concern.
            "volatile_load" => self.builtin_load(args),
            "volatile_store" => self.builtin_store(args),
            "env" => Self::builtin_env(args),
            "os_random" => Self::builtin_os_random(args),
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
            "atomic_new" => Self::builtin_atomic_new(args),
            "atomic_load" => Self::builtin_atomic_load(args),
            "atomic_store" => Self::builtin_atomic_store(args),
            "atomic_swap" => Self::builtin_atomic_swap(args),
            "atomic_cas" => Self::builtin_atomic_cas(args),
            "atomic_add" => Self::builtin_atomic_add(args),
            "atomic_sub" => Self::builtin_atomic_sub(args),
            "atomic_and" => Self::builtin_atomic_and(args),
            "atomic_or" => Self::builtin_atomic_or(args),
            "atomic_xor" => Self::builtin_atomic_xor(args),
            "atomic_load_ordered" => builtin_atomic_load_ordered(args),
            "atomic_store_ordered" => builtin_atomic_store_ordered(args),
            "atomic_swap_ordered" => builtin_atomic_swap_ordered(args),
            "atomic_cas_ordered" => builtin_atomic_cas_ordered(args),
            "atomic_add_ordered" => builtin_atomic_add_ordered(args),
            "atomic_sub_ordered" => builtin_atomic_sub_ordered(args),
            "atomic_and_ordered" => builtin_atomic_and_ordered(args),
            "atomic_or_ordered" => builtin_atomic_or_ordered(args),
            "atomic_xor_ordered" => builtin_atomic_xor_ordered(args),
            "fence" => builtin_fence(args),
            "tcp_connect" => self.builtin_tcp_connect(args),
            "tcp_listen" => self.builtin_tcp_listen(args),
            "tcp_accept" => self.builtin_tcp_accept(args),
            "tcp_accept_nb" => self.builtin_tcp_accept_nb(args),
            "tcp_read" => self.builtin_tcp_read(args),
            "tcp_read_nb" => self.builtin_tcp_read_nb(args),
            "tcp_write" => self.builtin_tcp_write(args),
            "tcp_shutdown" => self.builtin_tcp_shutdown(args),
            "tcp_close" => self.builtin_socket_close(args),
            "set_nonblocking" => self.builtin_set_nonblocking(args),
            "udp_bind" => self.builtin_udp_bind(args),
            "udp_send_to" => self.builtin_udp_send_to(args),
            "udp_recv" => self.builtin_udp_recv(args),
            "udp_recv_nb" => self.builtin_udp_recv_nb(args),
            "http_get" => Self::builtin_http_get(args),
            "http_post" => Self::builtin_http_post(args),
            "proc_spawn" => self.builtin_proc_spawn(args),
            "proc_wait" => self.builtin_proc_wait(args),
            "proc_stdout" => self.builtin_proc_stdout(args),
            "proc_stderr" => self.builtin_proc_stderr(args),
            "proc_kill" => self.builtin_proc_kill(args),
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

        // Borrow a reset environment from the pool (or make a fresh one) instead
        // of allocating per call; returned to the pool on the normal exit below.
        let mut env = match self.env_pool.pop() {
            Some(mut env) => {
                env.reset();
                env
            }
            None => Env::default(),
        };
        for (param, value) in function.params.iter().zip(args) {
            env.define(param.name.clone(), value);
        }

        self.call_stack.push(CallFrame {
            function: function.name.as_str(),
            span: Some(function.span),
        });
        let result = self.eval_block(&function.body, &mut env);

        // Attach the traceback lazily. `with_traceback` records only the first
        // (innermost) stack, so eagerly cloning `call_stack` on every successful
        // call — and on every frame an error merely passes through — is pure
        // waste that grows with recursion depth. Clone it only when this frame is
        // the one first recording a traceback, while the frame is still on the
        // stack so it is included.
        let control = match result {
            Err(error) => {
                let error = if error.traceback.is_empty() {
                    error.with_traceback(self.build_traceback())
                } else {
                    error
                };
                self.call_stack.pop();
                return Err(error);
            }
            Ok(control) => {
                self.call_stack.pop();
                control
            }
        };
        // Normal exit: return the environment to the pool; error paths drop theirs.
        self.env_pool.push(env);

        match control {
            Control::Return(value) | Control::Value(value) => Ok(value),
            Control::Break | Control::Continue => Err(RuntimeError::new(
                "L0410",
                "loop control escaped function body",
            )),
        }
    }

    /// Invoke a closure value: look its body up in the id-keyed closure table,
    /// bind the captured snapshot first and then the parameters (parameters shadow
    /// captures), evaluate the single-expression body, and return the value.
    /// Mirrors the AST runtime's `invoke_closure` one-to-one for backend parity.
    fn invoke_closure(
        &mut self,
        closure: &Closure,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let def = *self.closures.get(&closure.id).ok_or_else(|| {
            RuntimeError::new(
                "L0402",
                format!("closure #{} has no registered body", closure.id),
            )
        })?;
        if def.params.len() != args.len() {
            return Err(RuntimeError::new(
                "L0402",
                format!(
                    "closure expects {} arguments but got {}",
                    def.params.len(),
                    args.len()
                ),
            ));
        }
        let mut env = Env::default();
        for (name, value) in &closure.captured {
            env.define(name.clone(), value.clone());
        }
        for (name, value) in def.params.iter().zip(args) {
            env.define(name.clone(), value);
        }
        self.eval_expr(&def.body, &env)
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
                // Move-on-functional-update fast path: `let x = f(x, …)` re-binding
                // an existing innermost local consumes it by move, not clone.
                let value = match self.try_move_functional_update(name, value, env, true)? {
                    Some(result) => result,
                    None => self.eval_expr(value, env)?,
                };
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
                if path.is_empty() && matches!(op, AssignOp::Replace) {
                    // Whole-variable reassignment `x = RHS`: try the
                    // move-on-functional-update fast path (`x = f(x, …)`) before
                    // falling back to the ordinary clone path.
                    let new = match self.try_move_functional_update(name, value, env, false)? {
                        Some(result) => result,
                        None => self.eval_expr(value, env)?,
                    };
                    env.assign(name, new)?;
                } else {
                    let rhs = self.eval_expr(value, env)?;
                    if path.is_empty() {
                        let new = apply_compound(env.get(name)?, op, rhs)?;
                        env.assign(name, new)?;
                    } else {
                        // Mutate the element/field in place instead of cloning the
                        // whole array/struct, mutating a copy, and writing it back
                        // (O(len) per write, O(len^2) in a write loop).
                        let resolved = self.resolve_places(path, env)?;
                        let root = env.get_mut(name).ok_or_else(|| {
                            RuntimeError::new("L0403", format!("unknown variable `{name}`"))
                        })?;
                        let new = match op {
                            AssignOp::Replace => rhs,
                            _ => apply_compound(get_place(root, &resolved)?, op, rhs)?,
                        };
                        set_place(root, &resolved, new)?;
                    }
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

                // Bind the loop variable once and update it in place each pass
                // rather than re-`define`ing it (which clones the name and
                // reallocates a scope every iteration — measured ~2x on for-loops).
                // The body still runs in a fresh scope so its `let`s are cleared.
                // The final `pop_scope` always runs so the stack stays balanced for
                // `try`/`catch`.
                env.push_scope();
                env.define(name.clone(), Value::I64(current));
                let outcome: Result<Control, RuntimeError> = loop {
                    let running = if step > 0 {
                        current <= end
                    } else {
                        current >= end
                    };
                    if !running {
                        break Ok(Control::Value(Value::Void));
                    }
                    env.set_loop_var(name, Value::I64(current));
                    env.push_scope();
                    let result = self.eval_block(body, env);
                    env.pop_scope();

                    match result {
                        Ok(Control::Return(value)) => break Ok(Control::Return(value)),
                        Ok(Control::Break) => break Ok(Control::Value(Value::Void)),
                        Ok(Control::Continue) | Ok(Control::Value(_)) => {}
                        Err(error) => break Err(error),
                    }

                    current += step;
                };
                env.pop_scope();
                outcome
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
            // Inline assembly cannot run on the IR interpreter (raw machine code
            // requires native codegen + linking); reject it with `L0425`.
            IrStmt::Asm { .. } => Err(asm_interpreter_error()),
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
        let Value::Enum(e) = value else {
            return Err(RuntimeError::new(
                "L0383",
                "match scrutinee did not evaluate to an enum value",
            ));
        };
        let EnumValue {
            variant, payload, ..
        } = *e;
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

    /// Resolve a bare name to a value: a local binding (name scan), else a known
    /// unit enum variant, else a first-class function value. Shared by the
    /// `Variable` and (on a slot miss) `Local` evaluation arms.
    fn eval_variable_name(&self, name: &str, env: &Env) -> Result<Value, RuntimeError> {
        match env.get(name) {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(enum_name) = self.variants.get(name) {
                    Ok(Value::Enum(Box::new(EnumValue {
                        enum_name: enum_name.to_string(),
                        variant: name.to_string(),
                        payload: Vec::new(),
                    })))
                } else if self.functions.contains_key(name) {
                    Ok(Value::Func(name.to_string()))
                } else {
                    Err(error)
                }
            }
        }
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
            // A slot-resolved local read: index the binding directly (no name
            // scan). The lookup is validated, so a miss (resolver/runtime scope
            // divergence, or a name that is really an enum variant / function
            // rather than a local) falls back to the exact `Variable` path.
            IrExprKind::Local { name, packed } => match env.get_slot(*packed, name) {
                Some(value) => Ok(value.clone()),
                None => self.eval_variable_name(name, env),
            },
            IrExprKind::Variable(name) => self.eval_variable_name(name, env),
            IrExprKind::Index { target, index } => {
                // Fast path: a bare-variable target (name-scanned or slot-resolved)
                // is borrowed (clone only the element), so `a[i]` does not clone the
                // whole array/string every access (which is O(len) per read).
                if bare_local_name(&target.kind).is_some() {
                    let idx = self.eval_expr(index, env)?.as_i64()?;
                    if let Some(container) = bare_local_ref(&target.kind, env) {
                        return index_into(container, idx);
                    }
                    let owned = self.eval_expr(target, env)?;
                    return index_into(&owned, idx);
                }
                let target = self.eval_expr(target, env)?;
                let index = self.eval_expr(index, env)?.as_i64()?;
                index_into(&target, index)
            }
            IrExprKind::Field { target, field } => {
                // Fast path: borrow a bare-variable struct (name-scanned or
                // slot-resolved) and clone only the field read, instead of cloning
                // the whole struct on every `s.field`.
                if let Some(Value::Struct(s)) = bare_local_ref(&target.kind, env) {
                    return s
                        .fields
                        .iter()
                        .find(|(n, _)| n == field)
                        .map(|(_, value)| value.clone())
                        .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`")));
                }
                match self.eval_expr(target, env)? {
                    Value::Struct(s) => s
                        .fields
                        .into_iter()
                        .find(|(name, _)| name == field)
                        .map(|(_, value)| value)
                        .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`"))),
                    _ => Err(RuntimeError::new(
                        "L0371",
                        format!("cannot access field `{field}` on non-struct value"),
                    )),
                }
            }
            IrExprKind::Unary { op, expr } => {
                let value = self.eval_expr(expr, env)?;
                match op {
                    UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
                    // Bitwise NOT (one's complement); a fixed-width integer is
                    // re-normalized to its width.
                    UnaryOp::BitNot => match value {
                        Value::Int { value, ty } => Ok(Value::int(!value, ty)),
                        other => Ok(Value::I64(!other.as_i64()?)),
                    },
                    // Arithmetic negation, preserving the operand's numeric type.
                    UnaryOp::Negate => match value {
                        Value::Int { value, ty } => Ok(Value::int(value.wrapping_neg(), ty)),
                        Value::F64(f) => Ok(Value::F64(-f)),
                        Value::F32(f) => Ok(Value::F32(-f)),
                        other => Ok(Value::I64(other.as_i64()?.wrapping_neg())),
                    },
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
                // Resolve the call target with a single borrowing lookup. A call
                // name bound to a closure value invokes that closure (binding its
                // captured snapshot then the arguments); a name bound to a
                // function value dispatches through it; otherwise `name` is a plain
                // top-level function / builtin / constructor. Using `get_ref`
                // keeps the common case — an ordinary top-level call, where `name`
                // is not a local — free of the clone and the discarded "unknown
                // variable" error a bare `env.get` allocates on every such call.
                let target: &str = match env.get_ref(name) {
                    Some(Value::Closure(closure)) => {
                        let closure = closure.clone();
                        return self.invoke_closure(&closure, values);
                    }
                    Some(Value::Func(func)) => {
                        let func = func.clone();
                        return self.dispatch_named_call(&func, values);
                    }
                    _ => name,
                };
                self.dispatch_named_call(target, values)
            }
            IrExprKind::Await { expr } => {
                let value = self.eval_expr(expr, env)?;
                let future = expect_future("await", value)?;
                await_future(&future)
            }
            // Evaluating a closure literal snapshots the current environment's
            // in-scope locals by value and yields a `Value::Closure` carrying the
            // literal's id plus that snapshot. The body lives in `self.closures`
            // (keyed by id) and is looked up at invocation time, mirroring the AST
            // runtime exactly for backend parity.
            IrExprKind::Closure { id } => Ok(Value::Closure(Closure {
                id: *id,
                captured: env.snapshot_locals(),
            })),
        };
        result.map_err(|error| self.annotate_error(error, expr.span))
    }

    fn annotate_error(&self, error: RuntimeError, span: Span) -> RuntimeError {
        let error = error.with_span(span);
        match self.call_stack.last() {
            Some(frame) => error
                .with_function(frame.function.to_string())
                .with_traceback(self.build_traceback()),
            None => error,
        }
    }

    /// Materialize the active call stack as owned [`TraceFrame`]s for a
    /// `RuntimeError` — called only on the error path, so the per-frame name
    /// clone stays off the hot call path (the live `call_stack` borrows each
    /// name from the program).
    fn build_traceback(&self) -> Vec<TraceFrame> {
        self.call_stack
            .iter()
            .map(|frame| TraceFrame {
                function: frame.function.to_string(),
                span: frame.span,
            })
            .collect()
    }

    fn eval_binary(&self, left: Value, op: BinaryOp, right: Value) -> Result<Value, RuntimeError> {
        if let (Value::F64(l), Value::F64(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F64(l + r),
                BinaryOp::Subtract => Value::F64(l - r),
                BinaryOp::Multiply => Value::F64(l * r),
                BinaryOp::Divide => Value::F64(l / r),
                BinaryOp::Remainder => {
                    unreachable!("`%` requires integer operands (rejected by semantics)")
                }
                BinaryOp::Equal => Value::Bool(l == r),
                BinaryOp::NotEqual => Value::Bool(l != r),
                BinaryOp::Less => Value::Bool(l < r),
                BinaryOp::LessEqual => Value::Bool(l <= r),
                BinaryOp::Greater => Value::Bool(l > r),
                BinaryOp::GreaterEqual => Value::Bool(l >= r),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
                BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
                | BinaryOp::Shl
                | BinaryOp::Shr => {
                    unreachable!("bitwise ops require i64 operands (rejected by semantics)")
                }
            });
        }
        // 32-bit float arithmetic/comparison, identical to the AST runtime; the
        // native f32 storage rounds each result to f32 precision.
        if let (Value::F32(l), Value::F32(r)) = (&left, &right) {
            let (l, r) = (*l, *r);
            return Ok(match op {
                BinaryOp::Add => Value::F32(l + r),
                BinaryOp::Subtract => Value::F32(l - r),
                BinaryOp::Multiply => Value::F32(l * r),
                BinaryOp::Divide => Value::F32(l / r),
                BinaryOp::Remainder => {
                    unreachable!("`%` requires integer operands (rejected by semantics)")
                }
                BinaryOp::Equal => Value::Bool(l == r),
                BinaryOp::NotEqual => Value::Bool(l != r),
                BinaryOp::Less => Value::Bool(l < r),
                BinaryOp::LessEqual => Value::Bool(l <= r),
                BinaryOp::Greater => Value::Bool(l > r),
                BinaryOp::GreaterEqual => Value::Bool(l >= r),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
                BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
                | BinaryOp::Shl
                | BinaryOp::Shr => {
                    unreachable!("bitwise ops require i64 operands (rejected by semantics)")
                }
            });
        }
        // Fixed-width integer arithmetic/comparison, identical to the AST runtime:
        // same-tag operands, wrap-normalized result, plain `i64` ordering of the
        // normalized cells. Kept byte-for-byte in step with the other backends.
        if let (Value::Int { value: l, ty }, Value::Int { value: r, ty: rk }) = (&left, &right) {
            debug_assert_eq!(ty, rk, "mixed-width integer operands reached eval_binary");
            let (l, r, ty) = (*l, *r, *ty);
            return match op {
                BinaryOp::Add => Ok(Value::int(l.wrapping_add(r), ty)),
                BinaryOp::Subtract => Ok(Value::int(l.wrapping_sub(r), ty)),
                BinaryOp::Multiply => Ok(Value::int(l.wrapping_mul(r), ty)),
                BinaryOp::Divide => {
                    if r == 0 {
                        Err(RuntimeError::new("L0404", "division by zero"))
                    } else {
                        Ok(Value::int(int_div(l, r, ty), ty))
                    }
                }
                BinaryOp::Remainder => {
                    if r == 0 {
                        Err(RuntimeError::new("L0404", "remainder by zero"))
                    } else {
                        Ok(Value::int(int_rem(l, r, ty), ty))
                    }
                }
                BinaryOp::Equal => Ok(Value::Bool(l == r)),
                BinaryOp::NotEqual => Ok(Value::Bool(l != r)),
                BinaryOp::Less => Ok(Value::Bool(int_cmp(l, r, ty).is_lt())),
                BinaryOp::LessEqual => Ok(Value::Bool(int_cmp(l, r, ty).is_le())),
                BinaryOp::Greater => Ok(Value::Bool(int_cmp(l, r, ty).is_gt())),
                BinaryOp::GreaterEqual => Ok(Value::Bool(int_cmp(l, r, ty).is_ge())),
                // Bitwise ops mirror the AST runtime exactly.
                BinaryOp::BitAnd => Ok(Value::int(l & r, ty)),
                BinaryOp::BitOr => Ok(Value::int(l | r, ty)),
                BinaryOp::BitXor => Ok(Value::int(l ^ r, ty)),
                BinaryOp::Shl => Ok(Value::int(int_shl(l, r, ty), ty)),
                BinaryOp::Shr => Ok(Value::int(int_shr(l, r, ty), ty)),
                BinaryOp::And | BinaryOp::Or => {
                    unreachable!("logical ops short-circuit in eval_expr")
                }
            };
        }
        match op {
            BinaryOp::Add if matches!((&left, &right), (Value::String(_), Value::String(_))) => {
                // Reuse the left operand's heap buffer (see the AST runtime): the
                // `String + &str` is a `push_str`, keeping `s = s + piece` loops
                // O(n) overall rather than reallocating on every concat.
                Ok(Value::String(left.into_string()? + &right.as_string()?))
            }
            BinaryOp::Add => Ok(Value::I64(left.as_i64()? + right.as_i64()?)),
            BinaryOp::Subtract => Ok(Value::I64(left.as_i64()? - right.as_i64()?)),
            BinaryOp::Multiply => Ok(Value::I64(left.as_i64()? * right.as_i64()?)),
            BinaryOp::Divide => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "division by zero"))
                } else {
                    // Wrap `i64::MIN / -1` to `i64::MIN` (rather than panicking),
                    // matching the AST runtime and the native backend.
                    Ok(Value::I64(left.as_i64()?.wrapping_div(divisor)))
                }
            }
            BinaryOp::Remainder => {
                let divisor = right.as_i64()?;
                if divisor == 0 {
                    Err(RuntimeError::new("L0404", "remainder by zero"))
                } else {
                    Ok(Value::I64(left.as_i64()?.wrapping_rem(divisor)))
                }
            }
            BinaryOp::Equal => Ok(Value::Bool(left == right)),
            BinaryOp::NotEqual => Ok(Value::Bool(left != right)),
            // String ordering is lexicographic by Unicode code point, matching
            // the AST runtime.
            BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
                if matches!((&left, &right), (Value::String(_), Value::String(_))) =>
            {
                let (l, r) = (left.as_string()?, right.as_string()?);
                Ok(Value::Bool(match op {
                    BinaryOp::Less => l < r,
                    BinaryOp::LessEqual => l <= r,
                    BinaryOp::Greater => l > r,
                    BinaryOp::GreaterEqual => l >= r,
                    _ => unreachable!("guarded to ordering operators"),
                }))
            }
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
            // Integer bitwise ops on two i64s, using the shared masked-shift
            // helpers so the AST, IR, and bytecode backends are bit-identical.
            BinaryOp::BitAnd => Ok(Value::I64(left.as_i64()? & right.as_i64()?)),
            BinaryOp::BitOr => Ok(Value::I64(left.as_i64()? | right.as_i64()?)),
            BinaryOp::BitXor => Ok(Value::I64(left.as_i64()? ^ right.as_i64()?)),
            BinaryOp::Shl => Ok(Value::I64(shift_left(left.as_i64()?, right.as_i64()?))),
            BinaryOp::Shr => Ok(Value::I64(shift_right(left.as_i64()?, right.as_i64()?))),
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

    /// `size_of(x) -> i64`: the C-natural byte size of `x`'s type. See
    /// [`Value::layout_size`].
    fn builtin_size_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("size_of", 1, args.len()))?;
        value.layout_size().map(Value::I64).ok_or_else(|| {
            RuntimeError::new(
                "L0431",
                "size_of requires a type with a defined memory layout",
            )
        })
    }

    /// `align_of(x) -> i64`: the C-natural alignment of `x`'s type.
    fn builtin_align_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("align_of", 1, args.len()))?;
        value.layout_align().map(Value::I64).ok_or_else(|| {
            RuntimeError::new(
                "L0431",
                "align_of requires a type with a defined memory layout",
            )
        })
    }

    /// `offset_of(x, "field") -> i64`: the C-natural byte offset of `field`
    /// within struct value `x`.
    fn builtin_offset_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value, field]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("offset_of", 2, args.len()))?;
        let field = field.as_string()?;
        value
            .layout_field_offset(&field)
            .map(Value::I64)
            .ok_or_else(|| {
                RuntimeError::new(
                    "L0431",
                    format!("offset_of could not resolve field `{field}` in a struct value"),
                )
            })
    }

    /// `ptr_to_int(p) -> i64`: the integer handle of a raw pointer; round-trips
    /// with `int_to_ptr`.
    fn builtin_ptr_to_int(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ptr]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("ptr_to_int", 1, args.len()))?;
        Ok(Value::I64(ptr.as_ptr()? as i64))
    }

    /// `int_to_ptr(n) -> ptr<T>`: reconstruct a raw pointer from an integer
    /// handle (the inverse of `ptr_to_int`).
    fn builtin_int_to_ptr(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [handle]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("int_to_ptr", 1, args.len()))?;
        Ok(Value::Ptr(handle.as_i64()? as usize))
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

    /// `mono_now() -> i64`: nanoseconds since a fixed per-process monotonic
    /// baseline. Non-decreasing within a run. Routes through the shared
    /// `monotonic_now_nanos` baseline so the IR interpreter, the bytecode VM
    /// (which runs on this interpreter), and the AST runtime agree.
    fn builtin_mono_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("mono_now", 0, args.len()))?;
        Ok(Value::I64(monotonic_now_nanos()))
    }

    /// `wall_now() -> i64`: milliseconds since the Unix epoch (wall-clock time).
    fn builtin_wall_now(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("wall_now", 0, args.len()))?;
        Ok(Value::I64(wall_now_millis()))
    }

    /// `sleep_millis(ms i64) -> void`: sleep the current thread for `ms`
    /// milliseconds; a negative `ms` sleeps for zero (no error).
    fn builtin_sleep_millis(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [ms]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sleep_millis", 1, args.len()))?;
        let ms = expect_i64("sleep_millis", ms)?;
        sleep_millis(ms);
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

    /// `console_log(s string) -> void`: the JS/DOM host console builtin. On the
    /// WASM backend it lowers to a `call` of the imported
    /// `env.console_log(ptr, len)`; on the interpreters it prints the string as a
    /// stdout line, kept at parity with the AST runtime so all backends observe
    /// the same side effect.
    fn builtin_console_log(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("console_log", 1, args.len()))?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{text}").map_err(|error| {
            RuntimeError::resource("L0419", format!("failed to write to stdout: {error}"))
        })?;
        Ok(Value::Void)
    }

    /// `dom_set_text(id string, text string) -> void`: the DOM-write primitive. On
    /// the WASM backend it lowers to a `call` of the imported
    /// `env.dom_set_text(id_ptr, id_len, text_ptr, text_len)`; on the interpreters
    /// it prints the deterministic line `id=text`, kept at parity with the AST
    /// runtime so all backends observe the same side effect.
    fn builtin_dom_set_text(&self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Write;
        let [id, text]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("dom_set_text", 2, args.len()))?;
        let id = id.as_string()?;
        let text = text.as_string()?;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        writeln!(handle, "{id}={text}").map_err(|error| {
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
            | Value::Int { .. }
            | Value::F64(_)
            | Value::F32(_)
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

    /// `is_digit(c char) -> bool`: whether `c` is an ASCII digit (`0`-`9`).
    fn builtin_is_digit(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_digit", args, |c| c.is_ascii_digit())
    }

    /// `is_alpha(c char) -> bool`: whether `c` is an alphabetic character.
    fn builtin_is_alpha(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alpha", args, |c| c.is_alphabetic())
    }

    /// `is_alnum(c char) -> bool`: whether `c` is alphabetic or numeric.
    fn builtin_is_alnum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_alnum", args, |c| c.is_alphanumeric())
    }

    /// `is_whitespace(c char) -> bool`: whether `c` is a whitespace character.
    fn builtin_is_whitespace(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_whitespace", args, |c| c.is_whitespace())
    }

    /// `is_upper(c char) -> bool`: whether `c` is an uppercase character.
    fn builtin_is_upper(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_upper", args, |c| c.is_uppercase())
    }

    /// `is_lower(c char) -> bool`: whether `c` is a lowercase character.
    fn builtin_is_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        Self::char_predicate("is_lower", args, |c| c.is_lowercase())
    }

    /// Shared helper for the deterministic `char -> bool` classification
    /// predicates: unwrap a single `char` operand and apply `test`, reporting a
    /// runtime error (never a panic) on a non-char operand.
    fn char_predicate(
        name: &'static str,
        args: Vec<Value>,
        test: impl Fn(char) -> bool,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        match value {
            Value::Char(c) => Ok(Value::Bool(test(c))),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a char but got `{other}`"),
            )),
        }
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

    /// `to_<T>(x i64) -> T`: wrapping reinterpret of an `i64` into fixed-width
    /// integer `T`; shared by every `to_i8`/`to_i16`/…/`to_usize` conversion.
    fn builtin_to_int(
        name: &'static str,
        args: Vec<Value>,
        ty: IntKind,
    ) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 1, args.len()))?;
        Ok(Value::int(expect_i64(name, value)?, ty))
    }

    /// `to_i64(x) -> i64`: widen a fixed-width integer into `i64` (identity on
    /// the already-normalized cell).
    fn builtin_to_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_i64", 1, args.len()))?;
        match value {
            Value::Int { value, .. } => Ok(Value::I64(value)),
            other => Err(RuntimeError::new(
                "L0407",
                format!("to_i64 expects a fixed-width integer but got `{other}`"),
            )),
        }
    }

    /// `to_f32(x f64) -> f32`: round an `f64` to the nearest `f32`.
    fn builtin_to_f32(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_f32", 1, args.len()))?;
        Ok(Value::F32(value.as_f64()? as f32))
    }

    /// `to_f64(x f32) -> f64`: widen an `f32` to `f64` (exact).
    fn builtin_to_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_f64", 1, args.len()))?;
        match value {
            Value::F32(value) => Ok(Value::F64(f64::from(value))),
            other => Err(RuntimeError::new(
                "L0421",
                format!("to_f64 expects an f32 but got `{other}`"),
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

    /// `os_random(len i64) -> result<list<byte>, string>`: `len`
    /// cryptographically-secure random bytes from the operating-system CSPRNG as
    /// `ok(list<byte>)`, or `err(message)` if the OS RNG fails. `len == 0`
    /// returns `ok([])`; `len < 0` returns `err("os_random length must be
    /// non-negative")`. Never a seeded/deterministic PRNG and never a panic.
    /// Routes through the shared [`os_random_bytes`] helper so the IR
    /// interpreter, the bytecode VM (which runs on it), and the AST runtime all
    /// agree on behavior.
    fn builtin_os_random(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [len]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("os_random", 1, args.len()))?;
        let len = expect_i64("os_random", len)?;
        Ok(result_value(match os_random_bytes(len) {
            Ok(bytes) => Ok(Value::Array(bytes.into_iter().map(Value::Byte).collect())),
            Err(message) => Err(Value::String(message)),
        }))
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
        // `parallel_map` accepts either a named function value or a capturing
        // closure. A closure is self-contained (it carries its captured snapshot,
        // all `Send`) and the worker's fresh interpreter rebuilds the same
        // id-keyed body table from the shared module, so invoking it there is
        // sound and stays order-deterministic.
        let callable = match callee {
            Value::Func(name) => IrParallelCallable::Func(name),
            Value::Closure(closure) => IrParallelCallable::Closure(closure),
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
        let callable = &callable;
        let results: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = arg_values
                .iter()
                .map(|value| {
                    let callable = callable.clone();
                    let value = value.clone();
                    let arc = Arc::clone(module_arc);
                    scope.spawn(move || {
                        let mut runtime = IrRuntime::new(module, arc)?;
                        match callable {
                            IrParallelCallable::Func(name) => {
                                runtime.call_function(&name, vec![value])
                            }
                            IrParallelCallable::Closure(closure) => {
                                runtime.invoke_closure(&closure, vec![value])
                            }
                        }
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

    /// `atomic_new(v i64) -> atomic_i64`: allocate a fresh shared atomic cell
    /// initialized to `v`. Cloning the returned handle shares the same
    /// `Arc<AtomicI64>`, so several threads observe each other's updates.
    fn builtin_atomic_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_new", 1, args.len()))?;
        let value = expect_i64("atomic_new", value)?;
        Ok(Value::Atomic(SharedAtomic {
            cell: Arc::new(AtomicI64::new(value)),
        }))
    }

    /// `atomic_load(a atomic_i64) -> i64`: read the cell (SeqCst).
    fn builtin_atomic_load(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_load", 1, args.len()))?;
        let atomic = expect_atomic("atomic_load", atomic)?;
        Ok(Value::I64(atomic.cell.load(Ordering::SeqCst)))
    }

    /// `atomic_store(a atomic_i64, v i64) -> void`: write the cell (SeqCst).
    fn builtin_atomic_store(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_store", 2, args.len()))?;
        let atomic = expect_atomic("atomic_store", atomic)?;
        let value = expect_i64("atomic_store", value)?;
        atomic.cell.store(value, Ordering::SeqCst);
        Ok(Value::Void)
    }

    /// `atomic_swap(a atomic_i64, v i64) -> i64`: store `v`, return the previous
    /// value (SeqCst).
    fn builtin_atomic_swap(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_swap", 2, args.len()))?;
        let atomic = expect_atomic("atomic_swap", atomic)?;
        let value = expect_i64("atomic_swap", value)?;
        Ok(Value::I64(atomic.cell.swap(value, Ordering::SeqCst)))
    }

    /// `atomic_cas(a atomic_i64, expected i64, new i64) -> i64`: strong
    /// compare-and-swap. Returns the value that was in the cell (equal to
    /// `expected` on success). SeqCst on both success and failure.
    fn builtin_atomic_cas(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [atomic, expected, new]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("atomic_cas", 3, args.len()))?;
        let atomic = expect_atomic("atomic_cas", atomic)?;
        let expected = expect_i64("atomic_cas", expected)?;
        let new = expect_i64("atomic_cas", new)?;
        let observed =
            match atomic
                .cell
                .compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(prev) => prev,
                Err(current) => current,
            };
        Ok(Value::I64(observed))
    }

    /// `atomic_add(a atomic_i64, v i64) -> i64`: fetch-and-add, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_add(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_add", args)?;
        Ok(Value::I64(atomic.cell.fetch_add(value, Ordering::SeqCst)))
    }

    /// `atomic_sub(a atomic_i64, v i64) -> i64`: fetch-and-sub, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_sub(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_sub", args)?;
        Ok(Value::I64(atomic.cell.fetch_sub(value, Ordering::SeqCst)))
    }

    /// `atomic_and(a atomic_i64, v i64) -> i64`: fetch-and-and, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_and(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_and", args)?;
        Ok(Value::I64(atomic.cell.fetch_and(value, Ordering::SeqCst)))
    }

    /// `atomic_or(a atomic_i64, v i64) -> i64`: fetch-and-or, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_or(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_or", args)?;
        Ok(Value::I64(atomic.cell.fetch_or(value, Ordering::SeqCst)))
    }

    /// `atomic_xor(a atomic_i64, v i64) -> i64`: fetch-and-xor, returning the
    /// PREVIOUS value (SeqCst).
    fn builtin_atomic_xor(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let (atomic, value) = Self::atomic_binary_args("atomic_xor", args)?;
        Ok(Value::I64(atomic.cell.fetch_xor(value, Ordering::SeqCst)))
    }

    /// Shared argument-decoding for the `atomic_<op>(a atomic_i64, v i64)`
    /// fetch-and-op family: exactly two arguments, an atomic handle then an
    /// `i64` operand.
    fn atomic_binary_args(
        name: &str,
        args: Vec<Value>,
    ) -> Result<(SharedAtomic, i64), RuntimeError> {
        let [atomic, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity(name, 2, args.len()))?;
        let atomic = expect_atomic(name, atomic)?;
        let value = expect_i64(name, value)?;
        Ok((atomic, value))
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

    /// Push a freshly spawned child into the handle table, returning its index
    /// wrapped as a `Value::Process`. Mirrors `register_socket` and the AST
    /// interpreter's `register_process`.
    fn register_process(&mut self, resource: ProcessResource) -> Value {
        self.processes.push(Some(resource));
        Value::Process(self.processes.len() - 1)
    }

    /// Resolve a process handle argument to its live slot index. Mirrors
    /// `socket_slot` and the AST interpreter's `process_slot`.
    fn process_slot(&self, name: &str, value: &Value) -> Result<usize, RuntimeError> {
        let Value::Process(handle) = value else {
            return Err(RuntimeError::new(
                "L0417",
                format!("{name} expects a process but got `{value}`"),
            ));
        };
        match self.processes.get(*handle) {
            Some(Some(_)) => Ok(*handle),
            _ => Err(RuntimeError::new(
                "L0406",
                format!("{name} received a reaped or invalid process `{handle}`"),
            )),
        }
    }

    /// `proc_spawn(cmd string, args array<string>) -> result<process, string>`:
    /// mirrors the AST interpreter's `builtin_proc_spawn` exactly.
    fn builtin_proc_spawn(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [cmd, cmd_args]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_spawn", 2, args.len()))?;
        let cmd = expect_string("proc_spawn", cmd)?;
        let cmd_args = cmd_args.as_string_array()?;
        match Command::new(&cmd)
            .args(cmd_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => {
                let handle = self.register_process(ProcessResource { child });
                Ok(result_value(Ok(handle)))
            }
            Err(error) => Ok(result_value(Err(Value::String(error.to_string())))),
        }
    }

    /// `proc_wait(p process) -> result<i64, string>`: mirrors the AST interpreter.
    fn builtin_proc_wait(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_wait", 1, args.len()))?;
        let slot = self.process_slot("proc_wait", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                "proc_wait requires a live process".to_string(),
            ))));
        };
        match resource.child.wait() {
            Ok(status) => Ok(result_value(Ok(Value::I64(process_exit_code(&status))))),
            Err(error) => Ok(result_value(Err(Value::String(error.to_string())))),
        }
    }

    /// `proc_stdout(p process) -> result<string, string>`: mirrors the AST
    /// interpreter; the pipe is taken on first read (second read is EOF).
    fn builtin_proc_stdout(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_stdout", 1, args.len()))?;
        let slot = self.process_slot("proc_stdout", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                "proc_stdout requires a live process".to_string(),
            ))));
        };
        let mut buffer = String::new();
        match resource
            .child
            .stdout
            .take()
            .map(|mut pipe| pipe.read_to_string(&mut buffer))
        {
            None => Ok(result_value(Ok(Value::String(String::new())))),
            Some(Ok(_)) => Ok(result_value(Ok(Value::String(buffer)))),
            Some(Err(error)) => Ok(result_value(Err(Value::String(error.to_string())))),
        }
    }

    /// `proc_stderr(p process) -> result<string, string>`: mirrors the AST
    /// interpreter; the pipe is taken on first read (second read is EOF).
    fn builtin_proc_stderr(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_stderr", 1, args.len()))?;
        let slot = self.process_slot("proc_stderr", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                "proc_stderr requires a live process".to_string(),
            ))));
        };
        let mut buffer = String::new();
        match resource
            .child
            .stderr
            .take()
            .map(|mut pipe| pipe.read_to_string(&mut buffer))
        {
            None => Ok(result_value(Ok(Value::String(String::new())))),
            Some(Ok(_)) => Ok(result_value(Ok(Value::String(buffer)))),
            Some(Err(error)) => Ok(result_value(Err(Value::String(error.to_string())))),
        }
    }

    /// `proc_kill(p process) -> result<i64, string>`: mirrors the AST interpreter.
    fn builtin_proc_kill(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [proc]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("proc_kill", 1, args.len()))?;
        let slot = self.process_slot("proc_kill", &proc)?;
        let Some(resource) = self.processes[slot].as_mut() else {
            return Ok(result_value(Err(Value::String(
                "proc_kill requires a live process".to_string(),
            ))));
        };
        match resource.child.kill() {
            Ok(()) => Ok(result_value(Ok(Value::I64(0)))),
            Err(error) => Ok(result_value(Err(Value::String(error.to_string())))),
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

    /// `tcp_accept_nb(listener Socket) -> result<option<Socket>, string>`:
    /// non-blocking accept. Returns `ok(some(client))` when a connection is
    /// pending, `ok(none)` when the listener would block (no pending connection),
    /// and `err(message)` on a real error. The listener must first be put into
    /// non-blocking mode with `set_nonblocking`.
    fn builtin_tcp_accept_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [listener]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_accept_nb", 1, args.len()))?;
        let slot = self.socket_slot("tcp_accept_nb", &listener)?;
        let accepted = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.accept(),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_accept_nb requires a listening socket".to_string(),
                ))));
            }
        };
        match accepted {
            Ok((stream, _addr)) => {
                let socket = self.register_socket(SocketResource::Stream(stream));
                Ok(result_value(Ok(option_value(Some(socket)))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
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

    /// `tcp_read_nb(conn Socket, max i64) -> result<option<string>, string>`:
    /// non-blocking read of up to `max` bytes, returned as a lossy UTF-8 string.
    /// Returns `ok(some(data))` when bytes are available, `ok(some(""))` on a
    /// clean EOF (the peer closed the connection — matching blocking `tcp_read`),
    /// `ok(none)` when the stream would block (no data ready yet), and
    /// `err(message)` on a real error. `max` must be positive. The stream must
    /// first be put into non-blocking mode with `set_nonblocking`.
    fn builtin_tcp_read_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::Read;
        let [conn, max]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("tcp_read_nb", 2, args.len()))?;
        let slot = self.socket_slot("tcp_read_nb", &conn)?;
        let max = expect_i64("tcp_read_nb", max)?;
        if max <= 0 {
            return Ok(result_value(Err(Value::String(
                "tcp_read_nb requires a positive `max` byte count".to_string(),
            ))));
        }
        let mut buffer = vec![0u8; max as usize];
        let read = match &mut self.sockets[slot] {
            Some(SocketResource::Stream(stream)) => stream.read(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "tcp_read_nb requires a connected stream socket".to_string(),
                ))));
            }
        };
        match read {
            Ok(count) => Ok(result_value(Ok(option_value(Some(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
            }
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

    /// `set_nonblocking(sock Socket, enabled bool) -> result<i64, string>`: put a
    /// socket (a listener, connected stream, or UDP socket) into or out of
    /// non-blocking mode via std's `set_nonblocking`. In non-blocking mode,
    /// accept/read/recv operations that would block instead surface as
    /// `ErrorKind::WouldBlock`, which the `*_nb` builtins report as `ok(none)`.
    /// Returns `ok(0)` on success or `err(message)` on failure.
    fn builtin_set_nonblocking(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock, enabled]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("set_nonblocking", 2, args.len()))?;
        let slot = self.socket_slot("set_nonblocking", &sock)?;
        let enabled = expect_bool("set_nonblocking", enabled)?;
        let outcome = match &self.sockets[slot] {
            Some(SocketResource::Listener(listener)) => listener.set_nonblocking(enabled),
            Some(SocketResource::Stream(stream)) => stream.set_nonblocking(enabled),
            Some(SocketResource::Udp(socket)) => socket.set_nonblocking(enabled),
            None => {
                return Ok(result_value(Err(Value::String(
                    "set_nonblocking requires an open socket".to_string(),
                ))));
            }
        };
        match outcome {
            Ok(()) => Ok(result_value(Ok(Value::I64(0)))),
            Err(error) => Ok(net_err(&error)),
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

    /// `udp_recv_nb(sock Socket) -> result<option<string>, string>`: non-blocking
    /// receive of one datagram (sender address dropped), returned as a lossy
    /// UTF-8 string. Returns `ok(some(data))` when a datagram is ready,
    /// `ok(none)` when the socket would block (no datagram pending), and
    /// `err(message)` on a real error. The socket must first be put into
    /// non-blocking mode with `set_nonblocking`.
    fn builtin_udp_recv_nb(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [sock]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("udp_recv_nb", 1, args.len()))?;
        let slot = self.socket_slot("udp_recv_nb", &sock)?;
        let mut buffer = [0u8; 4096];
        let received = match &self.sockets[slot] {
            Some(SocketResource::Udp(socket)) => socket.recv_from(&mut buffer),
            _ => {
                return Ok(result_value(Err(Value::String(
                    "udp_recv_nb requires a UDP socket".to_string(),
                ))));
            }
        };
        match received {
            Ok((count, _addr)) => Ok(result_value(Ok(option_value(Some(Value::String(
                String::from_utf8_lossy(&buffer[..count]).into_owned(),
            )))))),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(result_value(Ok(option_value(None))))
            }
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

    /// `array_fill(n, x) -> array<T>`: a new array of length `n`, every element
    /// `x` (matches the AST runtime; a negative length is `L0433`).
    fn builtin_array_fill(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [count, value]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("array_fill", 2, args.len()))?;
        let n = expect_i64("array_fill", count)?;
        if n < 0 {
            return Err(RuntimeError::new(
                "L0433",
                format!("array_fill length `{n}` is negative"),
            ));
        }
        Ok(Value::Array(vec![value; n as usize]))
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

    /// `list_index_of(l, x) -> i64`: index of the first element equal to `x`, or -1.
    fn builtin_list_index_of(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, target]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_index_of", 2, args.len()))?;
        let values = expect_list("list_index_of", list)?;
        let index = values
            .iter()
            .position(|value| *value == target)
            .map(|i| i as i64)
            .unwrap_or(-1);
        Ok(Value::I64(index))
    }

    /// `list_contains(l, x) -> bool`: whether any element equals `x`.
    fn builtin_list_contains(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, target]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_contains", 2, args.len()))?;
        let values = expect_list("list_contains", list)?;
        Ok(Value::Bool(values.contains(&target)))
    }

    /// `reverse(l) -> list<T>`: a new list with the elements reversed.
    fn builtin_reverse(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("reverse", 1, args.len()))?;
        let mut values = expect_list("reverse", list)?;
        values.reverse();
        Ok(Value::Array(values))
    }

    /// `sort(l list<i64>) -> list<i64>`: a new list sorted ascending.
    fn builtin_sort(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sort", 1, args.len()))?;
        let values = expect_list("sort", list)?;
        sort_scalar_list("sort", values)
    }

    /// `sort_by(l list<T>, cmp fn(T, T) -> i64) -> list<T>`: return a new list
    /// sorted by the comparator (`cmp(a, b)` negative if `a` precedes `b`, zero
    /// if equal, positive if after). Uses a stable sort, so equal elements keep
    /// their input order. The comparator's error, if any, is propagated. Mirrors
    /// the AST runtime.
    fn builtin_sort_by(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sort_by", 2, args.len()))?;
        let mut values = expect_list("sort_by", list)?;
        // A comparator error must abort the whole sort, so capture the first
        // error out of band; `sort_by` itself cannot propagate `Result`.
        let mut error: Option<RuntimeError> = None;
        values.sort_by(|a, b| {
            if error.is_some() {
                return std::cmp::Ordering::Equal;
            }
            match self.invoke_callable("sort_by", callee.clone(), vec![a.clone(), b.clone()]) {
                Ok(Value::I64(n)) => n.cmp(&0),
                Ok(other) => {
                    error = Some(RuntimeError::new(
                        "L0417",
                        format!("sort_by comparator must return i64 but returned `{other}`"),
                    ));
                    std::cmp::Ordering::Equal
                }
                Err(err) => {
                    error = Some(err);
                    std::cmp::Ordering::Equal
                }
            }
        });
        if let Some(err) = error {
            return Err(err);
        }
        Ok(Value::Array(values))
    }

    /// `concat(a, b) -> list<T>`: a new list with `b`'s elements appended to `a`.
    fn builtin_concat(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [a, b]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("concat", 2, args.len()))?;
        let mut values = expect_list("concat", a)?;
        let mut rest = expect_list("concat", b)?;
        values.append(&mut rest);
        Ok(Value::Array(values))
    }

    /// `slice(l, start, end) -> list<T>`: the half-open range `[start, end)`,
    /// with `start`/`end` clamped into `[0, len]` (so it is always total).
    fn builtin_slice(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, start, end]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("slice", 3, args.len()))?;
        let values = expect_list("slice", list)?;
        let start = expect_i64("slice", start)?;
        let end = expect_i64("slice", end)?;
        let len = values.len() as i64;
        let start = start.clamp(0, len) as usize;
        let end = end.clamp(0, len) as usize;
        if start >= end {
            return Ok(Value::Array(Vec::new()));
        }
        Ok(Value::Array(values[start..end].to_vec()))
    }

    /// Invoke a first-class function value (`Value::Func` name or a capturing
    /// `Value::Closure`) with `args`, reusing the same call/closure machinery as
    /// direct dispatch and `parallel_map`. Shared by the higher-order list
    /// builtins so closures capture correctly. Mirrors the AST runtime.
    fn invoke_callable(
        &mut self,
        builtin: &str,
        callee: Value,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        match callee {
            Value::Func(name) => self.call_function(&name, args),
            Value::Closure(closure) => self.invoke_closure(&closure, args),
            other => Err(RuntimeError::new(
                "L0417",
                format!("{builtin} expects a function but got `{other}`"),
            )),
        }
    }

    /// `list_map(l list<T>, f fn(T) -> U) -> list<U>`: apply `f` to each element
    /// in order, collecting the mapped values into a new list.
    fn builtin_list_map(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_map", 2, args.len()))?;
        let values = expect_list("list_map", list)?;
        let mut mapped = Vec::with_capacity(values.len());
        for value in values {
            mapped.push(self.invoke_callable("list_map", callee.clone(), vec![value])?);
        }
        Ok(Value::Array(mapped))
    }

    /// `list_filter(l list<T>, pred fn(T) -> bool) -> list<T>`: keep the elements
    /// for which `pred` returns `true`, preserving input order.
    fn builtin_list_filter(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, callee]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_filter", 2, args.len()))?;
        let values = expect_list("list_filter", list)?;
        let mut kept = Vec::new();
        for value in values {
            let keep = self.invoke_callable("list_filter", callee.clone(), vec![value.clone()])?;
            if keep.as_bool()? {
                kept.push(value);
            }
        }
        Ok(Value::Array(kept))
    }

    /// `list_reduce(l list<T>, init U, f fn(U, T) -> U) -> U`: a left fold,
    /// threading the accumulator (starting at `init`) through `f(acc, element)`.
    fn builtin_list_reduce(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list, init, callee]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_reduce", 3, args.len()))?;
        let values = expect_list("list_reduce", list)?;
        let mut acc = init;
        for value in values {
            acc = self.invoke_callable("list_reduce", callee.clone(), vec![acc, value])?;
        }
        Ok(acc)
    }

    /// `map_new() -> map<K, V>`: a fresh empty map.
    fn builtin_map_new(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let []: [Value; 0] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_new", 0, args.len()))?;
        Ok(Value::Map(Box::default()))
    }

    /// `map_set(m, k, v) -> map<K, V>`: a new map with `k` mapped to `v`.
    /// Overwriting an existing key or appending a new one is O(1).
    fn builtin_map_set(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key, value]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_set", 3, args.len()))?;
        let mut entries = expect_map("map_set", map)?;
        entries.insert(key, value);
        Ok(Value::Map(Box::new(entries)))
    }

    /// `map_get(m, k) -> option<V>`: `some(v)` if present, else `none`. O(1).
    fn builtin_map_get(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_get", 2, args.len()))?;
        let entries = expect_map("map_get", map)?;
        let found = entries.get(&key).cloned();
        Ok(option_value(found))
    }

    /// `map_has(m, k) -> bool`. O(1).
    fn builtin_map_has(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_has", 2, args.len()))?;
        let entries = expect_map("map_has", map)?;
        Ok(Value::Bool(entries.contains_key(&key)))
    }

    /// `map_len(m) -> i64`.
    fn builtin_map_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_len", 1, args.len()))?;
        let entries = expect_map("map_len", map)?;
        Ok(Value::I64(entries.len() as i64))
    }

    /// `map_keys(m) -> list<K>`: the keys in insertion order.
    fn builtin_map_keys(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_keys", 1, args.len()))?;
        let entries = expect_map("map_keys", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(k, _)| k).collect(),
        ))
    }

    /// `map_values(m) -> list<V>`: the values in insertion order.
    fn builtin_map_values(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_values", 1, args.len()))?;
        let entries = expect_map("map_values", map)?;
        Ok(Value::Array(
            entries.into_entries().into_iter().map(|(_, v)| v).collect(),
        ))
    }

    /// `map_del(m, k) -> map<K, V>`: a new map without key `k`.
    fn builtin_map_del(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [map, key]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("map_del", 2, args.len()))?;
        let mut entries = expect_map("map_del", map)?;
        entries.remove(&key);
        Ok(Value::Map(Box::new(entries)))
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

    fn builtin_words(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("words", 1, args.len()))?;
        let text = expect_string("words", text)?;
        let parts = text
            .split_whitespace()
            .map(|part| Value::String(part.to_string()))
            .collect();
        Ok(Value::Array(parts))
    }

    fn builtin_count(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text, sub]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("count", 2, args.len()))?;
        let text = expect_string("count", text)?;
        let sub = expect_string("count", sub)?;
        // An empty needle has no well-defined non-overlapping count; define it as 0.
        let n = if sub.is_empty() {
            0
        } else {
            text.matches(sub.as_str()).count() as i64
        };
        Ok(Value::I64(n))
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

    /// `chars(s) -> list<char>`: the characters of `s` in order.
    fn builtin_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("chars", 1, args.len()))?;
        let text = expect_string("chars", text)?;
        Ok(Value::Array(text.chars().map(Value::Char).collect()))
    }

    /// `string_from_chars(cs) -> string`: concatenate a `list<char>` into a string.
    fn builtin_string_from_chars(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("string_from_chars", 1, args.len()))?;
        let values = expect_list("string_from_chars", list)?;
        let mut out = String::new();
        for value in values {
            match value {
                Value::Char(c) => out.push(c),
                other => {
                    return Err(RuntimeError::new(
                        "L0417",
                        format!("string_from_chars expects a list<char> but found `{other}`"),
                    ));
                }
            }
        }
        Ok(Value::String(out))
    }

    fn builtin_lower(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("lower", 1, args.len()))?;
        let text = expect_string("lower", text)?;
        Ok(Value::String(text.to_lowercase()))
    }

    /// `to_bytes(s string) -> list<byte>`: the UTF-8 encoding of `s` as a
    /// `list<byte>` (a `Value::Array` of `Value::Byte`, matching `read_bytes`).
    fn builtin_to_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("to_bytes", 1, args.len()))?;
        let text = expect_string("to_bytes", text)?;
        Ok(Value::Array(
            text.into_bytes().into_iter().map(Value::Byte).collect(),
        ))
    }

    /// `from_bytes(b list<byte>) -> result<string, string>`: decode `b` as UTF-8,
    /// returning `ok(s)` on success and `err(message)` (never a panic, never a
    /// lossy replacement) on invalid UTF-8.
    fn builtin_from_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [data]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("from_bytes", 1, args.len()))?;
        let bytes = Self::value_to_bytes("from_bytes", data)?;
        Ok(result_value(match String::from_utf8(bytes) {
            Ok(text) => Ok(Value::String(text)),
            Err(error) => Err(Value::String(format!("invalid utf-8: {error}"))),
        }))
    }

    /// `byte_len(s string) -> i64`: the number of UTF-8 bytes in `s` (distinct
    /// from `len`, which counts characters for a string).
    fn builtin_byte_len(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("byte_len", 1, args.len()))?;
        let text = expect_string("byte_len", text)?;
        Ok(Value::I64(text.len() as i64))
    }

    /// `parse_i64(s string) -> result<i64, string>`: parse `s` as a base-10
    /// signed 64-bit integer via Rust `str::parse::<i64>()`, returning `ok(n)`
    /// on success and `err(message)` on any failure (empty, non-numeric, or out
    /// of range). Whitespace is not trimmed, so a padded string is an `err`. The
    /// error message is a fixed string so every backend matches byte-for-byte.
    fn builtin_parse_i64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parse_i64", 1, args.len()))?;
        let text = expect_string("parse_i64", text)?;
        Ok(result_value(match text.parse::<i64>() {
            Ok(value) => Ok(Value::I64(value)),
            Err(_) => Err(Value::String(format!("cannot parse `{text}` as i64"))),
        }))
    }

    /// `parse_f64(s string) -> result<f64, string>`: parse `s` as an `f64` via
    /// Rust `str::parse::<f64>()`, returning `ok(x)` on success and
    /// `err(message)` on failure. The error message is a fixed string so every
    /// backend matches byte-for-byte.
    fn builtin_parse_f64(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [text]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("parse_f64", 1, args.len()))?;
        let text = expect_string("parse_f64", text)?;
        Ok(result_value(match text.parse::<f64>() {
            Ok(value) => Ok(Value::F64(value)),
            Err(_) => Err(Value::String(format!("cannot parse `{text}` as f64"))),
        }))
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

    /// `clamp(x, lo, hi) -> T`: `x` limited to `[lo, hi]`; total (for `lo > hi`
    /// yields `lo`, for f64 NaN `x` returns `x`). Mirrors the AST interpreter.
    fn builtin_clamp(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, lo, hi]: [Value; 3] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("clamp", 3, args.len()))?;
        match (x, lo, hi) {
            (Value::I64(x), Value::I64(lo), Value::I64(hi)) => Ok(Value::I64(if x < lo {
                lo
            } else if x > hi {
                hi
            } else {
                x
            })),
            (Value::F64(x), Value::F64(lo), Value::F64(hi)) => Ok(Value::F64(if x < lo {
                lo
            } else if x > hi {
                hi
            } else {
                x
            })),
            (x, lo, hi) => Err(RuntimeError::new(
                "L0417",
                format!(
                    "clamp expects three matching i64 or f64 values but got `{x}`, `{lo}`, and `{hi}`"
                ),
            )),
        }
    }

    /// `sign(x) -> i64`: `-1`/`0`/`1`; f64 `NaN`/`-0.0` map to `0`.
    fn builtin_sign(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("sign", 1, args.len()))?;
        match value {
            Value::I64(n) => Ok(Value::I64(n.signum())),
            Value::F64(n) => Ok(Value::I64(if n > 0.0 {
                1
            } else if n < 0.0 {
                -1
            } else {
                0
            })),
            other => Err(RuntimeError::new(
                "L0417",
                format!("sign expects an i64 or f64 but got `{other}`"),
            )),
        }
    }

    /// `gcd(a, b) -> i64`: non-negative greatest common divisor (total at
    /// `i64::MIN`; see `gcd_i64`).
    fn builtin_gcd(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [a, b]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("gcd", 2, args.len()))?;
        match (a, b) {
            (Value::I64(a), Value::I64(b)) => Ok(Value::I64(gcd_i64(a, b))),
            (a, b) => Err(RuntimeError::new(
                "L0417",
                format!("gcd expects two i64 values but got `{a}` and `{b}`"),
            )),
        }
    }

    /// `list_sum(l) -> T`: wrapping i64 / f64 sum; empty -> `0`/`0.0`.
    fn builtin_list_sum(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_sum", 1, args.len()))?;
        let values = expect_list("list_sum", list)?;
        list_sum_values("list_sum", values)
    }

    /// `list_min(l) -> option<T>`: `none` on empty, else `some(minimum)`.
    fn builtin_list_min(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_min", 1, args.len()))?;
        let values = expect_list("list_min", list)?;
        Ok(option_value(list_extreme("list_min", values, false)?))
    }

    /// `list_max(l) -> option<T>`: `none` on empty, else `some(maximum)`.
    fn builtin_list_max(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [list]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("list_max", 1, args.len()))?;
        let values = expect_list("list_max", list)?;
        Ok(option_value(list_extreme("list_max", values, true)?))
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

    /// `rotate_left(x, n)`: rotate the 64 bits of `x` left by `(n & 63)`
    /// positions, matching the AST runtime so every backend agrees.
    fn builtin_rotate_left(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, n]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rotate_left", 2, args.len()))?;
        match (x, n) {
            (Value::I64(x), Value::I64(n)) => {
                Ok(Value::I64(x.rotate_left(((n as u64) & 63) as u32)))
            }
            (x, n) => Err(RuntimeError::new(
                "L0417",
                format!("rotate_left expects two i64 values but got `{x}` and `{n}`"),
            )),
        }
    }

    /// `rotate_right(x, n)`: rotate the 64 bits of `x` right by `(n & 63)`
    /// positions, matching the AST runtime.
    fn builtin_rotate_right(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [x, n]: [Value; 2] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("rotate_right", 2, args.len()))?;
        match (x, n) {
            (Value::I64(x), Value::I64(n)) => {
                Ok(Value::I64(x.rotate_right(((n as u64) & 63) as u32)))
            }
            (x, n) => Err(RuntimeError::new(
                "L0417",
                format!("rotate_right expects two i64 values but got `{x}` and `{n}`"),
            )),
        }
    }

    /// `count_ones(x)`: population count of the 64-bit value `x` (0..=64).
    fn builtin_count_ones(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("count_ones", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.count_ones() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("count_ones expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `leading_zeros(x)`: number of leading zero bits in `x` (0..=64).
    fn builtin_leading_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("leading_zeros", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.leading_zeros() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("leading_zeros expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `trailing_zeros(x)`: number of trailing zero bits in `x` (0..=64).
    fn builtin_trailing_zeros(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("trailing_zeros", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.trailing_zeros() as i64)),
            other => Err(RuntimeError::new(
                "L0417",
                format!("trailing_zeros expects an i64 but got `{other}`"),
            )),
        }
    }

    /// `reverse_bytes(x)`: reverse the byte order of the 64-bit value `x`.
    fn builtin_reverse_bytes(args: Vec<Value>) -> Result<Value, RuntimeError> {
        let [value]: [Value; 1] = args
            .try_into()
            .map_err(|args: Vec<Value>| Self::wrong_arity("reverse_bytes", 1, args.len()))?;
        match value {
            Value::I64(x) => Ok(Value::I64(x.swap_bytes())),
            other => Err(RuntimeError::new(
                "L0417",
                format!("reverse_bytes expects an i64 but got `{other}`"),
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
        | IrStmt::Asm { span, .. }
        | IrStmt::Throw { span, .. }
        | IrStmt::Try { span, .. }
        | IrStmt::Match { span, .. } => *span,
        IrStmt::Return(Some(expr)) | IrStmt::Expr(expr) => expr.span,
        IrStmt::Return(None) => Span::new(1, 1),
    }
}

/// Conservative "does `name` appear anywhere in this IR expression?" walk, the
/// IR twin of the AST runtime's `expr_mentions_var`. Used by the
/// move-on-functional-update fast path to prove the target variable is not
/// referenced outside its single consuming argument. It over-approximates on
/// purpose (a mention inside a nested closure body or a matching call name still
/// counts); over-approximating only ever forgoes the optimization, never changes
/// a result, so the walk stays simple and total over `IrExprKind`.
fn expr_mentions_var(expr: &IrExpr, name: &str) -> bool {
    match &expr.kind {
        IrExprKind::Integer(_)
        | IrExprKind::Float(_)
        | IrExprKind::Bool(_)
        | IrExprKind::String(_)
        | IrExprKind::Char(_) => false,
        // A resolved `Local` still names the same binding, so it counts as a
        // mention. Missing it here would let the move-on-functional-update fast
        // path move a value that a `Local` still reads — an under-approximation
        // that must never happen (over-approximating only forgoes the move).
        IrExprKind::Variable(v) | IrExprKind::Local { name: v, .. } => v == name,
        IrExprKind::Array(items) => items.iter().any(|item| expr_mentions_var(item, name)),
        IrExprKind::Index { target, index } => {
            expr_mentions_var(target, name) || expr_mentions_var(index, name)
        }
        IrExprKind::Unary { expr, .. } => expr_mentions_var(expr, name),
        IrExprKind::Binary { left, right, .. } => {
            expr_mentions_var(left, name) || expr_mentions_var(right, name)
        }
        IrExprKind::Call { name: callee, args } => {
            callee == name || args.iter().any(|arg| expr_mentions_var(arg, name))
        }
        IrExprKind::Field { target, .. } => expr_mentions_var(target, name),
        IrExprKind::Await { expr } => expr_mentions_var(expr, name),
        IrExprKind::Closure { .. } => false,
    }
}

/// A lexical environment: a stack of scopes, each an insertion-ordered
/// association list of `(name, value)`. Function-call and block scopes are
/// small, so a linear-scan `Vec` beats a `HashMap` — it avoids a per-scope
/// bucket allocation and per-access string hashing, and its contiguous layout
/// is cache-friendly. `define` keeps at most one binding per name per scope
/// (replacing in place, like the previous `HashMap::insert`), so resolution
/// never disambiguates duplicates within a scope; cross-scope shadowing is
/// innermost-first. Mirrors the AST runtime's `Env` one-to-one.
#[derive(Debug, Clone)]
struct Env {
    scopes: Vec<Vec<(String, Value)>>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            scopes: vec![Vec::new()],
        }
    }
}

impl Env {
    /// Reset to a single empty scope so a pooled environment can be reused for the
    /// next call, keeping each scope's `Vec` capacity. Clearing every entry means
    /// no stale binding can leak into the reused environment.
    fn reset(&mut self) {
        self.scopes.truncate(1);
        match self.scopes.first_mut() {
            Some(first) => first.clear(),
            None => self.scopes.push(Vec::new()),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Update the loop variable's binding in the innermost scope in place. The
    /// range-`for` lowering calls this each iteration with the loop-variable scope
    /// innermost (the body scope has been popped), so it never allocates or clones
    /// the name — the hot-path replacement for a per-iteration `define`.
    fn set_loop_var(&mut self, name: &str, value: Value) {
        let scope = self.scopes.last_mut().expect("env always has a scope");
        for (existing, slot) in scope.iter_mut() {
            if existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name.to_string(), value));
    }

    fn define(&mut self, name: String, value: Value) {
        let scope = self.scopes.last_mut().expect("env always has a scope");
        for (existing, slot) in scope.iter_mut() {
            if *existing == name {
                *slot = value;
                return;
            }
        }
        scope.push((name, value));
    }

    /// Borrow the nearest binding of `name` mutably for in-place element/field
    /// mutation (`a[i] = v`), avoiding a whole-container clone + write-back.
    fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    return Some(slot);
                }
            }
        }
        None
    }

    fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    *slot = value;
                    return Ok(());
                }
            }
        }
        Err(RuntimeError::new(
            "L0403",
            format!("unknown variable `{name}`"),
        ))
    }

    fn get(&self, name: &str) -> Result<Value, RuntimeError> {
        self.get_ref(name)
            .cloned()
            .ok_or_else(|| RuntimeError::new("L0403", format!("unknown variable `{name}`")))
    }

    /// Borrow a binding's value without cloning it (innermost-first, like
    /// [`Env::get`]). Used to classify a call target on the
    /// move-on-functional-update fast path without paying for a clone.
    fn get_ref(&self, name: &str) -> Option<&Value> {
        for scope in self.scopes.iter().rev() {
            for (existing, value) in scope.iter() {
                if existing == name {
                    return Some(value);
                }
            }
        }
        None
    }

    /// Borrow a slot-resolved binding directly, with no name scan. `packed` is a
    /// `(depth, slot)` pair produced by [`resolve_slots`]: `depth` counts scopes up
    /// from the innermost and `slot` indexes within that scope. The lookup is
    /// **validated** — it confirms the binding at that position still carries
    /// `name` before returning it, and returns `None` (so the caller falls back to
    /// the name scan) if the position is out of range or the name does not match.
    /// That validation makes the fast path correct-or-slower by construction: a
    /// mis-resolved slot can never read the wrong binding, only miss and fall back.
    fn get_slot(&self, packed: u32, name: &str) -> Option<&Value> {
        let (depth, slot) = unpack_slot(packed);
        let idx = self.scopes.len().checked_sub(1 + depth)?;
        let (existing, value) = self.scopes.get(idx)?.get(slot)?;
        (existing == name).then_some(value)
    }


    /// True when `name` is bound in the innermost (current) scope. A `let x =
    /// f(x, …)` re-binding only moves when the consumed binding lives here,
    /// because `let` shadows into the innermost scope rather than overwriting an
    /// outer binding.
    fn innermost_has(&self, name: &str) -> bool {
        self.scopes
            .last()
            .is_some_and(|scope| scope.iter().any(|(n, _)| n == name))
    }

    /// True when `name` is bound in any scope (a normal local). A plain `x =
    /// f(x, …)` reassignment moves from — and writes back to — the *nearest*
    /// binding, and both [`Env::get`] and [`Env::assign`] resolve nearest-first to
    /// that same slot, so the move is safe at any scope depth (e.g. `x` declared
    /// outside a loop, reassigned inside it).
    fn is_bound(&self, name: &str) -> bool {
        self.get_ref(name).is_some()
    }

    /// Move the value out of the nearest scope binding `name`, leaving a cheap
    /// [`Value::Void`] placeholder in the same slot (no clone), and return the old
    /// value. Nearest-first, matching [`Env::get`]/[`Env::assign`] resolution, so
    /// the caller's write-back overwrites this exact slot. The placeholder is
    /// never observable (see the AST runtime twin for the full argument).
    fn move_out_nearest(&mut self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter_mut().rev() {
            for (existing, slot) in scope.iter_mut() {
                if existing == name {
                    return Some(std::mem::replace(slot, Value::Void));
                }
            }
        }
        None
    }

    /// Snapshot every in-scope local by value for closure frame capture, mirroring
    /// the AST runtime: one `(name, value.clone())` per visible binding, inner
    /// scopes shadowing outer ones, sorted by name for a deterministic order.
    fn snapshot_locals(&self) -> Vec<(String, Value)> {
        let mut flattened: HashMap<&str, &Value> = HashMap::new();
        for scope in &self.scopes {
            for (name, value) in scope {
                flattened.insert(name.as_str(), value);
            }
        }
        let mut captured: Vec<(String, Value)> = flattened
            .into_iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        captured.sort_by(|(a, _), (b, _)| a.cmp(b));
        captured
    }
}

struct Lowerer<'a> {
    program: &'a Program,
    signatures: &'a HashMap<String, Signature>,
    /// Declared return type of the function currently being lowered. Threaded so
    /// `return EXPR` and a function's final expression can supply the expected
    /// type that `none`/`ok`/`err` need. Set at the start of each function.
    current_return_type: std::cell::RefCell<TypeRef>,
    /// Statements hoisted while desugaring postfix `?` operators in the statement
    /// currently being lowered. Each `EXPR?` pushes a `let __q = <operand>`, a
    /// typed `let __v`, and a `match __q` (writing `__v` on success, `return`ing
    /// the failure value otherwise) here, then rewrites its position to reference
    /// `__v`. The block lowerers drain this in order before the statement, so the
    /// `?` node never reaches the IR — only `let`/`assign`/`match`/`return`, which
    /// every backend already handles.
    try_prelude: std::cell::RefCell<Vec<IrStmt>>,
    /// Monotonic counter for fresh `?`-desugar temp names, unique per program so
    /// hoisted temporaries never collide with user bindings or each other.
    next_try_temp: std::cell::Cell<usize>,
    /// Monotonic counter for fresh inline-conditional (`THEN if COND else ELSE`)
    /// desugar temp names, unique per program for the same reason.
    next_cond_temp: std::cell::Cell<usize>,
    /// Lowered closure bodies collected while lowering, keyed by parse-order id.
    /// Each `ExprKind::Closure` lowering registers an entry here and emits an
    /// `IrExprKind::Closure { id }` node; the accumulated table is attached to
    /// the `IrModule` at the end of lowering.
    closures: std::cell::RefCell<Vec<IrClosureDef>>,
}

impl<'a> Lowerer<'a> {
    fn new(program: &'a Program, signatures: &'a HashMap<String, Signature>) -> Self {
        Self {
            program,
            signatures,
            current_return_type: std::cell::RefCell::new(TypeRef::new("void")),
            try_prelude: std::cell::RefCell::new(Vec::new()),
            next_try_temp: std::cell::Cell::new(0),
            next_cond_temp: std::cell::Cell::new(0),
            closures: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn lower_program(&self) -> Result<IrModule, IrLoweringError> {
        // Extern (C-ABI) declarations are body-less: they are recorded by name in
        // `extern_functions` (below) and never lowered to an `IrFunction`.
        let functions = self
            .program
            .functions
            .iter()
            .filter(|function| !function.is_extern)
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
        // Record every `async fn` so the interpreter and VM spawn a thread on a
        // call to one (and yield a `Future`).
        let async_functions = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_async)
            .map(|function| function.name.clone())
            .collect();
        // Record every `extern fn` so a call resolves to an external-symbol call
        // on the native backend and to an `L0423` on the interpreters.
        let extern_functions = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_extern)
            .map(|function| function.name.clone())
            .collect();
        // The full C-ABI signature of each `extern fn`, so the native backend can
        // marshal argument/return scalar widths correctly. Same declaration order
        // as `extern_functions`.
        let extern_signatures = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_extern)
            .map(|function| IrExternSignature {
                name: function.name.clone(),
                params: function.params.iter().map(|p| p.ty.clone()).collect(),
                return_type: function.return_type.clone(),
            })
            .collect();
        // Record every `export fn` so the native backend emits an externally
        // visible, defined symbol for it under its plain C name. The function is
        // lowered like any ordinary function (it has a body); `export` only
        // affects native symbol visibility.
        let export_functions = self
            .program
            .functions
            .iter()
            .filter(|function| function.is_export)
            .map(|function| function.name.clone())
            .collect();
        // Every closure body lowered above (across functions and impl methods) was
        // collected into the accumulator; sort by id for a deterministic module.
        let mut closures = self.closures.borrow().clone();
        closures.sort_by_key(|def| def.id);
        Ok(IrModule {
            functions,
            structs,
            enums,
            impls,
            trait_methods,
            async_functions,
            extern_functions,
            extern_signatures,
            export_functions,
            closures,
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
        // The compiler-provided `MemoryOrder` enum is not part of the user
        // program's declarations, so resolve its unit variants explicitly (as
        // semantics and the interpreters do).
        if MEMORY_ORDER_VARIANTS.contains(&name) {
            return Some("MemoryOrder".to_string());
        }
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
                other => {
                    let stmt = self.lower_statement(other, scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(stmt);
                }
            }
        }
        Ok(lowered)
    }

    /// Move any `?`-desugar statements accumulated while lowering the current
    /// statement (see [`Lowerer::try_prelude`]) to the front of the statement's
    /// emitted output, preserving their left-to-right / inner-before-outer order.
    fn drain_try_prelude_into(&self, out: &mut Vec<IrStmt>) {
        let mut prelude = self.try_prelude.borrow_mut();
        out.extend(prelude.drain(..));
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
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(IrStmt::Expr(lowered_expr));
                }
                other => {
                    let stmt = self.lower_statement(other, scope)?;
                    self.drain_try_prelude_into(&mut lowered);
                    lowered.push(stmt);
                }
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
                let mut value = self.lower_expr(value, scope)?;
                // `s += c` where `s: string` and `c: char` coerces the char to a
                // string, matching `s + c`. Only the bare-local string target
                // needs this here; the AST interpreter handles it directly.
                if *op == AssignOp::Add
                    && path.is_empty()
                    && value.ty == TypeRef::new("char")
                    && scope.get(name).map(|t| t.name.as_str()) == Some("string")
                {
                    value = self.to_string_wrap(value);
                }
                Ok(IrStmt::Assign {
                    name: name.clone(),
                    path,
                    op: *op,
                    value,
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
            // `for x in coll` desugars to an index-based `for` over `0..len-1`.
            // The collection is bound to a hidden local (via the prelude) so it is
            // evaluated exactly once, then each element is read by `[]` (arrays and
            // strings) or `get` (lists) into `x` as the loop body's first binding.
            Stmt::ForEach {
                name,
                iterable,
                body,
                span,
            } => {
                let coll = self.lower_expr(iterable, scope)?;
                let coll_ty = coll.ty.clone();
                let elem_ty = if coll_ty.name == "string" {
                    TypeRef::new("char")
                } else {
                    coll_ty
                        .array_element()
                        .or_else(|| coll_ty.list_element())
                        .ok_or_else(|| {
                            IrLoweringError::new(
                                "`for … in` requires an array, list, or string",
                                Some(*span),
                            )
                        })?
                };
                // A bare variable iterable (`for x in xs`) is re-read for free, so
                // reference it directly — no hidden copy, which also keeps the
                // native backend's array-length inference intact. Only a computed
                // iterable is bound to a hidden local (evaluated once).
                let coll_binding = match &coll.kind {
                    IrExprKind::Variable(name) => Some(name.clone()),
                    _ => None,
                };
                let coll_name = coll_binding
                    .clone()
                    .unwrap_or_else(|| format!("__foreach_coll_{}_{}", span.line, span.column));
                let idx_name = format!("__foreach_idx_{}_{}", span.line, span.column);
                let i64_ty = TypeRef::new("i64");
                let coll_var = IrExpr {
                    kind: IrExprKind::Variable(coll_name.clone()),
                    ty: coll_ty.clone(),
                    span: *span,
                };
                let idx_var = IrExpr {
                    kind: IrExprKind::Variable(idx_name.clone()),
                    ty: i64_ty.clone(),
                    span: *span,
                };
                // element read: `coll[idx]` (array/string) or `get(coll, idx)` (list)
                let element = if coll_ty.list_element().is_some() {
                    IrExpr {
                        kind: IrExprKind::Call {
                            name: "get".to_string(),
                            args: vec![coll_var.clone(), idx_var.clone()],
                        },
                        ty: elem_ty.clone(),
                        span: *span,
                    }
                } else {
                    IrExpr {
                        kind: IrExprKind::Index {
                            target: Box::new(coll_var.clone()),
                            index: Box::new(idx_var.clone()),
                        },
                        ty: elem_ty.clone(),
                        span: *span,
                    }
                };
                // end = len(coll) - 1
                let end = IrExpr {
                    kind: IrExprKind::Binary {
                        left: Box::new(IrExpr {
                            kind: IrExprKind::Call {
                                name: "len".to_string(),
                                args: vec![coll_var],
                            },
                            ty: i64_ty.clone(),
                            span: *span,
                        }),
                        op: BinaryOp::Subtract,
                        right: Box::new(IrExpr {
                            kind: IrExprKind::Integer(1),
                            ty: i64_ty.clone(),
                            span: *span,
                        }),
                    },
                    ty: i64_ty.clone(),
                    span: *span,
                };
                // Lower the user body first (drains any nested preludes correctly),
                // then prepend `let x = <element>`.
                let mut loop_scope = scope.clone();
                loop_scope.insert(idx_name.clone(), i64_ty.clone());
                loop_scope.insert(name.clone(), elem_ty.clone());
                let mut for_body = vec![IrStmt::Let {
                    name: name.clone(),
                    ty: elem_ty,
                    value: element,
                    span: *span,
                }];
                for_body.extend(self.lower_block(body, &mut loop_scope)?);
                // Bind a computed collection once, before the loop (a bare variable
                // needs no binding — it is referenced directly above).
                if coll_binding.is_none() {
                    self.try_prelude.borrow_mut().push(IrStmt::Let {
                        name: coll_name,
                        ty: coll_ty,
                        value: coll,
                        span: *span,
                    });
                }
                Ok(IrStmt::For {
                    name: idx_name,
                    start: IrExpr {
                        kind: IrExprKind::Integer(0),
                        ty: i64_ty.clone(),
                        span: *span,
                    },
                    end,
                    step: None,
                    body: for_body,
                    span: *span,
                })
            }
            Stmt::Loop { body, span } => {
                let mut loop_scope = scope.clone();
                let body = self.lower_block(body, &mut loop_scope)?;
                Ok(IrStmt::Loop { body, span: *span })
            }
            // Inline assembly lowers straight through to an `IrStmt::Asm` carrying
            // the raw bytes. Semantics has already validated each byte is 0..=255,
            // so the `as u8` truncation is exact.
            Stmt::Asm { bytes, span } => Ok(IrStmt::Asm {
                bytes: bytes.iter().map(|byte| *byte as u8).collect(),
                span: *span,
            }),
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
                // `s[i]` on a string yields a `char`; otherwise the array element.
                let ty = if target.ty.name == "string" {
                    TypeRef::new("char")
                } else {
                    target.ty.array_element().ok_or_else(|| {
                        IrLoweringError::new(
                            "index target is not an array or string",
                            Some(target.span),
                        )
                    })?
                };
                (
                    IrExprKind::Index {
                        target: Box::new(target),
                        index: Box::new(index),
                    },
                    ty,
                )
            }
            ExprKind::Unary { op, expr } => {
                let inner = self.lower_expr(expr, scope)?;
                // Bitwise NOT preserves the operand's integer type (`i64` or any
                // fixed-width kind); logical NOT is `bool`.
                let ty = match op {
                    UnaryOp::Not => TypeRef::new("bool"),
                    UnaryOp::BitNot | UnaryOp::Negate => inner.ty.clone(),
                };
                (
                    IrExprKind::Unary {
                        op: *op,
                        expr: Box::new(inner),
                    },
                    ty,
                )
            }
            ExprKind::Binary { left, op, right } => {
                let left = self.lower_expr(left, scope)?;
                let right = self.lower_expr(right, scope)?;
                // `string + char` (either order) coerces the char to a string via
                // `to_string`, so every backend sees a plain two-string `+`.
                let (left, right) = self.coerce_string_char_add(*op, left, right);
                let ty = match op {
                    // `+` on two strings concatenates and yields a string.
                    BinaryOp::Add
                        if left.ty == TypeRef::new("string")
                            && right.ty == TypeRef::new("string") =>
                    {
                        TypeRef::new("string")
                    }
                    // Arithmetic preserves the operand's numeric type. Semantics
                    // guarantees both operands share one numeric type (i64/f64/f32
                    // or a fixed-width integer), so the result is that type; the
                    // two-string `+` concatenation is handled by the arm above.
                    // (The backends still derive float width/int kind structurally
                    // from leaf operands, so this only improves the node's recorded
                    // type — it does not change codegen eligibility or results.)
                    BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                        left.ty.clone()
                    }
                    // `%` (integer remainder) preserves the operand's integer type.
                    BinaryOp::Remainder => left.ty.clone(),
                    // Integer bitwise ops preserve the operand's integer type
                    // (`i64` or any fixed-width kind; both operands share it).
                    BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor
                    | BinaryOp::Shl
                    | BinaryOp::Shr => left.ty.clone(),
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
            ExprKind::Await { expr: inner } => {
                // `await e` requires `e: Future<T>`; the awaited result type `T`
                // is the future's inner argument. Semantics has already checked
                // this, so a non-future operand here is a lowering bug.
                let inner = self.lower_expr(inner, scope)?;
                let ty = inner.ty.generic_arg("Future").ok_or_else(|| {
                    IrLoweringError::new(
                        format!("`await` operand has non-future type `{}`", inner.ty.name),
                        Some(expr.span),
                    )
                })?;
                (
                    IrExprKind::Await {
                        expr: Box::new(inner),
                    },
                    ty,
                )
            }
            // Postfix `EXPR?` is desugared here so no `Try` node ever reaches the
            // IR (and the native/WASM backends never see it). We hoist the
            // supporting `let`/`match`/`return` into the statement prelude and
            // rewrite this position to a reference to the success temporary. The
            // recursive `lower_expr` on the operand desugars any inner `?` first,
            // so nested `?` hoist inner-before-outer.
            ExprKind::Try(inner) => {
                let (kind, ty) = self.desugar_try(inner, expr.span, scope)?;
                (kind, ty)
            }
            // Inline conditional `THEN if COND else ELSE`. Desugared here into a
            // hoisted temporary plus an `if` statement so the IR interpreter,
            // bytecode VM, native, and WASM backends need no conditional node
            // (mirrors the `?` desugar). The result flows back as a reference to
            // the temporary.
            ExprKind::Conditional {
                cond,
                then_branch,
                else_branch,
            } => {
                let temp = self.desugar_conditional(
                    cond,
                    then_branch,
                    else_branch,
                    expected,
                    expr.span,
                    scope,
                )?;
                (temp.kind, temp.ty)
            }
            // Membership `VALUE in COLLECTION` desugars to a builtin call, so no
            // backend needs an `in` node: `contains(collection, value)` for a
            // string (a char value coerced via `to_string`), or
            // `list_contains(collection, value)` for a `list<T>`.
            ExprKind::In { value, collection } => {
                let coll = self.lower_expr(collection, scope)?;
                let val = self.lower_expr(value, scope)?;
                let call = if coll.ty.name == "string" {
                    let needle = if val.ty.name == "char" {
                        self.to_string_wrap(val)
                    } else {
                        val
                    };
                    IrExprKind::Call {
                        name: "contains".to_string(),
                        args: vec![coll, needle],
                    }
                } else {
                    IrExprKind::Call {
                        name: "list_contains".to_string(),
                        args: vec![coll, val],
                    }
                };
                (call, TypeRef::new("bool"))
            }
            // String slice `target[start:end]` desugars to
            // `substring(target, start, end)`. An omitted `start` is `0`; an
            // omitted `end` is `len(target)` — in which case `target` is bound to
            // a temp so it is evaluated exactly once, before `start`.
            ExprKind::Slice { target, start, end } => {
                let i64_ty = TypeRef::new("i64");
                let string_ty = TypeRef::new("string");
                let span = expr.span;
                let target_ir = self.lower_expr(target, scope)?;

                let (target_arg, len_end): (IrExpr, Option<IrExpr>) = if end.is_none() {
                    let id = self.next_cond_temp.get();
                    self.next_cond_temp.set(id + 1);
                    let temp = format!("__slice_{id}");
                    self.try_prelude.borrow_mut().push(IrStmt::Let {
                        name: temp.clone(),
                        ty: string_ty.clone(),
                        value: target_ir,
                        span,
                    });
                    let var = IrExpr {
                        kind: IrExprKind::Variable(temp),
                        ty: string_ty.clone(),
                        span,
                    };
                    let len_call = IrExpr {
                        kind: IrExprKind::Call {
                            name: "len".to_string(),
                            args: vec![var.clone()],
                        },
                        ty: i64_ty.clone(),
                        span,
                    };
                    (var, Some(len_call))
                } else {
                    (target_ir, None)
                };

                let start_ir = match start {
                    Some(start) => self.lower_expr(start, scope)?,
                    None => IrExpr {
                        kind: IrExprKind::Integer(0),
                        ty: i64_ty.clone(),
                        span,
                    },
                };
                let end_ir = match end {
                    Some(end) => self.lower_expr(end, scope)?,
                    None => len_end.expect("len bound for omitted slice end"),
                };

                (
                    IrExprKind::Call {
                        name: "substring".to_string(),
                        args: vec![target_arg, start_ir, end_ir],
                    },
                    string_ty,
                )
            }
            // Lower a closure literal: lower its body in a child scope that layers
            // the closure parameters over the enclosing scope, register the lowered
            // `(param names, body)` in the module's closure table keyed by the
            // parse-order id, and emit a body-less `Closure { id }` node whose type
            // is `fn(param types) -> typeof(body)`. This mirrors the semantics
            // typing so IR types agree with the checker.
            ExprKind::Closure { id, params, body } => {
                let mut body_scope = scope.clone();
                for param in params {
                    body_scope.insert(param.name.clone(), param.ty.clone());
                }
                let lowered_body = self.lower_expr(body, &body_scope)?;
                let param_types: Vec<TypeRef> =
                    params.iter().map(|param| param.ty.clone()).collect();
                let ty = function_type(&param_types, &lowered_body.ty);
                self.closures.borrow_mut().push(IrClosureDef {
                    id: *id,
                    params: params.iter().map(|param| param.name.clone()).collect(),
                    body: lowered_body,
                });
                (IrExprKind::Closure { id: *id }, ty)
            }
        };

        Ok(IrExpr {
            kind,
            ty,
            span: expr.span,
        })
    }

    /// Desugar a postfix `EXPR?`. Lowers the operand, hoists the propagation
    /// scaffolding into [`Lowerer::try_prelude`], and returns the `(kind, ty)` of
    /// a reference to the freshly bound success temporary `__try_v_N: T`.
    ///
    /// For a `result<T, E>` operand it emits, in order:
    ///
    /// ```text
    /// let __try_q_N = <operand>          # result<T, E>
    /// let __try_v_N: T = __try_q_N       # initial binding, overwritten below
    /// match __try_q_N
    ///     ok(__try_ok_N) -> __try_v_N = __try_ok_N
    ///     err(__try_err_N) -> return err(__try_err_N)
    /// ```
    ///
    /// and for an `option<T>` operand the analogous `some`/`none` shape. The
    /// initial binding is immediately overwritten by the `ok`/`some` arm before
    /// any read, and the failure arm `return`s first, so its value is never
    /// observed; the interpreters are dynamically typed and the native/WASM
    /// backends demote any function containing a `match` to the interpreter, so
    /// the desugared IR runs identically on every backend.
    fn desugar_try(
        &self,
        inner: &Expr,
        span: Span,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<(IrExprKind, TypeRef), IrLoweringError> {
        let operand = self.lower_expr(inner, scope)?;
        let operand_ty = operand.ty.clone();
        let return_type = self.current_return_type.borrow().clone();

        // Fresh, collision-free temp names for this `?` site.
        let id = self.next_try_temp.get();
        self.next_try_temp.set(id + 1);
        let q_name = format!("__try_q_{id}");
        let v_name = format!("__try_v_{id}");
        let bind_name = format!("__try_x_{id}");

        // Resolve `(success variant, failure variant, payload type T)` from the
        // operand type. Semantics guarantees the operand is an `option`/`result`
        // and the return type is compatible, so anything else is a lowering bug.
        let (success_variant, failure_variant, payload_ty) =
            if let Some((ok_ty, _)) = operand_ty.result_args() {
                ("ok", "err", ok_ty)
            } else if let Some(payload) = operand_ty.option_element() {
                ("some", "none", payload)
            } else {
                return Err(IrLoweringError::new(
                    format!(
                        "`?` operand has non-option/result type `{}`",
                        operand_ty.name
                    ),
                    Some(span),
                ));
            };

        // `let __try_q_N = <operand>`
        let q_let = IrStmt::Let {
            name: q_name.clone(),
            ty: operand_ty.clone(),
            value: operand,
            span,
        };

        // `let __try_v_N: T = __try_q_N` (initial binding, overwritten by the
        // success arm before any read).
        let v_let = IrStmt::Let {
            name: v_name.clone(),
            ty: payload_ty.clone(),
            value: IrExpr {
                kind: IrExprKind::Variable(q_name.clone()),
                ty: operand_ty.clone(),
                span,
            },
            span,
        };

        // Success arm: `variant(__try_x_N) -> __try_v_N = __try_x_N`.
        let success_arm = IrMatchArm {
            pattern: IrMatchPattern::Variant {
                name: success_variant.to_string(),
                bindings: vec![bind_name.clone()],
            },
            body: vec![IrStmt::Assign {
                name: v_name.clone(),
                path: Vec::new(),
                op: AssignOp::Replace,
                value: IrExpr {
                    kind: IrExprKind::Variable(bind_name.clone()),
                    ty: payload_ty.clone(),
                    span,
                },
                span,
            }],
        };

        // Failure arm: `err(__try_x_N) -> return err(__try_x_N)` (or the `none`
        // analogue), rebuilding the failure value at the function's return type.
        let failure_arm = if failure_variant == "err" {
            let err_bind = format!("__try_e_{id}");
            let (_, err_ty) = operand_ty.result_args().ok_or_else(|| {
                IrLoweringError::new(
                    format!("`?` operand `{}` is not a result", operand_ty.name),
                    Some(span),
                )
            })?;
            IrMatchArm {
                pattern: IrMatchPattern::Variant {
                    name: "err".to_string(),
                    bindings: vec![err_bind.clone()],
                },
                body: vec![IrStmt::Return(Some(IrExpr {
                    kind: IrExprKind::Call {
                        name: "err".to_string(),
                        args: vec![IrExpr {
                            kind: IrExprKind::Variable(err_bind),
                            ty: err_ty,
                            span,
                        }],
                    },
                    ty: return_type.clone(),
                    span,
                }))],
            }
        } else {
            // `none -> return none` (a unit variant lowered as a no-arg `Call`,
            // matching how bare `none` construction lowers elsewhere).
            IrMatchArm {
                pattern: IrMatchPattern::Variant {
                    name: "none".to_string(),
                    bindings: Vec::new(),
                },
                body: vec![IrStmt::Return(Some(IrExpr {
                    kind: IrExprKind::Call {
                        name: "none".to_string(),
                        args: Vec::new(),
                    },
                    ty: return_type.clone(),
                    span,
                }))],
            }
        };

        let match_stmt = IrStmt::Match {
            scrutinee: IrExpr {
                kind: IrExprKind::Variable(q_name),
                ty: operand_ty,
                span,
            },
            arms: vec![success_arm, failure_arm],
            span,
        };

        // Hoist the scaffolding, in order, ahead of the statement being lowered.
        {
            let mut prelude = self.try_prelude.borrow_mut();
            prelude.push(q_let);
            prelude.push(v_let);
            prelude.push(match_stmt);
        }

        Ok((IrExprKind::Variable(v_name), payload_ty))
    }

    /// Desugar an inline conditional `THEN if COND else ELSE` into a hoisted
    /// temporary plus an `if` statement, returning a reference to the temporary.
    ///
    /// ```text
    /// let __cond_N: T = <zero of T>      # dead init, overwritten by both arms
    /// if COND:
    ///     <THEN's own hoisted prelude>
    ///     __cond_N = THEN
    /// else:
    ///     <ELSE's own hoisted prelude>
    ///     __cond_N = ELSE
    /// ```
    ///
    /// `COND` is evaluated unconditionally, so its prelude stays in the outer
    /// statement prelude; each branch's own prelude (from a nested `?`/ternary)
    /// is captured and placed inside that branch so it runs only when taken. The
    /// temporary's zero initializer is never observed (both arms assign before
    /// any read); semantics restricts the result type to a scalar or `string`
    /// (`L0436`), so a correctly-typed zero always exists and every backend can
    /// compile the desugared `if`.
    fn desugar_conditional(
        &self,
        cond: &Expr,
        then_branch: &Expr,
        else_branch: &Expr,
        expected: Option<&TypeRef>,
        span: Span,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<IrExpr, IrLoweringError> {
        // The condition is always evaluated: lower it normally so any prelude it
        // produces stays ahead of the `if` in the outer statement prelude.
        let cond_ir = self.lower_expr(cond, scope)?;
        // Each branch is only evaluated when taken: capture its own prelude so
        // nested hoists land inside the branch body, not before the `if`.
        let (then_prelude, then_ir) = self.lower_captured(then_branch, expected, scope)?;
        let (else_prelude, else_ir) = self.lower_captured(else_branch, expected, scope)?;

        let result_ty = expected.cloned().unwrap_or_else(|| then_ir.ty.clone());
        let id = self.next_cond_temp.get();
        self.next_cond_temp.set(id + 1);
        let temp = format!("__cond_{id}");

        let zero = self.zero_ir_expr(&result_ty, span)?;
        self.try_prelude.borrow_mut().push(IrStmt::Let {
            name: temp.clone(),
            ty: result_ty.clone(),
            value: zero,
            span,
        });

        let mut then_body = then_prelude;
        then_body.push(IrStmt::Assign {
            name: temp.clone(),
            path: Vec::new(),
            op: AssignOp::Replace,
            value: then_ir,
            span,
        });
        let mut else_body = else_prelude;
        else_body.push(IrStmt::Assign {
            name: temp.clone(),
            path: Vec::new(),
            op: AssignOp::Replace,
            value: else_ir,
            span,
        });
        self.try_prelude.borrow_mut().push(IrStmt::If {
            branches: vec![IrIfBranch {
                condition: cond_ir,
                body: then_body,
            }],
            else_body,
            span,
        });

        Ok(IrExpr {
            kind: IrExprKind::Variable(temp),
            ty: result_ty,
            span,
        })
    }

    /// Wrap an expression in a `to_string(...)` call typed `string`. Used to
    /// coerce a `char` operand of string concatenation into a one-character
    /// string so every backend sees a plain string-valued operand.
    fn to_string_wrap(&self, expr: IrExpr) -> IrExpr {
        let span = expr.span;
        IrExpr {
            kind: IrExprKind::Call {
                name: "to_string".to_string(),
                args: vec![expr],
            },
            ty: TypeRef::new("string"),
            span,
        }
    }

    /// Coerce a `string + char` / `char + string` addition so the char operand
    /// becomes a `to_string(...)` string; any other operands pass through
    /// unchanged. Semantics has already accepted the operand types.
    fn coerce_string_char_add(
        &self,
        op: BinaryOp,
        left: IrExpr,
        right: IrExpr,
    ) -> (IrExpr, IrExpr) {
        if !matches!(op, BinaryOp::Add) {
            return (left, right);
        }
        let string = TypeRef::new("string");
        let char_ty = TypeRef::new("char");
        if left.ty == string && right.ty == char_ty {
            (left, self.to_string_wrap(right))
        } else if left.ty == char_ty && right.ty == string {
            (self.to_string_wrap(left), right)
        } else {
            (left, right)
        }
    }

    /// Lower an expression while capturing exactly the statement-prelude entries
    /// its own lowering produced (e.g. a nested `?` or inline conditional),
    /// leaving any earlier prelude in place. Used to keep a conditional branch's
    /// hoisted work inside the branch that guards it.
    fn lower_captured(
        &self,
        expr: &Expr,
        expected: Option<&TypeRef>,
        scope: &HashMap<String, TypeRef>,
    ) -> Result<(Vec<IrStmt>, IrExpr), IrLoweringError> {
        let saved = self.try_prelude.borrow().len();
        let lowered = self.lower_expr_expected(expr, expected, scope)?;
        let prelude: Vec<IrStmt> = self.try_prelude.borrow_mut().drain(saved..).collect();
        Ok((prelude, lowered))
    }

    /// A type-correct zero value for `ty`, used as the dead initializer of an
    /// inline-conditional temporary. Only scalars and `string` are supported
    /// (semantics enforces this with `L0436`); anything else is a lowering bug.
    fn zero_ir_expr(&self, ty: &TypeRef, span: Span) -> Result<IrExpr, IrLoweringError> {
        let kind = match ty.name.as_str() {
            "i64" | "i8" | "i16" | "i32" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize"
            | "byte" => IrExprKind::Integer(0),
            "bool" => IrExprKind::Bool(false),
            "char" => IrExprKind::Char('\0'),
            "f64" | "f32" => IrExprKind::Float(0.0),
            "string" => IrExprKind::String(String::new()),
            other => {
                return Err(IrLoweringError::new(
                    format!(
                        "inline conditional over `{other}` is not supported; use an `if` statement"
                    ),
                    Some(span),
                ));
            }
        };
        Ok(IrExpr {
            kind,
            ty: ty.clone(),
            span,
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
            // `name` was bound to one of these four constructor names by the
            // `(name, payload_expr)` match at the top of this function.
            _ => unreachable!("constructor name is one of some/none/ok/err"),
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
            | "console_log" | "dom_set_text" | "flush" | "sleep_millis" | "assert"
            | "rc_release" | "ptr_write" | "volatile_store" | "region_create" | "tcp_close"
            | "tcp_shutdown" => TypeRef::new("void"),
            // Raw-memory layout queries fold to `i64` constants; the pointer
            // cast `ptr_to_int` likewise yields the integer handle.
            "size_of" | "align_of" | "offset_of" | "ptr_to_int" => TypeRef::new("i64"),
            // `int_to_ptr(n)` reconstructs a raw pointer. The concrete pointee is
            // fixed by the surrounding `let`/parameter annotation; the call node
            // itself carries the generic `ptr<i64>` handle spelling.
            "int_to_ptr" => TypeRef::new("ptr<i64>"),
            // `volatile_load(p)` reads the pointer's element type, like `load`.
            "volatile_load" => {
                let ptr = args.first().ok_or_else(|| {
                    IrLoweringError::new("volatile_load call missing pointer argument", Some(span))
                })?;
                ptr.ty.pointer_target().ok_or_else(|| {
                    IrLoweringError::new("volatile_load call argument is not a pointer", Some(span))
                })?
            }
            // Network builtins report failures as runtime `result` values.
            "tcp_connect" | "tcp_listen" | "tcp_accept" | "udp_bind" => {
                generic_type("result", &[TypeRef::new("Socket"), TypeRef::new("string")])
            }
            "tcp_read" | "udp_recv" | "http_get" | "http_post" | "from_bytes" | "proc_stdout"
            | "proc_stderr" => {
                generic_type("result", &[TypeRef::new("string"), TypeRef::new("string")])
            }
            // Non-blocking accept yields `result<option<Socket>, string>` where
            // the `ok(none)` arm is a would-block signal.
            "tcp_accept_nb" => generic_type(
                "result",
                &[option_type(&TypeRef::new("Socket")), TypeRef::new("string")],
            ),
            // Non-blocking read/recv yields `result<option<string>, string>`;
            // `ok(none)` is a would-block signal.
            "tcp_read_nb" | "udp_recv_nb" => generic_type(
                "result",
                &[option_type(&TypeRef::new("string")), TypeRef::new("string")],
            ),
            "tcp_write" | "udp_send_to" | "set_nonblocking" | "parse_i64" | "proc_wait"
            | "proc_kill" => generic_type("result", &[TypeRef::new("i64"), TypeRef::new("string")]),
            "parse_f64" => generic_type("result", &[TypeRef::new("f64"), TypeRef::new("string")]),
            // Process spawn returns a `process` handle in the `ok` arm.
            "proc_spawn" => {
                generic_type("result", &[TypeRef::new("process"), TypeRef::new("string")])
            }
            "read_file" | "sys_output" | "to_string" | "substring" | "join" | "trim"
            | "replace" | "upper" | "lower" | "repeat" => TypeRef::new("string"),
            "read_lines" | "list_dir" => {
                generic_type("list", std::slice::from_ref(&TypeRef::new("string")))
            }
            "read_bytes" | "to_bytes" => {
                generic_type("list", std::slice::from_ref(&TypeRef::new("byte")))
            }
            "file_exists" | "is_file" | "is_dir" | "contains" | "starts_with" | "ends_with"
            | "map_has" | "is_digit" | "is_alpha" | "is_alnum" | "is_whitespace" | "is_upper"
            | "is_lower" | "list_contains" => TypeRef::new("bool"),
            "sys_status" | "file_size" | "len" | "find" | "count" | "map_len" | "char_code"
            | "byte_val" | "byte_len" | "mono_now" | "wall_now" | "list_index_of" | "to_i64"
            | "sign" | "gcd" => TypeRef::new("i64"),
            "to_i8" => TypeRef::new("i8"),
            "to_u8" => TypeRef::new("u8"),
            "to_i16" => TypeRef::new("i16"),
            "to_i32" => TypeRef::new("i32"),
            "to_u16" => TypeRef::new("u16"),
            "to_u32" => TypeRef::new("u32"),
            "to_u64" => TypeRef::new("u64"),
            "to_isize" => TypeRef::new("isize"),
            "to_usize" => TypeRef::new("usize"),
            "to_f32" => TypeRef::new("f32"),
            // saturating/wrapping arithmetic returns the operand width `T`.
            "saturating_add" | "saturating_sub" | "saturating_mul" | "wrapping_add"
            | "wrapping_sub" | "wrapping_mul" => args
                .first()
                .map(|operand| operand.ty.clone())
                .ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing operand"), Some(span))
                })?,
            // checked arithmetic returns `option<T>`.
            "checked_add" | "checked_sub" | "checked_mul" => {
                let operand = args
                    .first()
                    .map(|operand| operand.ty.clone())
                    .ok_or_else(|| {
                        IrLoweringError::new(format!("{name} call missing operand"), Some(span))
                    })?;
                generic_type("option", std::slice::from_ref(&operand))
            }
            "char_from" => TypeRef::new("char"),
            "byte" => TypeRef::new("byte"),
            // `push`/`set`/`pop`/`reverse`/`concat`/`slice` return a new `list<T>`
            // of the same type as their (first) list argument (spelled `list<T>`).
            "push" | "set" | "pop" | "reverse" | "sort" | "sort_by" | "concat" | "slice" => {
                args.first().map(|list| list.ty.clone()).ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing list argument"), Some(span))
                })?
            }
            // `array_fill(n, value)` yields `array<T>` where `T` is the value's type.
            "array_fill" => {
                let value = args.get(1).ok_or_else(|| {
                    IrLoweringError::new("array_fill call missing value argument", Some(span))
                })?;
                TypeRef::new(format!("array<{}>", value.ty.name))
            }
            // `list_map(l list<T>, f fn(T) -> U)` yields `list<U>`, where `U` is
            // the mapping function's return type.
            "list_map" => {
                let func = args.get(1).ok_or_else(|| {
                    IrLoweringError::new("list_map call missing function argument", Some(span))
                })?;
                let (_, ret) = func.ty.function_signature().ok_or_else(|| {
                    IrLoweringError::new(
                        "list_map function argument is not a function type",
                        Some(span),
                    )
                })?;
                generic_type("list", std::slice::from_ref(&ret))
            }
            // `list_filter(l list<T>, pred fn(T) -> bool)` yields `list<T>`, the
            // same type as its list argument.
            "list_filter" => args.first().map(|list| list.ty.clone()).ok_or_else(|| {
                IrLoweringError::new("list_filter call missing list argument", Some(span))
            })?,
            // `list_reduce(l list<T>, init U, f fn(U, T) -> U)` yields `U`, the
            // accumulator type carried by the `init` argument.
            "list_reduce" => args.get(1).map(|init| init.ty.clone()).ok_or_else(|| {
                IrLoweringError::new("list_reduce call missing init argument", Some(span))
            })?,
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
            // `map_keys(m) -> list<K>` and `map_values(m) -> list<V>`.
            "map_keys" | "map_values" => {
                let map = args.first().ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing map argument"), Some(span))
                })?;
                let mut kv = map
                    .ty
                    .generic_args("map")
                    .filter(|args| args.len() == 2)
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("{name} call argument is not a map"),
                            Some(span),
                        )
                    })?;
                let element = if name == "map_keys" {
                    kv.remove(0)
                } else {
                    kv.remove(1)
                };
                generic_type("list", std::slice::from_ref(&element))
            }
            "split" | "words" => TypeRef::new("array<string>"),
            "chars" => generic_type("list", std::slice::from_ref(&TypeRef::new("char"))),
            "string_from_chars" => TypeRef::new("string"),
            // `env(name)` yields `option<string>`; `args()` yields `list<string>`.
            "env" => generic_type("option", std::slice::from_ref(&TypeRef::new("string"))),
            "args" => generic_type("list", std::slice::from_ref(&TypeRef::new("string"))),
            // `os_random(len)` yields `result<list<byte>, string>`.
            "os_random" => generic_type(
                "result",
                &[
                    generic_type("list", std::slice::from_ref(&TypeRef::new("byte"))),
                    TypeRef::new("string"),
                ],
            ),
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
            // Atomic (`atomic_i64`) builtins: the constructor yields the handle,
            // `atomic_store` is `void`, and every access/RMW yields `i64`.
            "atomic_new" => TypeRef::new("atomic_i64"),
            "atomic_store" | "atomic_store_ordered" | "fence" => TypeRef::new("void"),
            "atomic_load"
            | "atomic_swap"
            | "atomic_cas"
            | "atomic_add"
            | "atomic_sub"
            | "atomic_and"
            | "atomic_or"
            | "atomic_xor"
            | "atomic_load_ordered"
            | "atomic_swap_ordered"
            | "atomic_cas_ordered"
            | "atomic_add_ordered"
            | "atomic_sub_ordered"
            | "atomic_and_ordered"
            | "atomic_or_ordered"
            | "atomic_xor_ordered" => TypeRef::new("i64"),
            "sqrt" | "floor" | "ceil" | "round" | "sin" | "cos" | "tan" | "atan" | "exp" | "ln"
            | "log10" | "atan2" | "to_f64" => TypeRef::new("f64"),
            // Bit intrinsics on i64: rotations, popcount, leading/trailing zero
            // counts, and byte swap all return i64.
            "rotate_left" | "rotate_right" | "count_ones" | "leading_zeros" | "trailing_zeros"
            | "reverse_bytes" => TypeRef::new("i64"),
            "abs" | "min" | "max" | "pow" | "clamp" => {
                let value = args.first().ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing argument"), Some(span))
                })?;
                TypeRef::new(value.ty.name.clone())
            }
            // `list_sum(l)` returns the numeric element type `T` of its `list<T>`.
            "list_sum" => {
                let list = args.first().ok_or_else(|| {
                    IrLoweringError::new("list_sum call missing list argument", Some(span))
                })?;
                list.ty
                    .generic_args("list")
                    .filter(|args| args.len() == 1)
                    .map(|mut args| args.remove(0))
                    .ok_or_else(|| {
                        IrLoweringError::new("list_sum call argument is not a list", Some(span))
                    })?
            }
            // `list_min(l)` / `list_max(l)` return `option<T>` over a `list<T>`.
            "list_min" | "list_max" => {
                let list = args.first().ok_or_else(|| {
                    IrLoweringError::new(format!("{name} call missing list argument"), Some(span))
                })?;
                let element = list
                    .ty
                    .generic_args("list")
                    .filter(|args| args.len() == 1)
                    .map(|mut args| args.remove(0))
                    .ok_or_else(|| {
                        IrLoweringError::new(
                            format!("{name} call argument is not a list"),
                            Some(span),
                        )
                    })?;
                generic_type("option", std::slice::from_ref(&element))
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
                    // Calling an `async fn` yields a `Future<return_type>`, matching
                    // the semantic type; `await` later resolves the inner `T`.
                    if signature.is_async {
                        generic_type("Future", std::slice::from_ref(&signature.return_type))
                    } else {
                        signature.return_type.clone()
                    }
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

    /// Recursively collect every slot-resolved `Local { name, packed }` and every
    /// remaining name-scanned `Variable(name)` read in a resolved block. Used by the
    /// slot-resolution tests to assert both that reads are rewritten and that their
    /// `(depth, slot)` encoding is exactly right.
    fn collect_reads(body: &[IrStmt], locals: &mut Vec<(String, u32)>, vars: &mut Vec<String>) {
        fn walk_expr(e: &IrExpr, locals: &mut Vec<(String, u32)>, vars: &mut Vec<String>) {
            match &e.kind {
                IrExprKind::Local { name, packed } => locals.push((name.clone(), *packed)),
                IrExprKind::Variable(name) => vars.push(name.clone()),
                IrExprKind::Binary { left, right, .. } => {
                    walk_expr(left, locals, vars);
                    walk_expr(right, locals, vars);
                }
                IrExprKind::Unary { expr: inner, .. } | IrExprKind::Await { expr: inner } => {
                    walk_expr(inner, locals, vars);
                }
                IrExprKind::Index { target, index } => {
                    walk_expr(target, locals, vars);
                    walk_expr(index, locals, vars);
                }
                IrExprKind::Field { target, .. } => walk_expr(target, locals, vars),
                IrExprKind::Call { args, .. } => {
                    for arg in args {
                        walk_expr(arg, locals, vars);
                    }
                }
                IrExprKind::Array(items) => {
                    for item in items {
                        walk_expr(item, locals, vars);
                    }
                }
                _ => {}
            }
        }
        for stmt in body {
            match stmt {
                IrStmt::Let { value, .. }
                | IrStmt::Expr(value)
                | IrStmt::Throw { value, .. } => walk_expr(value, locals, vars),
                IrStmt::Return(Some(value)) => walk_expr(value, locals, vars),
                IrStmt::Assign { value, path, .. } => {
                    walk_expr(value, locals, vars);
                    for place in path {
                        if let IrPlace::Index(index) = place {
                            walk_expr(index, locals, vars);
                        }
                    }
                }
                IrStmt::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for branch in branches {
                        walk_expr(&branch.condition, locals, vars);
                        collect_reads(&branch.body, locals, vars);
                    }
                    collect_reads(else_body, locals, vars);
                }
                IrStmt::While {
                    condition, body, ..
                } => {
                    walk_expr(condition, locals, vars);
                    collect_reads(body, locals, vars);
                }
                IrStmt::For {
                    start,
                    end,
                    step,
                    body,
                    ..
                } => {
                    walk_expr(start, locals, vars);
                    walk_expr(end, locals, vars);
                    if let Some(step) = step {
                        walk_expr(step, locals, vars);
                    }
                    collect_reads(body, locals, vars);
                }
                IrStmt::Loop { body, .. } => collect_reads(body, locals, vars),
                IrStmt::Try {
                    body, catch_body, ..
                } => {
                    collect_reads(body, locals, vars);
                    collect_reads(catch_body, locals, vars);
                }
                IrStmt::Match { scrutinee, arms, .. } => {
                    walk_expr(scrutinee, locals, vars);
                    for arm in arms {
                        collect_reads(&arm.body, locals, vars);
                    }
                }
                _ => {}
            }
        }
    }

    #[test]
    fn slot_resolution_rewrites_local_reads_with_exact_slots() {
        let mut module =
            lower_source("fn add x i64 y i64 -> i64\n    let s i64 = x + y\n    s\n");
        resolve_module_slots(&mut module);
        let mut locals = Vec::new();
        let mut vars = Vec::new();
        collect_reads(&module.functions[0].body, &mut locals, &mut vars);
        // Params `x`,`y` are slots 0,1 of the function scope; `s` is slot 2. Every
        // read is resolved (function scope is the innermost, so depth 0).
        assert!(locals.contains(&("x".to_string(), pack_slot(0, 0).unwrap())));
        assert!(locals.contains(&("y".to_string(), pack_slot(0, 1).unwrap())));
        assert!(locals.contains(&("s".to_string(), pack_slot(0, 2).unwrap())));
        assert!(
            vars.is_empty(),
            "every local read should be slot-resolved, found name-scanned {vars:?}"
        );
    }

    #[test]
    fn slot_resolution_models_the_two_scope_for_loop() {
        // The `for` loop variable lives one scope above the body (the evaluator
        // pushes a loop-var scope, then a separate body scope each iteration), so
        // from inside the body `i` is at depth 1 slot 0 and the outer `t` — the
        // only function-scope local — is at depth 2 slot 0.
        let mut module = lower_source(
            "fn main -> i64\n    let t i64 = 0\n    for i from 1 to 3\n        t = t + i\n    t\n",
        );
        resolve_module_slots(&mut module);
        let mut locals = Vec::new();
        let mut vars = Vec::new();
        collect_reads(&module.functions[0].body, &mut locals, &mut vars);
        assert!(
            locals.contains(&("i".to_string(), pack_slot(1, 0).unwrap())),
            "loop var `i` should resolve to depth 1 slot 0, got {locals:?}"
        );
        assert!(
            locals.contains(&("t".to_string(), pack_slot(2, 0).unwrap())),
            "outer `t` read inside the loop body should be depth 2 slot 0, got {locals:?}"
        );
    }

    #[test]
    fn slot_resolution_keeps_shadowing_correct_across_backends() {
        // The inner `x` (10) shadows the outer `x` (1) only inside the `if` body,
        // so `s` accumulates 10 there and 1 after — 11 total. A wrong `(depth,
        // slot)` for either `x` would read the wrong binding; every backend
        // (including the slot-resolved IR/bytecode) must still return 11.
        let source = "fn main -> i64\n    let x i64 = 1\n    let s i64 = 0\n    if x == 1\n        let x i64 = 10\n        s = s + x\n    s = s + x\n    s\n";
        let (ast, ir, bytecode) = run_all_backends(source);
        assert_eq!(ast, Value::I64(11));
        assert_eq!(ir, Value::I64(11));
        assert_eq!(bytecode, Value::I64(11));
    }

    fn run_all_backend_variants(source: &str) -> (Value, Value, Value, Value, Value) {
        let tokens = lex(source).expect("lex");
        let program = parse(&tokens).expect("parse");
        let checked = validate_executable(&program).expect("semantic");
        let ir = lower(&checked).expect("lower");
        let bytecode = lower_to_bytecode(&ir);
        let (optimized, _) = optimize(&ir, &OptimizationConfig::full());
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
    fn memory_analysis_reports_memory_operations_and_safety_metadata() {
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
    fn full_optimizer_runs_full_pass_pipeline() {
        let module =
            lower_source("fn main -> i64\n    let value i64 = 40 + 2\n    return value\n    0\n");
        let (optimized, report) = optimize(&module, &OptimizationConfig::full());

        assert_eq!(
            report.applied_passes,
            vec![
                OptimizationPass::Inlining,
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
    fn inlining_replaces_leaf_helper_calls_and_preserves_results() {
        // `rem` is a pure leaf helper; its call inside `f` should be inlined to
        // `n - (n / 3) * 3`, and the program's result must be unchanged.
        let module = lower_source(
            "fn rem a i64 b i64 -> i64\n    a - (a / b) * b\nfn f n i64 -> i64\n    rem(n, 3)\nfn main -> i64\n    f(17)\n",
        );
        let before = run_main(&module).expect("run before");
        let (optimized, report) = optimize(&module, &OptimizationConfig::inlining());
        assert_eq!(report.inlined_calls, 1, "the rem call should be inlined");
        // `f`'s body no longer contains a Call node.
        let f = optimized
            .functions
            .iter()
            .find(|fun| fun.name == "f")
            .expect("f exists");
        let body = match &f.body[0] {
            IrStmt::Return(Some(body)) | IrStmt::Expr(body) => body,
            other => panic!("expected a single yielded expression, got {other:?}"),
        };
        assert!(
            !contains_call(body),
            "inlined body must have no call: {body:?}"
        );
        assert_eq!(run_main(&optimized).expect("run after"), before);
        assert_eq!(before, Value::I64(2)); // 17 % 3 == 2
    }

    fn contains_call(expr: &IrExpr) -> bool {
        match &expr.kind {
            IrExprKind::Call { .. } => true,
            IrExprKind::Unary { expr, .. } => contains_call(expr),
            IrExprKind::Binary { left, right, .. } => contains_call(left) || contains_call(right),
            _ => false,
        }
    }

    #[test]
    fn bytecode_artifact_round_trips_and_executes() {
        let ir = lower_source("fn main -> i64\n    40 + 2\n");
        let bytecode = lower_to_bytecode(&ir);
        let encoded = encode_bytecode_artifact(&bytecode).expect("encode artifact");
        let artifact = decode_bytecode_artifact(&encoded).expect("decode artifact");

        assert_eq!(artifact.format, BYTECODE_ARTIFACT_FORMAT);
        assert_eq!(artifact.version, BYTECODE_ARTIFACT_VERSION);
        assert_eq!(artifact.metadata.target, "lullaby-vm");
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
            "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":2,\"entry\":\"main\",\"metadata\":{{\"producer\":\"test\",\"target\":\"lullaby-vm\",\"payload\":\"structured-bytecode\"}},\"function_table\":[],\"module\":{{\"functions\":[]}}}}"
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
            "{{\"format\":\"{BYTECODE_ARTIFACT_FORMAT}\",\"version\":{BYTECODE_ARTIFACT_VERSION},\"entry\":\"main\",\"metadata\":{{\"producer\":\"test\",\"target\":\"lullaby-vm\",\"payload\":\"instruction-bytecode\"}},\"function_table\":[],\"module\":{{\"functions\":[]}}}}"
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
            async_functions: Vec::new(),
            extern_functions: Vec::new(),
            extern_signatures: Vec::new(),
            export_functions: Vec::new(),
            closures: Vec::new(),
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
            async_functions: Vec::new(),
            extern_functions: Vec::new(),
            extern_signatures: Vec::new(),
            export_functions: Vec::new(),
            closures: Vec::new(),
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
        let (optimized, report) = optimize(&ir, &OptimizationConfig::full());
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
    fn to_bytes_from_bytes_byte_len_match_across_backend_variants() {
        // Round-trips "Hi" through `to_bytes`/`from_bytes`, checks byte values,
        // and contrasts `byte_len` (UTF-8 bytes) with `len` (characters) on a
        // multi-byte string. 72 + 105 + len("Hi")=2 + (byte_len=5 - len=4)=1.
        let source = concat!(
            "fn main -> i64\n",
            "    let bytes list<byte> = to_bytes(\"Hi\")\n",
            "    let first i64 = byte_val(get(bytes, 0))\n",
            "    let second i64 = byte_val(get(bytes, 1))\n",
            "    match from_bytes(bytes)\n",
            "        ok(s) -> first + second + len(s) + (byte_len(\"café\") - len(\"café\"))\n",
            "        err(m) -> 0 - len(m)\n",
        );
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ast, Value::I64(180));
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
        assert_eq!(optimized_ir, ast);
        assert_eq!(optimized_bytecode, ast);
    }

    #[test]
    fn from_bytes_invalid_utf8_err_matches_across_backend_variants() {
        // A lone `0xFF` byte is invalid UTF-8: every backend takes the `err`
        // branch identically (no panic, no lossy replacement).
        let source = concat!(
            "fn main -> i64\n",
            "    let bad list<byte> = push(list_new(), byte(255))\n",
            "    match from_bytes(bad)\n",
            "        ok(s) -> len(s)\n",
            "        err(m) -> 1\n",
        );
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ast, Value::I64(1));
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
        assert_eq!(optimized_ir, ast);
        assert_eq!(optimized_bytecode, ast);
    }

    #[test]
    fn os_random_structural_result_matches_across_backend_variants() {
        // `os_random` bytes are non-deterministic, so this asserts only
        // structural, backend-invariant facts: `os_random(16)` yields 16 bytes,
        // `os_random(0)` yields an empty list, and `os_random(-1)` yields `err`
        // (never a panic). Fixed total: 1 + 1 + 1 = 3 on every backend.
        let source = concat!(
            "fn ok_len n i64 -> i64\n",
            "    match os_random(n)\n",
            "        ok(bytes) -> len(bytes)\n",
            "        err(_) -> 0 - 1\n\n",
            "fn main -> i64\n",
            "    let a i64 = 0\n",
            "    if ok_len(16) == 16\n",
            "        a = 1\n",
            "    let b i64 = 0\n",
            "    if ok_len(0) == 0\n",
            "        b = 1\n",
            "    let c i64 = 0\n",
            "    if ok_len(0 - 1) == 0 - 1\n",
            "        c = 1\n",
            "    a + b + c\n",
        );
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ast, Value::I64(3));
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
        assert_eq!(optimized_ir, ast);
        assert_eq!(optimized_bytecode, ast);
    }

    // ---- move-on-functional-update optimization: correctness parity ----
    //
    // These exercise the `x = f(x, …)` accumulation idiom, whose target argument
    // is now MOVED (not cloned) out of the environment on the interpreter fast
    // path. Every case runs through all five interpreter variants
    // (AST / IR / bytecode / optimized IR / optimized bytecode) and asserts the
    // result equals what the pre-optimization clone path produced, proving the
    // move changes no observable behavior.

    fn assert_all_variants_eq(source: &str, expected: i64) {
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ast, Value::I64(expected), "AST result");
        assert_eq!(ir, ast, "IR result differs from AST");
        assert_eq!(bytecode, ast, "bytecode result differs from AST");
        assert_eq!(optimized_ir, ast, "optimized IR result differs from AST");
        assert_eq!(
            optimized_bytecode, ast,
            "optimized bytecode result differs from AST"
        );
    }

    #[test]
    fn move_update_builds_list_correctly_across_backends() {
        // `l = push(l, i)` fifty times, then sum every element back. The moved
        // accumulation must yield the exact same list a clone would: sum of
        // 0..=49 is 1225, and the length is 50.
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    for i from 0 to 49\n",
            "        l = push(l, i)\n",
            "    let total i64 = 0\n",
            "    let n i64 = len(l)\n",
            "    for i from 0 to n - 1\n",
            "        total += get(l, i)\n",
            "    total * 100 + len(l)\n",
        );
        // 1225 * 100 + 50 = 122550
        assert_all_variants_eq(source, 122550);
    }

    #[test]
    fn move_update_does_not_corrupt_aliased_binding_across_backends() {
        // Aliasing safety: `let b = a` clones `a`; a subsequent `a = push(a, 9)`
        // moves `a`'s slot but must leave the independent `b` untouched.
        let source = concat!(
            "fn main -> i64\n",
            "    let a list<i64> = list_new()\n",
            "    a = push(a, 1)\n",
            "    a = push(a, 2)\n",
            "    a = push(a, 3)\n",
            "    let b list<i64> = a\n",
            "    a = push(a, 9)\n",
            "    let bsum i64 = get(b, 0) + get(b, 1) + get(b, 2)\n",
            "    len(a) * 10000 + get(a, 3) * 100 + len(b) * 10 + bsum\n",
        );
        // a = [1,2,3,9] (len 4, a[3]=9); b = [1,2,3] (len 3, sum 6).
        // 4*10000 + 9*100 + 3*10 + 6 = 40936. A corrupted (aliased) b would make
        // len(b)=4 and bsum include the 9, changing the result.
        assert_all_variants_eq(source, 40936);
    }

    #[test]
    fn multi_occurrence_concat_is_not_optimized_but_correct_across_backends() {
        // `l = concat(l, l)` has the target in two arguments, so the move must NOT
        // fire; the result must still double the list correctly.
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 1)\n",
            "    l = push(l, 2)\n",
            "    l = concat(l, l)\n",
            "    len(l) * 10 + get(l, 0) + get(l, 1) + get(l, 2) + get(l, 3)\n",
        );
        // l = [1,2,1,2]: 4*10 + 1+2+1+2 = 46.
        assert_all_variants_eq(source, 46);
    }

    #[test]
    fn target_in_nested_argument_is_not_optimized_but_correct_across_backends() {
        // `l = set(l, len(l) - 1, 99)`: the target appears bare in arg0 and nested
        // inside arg1 (`len(l)`), so the move must NOT fire; setting the last
        // element must still work.
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 1)\n",
            "    l = push(l, 2)\n",
            "    l = push(l, 3)\n",
            "    l = set(l, len(l) - 1, 99)\n",
            "    get(l, 0) * 100 + get(l, 1) * 10 + get(l, 2)\n",
        );
        // l = [1,2,99]: 1*100 + 2*10 + 99 = 219.
        assert_all_variants_eq(source, 219);
    }

    #[test]
    fn move_update_sort_across_backends() {
        // `l = sort(l)` moves the list into the sort; the ordering must be exact.
        let source = concat!(
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 3)\n",
            "    l = push(l, 1)\n",
            "    l = push(l, 2)\n",
            "    l = sort(l)\n",
            "    get(l, 0) * 100 + get(l, 1) * 10 + get(l, 2)\n",
        );
        // sorted [1,2,3]: 123.
        assert_all_variants_eq(source, 123);
    }

    #[test]
    fn move_update_map_set_accumulation_across_backends() {
        // `m = map_set(m, k, v)` accumulation with a repeated key overwrite; the
        // moved map must read back the final values.
        let source = concat!(
            "fn get_or m map<string, i64> k string fallback i64 -> i64\n",
            "    match map_get(m, k)\n",
            "        some(v) -> v\n",
            "        none -> fallback\n\n",
            "fn main -> i64\n",
            "    let m map<string, i64> = map_new()\n",
            "    m = map_set(m, \"a\", 1)\n",
            "    m = map_set(m, \"b\", 2)\n",
            "    m = map_set(m, \"a\", 10)\n",
            "    get_or(m, \"a\", 0) * 1000 + get_or(m, \"b\", 0) * 10 + map_len(m)\n",
        );
        // a=10, b=2, map_len=2 => 10000 + 20 + 2 = 10022.
        assert_all_variants_eq(source, 10022);
    }

    #[test]
    fn move_update_string_replace_across_backends() {
        // `s = replace(s, from, to)` moves the string into the builtin; the result
        // must match the clone path exactly.
        let source = concat!(
            "fn main -> i64\n",
            "    let s string = \"aaa\"\n",
            "    s = replace(s, \"a\", \"b\")\n",
            "    s = replace(s, \"b\", \"cc\")\n",
            "    len(s)\n",
        );
        // "aaa" -> "bbb" -> "cccccc": length 6.
        assert_all_variants_eq(source, 6);
    }

    // ---- move-on-functional-update: the `+` OPERATOR string-building form ----
    //
    // `s = s + piece` in a loop now moves the accumulator out of the environment
    // to serve as the left operand, so `eval_binary`'s string concat reuses its
    // heap buffer (`push_str`) instead of cloning-then-reallocating. That makes
    // building O(n) rather than O(n^2). These fixtures prove the move changes no
    // observable result across all five interpreter variants.

    #[test]
    fn move_update_string_plus_builds_fifty_chars_across_backends() {
        // `s = s + "x"` fifty times: the buffer-reusing move must yield exactly
        // fifty characters, identical to the clone path.
        let source = concat!(
            "fn main -> i64\n",
            "    let s string = \"\"\n",
            "    for i from 1 to 50\n",
            "        s = s + \"x\"\n",
            "    len(s)\n",
        );
        assert_all_variants_eq(source, 50);
    }

    #[test]
    fn move_update_string_plus_to_string_across_backends() {
        // `s = s + to_string(i)` for i in 0..=9 builds "0123456789"; the other
        // operand is a call evaluated before the move. Length 10.
        let source = concat!(
            "fn main -> i64\n",
            "    let s string = \"\"\n",
            "    for i from 0 to 9\n",
            "        s = s + to_string(i)\n",
            "    len(s)\n",
        );
        assert_all_variants_eq(source, 10);
    }

    #[test]
    fn move_update_string_plus_does_not_corrupt_aliased_binding_across_backends() {
        // Aliasing safety for the `+` form: `let b = a` clones `a`; a later
        // `a = a + "c"` moves `a`'s slot but must leave `b` untouched.
        let source = concat!(
            "fn main -> i64\n",
            "    let a string = \"ab\"\n",
            "    let b string = a\n",
            "    a = a + \"c\"\n",
            "    len(a) * 10 + len(b)\n",
        );
        // a = "abc" (len 3), b = "ab" (len 2) => 32. A corrupted b would read len 3.
        assert_all_variants_eq(source, 32);
    }

    #[test]
    fn multi_occurrence_string_plus_is_not_optimized_but_correct_across_backends() {
        // `s = s + s` has the target as BOTH operands, so the move must NOT fire;
        // the doubling must still be correct.
        let source = concat!(
            "fn main -> i64\n",
            "    let s string = \"ab\"\n",
            "    s = s + s\n",
            "    s = s + s\n",
            "    len(s)\n",
        );
        // "ab" -> "abab" -> "abababab": length 8.
        assert_all_variants_eq(source, 8);
    }

    #[test]
    fn target_nested_with_others_in_string_plus_is_not_optimized_but_correct_across_backends() {
        // `s = pre + s + suf` parses as `(pre + s) + suf`: the target appears
        // alongside other operands (never as the single bare operand of the top
        // op), so the move must NOT fire; wrapping must still be correct.
        let source = concat!(
            "fn main -> i64\n",
            "    let s string = \"X\"\n",
            "    let pre string = \"[\"\n",
            "    let suf string = \"]\"\n",
            "    s = pre + s + suf\n",
            "    s = pre + s + suf\n",
            "    len(s)\n",
        );
        // "X" -> "[X]" -> "[[X]]": length 5.
        assert_all_variants_eq(source, 5);
    }

    #[test]
    fn move_update_string_plus_right_operand_target_across_backends() {
        // Right-operand-is-target: `s = p + s` has `s` as the single bare RIGHT
        // operand, so the move fires on that operand (prepending). Three
        // iterations: "!" -> "ab!" -> "abab!" -> "ababab!", length 7.
        let source = concat!(
            "fn main -> i64\n",
            "    let s string = \"!\"\n",
            "    let p string = \"ab\"\n",
            "    for i from 1 to 3\n",
            "        s = p + s\n",
            "    len(s)\n",
        );
        assert_all_variants_eq(source, 7);
    }

    #[test]
    fn move_update_string_plus_error_in_other_operand_leaves_target_intact_across_backends() {
        // Error-path safety for the `+` form: in `s = s + boom()` the non-target
        // operand (`boom()`) is evaluated BEFORE the target `s` is moved out, so a
        // throw there leaves `s` intact and a surrounding `catch` observes the
        // original string — never a moved-out placeholder.
        let source = concat!(
            "fn boom -> string\n",
            "    throw \"boom\"\n\n",
            "fn main -> i64\n",
            "    let s string = \"ab\"\n",
            "    let caught i64 = 0\n",
            "    try\n",
            "        s = s + boom()\n",
            "    catch m\n",
            "        caught = 1\n",
            "    caught * 100 + len(s)\n",
        );
        // boom() throws before the move: s stays "ab" (len 2); caught=1 => 102.
        assert_all_variants_eq(source, 102);
    }

    #[test]
    fn move_update_numeric_plus_one_is_correct_across_backends() {
        // `n = n + 1` (numeric, `Copy`-cheap operand) exercises the same binary
        // move path and must stay exactly correct.
        let source = concat!(
            "fn main -> i64\n",
            "    let n i64 = 0\n",
            "    for i from 1 to 10\n",
            "        n = n + 1\n",
            "    n\n",
        );
        assert_all_variants_eq(source, 10);
    }

    #[test]
    fn move_update_error_in_other_argument_leaves_target_intact_across_backends() {
        // Error-path safety: in `l = push(l, boom())` the *other* argument throws
        // BEFORE the target `l` is moved out. The move fast path evaluates other
        // arguments first, so `l` stays intact and a surrounding `catch` observes
        // the original list — never a moved-out placeholder.
        let source = concat!(
            "fn boom -> i64\n",
            "    throw \"boom\"\n\n",
            "fn main -> i64\n",
            "    let l list<i64> = list_new()\n",
            "    l = push(l, 1)\n",
            "    l = push(l, 2)\n",
            "    let caught i64 = 0\n",
            "    try\n",
            "        l = push(l, boom())\n",
            "    catch m\n",
            "        caught = 1\n",
            "    caught * 1000 + len(l) * 10 + get(l, 0) + get(l, 1)\n",
        );
        // boom() throws before the move: l stays [1,2]; caught=1.
        // 1*1000 + 2*10 + 1 + 2 = 1023. A corrupted l would fail len/get or differ.
        assert_all_variants_eq(source, 1023);
    }

    #[test]
    fn user_function_accumulation_is_correct_across_backends() {
        // `acc = add_one(acc)` calls a *user* function, which the fast path does
        // NOT move (user code could raise a catchable error). It must still be
        // correct via the clone path.
        let source = concat!(
            "fn double x list<i64> -> list<i64>\n",
            "    push(x, len(x))\n\n",
            "fn main -> i64\n",
            "    let acc list<i64> = list_new()\n",
            "    acc = double(acc)\n",
            "    acc = double(acc)\n",
            "    acc = double(acc)\n",
            "    len(acc) * 100 + get(acc, 0) * 10 + get(acc, 1) + get(acc, 2)\n",
        );
        // acc: [] -> [0] -> [0,1] -> [0,1,2]: len 3, 3*100 + 0 + 1 + 2 = 303.
        assert_all_variants_eq(source, 303);
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
        // The `?` error-propagation fixture exercises the AST early-return signal
        // and the IR `?`-desugar at parity across all five backend variants.
        assert!(covered.contains(&"run_error_propagation.lby".to_string()));
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
    fn try_operator_desugars_to_let_match_return_and_runs_at_parity() {
        // The IR never contains a `?`/`Try` node (there is no such IrExprKind):
        // `?` is desugared during lowering into a `let __try_q`, a typed
        // `let __try_v`, and a `match` whose failure arm `return`s. The success
        // temporary is what the original position references.
        let source = concat!(
            "fn checked n i64 -> result<i64, string>\n",
            "    if n < 0\n",
            "        return err(\"neg\")\n",
            "    ok(n)\n\n",
            "fn use_it a i64 -> result<i64, string>\n",
            "    let x i64 = checked(a)?\n",
            "    ok(x + 1)\n\n",
            "fn unwrap r result<i64, string> -> i64\n",
            "    match r\n",
            "        ok(v) -> v\n",
            "        err(m) -> 0 - len(m)\n\n",
            "fn main -> i64\n",
            "    unwrap(use_it(4)) + unwrap(use_it(-1))\n",
        );
        let module = lower_source(source);
        let use_it = module
            .functions
            .iter()
            .find(|function| function.name == "use_it")
            .expect("use_it function");

        // The `?`-desugar scaffolding was hoisted ahead of the `let x` binding:
        // `let __try_q_*`, then a typed `let __try_v_*`, then a `match` with a
        // `return`ing failure arm.
        assert!(
            matches!(&use_it.body[0], IrStmt::Let { name, .. } if name.starts_with("__try_q_")),
            "first hoisted statement binds the operand temp: {:?}",
            use_it.body[0]
        );
        let IrStmt::Let {
            name: v_name,
            ty: v_ty,
            ..
        } = &use_it.body[1]
        else {
            panic!(
                "expected the success temp binding, got {:?}",
                use_it.body[1]
            );
        };
        assert!(
            v_name.starts_with("__try_v_"),
            "success temp name: {v_name}"
        );
        assert_eq!(
            *v_ty,
            TypeRef::new("i64"),
            "success temp is typed as the payload"
        );
        let IrStmt::Match { arms, .. } = &use_it.body[2] else {
            panic!("expected the propagation match, got {:?}", use_it.body[2]);
        };
        // Two arms: `ok(..) -> __try_v = ..` and `err(..) -> return err(..)`.
        assert_eq!(arms.len(), 2, "ok + err arms");
        let has_returning_err_arm = arms.iter().any(|arm| {
            matches!(&arm.pattern, IrMatchPattern::Variant { name, .. } if name == "err")
                && matches!(arm.body.as_slice(), [IrStmt::Return(Some(_))])
        });
        assert!(has_returning_err_arm, "err arm returns the failure value");
        // The original `let x` position now references the success temp.
        let IrStmt::Let { value, .. } = &use_it.body[3] else {
            panic!("expected the rewritten `let x`, got {:?}", use_it.body[3]);
        };
        assert_eq!(value.kind, IrExprKind::Variable(v_name.clone()));

        // All five backend variants agree on the observable result.
        // unwrap(use_it(4)) = 5; unwrap(use_it(-1)) = -len("neg") = -3.
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ast, Value::I64(2));
        assert_eq!(ir, ast);
        assert_eq!(bytecode, ast);
        assert_eq!(optimized_ir, ast);
        assert_eq!(optimized_bytecode, ast);
    }

    #[test]
    fn closure_lowers_to_id_node_and_registers_body_table() {
        // A closure literal lowers to a body-less `IrExprKind::Closure { id }` node
        // typed `fn(i64) -> i64`, and its body is registered in the module's
        // closure table keyed by that id.
        let source = concat!(
            "fn apply f fn(i64) -> i64 v i64 -> i64\n",
            "    f(v)\n\n",
            "fn main -> i64\n",
            "    let n i64 = 10\n",
            "    let add_n fn(i64) -> i64 = fn x i64 -> x + n\n",
            "    apply(add_n, 5) + add_n(2)\n",
        );
        let module = lower_source(source);
        assert_eq!(module.closures.len(), 1, "one closure body registered");
        let def = &module.closures[0];
        assert_eq!(def.params, vec!["x".to_string()]);

        let main = module
            .functions
            .iter()
            .find(|function| function.name == "main")
            .expect("main");
        let IrStmt::Let { value, .. } = &main.body[1] else {
            panic!("expected the `let add_n` binding");
        };
        let IrExprKind::Closure { id } = &value.kind else {
            panic!("expected a closure node, got {:?}", value.kind);
        };
        assert_eq!(*id, def.id, "the node id keys the closure table");
        assert_eq!(
            value.ty,
            function_type(&[TypeRef::new("i64")], &TypeRef::new("i64"))
        );
    }

    #[test]
    fn closure_runs_at_parity_across_all_backend_variants() {
        // The canonical capture example returns 27 identically on the AST, IR, and
        // bytecode interpreters plus their optimized variants.
        let source = concat!(
            "fn apply f fn(i64) -> i64 v i64 -> i64\n",
            "    f(v)\n\n",
            "fn main -> i64\n",
            "    let n i64 = 10\n",
            "    let add_n fn(i64) -> i64 = fn x i64 -> x + n\n",
            "    apply(add_n, 5) + add_n(2)\n",
        );
        let (ast, ir, bytecode, optimized_ir, optimized_bytecode) =
            run_all_backend_variants(source);
        assert_eq!(ast, Value::I64(27));
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
