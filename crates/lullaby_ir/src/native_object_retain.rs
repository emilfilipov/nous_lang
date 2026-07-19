//! Cross-call arena retention analysis (arena increment **I2**). Split out of
//! `native_object_eligibility.rs` (which is over the size backlog cap), mirroring how
//! `native_object_confine.rs` was split out; sees the parent's items — `type_is_heap`,
//! `is_pointer_type`, `alloc_defeats_arena`, the bytecode IR — via `use super::*`.
//!
//! # Why this exists
//!
//! The safe-tier arena reclaims a function's heap by rewinding the shared bump
//! pointer (`__lullaby_heap_next`) on every return edge. That is sound only if no
//! value the function allocated is still reachable after it returns. Until this
//! increment the arena fired **only for LEAF functions** (criterion 3 of
//! [`arena_eligible_functions`]) — a function that calls no user/`extern` function —
//! because a callee could stash a pointer to one of the caller's about-to-be-rewound
//! heap cells, producing a cross-call **use-after-free**.
//!
//! This module widens that: a caller stays arena-eligible if **every callee it
//! invokes is provably NON-retaining**, computed once per module as a bottom-up
//! **retention summary** ([`retaining_summary`]). The caller-side gate is
//! [`all_callees_non_retaining`], which criterion 3 now calls instead of the leaf
//! test.
//!
//! # The predicate — `non_retaining(f)` holds iff ALL of R1..R4
//!
//! A function `f` is **retaining** (the summary maps it to `true`) unless it clears
//! every one of these; the whole analysis is a conservative sound **under**-approximation
//! of "provably local" — **default-deny: anything not provably non-retaining is
//! retaining**.
//!
//! * **R1 — scalar return.** `f`'s return type is not a heap value
//!   (`!type_is_heap(return_type, heap_aggs)`, the exact criterion-1 test) and is not
//!   a `fn(...)` value (a closure return can carry heap captures). Returning a *fresh*
//!   heap value is provably safe but is DEFERRED past slice 1. **Stage-4b carve-out:** a
//!   `fn(...)` return that is a **promotable closure factory** ([`returns_promotable_closure`]
//!   — every edge a fresh, flat, scalar-capture closure literal ≤ 8 words) is NON-retaining,
//!   because its survivor lands in the caller's region (`markF ≥ markC`) and dies at the
//!   caller's rewind. This is computed PURELY LOCALLY (`f`'s body + closure layouts, never
//!   `f`'s own arena status) to avoid a summary→eligibility→summary cycle the single sweep
//!   cannot express.
//! * **R2 — no escaping capture.** No closure literal, no `await`, and no
//!   `spawn`/`tell`/`ask` call, and `f` is not an `async fn`. A capture block or a
//!   spawned task can outlive the call carrying a heap pointer. **Stage-4b carve-out:** a
//!   promotable factory's own closure LITERALS are not counted (they are the promoted
//!   survivor / non-escaping helpers), so it checks only the residual
//!   `spawn`/`await`/`tell`/`ask` sub-channel ([`body_has_spawn_or_await_channel`]); every
//!   other function still treats a closure literal itself as a capture channel.
//! * **R3 — no raw-memory aliasing.** No `ptr<…>`-typed expression (reuse
//!   [`is_pointer_type`] — it uniformly catches `addr_of`/`ptr_offset`/`ptr_cast`/
//!   `int_to_ptr`/every raw builtin by TYPE), no inline `asm`, and no `alloc` heap box
//!   (reuse [`alloc_defeats_arena`]).
//! * **R4 — every call is to a resolvable non-retaining target.** For each call in
//!   `f`'s body: a **native builtin** is ok (it reads/computes; no user-visible
//!   retention); a **module function** requires `non_retaining(callee)`; an **`extern`
//!   C function** is retaining (C can stash the pointer); an **indirect target** — a
//!   `fn`-typed parameter, or a `fn`-typed local bound to something other than an inline
//!   closure literal (an unknown factory/first-class-fn value) — is retaining. A call to
//!   a local bound by an inline **closure literal** is ok (like a builtin): the native
//!   closure rules make it a known, non-escaping, scalar-capturing, heap-free-body target
//!   that cannot stash a heap pointer past the call.
//!
//! # The critical default-inversion (leads the soundness argument)
//!
//! The pre-existing `body_calls_user` walk answers "unknown name ⇒ NOT a user call ⇒
//! safe/builtin". This module **inverts that default for the escape question**: an
//! `extern` or **indirect** call name is treated as **retaining** (deny), never as a
//! harmless builtin. Concretely, [`classify_call`] resolves a name against, in order,
//! the caller's **fn-typed bindings** (indirect ⇒ deny), the module-function set
//! (⇒ require the summary), and the extern set (⇒ deny); only a name that is none of
//! those is a builtin (⇒ ok). Because a well-typed module has already resolved every
//! call, the only "unknown" names are indirect ones — and those are denied. If this
//! default were kept as `body_calls_user`'s, an unknown indirect callee could retain
//! freely and the caller's rewind would be a use-after-free.
//!
//! # Bottom-up computation — ONE reverse-topological sweep, NO fixpoint
//!
//! Each function's **local** properties (R1/R2/R3, plus the R4 extern/indirect
//! denials, which depend only on `f`) are computed directly. The only cross-function
//! dependency is R4's module-function calls, so the summary is a single linear sweep
//! over the module call graph (a memoized DFS, [`resolve_retaining`]): a function is
//! retaining iff it is locally-retaining or any module callee is retaining. **Cycles
//! are pre-poisoned**: any function that sits on a cycle (self-recursion, mutual
//! recursion, any SCC — detected as a back-edge to a node already on the DFS stack) is
//! retaining without inspecting its callees, and that poison propagates outward
//! through R4. The lattice is monotone and the recursion cases are pre-poisoned, so no
//! fixpoint iteration is needed — the sweep is `O(nodes + edges)`, preserving the
//! compile-speed moat. Recursion/SCC relaxation is DEFERRED past slice 1.
//!
//! See `documents/lullaby_memory_management.md` and
//! `documents/execution_tiers_and_1_0_scope.md` for the full soundness argument and
//! the escape-channel table.

