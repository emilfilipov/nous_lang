use std::collections::{HashMap, HashSet};
use std::fs;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::process::{Command, Stdio};
use std::rc::Rc;
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
pub mod rc_prototype;
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
    run_shared(module, args, false)
}

/// The bytecode-tier entry: identical execution to [`run_main_with_args`] but
/// with the flat dispatch-loop VM enabled, so eligible functions run through the
/// linear `VmProgram` instead of the recursive tree-walker.
fn run_main_with_args_vm(module: &IrModule, args: Vec<String>) -> Result<Value, RuntimeError> {
    run_shared(module, args, true)
}

fn run_shared(module: &IrModule, args: Vec<String>, use_vm: bool) -> Result<Value, RuntimeError> {
    let mut owned = module.clone();
    resolve_module_slots(&mut owned);
    let arc = Arc::new(owned);
    run_main_shared(arc, args, use_vm)
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
        IrStmt::Assign { path, value, .. } => {
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

fn run_main_shared(
    arc: Arc<IrModule>,
    args: Vec<String>,
    use_vm: bool,
) -> Result<Value, RuntimeError> {
    let mut runtime = IrRuntime::new(&arc, Arc::clone(&arc))?;
    runtime.program_args = args;
    runtime.use_vm = use_vm;
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
    run_main_with_args_vm(&ir, args)
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

#[path = "ir_interpreter.rs"]
mod ir_interpreter;
pub(crate) use ir_interpreter::*;

// -- Flat bytecode VM ---------------------------------------------------------
//
// The bytecode tier compiles each eligible function body once into a linear
// `VmProgram` (a `Vec<VmOp>`) and executes it with a single `loop { match }`
// dispatch loop and slot-indexed locals — no recursive tree-walk, no scope
// stack, no name scan — so it is distinctly faster than the IR tree-walker.
// Every actual operation (arithmetic, calls, indexing, field reads) reuses the
// exact `Value` helpers the tree-walker uses, so only control-flow lowering is
// new: results are identical to the AST and IR tiers (backend parity, enforced
// by the cross-tier tests). Functions containing constructs the compiler does
// not lower (`match`, `try`, `throw`, `asm`, closures, `await`, indexed/field
// assignment, or a call through a local function value) are marked ineligible
// and fall back to the tree-walker, so correctness never depends on coverage.

/// One instruction of the flat VM. Locals are addressed by their frame slot;
/// jumps carry absolute op indices patched at compile time.
enum VmOp {
    PushConst(Value),
    PushVoid,
    LoadLocal(usize),
    StoreLocal(usize),
    Binary(BinaryOp),
    Unary(UnaryOp),
    Index,
    /// `a[i]` where `a` is a bare local: borrow the container from its slot and
    /// clone only the element (the tree-walker's borrow fast path).
    IndexLocal(usize),
    Field(String),
    /// `s.field` where `s` is a bare local: borrow the struct from its slot.
    FieldLocal(usize, String),
    Call(String, usize),
    MakeArray(usize),
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    Pop,
    CheckStepNonzero(usize),
    /// Push whether the range-`for` counter (slot `var`) is still within `end`
    /// given `step`'s sign — the loop-continuation test.
    ForCheck {
        var: usize,
        end: usize,
        step: usize,
    },
    /// Advance the range-`for` counter (slot `var`) by `step` (wrapping).
    ForStep {
        var: usize,
        step: usize,
    },
    Return,
}

/// A compiled function body: the op stream, a parallel span per op (so a runtime
/// error carries the same source span the tree-walker would attach), and the
/// number of frame slots it uses.
struct VmProgram {
    ops: Vec<VmOp>,
    spans: Vec<Span>,
    frame_size: usize,
}

/// The control outcome of executing one [`VmOp`].
enum VmStep {
    Next,
    Jump(usize),
    Return(Value),
}

/// Apply a unary operator to a value — the exact logic of the tree-walker's
/// `Unary` arm, reused by the VM so results match.
fn eval_unary_value(op: UnaryOp, value: Value) -> Result<Value, RuntimeError> {
    match op {
        UnaryOp::Not => Ok(Value::Bool(!value.as_bool()?)),
        UnaryOp::BitNot => match value {
            Value::Int { value, ty } => Ok(Value::int(!value, ty)),
            other => Ok(Value::I64(!other.as_i64()?)),
        },
        UnaryOp::Negate => match value {
            Value::Int { value, ty } => Ok(Value::int(value.wrapping_neg(), ty)),
            Value::F64(f) => Ok(Value::F64(-f)),
            Value::F32(f) => Ok(Value::F32(-f)),
            other => Ok(Value::I64(other.as_i64()?.wrapping_neg())),
        },
    }
}

/// Read a struct field — the tree-walker's `Field` logic, reused by the VM.
fn field_of(target: &Value, field: &str) -> Result<Value, RuntimeError> {
    match target {
        Value::Struct(s) => s
            .fields
            .iter()
            .find(|(name, _)| name == field)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| RuntimeError::new("L0371", format!("no field `{field}`"))),
        _ => Err(RuntimeError::new(
            "L0371",
            format!("cannot access field `{field}` on non-struct value"),
        )),
    }
}

/// The binary operator a compound assignment applies (`x += v` is `x = x + v`).
fn assign_binop(op: AssignOp) -> BinaryOp {
    match op {
        AssignOp::Add => BinaryOp::Add,
        AssignOp::Subtract => BinaryOp::Subtract,
        AssignOp::Multiply => BinaryOp::Multiply,
        AssignOp::Divide => BinaryOp::Divide,
        AssignOp::Remainder => BinaryOp::Remainder,
        AssignOp::Replace => unreachable!("Replace is not a compound op"),
    }
}

/// Break/continue patch targets for one loop being compiled (mirrors the native
/// backend's `NativeLoop`). `continue_target` is set when known up front
/// (`while`/`loop` continue to the top); a range-`for` continue jumps forward to
/// its step block, so those jumps are recorded and patched once its offset exists.
struct VmLoop {
    continue_target: Option<usize>,
    continue_sites: Vec<usize>,
    break_sites: Vec<usize>,
}

/// Compiles an [`IrFunction`] body into a [`VmProgram`], assigning every binding
/// a flat frame slot and linearizing control flow to jumps. Returns `Err(())` the
/// moment it meets a construct it does not lower, so the caller falls back.
struct VmCompiler {
    ops: Vec<VmOp>,
    /// Source span for each emitted op (parallel to `ops`), so a failing op
    /// reports the same span the tree-walker would attach.
    spans: Vec<Span>,
    /// The span attached to subsequently-emitted ops; set to the current
    /// statement/expression before emitting its op.
    cur_span: Span,
    /// Lexical scopes of `(name, slot)`, searched innermost-first (and newest-first
    /// within a scope, so a re-`let` shadows) — matching the tree-walker's
    /// resolution. Slots themselves are unique across the whole function.
    scopes: Vec<Vec<(String, usize)>>,
    next_slot: usize,
    /// Every name ever bound as a local, so a `Call` whose name is a local (a
    /// first-class function value) can be rejected as ineligible.
    locals: HashSet<String>,
    loops: Vec<VmLoop>,
}

fn compile_function_to_vm(function: &IrFunction) -> Option<VmProgram> {
    let mut c = VmCompiler {
        ops: Vec::new(),
        spans: Vec::new(),
        cur_span: function.span,
        scopes: vec![Vec::new()],
        next_slot: 0,
        locals: HashSet::new(),
        loops: Vec::new(),
    };
    for param in &function.params {
        c.declare(&param.name);
    }
    // The function body is the function scope (params + top-level lets share it,
    // like the tree-walker): compile the statements in tail position so the last
    // one's value is the implicit return, then a trailing `Return` yields it.
    c.compile_stmts(&function.body, true).ok()?;
    c.emit(VmOp::Return);
    Some(VmProgram {
        ops: c.ops,
        spans: c.spans,
        frame_size: c.next_slot,
    })
}

impl VmCompiler {
    fn emit(&mut self, op: VmOp) -> usize {
        let index = self.ops.len();
        self.ops.push(op);
        self.spans.push(self.cur_span);
        index
    }

    fn patch(&mut self, site: usize, target: usize) {
        match &mut self.ops[site] {
            VmOp::Jump(t) | VmOp::JumpIfFalse(t) | VmOp::JumpIfTrue(t) => *t = target,
            _ => unreachable!("patch site is not a jump"),
        }
    }

    /// Introduce a binding, giving it a fresh unique slot in the current scope.
    fn declare(&mut self, name: &str) -> usize {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .push((name.to_string(), slot));
        self.locals.insert(name.to_string());
        slot
    }

    /// A slot with no source name (a range-`for`'s `end`/`step` temporaries).
    fn alloc_temp(&mut self) -> usize {
        let slot = self.next_slot;
        self.next_slot += 1;
        slot
    }

    fn resolve(&self, name: &str) -> Option<usize> {
        for scope in self.scopes.iter().rev() {
            if let Some((_, slot)) = scope.iter().rev().find(|(n, _)| n == name) {
                return Some(*slot);
            }
        }
        None
    }

    fn bare_local_slot(&self, expr: &IrExpr) -> Option<usize> {
        match &expr.kind {
            IrExprKind::Variable(name) | IrExprKind::Local { name, .. } => self.resolve(name),
            _ => None,
        }
    }

    fn compile_stmts(&mut self, body: &[IrStmt], tail: bool) -> Result<(), ()> {
        if body.is_empty() {
            if tail {
                self.emit(VmOp::PushVoid);
            }
            return Ok(());
        }
        let last = body.len() - 1;
        for (index, stmt) in body.iter().enumerate() {
            self.compile_stmt(stmt, tail && index == last)?;
        }
        Ok(())
    }

    fn compile_scoped_block(&mut self, body: &[IrStmt], tail: bool) -> Result<(), ()> {
        self.scopes.push(Vec::new());
        let result = self.compile_stmts(body, tail);
        self.scopes.pop();
        result
    }

    fn compile_stmt(&mut self, stmt: &IrStmt, tail: bool) -> Result<(), ()> {
        self.cur_span = statement_span(stmt);
        match stmt {
            IrStmt::Let { name, value, .. } => {
                self.compile_expr(value)?;
                let slot = self.declare(name);
                self.emit(VmOp::StoreLocal(slot));
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Assign {
                name,
                path,
                op,
                value,
                ..
            } => {
                if !path.is_empty() {
                    return Err(()); // indexed/field assignment: fall back
                }
                let slot = self.resolve(name).ok_or(())?;
                match op {
                    AssignOp::Replace => {
                        self.compile_expr(value)?;
                        self.emit(VmOp::StoreLocal(slot));
                    }
                    other => {
                        self.emit(VmOp::LoadLocal(slot));
                        self.compile_expr(value)?;
                        self.emit(VmOp::Binary(assign_binop(*other)));
                        self.emit(VmOp::StoreLocal(slot));
                    }
                }
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Return(expr) => {
                match expr {
                    Some(expr) => self.compile_expr(expr)?,
                    None => {
                        self.emit(VmOp::PushVoid);
                    }
                }
                self.emit(VmOp::Return);
            }
            IrStmt::Expr(expr) => {
                self.compile_expr(expr)?;
                if !tail {
                    self.emit(VmOp::Pop);
                }
            }
            IrStmt::If {
                branches,
                else_body,
                ..
            } => {
                let mut end_jumps = Vec::new();
                for branch in branches {
                    self.compile_expr(&branch.condition)?;
                    let skip = self.emit(VmOp::JumpIfFalse(0));
                    self.compile_scoped_block(&branch.body, tail)?;
                    end_jumps.push(self.emit(VmOp::Jump(0)));
                    let next = self.ops.len();
                    self.patch(skip, next);
                }
                self.compile_scoped_block(else_body, tail)?;
                let end = self.ops.len();
                for jump in end_jumps {
                    self.patch(jump, end);
                }
            }
            IrStmt::While {
                condition, body, ..
            } => {
                let top = self.ops.len();
                self.compile_expr(condition)?;
                let exit = self.emit(VmOp::JumpIfFalse(0));
                self.loops.push(VmLoop {
                    continue_target: Some(top),
                    continue_sites: Vec::new(),
                    break_sites: Vec::new(),
                });
                self.compile_scoped_block(body, false)?;
                self.emit(VmOp::Jump(top));
                let loop_ctx = self.loops.pop().expect("loop pushed");
                let end = self.ops.len();
                self.patch(exit, end);
                for site in loop_ctx.break_sites {
                    self.patch(site, end);
                }
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::For {
                name,
                start,
                end,
                step,
                body,
                ..
            } => {
                // The loop variable lives in its own scope (popped after the loop).
                self.scopes.push(Vec::new());
                self.compile_expr(start)?;
                let var = self.declare(name);
                self.emit(VmOp::StoreLocal(var));
                self.compile_expr(end)?;
                let end_slot = self.alloc_temp();
                self.emit(VmOp::StoreLocal(end_slot));
                match step {
                    Some(step) => self.compile_expr(step)?,
                    None => {
                        self.emit(VmOp::PushConst(Value::I64(1)));
                    }
                }
                let step_slot = self.alloc_temp();
                self.emit(VmOp::StoreLocal(step_slot));
                self.emit(VmOp::CheckStepNonzero(step_slot));
                let top = self.ops.len();
                self.emit(VmOp::ForCheck {
                    var,
                    end: end_slot,
                    step: step_slot,
                });
                let exit = self.emit(VmOp::JumpIfFalse(0));
                self.loops.push(VmLoop {
                    continue_target: None,
                    continue_sites: Vec::new(),
                    break_sites: Vec::new(),
                });
                self.compile_scoped_block(body, false)?;
                let loop_ctx = self.loops.pop().expect("loop pushed");
                let step_pc = self.ops.len();
                self.emit(VmOp::ForStep {
                    var,
                    step: step_slot,
                });
                self.emit(VmOp::Jump(top));
                let end_pc = self.ops.len();
                self.patch(exit, end_pc);
                for site in loop_ctx.break_sites {
                    self.patch(site, end_pc);
                }
                for site in loop_ctx.continue_sites {
                    self.patch(site, step_pc);
                }
                self.scopes.pop();
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Loop { body, .. } => {
                let top = self.ops.len();
                self.loops.push(VmLoop {
                    continue_target: Some(top),
                    continue_sites: Vec::new(),
                    break_sites: Vec::new(),
                });
                self.compile_scoped_block(body, false)?;
                self.emit(VmOp::Jump(top));
                let loop_ctx = self.loops.pop().expect("loop pushed");
                let end = self.ops.len();
                for site in loop_ctx.break_sites {
                    self.patch(site, end);
                }
                if tail {
                    self.emit(VmOp::PushVoid);
                }
            }
            IrStmt::Break(_) => {
                let site = self.emit(VmOp::Jump(0));
                self.loops.last_mut().ok_or(())?.break_sites.push(site);
            }
            IrStmt::Continue(_) => {
                let target = self.loops.last().ok_or(())?.continue_target;
                match target {
                    Some(top) => {
                        self.emit(VmOp::Jump(top));
                    }
                    None => {
                        let site = self.emit(VmOp::Jump(0));
                        self.loops
                            .last_mut()
                            .expect("loop present")
                            .continue_sites
                            .push(site);
                    }
                }
            }
            // Constructs the VM does not lower: fall back to the tree-walker.
            IrStmt::Try { .. }
            | IrStmt::Match { .. }
            | IrStmt::Throw { .. }
            | IrStmt::Asm { .. } => return Err(()),
        }
        Ok(())
    }

    fn compile_expr(&mut self, expr: &IrExpr) -> Result<(), ()> {
        let span = expr.span;
        self.cur_span = span;
        match &expr.kind {
            IrExprKind::Integer(value) => {
                self.emit(VmOp::PushConst(Value::I64(*value)));
            }
            IrExprKind::Float(value) => {
                self.emit(VmOp::PushConst(Value::F64(*value)));
            }
            IrExprKind::Bool(value) => {
                self.emit(VmOp::PushConst(Value::Bool(*value)));
            }
            IrExprKind::String(value) => {
                self.emit(VmOp::PushConst(Value::String(value.clone().into())));
            }
            IrExprKind::Char(value) => {
                self.emit(VmOp::PushConst(Value::Char(*value)));
            }
            IrExprKind::Variable(name) | IrExprKind::Local { name, .. } => {
                // A bare name that is not a local (an enum variant or a top-level
                // function used as a value) needs the tree-walker's fallback logic.
                let slot = self.resolve(name).ok_or(())?;
                self.emit(VmOp::LoadLocal(slot));
            }
            IrExprKind::Unary { op, expr } => {
                self.compile_expr(expr)?;
                self.cur_span = span;
                self.emit(VmOp::Unary(*op));
            }
            IrExprKind::Binary { left, op, right } => match op {
                // Short-circuit: `a && b` = if !a { false } else { b }.
                BinaryOp::And => {
                    self.compile_expr(left)?;
                    let to_false = self.emit(VmOp::JumpIfFalse(0));
                    self.compile_expr(right)?;
                    let to_end = self.emit(VmOp::Jump(0));
                    let false_pc = self.ops.len();
                    self.patch(to_false, false_pc);
                    self.emit(VmOp::PushConst(Value::Bool(false)));
                    let end = self.ops.len();
                    self.patch(to_end, end);
                }
                // `a || b` = if a { true } else { b }.
                BinaryOp::Or => {
                    self.compile_expr(left)?;
                    let to_true = self.emit(VmOp::JumpIfTrue(0));
                    self.compile_expr(right)?;
                    let to_end = self.emit(VmOp::Jump(0));
                    let true_pc = self.ops.len();
                    self.patch(to_true, true_pc);
                    self.emit(VmOp::PushConst(Value::Bool(true)));
                    let end = self.ops.len();
                    self.patch(to_end, end);
                }
                _ => {
                    self.compile_expr(left)?;
                    self.compile_expr(right)?;
                    self.cur_span = span;
                    self.emit(VmOp::Binary(*op));
                }
            },
            IrExprKind::Index { target, index } => {
                if let Some(slot) = self.bare_local_slot(target) {
                    self.compile_expr(index)?;
                    self.cur_span = span;
                    self.emit(VmOp::IndexLocal(slot));
                } else {
                    self.compile_expr(target)?;
                    self.compile_expr(index)?;
                    self.cur_span = span;
                    self.emit(VmOp::Index);
                }
            }
            IrExprKind::Field { target, field } => {
                self.cur_span = span;
                if let Some(slot) = self.bare_local_slot(target) {
                    self.emit(VmOp::FieldLocal(slot, field.clone()));
                } else {
                    self.compile_expr(target)?;
                    self.cur_span = span;
                    self.emit(VmOp::Field(field.clone()));
                }
            }
            IrExprKind::Call { name, args } => {
                // A call through a local (a first-class function value) needs the
                // tree-walker's env-based dispatch; the VM only calls by name.
                if self.locals.contains(name) {
                    return Err(());
                }
                for arg in args {
                    self.compile_expr(arg)?;
                }
                self.cur_span = span;
                self.emit(VmOp::Call(name.clone(), args.len()));
            }
            IrExprKind::Array(elements) => {
                for element in elements {
                    self.compile_expr(element)?;
                }
                self.emit(VmOp::MakeArray(elements.len()));
            }
            // `await` and closure literals are not lowered by the VM.
            IrExprKind::Await { .. } | IrExprKind::Closure { .. } => return Err(()),
        }
        Ok(())
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
#[path = "ir_lib_tests.rs"]
mod tests;
