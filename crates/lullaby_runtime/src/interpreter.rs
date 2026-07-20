//! The AST interpreter: the `Runtime` evaluator, the `Control` flow signal, closure
//! collection, the move-on-functional-update analysis, the **env shelf** that lets a
//! callee reach its caller's locals for a cross-frame `addr_of`, and the
//! `run_main`/`run_named_function` entry points. Split out of `lib.rs` as a
//! behavior-preserving code move. The builtin and expression/statement halves of
//! `impl Runtime` remain in the crate root's `builtins` and `eval` submodules (which
//! reach these types through `use super::*`), so the items they touch are re-exported
//! at the crate root. The lexical `Env` lives in `runtime_env.rs` (mirroring the
//! IR/bytecode tiers' `ir_env.rs`), split out to keep this file under the size cap. The
//! value model and the shared helper modules are reached through `use crate::*`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use std::collections::VecDeque;

use lullaby_diagnostics::Span;
use lullaby_parser::{ActorDecl, BinaryOp, Expr, ExprKind, Function, Place, Program, Stmt};

use crate::*;

pub fn run_main(program: &Program) -> Result<Value, RuntimeError> {
    run_main_with_args(program, Vec::new())
}

/// Run `main` with the running program's CLI arguments, which the `args()`
/// builtin exposes. `run_main` is the zero-argument wrapper.
///
/// The program is wrapped in an `Arc<Program>` here so a detached thread created
/// by `spawn` can own a share of the program and run Lullaby independently. The
/// interpreter keeps its usual `&Program` borrow (from `&*arc`, which the `Arc`
/// outlives) for normal use, and ALSO holds an owned `Arc<Program>` clone purely
/// to hand to spawned threads. These are two separate handles to the same shared
/// data — not a self-referential struct.
pub fn run_main_with_args(program: &Program, args: Vec<String>) -> Result<Value, RuntimeError> {
    // Run the whole evaluation on a dedicated large-stack thread so a deeply
    // recursive program cannot overflow the host thread's default stack (see
    // `interp_stack`). The uniform depth bound turns genuinely unbounded recursion
    // into a clean `L0466` before even this large stack can overflow.
    crate::run_on_interpreter_stack(move || {
        let arc = Arc::new(program.clone());
        run_main_shared(arc, args)
    })
}

/// Shared-program entry: build an interpreter borrowing `&*arc` while retaining
/// an owned `Arc<Program>` clone for detached-thread spawning.
fn run_main_shared(arc: Arc<Program>, args: Vec<String>) -> Result<Value, RuntimeError> {
    if !arc.functions.iter().any(|function| function.name == "main") {
        return Err(RuntimeError::new("L0422", "missing `main` function"));
    }
    let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
    runtime.program_args = args;
    let result = runtime.call_function("main", Vec::new())?;
    // Graceful drain: process every outstanding actor message (run-to-completion,
    // one at a time, FIFO) before the program exits, so a `tell` with an
    // observable side effect (e.g. `print`) produces deterministic output.
    runtime.drain_actors()?;
    Ok(result)
}

/// Run a single named zero-argument function against `program` through the AST
/// interpreter, mirroring `run_main` but for an arbitrary entry point (used by
/// the `lullaby test` runner). The program need not define `main`. Returns the
/// function's value on success, or the propagated `RuntimeError` — including a
/// user `throw` / failed `assert` (code `L0420`) — on failure.
pub fn run_named_function(program: &Program, name: &str) -> Result<Value, RuntimeError> {
    // Same large-stack evaluation as `run_main_with_args` (this is the `lullaby
    // test` entry point), so a deeply recursive test body cannot overflow the host
    // stack and instead ends in a clean `L0466` at the shared bound.
    crate::run_on_interpreter_stack(move || {
        let arc = Arc::new(program.clone());
        let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
        let result = runtime.call_function(name, Vec::new())?;
        runtime.drain_actors()?;
        Ok(result)
    })
}

/// Register every `ExprKind::Closure` reachable from a block of statements into
/// `table`, keyed by the closure's parse-order `id`. The body borrows the program
/// with the lifetime `'a`; the parameter names are cloned. Nested closures (a
/// closure whose body is itself a closure) are collected recursively.
fn collect_closures_in_block<'a>(
    body: &'a [Stmt],
    table: &mut HashMap<usize, (Vec<String>, &'a Expr)>,
) {
    for stmt in body {
        collect_closures_in_stmt(stmt, table);
    }
}

fn collect_closures_in_stmt<'a>(
    stmt: &'a Stmt,
    table: &mut HashMap<usize, (Vec<String>, &'a Expr)>,
) {
    match stmt {
        Stmt::Let { value, .. } | Stmt::Assign { value, .. } | Stmt::Throw { value, .. } => {
            collect_closures_in_expr(value, table);
        }
        Stmt::Return(Some(expr)) | Stmt::Expr(expr) => collect_closures_in_expr(expr, table),
        Stmt::Return(None)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::Asm { .. }
        | Stmt::Region(_) => {}
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            for branch in branches {
                collect_closures_in_expr(&branch.condition, table);
                collect_closures_in_block(&branch.body, table);
            }
            collect_closures_in_block(else_body, table);
        }
        Stmt::While {
            condition, body, ..
        } => {
            collect_closures_in_expr(condition, table);
            collect_closures_in_block(body, table);
        }
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            collect_closures_in_expr(start, table);
            collect_closures_in_expr(end, table);
            if let Some(step) = step {
                collect_closures_in_expr(step, table);
            }
            collect_closures_in_block(body, table);
        }
        Stmt::ForEach { iterable, body, .. } => {
            collect_closures_in_expr(iterable, table);
            collect_closures_in_block(body, table);
        }
        Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } | Stmt::RegionBlock { body, .. } => {
            collect_closures_in_block(body, table);
        }
        Stmt::Try {
            body, catch_body, ..
        } => {
            collect_closures_in_block(body, table);
            collect_closures_in_block(catch_body, table);
        }
    }
}