use super::*;
use std::collections::{HashMap, HashSet};

/// Compute, for every function in `module`, whether it is **retaining** (`true`,
/// conservative default) or provably **non-retaining** (`false`). Keyed by function
/// name. `heap_aggs` is the module's heap-carrying-aggregate set (including heap-`T`
/// generic instantiations), so R1 recognizes a struct/enum/`Box<string>` return as
/// heap. Computed as one bottom-up sweep with cycles pre-poisoned (see the module
/// docs) — no fixpoint.
///
/// The summary covers **all** module functions, not just the arena-eligible ones,
/// because an arena caller may invoke a module function that is not itself
/// arena-eligible; that callee must still be proven non-retaining.
pub(crate) fn retaining_summary(
    module: &BytecodeModule,
    heap_aggs: &HashSet<String>,
    closure_layouts: &HashMap<usize, ClosureLayout>,
) -> HashMap<String, bool> {
    let module_fns: HashSet<&str> = module.functions.iter().map(|f| f.name.as_str()).collect();
    let extern_fns: HashSet<&str> = module.extern_functions.iter().map(String::as_str).collect();
    let async_fns: HashSet<&str> = module.async_functions.iter().map(String::as_str).collect();

    // Per-function local property (R1/R2/R3 + the R4 extern/indirect denials) and the
    // module-function callee edges (the only cross-function R4 dependency).
    let mut local_retaining: HashMap<&str, bool> = HashMap::new();
    let mut callees: HashMap<&str, Vec<&str>> = HashMap::new();
    for f in &module.functions {
        let fn_typed = fn_typed_binding_names(f);
        let calls = analyze_calls(&f.instructions, &module_fns, &extern_fns, &fn_typed);
        let local =
            function_is_locally_retaining(f, module, heap_aggs, &async_fns, closure_layouts)
                || calls.has_denied_call;
        local_retaining.insert(f.name.as_str(), local);
        callees.insert(f.name.as_str(), calls.module_callees);
    }

    // Bottom-up sweep: retaining(f) = local_retaining(f) OR any module callee
    // retaining. Cycles are pre-poisoned by the on-stack (gray) back-edge check.
    let mut memo: HashMap<String, bool> = HashMap::new();
    let mut on_stack: HashSet<&str> = HashSet::new();
    for f in &module.functions {
        resolve_retaining(
            f.name.as_str(),
            &local_retaining,
            &callees,
            &mut memo,
            &mut on_stack,
        );
    }
    memo
}

