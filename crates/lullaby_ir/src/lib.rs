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
    ArenaState, ArithOp, Closure, EnumValue, Future, IntKind, MEMORY_ORDER_VARIANTS, OverflowMode,
    ProcessResource, RawPointerMemory, RawResolve, ResolvedPlace, RootSlot, RuntimeError,
    SharedAtomic, SharedMutex, SocketResource, StructValue, Task, Value, apply_compound, arena_key,
    arena_overflow_error, asm_interpreter_error, await_future, builtin_atomic_add_ordered,
    builtin_atomic_and_ordered, builtin_atomic_cas_ordered, builtin_atomic_load_ordered,
    builtin_atomic_or_ordered, builtin_atomic_store_ordered, builtin_atomic_sub_ordered,
    builtin_atomic_swap_ordered, builtin_atomic_xor_ordered, builtin_fence, char_find,
    checked_div_rem, dangling_place, expect_atomic, expect_bool, expect_chan, expect_future,
    expect_i64, expect_list, expect_map, expect_mutex, expect_string, expect_task,
    extern_call_error, gcd_i64, get_place, http_exchange, int_cmp, int_div, int_rem, int_shl,
    int_shr, join_task, list_extreme, list_sum_values, monotonic_now_nanos, net_err, new_chan,
    option_value, os_random_bytes, overflow_arith, port_io_interpreter_error, process_exit_code,
    read_stdin_all, read_stdin_line, result_value, scalar_order_keys, set_place, shift_left,
    shift_right, sleep_millis, sort_scalar_list, unmapped_raw, unreachable_frame, value_type_name,
    wall_now_millis,
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
///
/// `prelude` holds the statements the *body's own* lowering hoisted — an inline
/// conditional (`A if C else B`) or a postfix `?` desugars into a temporary plus
/// an `if`/`match`, and that scaffolding reads the closure's **parameters**, so it
/// belongs to the closure's frame and nowhere else. It is evaluated on every
/// invocation, in the closure's own environment, immediately before `body`.
///
/// Keeping it here is load-bearing. It used to drain into the *enclosing*
/// function's statement list, where the closure's parameters do not exist: the
/// AST interpreter (which evaluates `ExprKind::Conditional` directly and never
/// hoists) returned the right answer while the IR interpreter and bytecode VM
/// died at runtime with `L0403 unknown variable \`x\`` — naming the user's own
/// closure parameter, in the *enclosing* function's frame. A closure body is a
/// single expression in the surface grammar, but its lowering is not, and this
/// field is where that gap is carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrClosureDef {
    pub id: usize,
    pub params: Vec<String>,
    /// Body-local hoisted scaffolding, run in the closure's frame before `body`.
    /// Serde-defaulted so existing artifacts stay loadable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prelude: Vec<IrStmt>,
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
///
/// `type_params` are the declared generic type-parameter names in source order
/// (`["T"]` for `struct Box<T>`, `["K", "V"]` for `struct Pair<K, V>`); empty for a
/// non-generic struct. The field types of a generic struct mention these names as
/// ordinary `TypeRef`s (`Box`'s `value` field has type `T`). The interpreters run
/// generic types by erasure and never consult this list, but the native backend
/// uses it to **monomorphize** a concrete instantiation (`Box<i64>`) by zipping the
/// parameters against the spelling's type arguments and substituting them into the
/// field types before computing the native layout. Serde-defaulted to an empty list
/// so existing `.lbc` artifacts and JSON snapshots without this field stay valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrStructDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_params: Vec<String>,
    pub fields: Vec<(String, TypeRef)>,
}

/// An enum type in the IR: name plus ordered variants, each a name plus an
/// ordered list of positional payload types.
///
/// `type_params` mirrors [`IrStructDef::type_params`]: the declared generic
/// type-parameter names in source order (`["T"]` for `enum Opt<T>`), used by the
/// native backend to monomorphize a concrete enum instantiation (`Opt<i64>`) by
/// substituting the spelling's type arguments into each variant's payload types.
/// Serde-defaulted so existing artifacts and snapshots stay valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IrEnumDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_params: Vec<String>,
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
    /// A slot-resolved local read produced by `resolve_module_slots` at interpretation
    /// time: `Local(packed)` where `packed = ((depth << 16) | slot) + 1` names the
    /// binding by its `(scopes-from-innermost depth, index-within-scope slot)`
    /// instead of by name, so `Env::get_slot` indexes it directly with no
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
    /// An optional dedicated diagnostic code. Most IR-lowering failures share the
    /// generic `L0501` code (assigned by the CLI reporter); a few carry a specific
    /// code — e.g. an actor program rejected here reports `L0355` — which the
    /// reporter uses in place of `L0501`.
    pub code: Option<&'static str>,
}

impl IrLoweringError {
    fn new(message: impl Into<String>, span: Option<Span>) -> Self {
        Self {
            message: message.into(),
            span,
            code: None,
        }
    }

    /// Attach a specific diagnostic code to this error (overriding the generic
    /// `L0501` the reporter would otherwise use).
    fn with_code(mut self, code: &'static str) -> Self {
        self.code = Some(code);
        self
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
/// its parameter names, the body-local hoisted `prelude`, and its
/// instruction-body expression. Mirrors [`IrClosureDef`], round-tripped when the
/// bytecode module is built from / lowered back to the IR.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BytecodeClosureDef {
    pub id: usize,
    pub params: Vec<String>,
    /// Mirror of [`IrClosureDef::prelude`] — the scaffolding an inline
    /// conditional / `?` in the body hoisted, which reads the closure's
    /// parameters and so must run in the closure's frame, not the enclosing one.
    /// Serde-defaulted so existing `.lbc` artifacts stay loadable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prelude: Vec<BytecodeInstruction>,
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
                prelude: def.prelude.iter().map(lower_bytecode_instruction).collect(),
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
                prelude: def.prelude.iter().map(bytecode_instruction_to_ir).collect(),
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

#[path = "ir_optimizer.rs"]
mod ir_optimizer;
pub(crate) use ir_optimizer::*;

/// The IR/bytecode interpreters' lexical environment, split out of `bytecode_vm.rs`.
#[path = "ir_env.rs"]
mod ir_env;
pub(crate) use ir_env::Env;

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
#[path = "bytecode_vm.rs"]
mod bytecode_vm;
pub(crate) use bytecode_vm::*;

#[cfg(test)]
#[path = "ir_lib_tests.rs"]
mod tests;