fn collect_closures_in_expr<'a>(
    expr: &'a Expr,
    table: &mut HashMap<usize, (Vec<String>, &'a Expr)>,
) {
    match &expr.kind {
        ExprKind::Closure { id, params, body } => {
            let names = params.iter().map(|param| param.name.clone()).collect();
            table.insert(*id, (names, body.as_ref()));
            // A closure body may itself contain further closures.
            collect_closures_in_expr(body, table);
        }
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Char(_)
        | ExprKind::Variable(_) => {}
        ExprKind::Array(items) => {
            for item in items {
                collect_closures_in_expr(item, table);
            }
        }
        ExprKind::ArrayFill { value, count } => {
            collect_closures_in_expr(value, table);
            collect_closures_in_expr(count, table);
        }
        ExprKind::Index { target, index } => {
            collect_closures_in_expr(target, table);
            collect_closures_in_expr(index, table);
        }
        ExprKind::Unary { expr, .. } | ExprKind::Await { expr } => {
            collect_closures_in_expr(expr, table);
        }
        ExprKind::Binary { left, right, .. } => {
            collect_closures_in_expr(left, table);
            collect_closures_in_expr(right, table);
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                collect_closures_in_expr(arg, table);
            }
        }
        ExprKind::Spawn { args, .. } => {
            for arg in args {
                collect_closures_in_expr(arg, table);
            }
        }
        ExprKind::Tell { target, args, .. } => {
            collect_closures_in_expr(target, table);
            for arg in args {
                collect_closures_in_expr(arg, table);
            }
        }
        ExprKind::StructLiteral { fields, .. } => {
            for (_, value) in fields {
                collect_closures_in_expr(value, table);
            }
        }
        ExprKind::Field { target, .. } => collect_closures_in_expr(target, table),
        ExprKind::Match { scrutinee, arms } => {
            collect_closures_in_expr(scrutinee, table);
            for arm in arms {
                collect_closures_in_block(&arm.body, table);
            }
        }
        ExprKind::Try(inner) => collect_closures_in_expr(inner, table),
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_closures_in_expr(cond, table);
            collect_closures_in_expr(then_branch, table);
            collect_closures_in_expr(else_branch, table);
        }
        ExprKind::In { value, collection } => {
            collect_closures_in_expr(value, table);
            collect_closures_in_expr(collection, table);
        }
        ExprKind::Slice { target, start, end } => {
            collect_closures_in_expr(target, table);
            if let Some(start) = start {
                collect_closures_in_expr(start, table);
            }
            if let Some(end) = end {
                collect_closures_in_expr(end, table);
            }
        }
        ExprKind::Combinator { operand, .. } => collect_closures_in_expr(operand, table),
    }
}

/// The function value `parallel_map` runs on each worker thread: either a named
/// top-level function or a self-contained capturing closure. Both are `Send`
/// (`String` / `Closure`), so they cross the scoped-thread boundary safely.
#[derive(Debug, Clone)]
pub(crate) enum ParallelCallable {
    Func(String),
    Closure(Closure),
}

/// One entry in the interpreter's active call stack. The function name is
/// *borrowed* from the program (`&'a str`), so pushing a frame on every call is
/// allocation-free; the owned [`TraceFrame`]s a `RuntimeError` carries are
/// materialized only when a traceback is actually attached on the error path.
pub(crate) struct CallFrame<'a> {
    pub(crate) function: &'a str,
    pub(crate) span: Option<Span>,
}