/// Memoized DFS over the module call graph. Returns whether `name` is retaining.
///
/// Cycle handling (default-deny): reaching a `name` that is already on the DFS stack
/// (a back-edge) means `name` sits on a cycle, so it is poisoned to `true` WITHOUT
/// being memoized — its own frame, as the stack unwinds, computes the same `true`
/// (one of its callees transitively reaches it as a gray node), and every caller that
/// reaches the cycle inherits the poison through R4. A `false` verdict is only ever
/// recorded once a function's every callee has been fully resolved non-retaining, so
/// no node is memoized non-retaining while still on a cycle.
fn resolve_retaining<'a>(
    name: &'a str,
    local: &HashMap<&'a str, bool>,
    callees: &HashMap<&'a str, Vec<&'a str>>,
    memo: &mut HashMap<String, bool>,
    on_stack: &mut HashSet<&'a str>,
) -> bool {
    if let Some(v) = memo.get(name) {
        return *v;
    }
    // Back-edge to a function already being resolved ⇒ `name` is on a cycle ⇒ poison.
    if on_stack.contains(name) {
        return true;
    }
    // A name with no local entry is not a module function (extern/indirect/builtin
    // edges are never enqueued here); treat defensively as retaining (default-deny).
    let Some(&local_r) = local.get(name) else {
        return true;
    };
    if local_r {
        memo.insert(name.to_string(), true);
        return true;
    }
    on_stack.insert(name);
    let mut retaining = false;
    if let Some(cs) = callees.get(name) {
        for &callee in cs {
            if resolve_retaining(callee, local, callees, memo, on_stack) {
                retaining = true;
                break;
            }
        }
    }
    on_stack.remove(name);
    memo.insert(name.to_string(), retaining);
    retaining
}

/// Whether every callee of `function` is provably non-retaining, given the module's
/// `summary` and the module-function / `extern` name sets. This is the caller-side
/// gate criterion 3 uses in place of the old leaf test. DEFAULT-DENY: an indirect
/// call, an `extern` call, or a module call the summary marks retaining (or does not
/// cover) fails.
pub(crate) fn all_callees_non_retaining(
    function: &BytecodeFunction,
    module_fns: &HashSet<&str>,
    extern_fns: &HashSet<&str>,
    summary: &HashMap<String, bool>,
) -> bool {
    let fn_typed = fn_typed_binding_names(function);
    !body_has_retaining_callee(
        &function.instructions,
        module_fns,
        extern_fns,
        &fn_typed,
        summary,
    )
}

// -- Local (per-function) retention properties: R1, R2, R3 -------------------

/// Whether `f` is retaining by one of its LOCAL properties alone (R1/R2/R3 and the
/// async check), independent of its callees. The R4 extern/indirect denials are
/// folded in by the caller (`retaining_summary`) via [`analyze_calls`].
fn function_is_locally_retaining(
    f: &BytecodeFunction,
    module: &BytecodeModule,
    heap_aggs: &HashSet<String>,
    async_fns: &HashSet<&str>,
    closure_layouts: &HashMap<usize, ClosureLayout>,
) -> bool {
    // A PROMOTABLE closure factory (arena stage-4b): its returned closure literal is a
    // fresh, flat, scalar-capture block that lands in the CALLER's region (`markF ≥
    // markC`) and dies at the caller's rewind, so the factory does NOT retain a heap
    // pointer past its return — whether it promotes (arena) or bump-allocates
    // (off-arena). This is the R1 carve-out: it is computed PURELY LOCALLY (a function
    // of `f`'s body + the closure layouts, never of whether `f` is itself arena) so
    // the summary→eligibility→summary cycle the single-sweep DFS cannot express is
    // avoided. See `returns_promotable_closure`.
    let promotable = returns_promotable_closure(f, closure_layouts);
    // R1 — scalar return. A heap return (string/list/map/array<string>, or a
    // heap-carrying struct/enum/generic instantiation) lets a live heap value leave
    // the call ⇒ retaining. (A fresh heap return is provably safe but DEFERRED.)
    if type_is_heap(&f.return_type, heap_aggs) {
        return true;
    }
    // R1 — a `fn(...)` return retains UNLESS it is a promotable closure factory (its
    // survivor lands in the caller's region, per the carve-out above). A returned fn
    // PARAMETER, a heap-capturing / above-cap / call-returned closure return is not
    // promotable and still retains.
    if f.return_type.is_function() && !promotable {
        return true;
    }
    // R2 — no escaping capture: an `async fn` spawns a thread carrying its arguments.
    if async_fns.contains(f.name.as_str()) {
        return true;
    }
    // R2 — the capture channels. For a PROMOTABLE factory its closure LITERALS are not
    // a retention channel: each is either the returned survivor (promoted into the
    // caller's region) or a non-escaping helper closure dead at its return (reclaimed
    // by `f`'s own rewind, or below the survivor in `f`'s region), and — with no
    // mutable globals in the language — a fresh flat closure has no way to hand a heap
    // pointer to a third party EXCEPT `spawn`/`tell`/`ask`/`await`. So a promotable
    // factory checks only that residual sub-channel; every other function treats a
    // closure literal itself as a capture channel. This is the R2 half of the stage-4b
    // carve-out.
    let has_channel = if promotable {
        body_has_spawn_or_await_channel(&f.instructions)
    } else {
        body_has_capture_channel(&f.instructions)
    };
    if has_channel {
        return true;
    }
    // R3 — no `alloc` heap box (manually managed; invisible to the escape analysis).
    if alloc_defeats_arena(&f.instructions, &module.closures) {
        return true;
    }
    // R3 — no raw pointer / inline `asm` (either can alias a heap cell past the call).
    if body_has_pointer_or_asm(&f.instructions) {
        return true;
    }
    false
}

/// Whether a body contains an R2 capture channel: a closure literal, an `await`, or a
/// `spawn`/`tell`/`ask` call. Any of these can carry a heap pointer past the call.
fn body_has_capture_channel(body: &[BytecodeInstruction]) -> bool {
    body.iter().any(instruction_has_capture_channel)
}

fn instruction_has_capture_channel(instruction: &BytecodeInstruction) -> bool {
    if fold_instruction_bodies_any(instruction, &mut |body| body_has_capture_channel(body)) {
        return true;
    }
    instruction_exprs(instruction)
        .iter()
        .any(|e| expr_has_capture_channel(e))
}

fn expr_has_capture_channel(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        // A closure literal allocates a `[code_ptr][captures…]` block that can
        // outlive the call carrying a captured heap pointer.
        BytecodeExprKind::Closure { .. } => true,
        // `await` blocks on a future produced by a spawned task; `spawn`/`tell`/`ask`
        // hand a value to another thread/actor that can retain it.
        BytecodeExprKind::Await { .. } => true,
        BytecodeExprKind::Call { name, args } => {
            matches!(name.as_str(), "spawn" | "tell" | "ask")
                || args.iter().any(expr_has_capture_channel)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_has_capture_channel(left) || expr_has_capture_channel(right)
        }
        BytecodeExprKind::Unary { expr, .. } => expr_has_capture_channel(expr),
        BytecodeExprKind::Index { target, index } => {
            expr_has_capture_channel(target) || expr_has_capture_channel(index)
        }
        BytecodeExprKind::Field { target, .. } => expr_has_capture_channel(target),
        BytecodeExprKind::Array(elements) => elements.iter().any(expr_has_capture_channel),
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_) => false,
    }
}