pub(crate) struct Runtime<'a> {
    /// The whole program, borrowed so a builtin can spawn sibling interpreters
    /// over the same shared `&Program` (used by `parallel_map`'s scoped threads).
    pub(crate) program: &'a Program,
    /// An owned share of the same program, handed by `.clone()` to detached
    /// threads created by `spawn` so they can build their own interpreter over
    /// `&*arc` and outlive the `spawn` call. Separate handle, not self-referential.
    pub(crate) program_arc: Arc<Program>,
    pub(crate) functions: HashMap<&'a str, &'a Function>,
    /// The running program's CLI arguments, exposed by the `args()` builtin.
    pub(crate) program_args: Vec<String>,
    /// Declared struct types: name -> ordered field names, used to build struct
    /// values from positional construction arguments.
    pub(crate) structs: HashMap<&'a str, Vec<String>>,
    /// Enum variant name -> owning enum name. Variant names are globally unique,
    /// so this resolves both unit and payload construction.
    pub(crate) variants: HashMap<&'a str, &'a str>,
    pub(crate) heap: Vec<Option<Value>>,
    /// Freestanding-tier byte-addressed raw-pointer space backing `addr_of` /
    /// `ptr_offset` / `ptr_cast` (disjoint from `heap`; see `raw_pointer.rs`).
    pub(crate) raw_ptrs: crate::RawPointerMemory,
    /// Ownership counts for reference-counted (`rc<T>`) heap slots, keyed by
    /// slot index. Slots not present here are raw pointers / plain allocations.
    pub(crate) refcounts: HashMap<usize, usize>,
    /// Per-runtime table of open network sockets. A `Value::Socket(i)` indexes
    /// this vector; closing a socket sets its slot to `None`, mirroring the heap.
    pub(crate) sockets: Vec<Option<SocketResource>>,
    /// Per-runtime table of live external processes. A `Value::Process(i)` indexes
    /// this vector; a killed/reaped process keeps its slot but `child.stdout`/
    /// `stderr` are drained on read. Mirrors `sockets`.
    pub(crate) processes: Vec<Option<ProcessResource>>,
    pub(crate) call_stack: Vec<CallFrame<'a>>,
    /// Trait-method dispatch table: `(receiver type name, method name)` -> the
    /// impl function. Built once from every `impl Trait for Type` block.
    pub(crate) impl_methods: HashMap<(String, String), &'a Function>,
    /// Names that are trait methods (declared in some `trait`). A call to one of
    /// these dispatches on the receiver's runtime type via `impl_methods`.
    pub(crate) trait_method_names: HashSet<String>,
    /// Names of `async fn` functions. Calling one spawns an OS thread running its
    /// body and yields a `Value::Future` that `await` resolves.
    pub(crate) async_functions: HashSet<&'a str>,
    /// Names of `extern fn` (C-ABI) functions. The interpreter cannot execute C,
    /// so a call to one raises `L0423` before any builtin/user dispatch.
    pub(crate) extern_functions: HashSet<&'a str>,
    /// The failure value carried by an in-flight postfix `?` early return. When
    /// `EXPR?` hits `none`/`err` it stashes the whole enum value here and raises
    /// the `L0430` sentinel; `invoke_function` (the call boundary) takes this
    /// value and returns it as the enclosing function's result. The unwind is
    /// synchronous — nothing else runs between the raise and the catch — so a
    /// single slot is sufficient and never observed as stale.
    pub(crate) pending_try_return: Option<Value>,
    /// The closure-body table: `closure id -> (parameter names, body expression)`.
    /// Built once at construction by walking every function/impl-method body for
    /// `ExprKind::Closure` nodes. A `Value::Closure` carries only its `id`, so an
    /// invocation looks its body up here — the runtime value stays backend-neutral
    /// and stores no AST node. Bodies borrow the program with lifetime `'a`.
    pub(crate) closures: HashMap<usize, (Vec<String>, &'a Expr)>,
    /// Declared actor types, keyed by name and borrowed from the program. Used by
    /// `spawn` to construct an instance and by `tell`/drain to dispatch a message
    /// to the right handler body.
    pub(crate) actors: HashMap<&'a str, &'a ActorDecl>,
    /// Live actor instances. A `Value::ActorRef(i)` indexes this vector; the slot
    /// holds the actor's private `state` plus its supervision links. The vector
    /// only grows: a stopped actor keeps its slot (flagged `stopped`) so its
    /// handle stays a valid index and messages to it fail cleanly rather than
    /// dangling.
    pub(crate) actor_instances: Vec<ActorInstance>,
    /// The global message mailbox: a FIFO queue of pending deliveries. `tell`/
    /// `ask` enqueue; the scheduler dequeues the first *deliverable* message (one
    /// whose target actor is not already running a turn) and runs its handler to
    /// completion. Single-threaded and deterministic: the same program always
    /// produces the same output.
    pub(crate) actor_mailbox: VecDeque<ActorMessage>,
    /// One-shot reply slots for `ask` request-reply futures. A
    /// `Value::ActorFuture(i)` indexes this vector; the slot is
    /// [`ReplySlot::Pending`] until the target handler's turn completes and
    /// writes its reply value, then `await` takes it. A slot whose target actor
    /// stopped before running the request becomes [`ReplySlot::Unavailable`], so
    /// the awaiting side gets a deterministic `L0359` instead of hanging. Slots
    /// live until the program ends (a one-shot future is awaited once).
    pub(crate) actor_reply_slots: Vec<ReplySlot>,
    /// The set of actor ids whose turn is currently on the (Rust) call stack — an
    /// actor is "busy" from the moment it begins a message turn until that turn
    /// (including any `await`s inside it) completes. The scheduler never starts a
    /// second turn for a busy actor: this is the **non-reentrant run-to-completion**
    /// guarantee that keeps each actor's `state` single-writer. A `tell`/`ask` to a
    /// busy actor stays queued until the actor is free; an `await` that could only
    /// be satisfied by re-entering a busy actor is a deterministic deadlock
    /// (`L0356`).
    pub(crate) busy_actors: std::collections::HashSet<usize>,
    /// The actor whose turn is currently executing, or `None` when control is in
    /// `main` (or a free function called from it) rather than inside a handler.
    /// A `spawn` reads this to record the new actor's **supervisor**: the actor
    /// that spawned it, per the supervision tree rooted at `main`. Saved and
    /// restored around every turn, so it always names the innermost running
    /// handler.
    pub(crate) current_actor: Option<usize>,
    /// Supervisory actions deferred because their target was mid-turn when the
    /// failure was decided — the usual case for an escalation, whose supervisor is
    /// typically blocked in `await ask child...`. Applied at that actor's turn
    /// boundary, upholding run-to-completion: a turn always finishes, and the
    /// action then lands on state no live turn is holding. Keyed by actor id; an
    /// actor's action is a function of its own single policy, so a repeated
    /// failure within one turn records the same action.
    pub(crate) pending_supervision: HashMap<usize, SupervisionAction>,
    /// Actors whose `init` is currently re-running for a `supervise restart`. A
    /// restart cannot loop on a poison message, but an `init` that itself fails
    /// would restart forever; this guard turns that into a deterministic `L0363`.
    pub(crate) restarting: std::collections::HashSet<usize>,
    /// A free-list of reusable per-call environments. Function invocation is on the
    /// hot path and each call needs a fresh `Env`; rather than allocate one (its
    /// scope `Vec` plus a first-scope `Vec` that grows as parameters bind) on every
    /// call, callees borrow a reset `Env` from here and return it on a normal exit,
    /// so a deep or repeated call reuses buffers instead of reallocating. Envs are
    /// only returned on the success path; error/`?`-unwind paths simply drop theirs
    /// (correctness is unaffected — a smaller pool just means a few more allocs).
    pub(crate) env_pool: Vec<Env>,
    /// The **env shelf**: the ancestor frames' environments, innermost last.
    ///
    /// At a call boundary the caller's `Env` is swapped out of its `&mut Env` slot
    /// and pushed here for the dynamic extent of the call, so a callee can reach its
    /// caller's locals — the out-parameter idiom, `poke(addr_of(x))`. The *current*
    /// frame is deliberately **not** here: it stays a plain `&mut Env`, which is what
    /// keeps every variable access exactly as cheap as it was (see
    /// `crate::raw_pointer`'s module docs for why this beats `Rc<RefCell<Env>>` and a
    /// frame-id-indexed `Vec<Env>`).
    ///
    /// Only populated once the program has taken an address
    /// ([`RawPointerMemory::shelf_needed`]), so ordinary code never touches it.
    pub(crate) env_shelf: Vec<Env>,
}