/// Whether a body contains the R2 sub-channel that survives the stage-4b promotable-
/// factory carve-out: an `await`, or a `spawn`/`tell`/`ask` call. This is
/// [`body_has_capture_channel`] MINUS the closure-literal channel — a promotable
/// factory's own closure literals are the promoted survivor / non-escaping helpers, so
/// only handing a heap pointer to another thread/actor (or blocking on one) can still
/// leak past its return. See [`function_is_locally_retaining`].
fn body_has_spawn_or_await_channel(body: &[BytecodeInstruction]) -> bool {
    body.iter().any(instruction_has_spawn_or_await_channel)
}

fn instruction_has_spawn_or_await_channel(instruction: &BytecodeInstruction) -> bool {
    if fold_instruction_bodies_any(instruction, &mut |body| {
        body_has_spawn_or_await_channel(body)
    }) {
        return true;
    }
    instruction_exprs(instruction)
        .iter()
        .any(|e| expr_has_spawn_or_await_channel(e))
}

fn expr_has_spawn_or_await_channel(expr: &BytecodeExpr) -> bool {
    match &expr.kind {
        // A closure literal is NOT counted here (that is exactly the carve-out): a
        // promotable factory's literals are the survivor / non-escaping helpers.
        BytecodeExprKind::Closure { .. } => false,
        // `await` blocks on a future produced by a spawned task; `spawn`/`tell`/`ask`
        // hand a value to another thread/actor that can retain it.
        BytecodeExprKind::Await { .. } => true,
        BytecodeExprKind::Call { name, args } => {
            matches!(name.as_str(), "spawn" | "tell" | "ask")
                || args.iter().any(expr_has_spawn_or_await_channel)
        }
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_has_spawn_or_await_channel(left) || expr_has_spawn_or_await_channel(right)
        }
        BytecodeExprKind::Unary { expr, .. } => expr_has_spawn_or_await_channel(expr),
        BytecodeExprKind::Index { target, index } => {
            expr_has_spawn_or_await_channel(target) || expr_has_spawn_or_await_channel(index)
        }
        BytecodeExprKind::Field { target, .. } => expr_has_spawn_or_await_channel(target),
        BytecodeExprKind::Array(elements) => elements.iter().any(expr_has_spawn_or_await_channel),
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_) => false,
    }
}

/// Whether a body contains an R3 raw-memory-aliasing channel: any `ptr<…>`-typed
/// expression or an inline `asm` block. `alloc` is covered separately by
/// [`alloc_defeats_arena`].
fn body_has_pointer_or_asm(body: &[BytecodeInstruction]) -> bool {
    body.iter().any(instruction_has_pointer_or_asm)
}

fn instruction_has_pointer_or_asm(instruction: &BytecodeInstruction) -> bool {
    if matches!(instruction, BytecodeInstruction::Asm { .. }) {
        return true;
    }
    if fold_instruction_bodies_any(instruction, &mut |body| body_has_pointer_or_asm(body)) {
        return true;
    }
    instruction_exprs(instruction)
        .iter()
        .any(|e| expr_has_pointer(e))
}

fn expr_has_pointer(expr: &BytecodeExpr) -> bool {
    if is_pointer_type(&expr.ty) {
        return true;
    }
    match &expr.kind {
        BytecodeExprKind::Call { args, .. } => args.iter().any(expr_has_pointer),
        BytecodeExprKind::Binary { left, right, .. } => {
            expr_has_pointer(left) || expr_has_pointer(right)
        }
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => {
            expr_has_pointer(expr)
        }
        BytecodeExprKind::Index { target, index } => {
            expr_has_pointer(target) || expr_has_pointer(index)
        }
        BytecodeExprKind::Field { target, .. } => expr_has_pointer(target),
        BytecodeExprKind::Array(elements) => elements.iter().any(expr_has_pointer),
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_)
        | BytecodeExprKind::Closure { .. } => false,
    }
}

// -- Call classification (R4) ------------------------------------------------

/// The names whose call is an **indirect, retaining** target in `f`: its `fn(...)`-typed
/// parameters, plus any `fn`-typed `let` bound to something OTHER than an inline closure
/// literal (a first-class function value / factory result — an unknown target). A call
/// whose name is in this set is treated as retaining (default-deny) regardless of whether
/// the name also shadows a module function.
///
/// A `let f fn(...) = fn … ` bound by an inline **closure literal** is deliberately
/// EXCLUDED — a call to it is treated like a builtin (non-retaining). Such a closure is a
/// KNOWN, non-escaping, scalar-capturing, heap-free-body target (the native closure
/// Stage-2 rules enforce exactly that, or the function skips native), so it cannot stash a
/// heap pointer past the call. Excluding it preserves the pre-I2 treatment (the old
/// `body_calls_user` leaf test also ignored a closure-local call), which the
/// closure-per-iteration arena reclaim (`native_closure_reclaim`) depends on: denying it
/// the arena would leak a closure block each iteration and exhaust the heap.
fn fn_typed_binding_names(f: &BytecodeFunction) -> HashSet<String> {
    let mut names = HashSet::new();
    for p in &f.params {
        if p.ty.is_function() {
            names.insert(p.name.clone());
        }
    }
    collect_fn_typed_lets(&f.instructions, &mut names);
    names
}

fn collect_fn_typed_lets(body: &[BytecodeInstruction], out: &mut HashSet<String>) {
    for instruction in body {
        if let BytecodeInstruction::Let {
            name, ty, value, ..
        } = instruction
            && ty.is_function()
            // A closure LITERAL binding is a known non-escaping target (excluded); any
            // other fn-typed binding is an unknown indirect target (denied).
            && !matches!(value.kind, BytecodeExprKind::Closure { .. })
        {
            out.insert(name.clone());
        }
        fold_instruction_bodies(instruction, &mut |body| collect_fn_typed_lets(body, out));
    }
}

/// The result of scanning a function body's calls for the retention summary: the
/// module-function callee names (the sweep's edges) and whether any call is a
/// denied (extern or indirect) target (folded into the function's local property).
struct CallAnalysis<'a> {
    module_callees: Vec<&'a str>,
    has_denied_call: bool,
}

/// Classify a single call `name` for R4. Checked in order so an indirect binding that
/// shadows a module/extern name is still treated as indirect (default-deny).
enum CallClass<'a> {
    /// A module function — require `non_retaining(callee)` via the sweep.
    Module(&'a str),
    /// An `extern` C function or an indirect (`fn`-param / closure-local) call —
    /// retaining.
    Denied,
    /// A native builtin — ok (reads/computes; no user-visible retention).
    Builtin,
}

fn classify_call<'a>(
    name: &str,
    module_fns: &HashSet<&'a str>,
    extern_fns: &HashSet<&str>,
    fn_typed: &HashSet<String>,
) -> CallClass<'a> {
    if fn_typed.contains(name) {
        return CallClass::Denied;
    }
    // Return the module-lifetime `&str` from the set (not the caller-supplied `name`,
    // whose lifetime is only the traversal), so a collected callee outlives the walk.
    if let Some(&m) = module_fns.get(name) {
        return CallClass::Module(m);
    }
    if extern_fns.contains(name) {
        return CallClass::Denied;
    }
    CallClass::Builtin
}

/// Scan a body's calls, collecting module-function callees and whether any call is a
/// denied (extern/indirect) target.
fn analyze_calls<'a>(
    body: &[BytecodeInstruction],
    module_fns: &HashSet<&'a str>,
    extern_fns: &HashSet<&str>,
    fn_typed: &HashSet<String>,
) -> CallAnalysis<'a> {
    let mut analysis = CallAnalysis {
        module_callees: Vec::new(),
        has_denied_call: false,
    };
    scan_calls(body, module_fns, extern_fns, fn_typed, &mut analysis);
    analysis
}