impl<'a> Runtime<'a> {
    /// Build an interpreter over the borrowed program `program` while retaining an
    /// owned `Arc<Program>` (`program_arc`) that points at the same data, used
    /// only to hand a share to detached `spawn`ed threads. The caller passes both
    /// handles (e.g. `Runtime::new(&arc, Arc::clone(&arc))`).
    pub(crate) fn new(
        program: &'a Program,
        program_arc: Arc<Program>,
    ) -> Result<Self, RuntimeError> {
        let functions = program
            .functions
            .iter()
            .map(|function| (function.name.as_str(), function))
            .collect::<HashMap<_, _>>();

        // Build the trait-method dispatch table from all impl blocks and record
        // the set of trait method names so calls can be recognized.
        let mut impl_methods = HashMap::new();
        let mut trait_method_names = HashSet::new();
        for decl in &program.traits {
            for method in &decl.methods {
                trait_method_names.insert(method.name.clone());
            }
        }
        for decl in &program.impls {
            for method in &decl.methods {
                impl_methods.insert((decl.type_name.clone(), method.name.clone()), method);
            }
            // Inherent (`impl Box<T>`) methods dispatch by the receiver's runtime
            // type exactly like trait methods under erasure, so their names join
            // the receiver-dispatch set. `decl.type_name` is the base type name
            // (`Box`), which matches `value_type_name` of a `Box<...>` value.
            if decl.is_inherent() {
                for method in &decl.methods {
                    trait_method_names.insert(method.name.clone());
                }
            }
        }

        let structs = program
            .structs
            .iter()
            .map(|declaration| {
                (
                    declaration.name.as_str(),
                    declaration
                        .fields
                        .iter()
                        .map(|field| field.name.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();

        let mut variants = HashMap::new();
        // Built-in `option`/`result` generic-enum variants. Registered like user
        // variants so construction and `match` reuse the same `Value::Enum` path.
        variants.insert("some", "option");
        variants.insert("none", "option");
        variants.insert("ok", "result");
        variants.insert("err", "result");
        // Compiler-provided `MemoryOrder` enum: its five unit variants construct
        // the ordering values consumed by the ordering-taking atomic builtins and
        // `fence`. Registered like `option`/`result` so bare `acquire`/`seq_cst`/…
        // build `Value::Enum` through the shared unit-variant path.
        for variant in MEMORY_ORDER_VARIANTS {
            variants.insert(variant, "MemoryOrder");
        }
        for declaration in &program.enums {
            for variant in &declaration.variants {
                variants.insert(variant.name.as_str(), declaration.name.as_str());
            }
        }

        let actors = program
            .actors
            .iter()
            .map(|actor| (actor.name.as_str(), actor))
            .collect::<HashMap<_, _>>();

        let async_functions = program
            .functions
            .iter()
            .filter(|function| function.is_async)
            .map(|function| function.name.as_str())
            .collect::<HashSet<_>>();

        let extern_functions = program
            .functions
            .iter()
            .filter(|function| function.is_extern)
            .map(|function| function.name.as_str())
            .collect::<HashSet<_>>();

        // Build the closure-body table by walking every function and impl-method
        // body for `ExprKind::Closure` nodes. Each node's parse-order `id` keys a
        // `(param names, body)` entry; the runtime `Value::Closure` carries only
        // the id, so this is where the id is resolved back to a body to evaluate.
        let mut closures = HashMap::new();
        for function in &program.functions {
            collect_closures_in_block(&function.body, &mut closures);
        }
        for decl in &program.impls {
            for method in &decl.methods {
                collect_closures_in_block(&method.body, &mut closures);
            }
        }

        Ok(Self {
            program,
            program_arc,
            functions,
            program_args: Vec::new(),
            structs,
            variants,
            heap: Vec::new(),
            raw_ptrs: crate::RawPointerMemory::default(),
            refcounts: HashMap::new(),
            sockets: Vec::new(),
            processes: Vec::new(),
            call_stack: Vec::new(),
            impl_methods,
            trait_method_names,
            async_functions,
            extern_functions,
            pending_try_return: None,
            closures,
            actors,
            actor_instances: Vec::new(),
            actor_mailbox: VecDeque::new(),
            actor_reply_slots: Vec::new(),
            busy_actors: std::collections::HashSet::new(),
            current_actor: None,
            pending_supervision: HashMap::new(),
            restarting: std::collections::HashSet::new(),
            env_pool: Vec::new(),
            env_shelf: Vec::new(),
        })
    }

    /// Spawn an `async fn` call on a new OS thread that owns a share of the
    /// program (an `Arc<Program>` clone) and builds its own interpreter, then
    /// return a `Value::Future` handle so `await` retrieves the produced value.
    /// The argument values are already evaluated and are `Send`, so they cross
    /// the thread boundary safely; heaps are per-thread.
    fn spawn_async(&self, name: &str, args: Vec<Value>) -> Value {
        let arc = Arc::clone(&self.program_arc);
        let func_name = name.to_string();
        let handle = crate::spawn_interpreter_thread(move || {
            let mut runtime = Runtime::new(&arc, Arc::clone(&arc))?;
            runtime.call_function(&func_name, args)
        });
        Value::Future(Future {
            handle: Arc::new(Mutex::new(Some(handle))),
        })
    }

    /// True when a call to `name` is a plain builtin (or infallible enum/struct
    /// constructor) rather than anything that could raise a *catchable* `L0420`
    /// user error. This is the safety gate for the move-on-functional-update fast
    /// path: builtins in the accumulation idiom (`push`, `concat`, `map_set`,
    /// `sort`, `replace`, `set`, …) only ever fail with non-catchable errors that
    /// halt the program, so moving the consumed argument out can never leave a
    /// moved-out placeholder observable by a surrounding `catch`. Excluded, because
    /// they can run user code (which may `throw`) or dispatch by value:
    /// closure/func-valued variables, `extern`/`async` functions, trait methods,
    /// user-defined functions, and `assert` (the one builtin that raises `L0420`).
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

    /// The move-on-functional-update fast path for the pervasive `x = f(x, …)`
    /// (CALL) and `x = x <binop> e` / `x = e <binop> x` (BINARY) accumulation
    /// idioms. When the assignment target `name` appears **exactly once** — as a
    /// bare call argument, or as exactly one bare operand of a binary op — and
    /// nowhere else in the RHS, and `name` is a local, this evaluates the RHS
    /// with that one occurrence **moved** out of the environment instead of
    /// cloned, and returns `Some(result)`. Returning `None` means the pattern did
    /// not apply and the caller must fall back to the ordinary clone path.
    ///
    /// The binary form is what makes `s = s + piece` in a loop O(n): the moved
    /// left operand's heap buffer is reused by `eval_binary`'s string concat
    /// (see [`Value::into_string`]) instead of being cloned on read.
    ///
    /// Safety: moving is observably identical to cloning here because `name` is
    /// (a) consumed exactly once, (b) not read anywhere else in the statement, and
    /// (c) immediately overwritten with the result. The *other* operand/arguments
    /// are evaluated *before* the move, so a failure while evaluating them leaves
    /// `name` intact. The consuming op cannot raise a *catchable* error: builtins
    /// on the call path are gated to those that never raise `L0420`, and
    /// `eval_binary` only ever raises non-catchable errors (e.g. `L0404`
    /// div-by-zero, `L0417` type) — only user-thrown `L0420` is recoverable by a
    /// `catch` — so a mid-op failure halts the program with the moved-out
    /// placeholder never observed. Short-circuit `and`/`or` are excluded: they do
    /// not route through `eval_binary` and evaluating the non-target operand early
    /// would change their conditional-evaluation semantics.
    pub(crate) fn try_move_functional_update(
        &mut self,
        name: &str,
        rhs: &Expr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        match &rhs.kind {
            ExprKind::Call { name: callee, args } => {
                self.try_move_call_update(name, callee, args, env, require_innermost)
            }
            ExprKind::Binary { op, left, right } => {
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
        args: &[Expr],
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        // Never optimize when the call target is the variable itself (`x = x(x)`):
        // moving `x` out would change how the call name resolves.
        if callee == name {
            return Ok(None);
        }
        if !self.is_move_safe_builtin(callee, env) {
            return Ok(None);
        }
        // The consumed binding must be a local. For a `let` re-binding it must be
        // the innermost binding (because `let` shadows into the innermost scope);
        // for a plain reassignment any-scope is fine (moved from, and written back
        // to, the nearest binding). Checked before locating the argument so the
        // common non-matching cases stay cheap.
        let bound = if require_innermost {
            env.innermost_has(name)
        } else {
            env.is_bound(name)
        };
        if !bound {
            return Ok(None);
        }
        // Locate the single bare `Variable(name)` argument and prove `name` does
        // not appear anywhere else in the argument list.
        let mut target_idx: Option<usize> = None;
        for (i, arg) in args.iter().enumerate() {
            let is_bare = matches!(&arg.kind, ExprKind::Variable(v) if v == name);
            if is_bare && target_idx.is_none() {
                target_idx = Some(i);
            } else if expr_mentions_var(arg, name) {
                return Ok(None);
            }
        }
        let Some(target_idx) = target_idx else {
            return Ok(None);
        };
        // Evaluate every *other* argument first, in source order. If one fails
        // (e.g. a nested `throw`), `name` is still intact and the env consistent.
        let mut evaluated: Vec<Option<Value>> = Vec::with_capacity(args.len());
        for (i, arg) in args.iter().enumerate() {
            if i == target_idx {
                evaluated.push(None);
            } else {
                evaluated.push(Some(self.eval_expr(arg, env)?));
            }
        }
        // All other arguments succeeded: move the target's value out (no clone),
        // leaving a placeholder in its slot for the caller's write-back.
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
        // `callee` is a plain builtin/constructor here (closures/func values/
        // extern/async/user functions were excluded), so dispatch it directly.
        Ok(Some(self.with_env_shelved(env, |me| {
            me.call_function(callee, values)
        })?))
    }

    /// `x = x <binop> e` / `x = e <binop> x` arm of
    /// [`Self::try_move_functional_update`]. Fires when exactly one operand is the
    /// bare variable `name` and `name` appears nowhere else in either operand.
    fn try_move_binary_update(
        &mut self,
        name: &str,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        env: &mut Env,
        require_innermost: bool,
    ) -> Result<Option<Value>, RuntimeError> {
        // Short-circuit operators are evaluated in `eval_expr`, not `eval_binary`,
        // and their right operand is conditional; reordering evaluation would
        // change semantics, so never optimize them.
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
        // Exactly one operand must be the bare `Variable(name)`, and `name` must
        // not appear anywhere in the *other* operand. `s = s + s`, `s = pre + s +
        // suf`, `n = a - n + n`, etc. therefore fall back to the clone path.
        let left_bare = matches!(&left.kind, ExprKind::Variable(v) if v == name);
        let right_bare = matches!(&right.kind, ExprKind::Variable(v) if v == name);
        let target_is_left = if left_bare && !expr_mentions_var(right, name) {
            true
        } else if right_bare && !expr_mentions_var(left, name) {
            false
        } else {
            return Ok(None);
        };
        // Evaluate the non-target operand *before* moving the target, so a failure
        // there leaves `name` intact and the env consistent.
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

    /// Run `body` with `env` — the *calling* frame's environment — moved onto the env
    /// shelf, so anything `body` invokes can reach it by [`RootSlot::env`]. This is
    /// what makes `poke(addr_of(x))` write the caller's real `x`. Mirrors the
    /// IR/bytecode interpreter's `with_env_shelved` exactly, for backend parity.
    ///
    /// The swap leaves an [`Env::hollow`] placeholder in the caller's slot. Nothing
    /// reads it: the caller is suspended for exactly the extent of `body`, and the
    /// real environment is swapped back before it resumes.
    ///
    /// Wrapping `body` in a closure rather than exposing raw push/pop is deliberate —
    /// it makes the restore unconditional, so a `?` anywhere inside cannot leave the
    /// caller holding a hollow environment or the shelf unbalanced.
    ///
    /// # Why the gate is sound
    ///
    /// Shelving is skipped entirely unless a raw region is live. That is not a
    /// heuristic: with no region, `RawPointerMemory::resolve` cannot return a place at
    /// all, so nothing can consult the shelf and its contents are unobservable. The
    /// decision is captured in a local rather than re-tested on the way out, so a
    /// callee that takes the program's *first* address mid-`body` cannot desynchronize
    /// the push from the pop.
    ///
    /// The invariant it maintains: **every live region's `Env` is either the current
    /// frame's `&mut Env` or on the shelf.** A region created in frame `F` keeps
    /// `shelf_needed` true for as long as `F` lives, so every call `F` makes from that
    /// point on shelves `F`'s environment — and `F`'s region cannot outlive `F`,
    /// because `RawPointerMemory::exit_frame` drops it.
    ///
    /// `#[inline]` matters: this wraps every call the tree-walker makes, and inlining
    /// is what collapses the untaken branch into a single predictable test next to the
    /// dispatch rather than a closure call through a function boundary.
    #[inline]
    pub(crate) fn with_env_shelved<R>(
        &mut self,
        env: &mut Env,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if !self.raw_ptrs.shelf_needed() {
            return body(self);
        }
        self.shelve_and_run(env, body)
    }

    /// The cold half of [`Self::with_env_shelved`], kept out of line so the fast path
    /// stays small enough to inline into each call site.
    #[inline(never)]
    fn shelve_and_run<R>(&mut self, env: &mut Env, body: impl FnOnce(&mut Self) -> R) -> R {
        let mut hollow = Env::hollow();
        std::mem::swap(env, &mut hollow);
        self.env_shelf.push(hollow);
        let result = body(self);
        let mut restored = self
            .env_shelf
            .pop()
            .expect("env shelf push/pop are paired by `with_env_shelved`");
        std::mem::swap(env, &mut restored);
        result
    }

    /// Find a shelved ancestor frame's environment by its [`RootSlot::env`] id.
    /// Ancestors only — the current frame is never on the shelf.
    pub(crate) fn shelf_env(&self, id: u64) -> Option<&Env> {
        self.env_shelf.iter().find(|env| env.id() == id)
    }

    /// Mutable counterpart of [`Self::shelf_env`] — the write half of a cross-frame
    /// `ptr_write`.
    pub(crate) fn shelf_env_mut(&mut self, id: u64) -> Option<&mut Env> {
        self.env_shelf.iter_mut().find(|env| env.id() == id)
    }

    /// Locate the environment owning `root`, checking the current frame first (the
    /// overwhelmingly common in-frame case) and then the shelf.
    pub(crate) fn owning_env<'e>(&'e self, root: &RootSlot, env: &'e Env) -> Option<&'e Env> {
        if env.id() == root.env {
            return Some(env);
        }
        self.shelf_env(root.env)
    }

    /// Dispatch a call to an already-resolved top-level function name: reject an
    /// `extern fn` (C-ABI, native-only) with `L0423`, spawn an `async fn` on its
    /// own OS thread yielding a `Future`, or invoke the function / builtin /
    /// constructor synchronously through [`Self::call_function`].
    pub(crate) fn dispatch_named_call(
        &mut self,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        if self.extern_functions.contains(name) {
            return Err(extern_call_error(name));
        }
        if self.async_functions.contains(name) {
            Ok(self.spawn_async(name, args))
        } else {
            self.call_function(name, args)
        }
    }

    pub(crate) fn call_function(
        &mut self,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        // Trait-method dispatch: when `name` is a trait method, select the impl
        // by the receiver `args[0]`'s runtime type and invoke it. Because
        // generics are erased, a bounded-generic `v.show()` is the same lookup.
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
            "read_line" => Self::builtin_read_line(args),
            "read_all" => Self::builtin_read_all(args),
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
            // `checked_div`/`checked_rem` are shadowable by a user function of the
            // same name (matching the semantic checker's guard), so the builtin
            // only fires when the program defines no such function.
            "checked_div" if !self.functions.contains_key("checked_div") => {
                checked_div_rem(name, args, false)
            }
            "checked_rem" if !self.functions.contains_key("checked_rem") => {
                checked_div_rem(name, args, true)
            }
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
            "floor" => Self::builtin_floor(args),
            "ceil" => Self::builtin_ceil(args),
            "round" => Self::builtin_round(args),
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
            "rc_new" => self.builtin_rc_new(args),
            "rc_clone" => self.builtin_rc_clone(args),
            "rc_release" => self.builtin_rc_release(args),
            "rc_get" | "ref_get" => self.builtin_ref_get(name, args),
            // `ptr_read` routes through the raw-aware load so an `addr_of`-derived
            // byte address reads its region and a heap-slot handle reads the heap.
            "ptr_read" => self.builtin_load(args),
            "rc_borrow" => self.builtin_rc_borrow(args),
            "share" => self.builtin_share(args),
            "shared_get" => self.builtin_shared_get(args),
            "ptr_write" => self.builtin_store(args),
            "size_of" => Self::builtin_size_of(args),
            "align_of" => Self::builtin_align_of(args),
            "offset_of" => Self::builtin_offset_of(args),
            "ptr_to_int" => Self::builtin_ptr_to_int(args),
            "int_to_ptr" => Self::builtin_int_to_ptr(args),
            // `ptr_offset` scales by the region stride; `ptr_cast` reinterprets the
            // pointee type and is the identity on the pointer address.
            "ptr_offset" => self.builtin_ptr_offset(args),
            "ptr_cast" => Self::builtin_ptr_cast(args),
            // Volatile raw-memory access behaves exactly like `load`/`store` on
            // the interpreters' single-threaded abstract heap; the no-elision /
            // no-reordering guarantee is a native-codegen concern.
            "volatile_load" => self.builtin_load(args),
            "volatile_store" => self.builtin_store(args),
            // Port-mapped I/O is native-only: `in`/`out` are privileged x86
            // instructions over the CPU's I/O port space, which no interpreter
            // models. Refuse with `L0444` rather than fabricate a device value.
            "port_in8" | "port_in16" | "port_in32" | "port_out8" | "port_out16" | "port_out32" => {
                Err(port_io_interpreter_error(name))
            }

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
            _ => {
                let function = *self.functions.get(name).ok_or_else(|| {
                    RuntimeError::new("L0401", format!("unknown function `{name}`"))
                })?;
                self.invoke_function(function, args)
            }
        }
    }
}

pub(crate) enum Control {
    Return(Value),
    Break,
    Continue,
    Value(Value),
}

/// Sum the elements of a numeric list. `list<i64>` sums with wrapping
/// arithmetic (matching the interpreter's `+`), `list<f64>` sums as f64. An
/// empty list yields `0`/`0.0` (defaulting to `i64` `0`, which the semantic
/// type check pins to the element type). A non-numeric element is a runtime
/// type error (`L0417`).
/// Read one element from an indexable value (`string` char or `array` element)
/// by **borrowing** the container and cloning only the element. Used on the
/// `a[i]` hot path so a bare-variable index does not clone the whole container.
pub(crate) fn index_into(container: &Value, index: i64) -> Result<Value, RuntimeError> {
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

pub(crate) fn statement_span(statement: &Stmt) -> Span {
    match statement {
        Stmt::Let { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::Break(span)
        | Stmt::Continue(span)
        | Stmt::If { span, .. }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::ForEach { span, .. }
        | Stmt::Loop { span, .. }
        | Stmt::Unsafe { span, .. }
        | Stmt::RegionBlock { span, .. }
        | Stmt::Asm { span, .. }
        | Stmt::Throw { span, .. }
        | Stmt::Try { span, .. } => *span,
        Stmt::Region(decl) => decl.span,
        Stmt::Return(Some(expr)) | Stmt::Expr(expr) => expr.span,
        Stmt::Return(None) => Span::new(1, 1),
    }
}

/// Conservative "does `name` appear anywhere in this expression?" walk used by
/// the move-on-functional-update fast path to prove the target variable is not
/// referenced outside its single consuming argument. It over-approximates on
/// purpose: a mention inside a nested closure body (which may actually bind a
/// fresh `name`) still counts, and a call *name* equal to `name` counts too.
/// Over-approximating only ever forgoes the optimization — it never changes an
/// observable result — so the walk stays simple and total over `ExprKind`.
fn expr_mentions_var(expr: &Expr, name: &str) -> bool {
    match &expr.kind {
        ExprKind::Integer(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::String(_)
        | ExprKind::Char(_) => false,
        ExprKind::Variable(v) => v == name,
        ExprKind::Array(items) => items.iter().any(|item| expr_mentions_var(item, name)),
        ExprKind::ArrayFill { value, count } => {
            expr_mentions_var(value, name) || expr_mentions_var(count, name)
        }
        ExprKind::Index { target, index } => {
            expr_mentions_var(target, name) || expr_mentions_var(index, name)
        }
        ExprKind::Unary { expr, .. } => expr_mentions_var(expr, name),
        ExprKind::Binary { left, right, .. } => {
            expr_mentions_var(left, name) || expr_mentions_var(right, name)
        }
        ExprKind::Call { name: callee, args } => {
            callee == name || args.iter().any(|arg| expr_mentions_var(arg, name))
        }
        ExprKind::Spawn { args, .. } => args.iter().any(|arg| expr_mentions_var(arg, name)),
        ExprKind::Tell { target, args, .. } => {
            expr_mentions_var(target, name) || args.iter().any(|arg| expr_mentions_var(arg, name))
        }
        ExprKind::StructLiteral { fields, .. } => fields
            .iter()
            .any(|(_, value)| expr_mentions_var(value, name)),
        ExprKind::Field { target, .. } => expr_mentions_var(target, name),
        ExprKind::Match { scrutinee, arms } => {
            expr_mentions_var(scrutinee, name)
                || arms
                    .iter()
                    .any(|arm| arm.body.iter().any(|stmt| stmt_mentions_var(stmt, name)))
        }
        ExprKind::Await { expr } => expr_mentions_var(expr, name),
        ExprKind::Try(inner) => expr_mentions_var(inner, name),
        ExprKind::Closure { body, .. } => expr_mentions_var(body, name),
        ExprKind::Conditional {
            cond,
            then_branch,
            else_branch,
        } => {
            expr_mentions_var(cond, name)
                || expr_mentions_var(then_branch, name)
                || expr_mentions_var(else_branch, name)
        }
        ExprKind::In { value, collection } => {
            expr_mentions_var(value, name) || expr_mentions_var(collection, name)
        }
        ExprKind::Slice { target, start, end } => {
            expr_mentions_var(target, name)
                || start
                    .as_deref()
                    .is_some_and(|start| expr_mentions_var(start, name))
                || end
                    .as_deref()
                    .is_some_and(|end| expr_mentions_var(end, name))
        }
        ExprKind::Combinator { operand, .. } => expr_mentions_var(operand, name),
    }
}

/// Statement-level companion to [`expr_mentions_var`] for `match` arm bodies.
/// Also conservative: any syntactic mention of `name` counts.
fn stmt_mentions_var(stmt: &Stmt, name: &str) -> bool {
    match stmt {
        Stmt::Let { value, .. } | Stmt::Throw { value, .. } => expr_mentions_var(value, name),
        Stmt::Assign {
            name: target,
            path,
            value,
            ..
        } => {
            target == name
                || path.iter().any(|place| match place {
                    Place::Field(_) => false,
                    Place::Index(index) => expr_mentions_var(index, name),
                })
                || expr_mentions_var(value, name)
        }
        Stmt::Expr(expr) => expr_mentions_var(expr, name),
        Stmt::Return(expr) => expr.as_ref().is_some_and(|e| expr_mentions_var(e, name)),
        Stmt::If {
            branches,
            else_body,
            ..
        } => {
            branches.iter().any(|branch| {
                expr_mentions_var(&branch.condition, name)
                    || branch.body.iter().any(|s| stmt_mentions_var(s, name))
            }) || else_body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::While {
            condition, body, ..
        } => expr_mentions_var(condition, name) || body.iter().any(|s| stmt_mentions_var(s, name)),
        Stmt::For {
            start,
            end,
            step,
            body,
            ..
        } => {
            expr_mentions_var(start, name)
                || expr_mentions_var(end, name)
                || step.as_ref().is_some_and(|e| expr_mentions_var(e, name))
                || body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::ForEach { iterable, body, .. } => {
            expr_mentions_var(iterable, name) || body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::Loop { body, .. } | Stmt::Unsafe { body, .. } | Stmt::RegionBlock { body, .. } => {
            body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::Try {
            body, catch_body, ..
        } => {
            body.iter().any(|s| stmt_mentions_var(s, name))
                || catch_body.iter().any(|s| stmt_mentions_var(s, name))
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Region(_) | Stmt::Asm { .. } => false,
    }
}