fn scan_calls<'a>(
    body: &[BytecodeInstruction],
    module_fns: &HashSet<&'a str>,
    extern_fns: &HashSet<&str>,
    fn_typed: &HashSet<String>,
    analysis: &mut CallAnalysis<'a>,
) {
    for instruction in body {
        fold_instruction_bodies(instruction, &mut |b| {
            scan_calls(b, module_fns, extern_fns, fn_typed, analysis)
        });
        for expr in instruction_exprs(instruction) {
            scan_calls_in_expr(expr, module_fns, extern_fns, fn_typed, analysis);
        }
    }
}

fn scan_calls_in_expr<'a>(
    expr: &BytecodeExpr,
    module_fns: &HashSet<&'a str>,
    extern_fns: &HashSet<&str>,
    fn_typed: &HashSet<String>,
    analysis: &mut CallAnalysis<'a>,
) {
    if let BytecodeExprKind::Call { name, args } = &expr.kind {
        match classify_call(name, module_fns, extern_fns, fn_typed) {
            CallClass::Module(m) => analysis.module_callees.push(m),
            CallClass::Denied => analysis.has_denied_call = true,
            CallClass::Builtin => {}
        }
        for arg in args {
            scan_calls_in_expr(arg, module_fns, extern_fns, fn_typed, analysis);
        }
        return;
    }
    for child in expr_all_children(expr) {
        scan_calls_in_expr(child, module_fns, extern_fns, fn_typed, analysis);
    }
}

/// Whether the caller side finds any retaining callee (the negation is
/// [`all_callees_non_retaining`]). Mirrors [`scan_calls`]'s classification exactly, so
/// the two sides never disagree on which names are indirect/extern/module/builtin.
fn body_has_retaining_callee(
    body: &[BytecodeInstruction],
    module_fns: &HashSet<&str>,
    extern_fns: &HashSet<&str>,
    fn_typed: &HashSet<String>,
    summary: &HashMap<String, bool>,
) -> bool {
    body.iter().any(|instruction| {
        fold_instruction_bodies_any(instruction, &mut |b| {
            body_has_retaining_callee(b, module_fns, extern_fns, fn_typed, summary)
        }) || instruction_exprs(instruction)
            .iter()
            .any(|e| expr_has_retaining_callee(e, module_fns, extern_fns, fn_typed, summary))
    })
}

fn expr_has_retaining_callee(
    expr: &BytecodeExpr,
    module_fns: &HashSet<&str>,
    extern_fns: &HashSet<&str>,
    fn_typed: &HashSet<String>,
    summary: &HashMap<String, bool>,
) -> bool {
    if let BytecodeExprKind::Call { name, args } = &expr.kind {
        let denied = match classify_call(name, module_fns, extern_fns, fn_typed) {
            // A module callee is retaining if the summary says so — or, defensively, if
            // it is missing from the summary (default-deny).
            CallClass::Module(m) => summary.get(m).copied().unwrap_or(true),
            CallClass::Denied => true,
            CallClass::Builtin => false,
        };
        return denied
            || args
                .iter()
                .any(|a| expr_has_retaining_callee(a, module_fns, extern_fns, fn_typed, summary));
    }
    expr_all_children(expr)
        .iter()
        .any(|c| expr_has_retaining_callee(c, module_fns, extern_fns, fn_typed, summary))
}

// -- Shared traversal helpers ------------------------------------------------

/// Every immediate child expression of `expr` (a superset of [`expr_children`] that
/// also descends `Await`, so no capture/pointer/call node is missed). Value nodes
/// (literals, variables, closures) have no expression children.
fn expr_all_children(expr: &BytecodeExpr) -> Vec<&BytecodeExpr> {
    match &expr.kind {
        BytecodeExprKind::Binary { left, right, .. } => vec![left, right],
        BytecodeExprKind::Unary { expr, .. } | BytecodeExprKind::Await { expr } => vec![expr],
        BytecodeExprKind::Call { args, .. } => args.iter().collect(),
        BytecodeExprKind::Array(elements) => elements.iter().collect(),
        BytecodeExprKind::Field { target, .. } => vec![target],
        BytecodeExprKind::Index { target, index } => vec![target, index],
        BytecodeExprKind::Integer(_)
        | BytecodeExprKind::Float(_)
        | BytecodeExprKind::Bool(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Variable(_)
        | BytecodeExprKind::Closure { .. } => Vec::new(),
    }
}

/// Every top-level expression carried directly by `instruction` (not its nested
/// statement bodies — those are visited via [`fold_instruction_bodies`]).
fn instruction_exprs(instruction: &BytecodeInstruction) -> Vec<&BytecodeExpr> {
    match instruction {
        BytecodeInstruction::Let { value, .. }
        | BytecodeInstruction::Assign { value, .. }
        | BytecodeInstruction::Expr(value)
        | BytecodeInstruction::Throw { value, .. } => {
            let mut out = vec![value];
            // An `Assign` path may carry index expressions.
            if let BytecodeInstruction::Assign { path, .. } = instruction {
                for p in path {
                    if let BytecodePlace::Index(index) = p {
                        out.push(index);
                    }
                }
            }
            out
        }
        BytecodeInstruction::Return(Some(value)) => vec![value],
        BytecodeInstruction::If { branches, .. } => branches.iter().map(|b| &b.condition).collect(),
        BytecodeInstruction::While { condition, .. } => vec![condition],
        BytecodeInstruction::For {
            start, end, step, ..
        } => {
            let mut out = vec![start, end];
            if let Some(s) = step {
                out.push(s);
            }
            out
        }
        BytecodeInstruction::Match { scrutinee, .. } => vec![scrutinee],
        // `Try` carries only nested bodies (visited via `fold_instruction_bodies`),
        // no top-level expression of its own.
        BytecodeInstruction::Return(None)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Loop { .. }
        | BytecodeInstruction::RegionBlock { .. }
        | BytecodeInstruction::Try { .. }
        | BytecodeInstruction::Asm { .. } => Vec::new(),
    }
}

/// Apply `on_body` to each nested statement body of `instruction`, returning whether
/// any returned `true`. Used by the `_any` scanners so control-flow nesting is
/// traversed once, uniformly.
fn fold_instruction_bodies_any(
    instruction: &BytecodeInstruction,
    on_body: &mut dyn FnMut(&[BytecodeInstruction]) -> bool,
) -> bool {
    match instruction {
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => branches.iter().any(|b| on_body(&b.body)) || on_body(else_body),
        BytecodeInstruction::While { body, .. }
        | BytecodeInstruction::For { body, .. }
        | BytecodeInstruction::Loop { body, .. }
        | BytecodeInstruction::RegionBlock { body, .. } => on_body(body),
        BytecodeInstruction::Match { arms, .. } => arms.iter().any(|arm| on_body(&arm.body)),
        BytecodeInstruction::Try {
            body, catch_body, ..
        } => on_body(body) || on_body(catch_body),
        BytecodeInstruction::Let { .. }
        | BytecodeInstruction::Assign { .. }
        | BytecodeInstruction::Return(_)
        | BytecodeInstruction::Break(_)
        | BytecodeInstruction::Continue(_)
        | BytecodeInstruction::Expr(_)
        | BytecodeInstruction::Throw { .. }
        | BytecodeInstruction::Asm { .. } => false,
    }
}

/// Apply `on_body` to each nested statement body of `instruction` (for side-effecting
/// collectors that do not short-circuit).
fn fold_instruction_bodies(
    instruction: &BytecodeInstruction,
    on_body: &mut dyn FnMut(&[BytecodeInstruction]),
) {
    fold_instruction_bodies_any(instruction, &mut |body| {
        on_body(body);
        false
    });
}
