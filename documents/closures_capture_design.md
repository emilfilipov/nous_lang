# Environment-Capturing Closures Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This note designs **environment-capturing closures** for Lullaby: the deferred
follow-up increment named in [[closures_design]] ("Closures with capture"). It
builds directly on the delivered first-class function values — `fn(T) -> R`
values that refer to a top-level function by name and run at backend parity as
`Value::Func(String)`. That increment intentionally captured *nothing*; this one
adds an inline lambda literal that closes over surrounding locals.

The design is implementation-grade and tracks the real code: the AST runtime's
`Value` enum and `Env` scope stack in `crates/lullaby_runtime/src/lib.rs`, the
IR interpreter's mirror in `crates/lullaby_ir/src/lib.rs`, the bytecode VM that
round-trips through the IR, and the shared-handle precedent set by
`Value::Mutex`/`Value::Chan`/`Value::Task`/`Value::Future` in
[[concurrency_design]]. The guiding constraint is the project rule that a new
value must run **identically on the AST interpreter, the IR interpreter, and the
bytecode VM** (plus their optimized variants), because the parity harness runs
every fixture on all five.

ClickUp: list **"10 Language Completeness"**, ticket **"Environment-capturing
closures"**.

## Goals and non-goals

Goals for the first increment:

- An inline lambda literal whose value is an ordinary `fn(T) -> R` value, so it
  interoperates with *everything that already accepts a function value*:
  `apply`, `parallel_map`, function-typed `let`/parameters, and returning a
  function from a function.
- **Capture by value** of the free locals the body references, with capture
  timing and value semantics defined precisely and consistently with Lullaby's
  existing value-semantic collections.
- Backend parity: one shared runtime representation built, cloned, and invoked
  the same way on all three interpreters.

Explicit non-goals (deferred, listed under [Scope and sequencing](#scope-and-sequencing)):
capture by reference / shared mutable capture cells, closures that outlive and
mutate their defining scope, `struct`/generic closure *types* beyond the erased
`fn(T) -> R` spelling, native/WASM codegen of closures, and a general
`spawn(closure)`.

## Syntax

A closure literal reuses the `fn` keyword — no new keyword — and keeps the
declaration's `name type` parameter shape, but omits the name and the block,
placing the body inline after `->`. The canonical spelling is a single logical
line:

```
fn PARAMS -> EXPR
```

where `PARAMS` is zero or more `name type` pairs (the exact shape a top-level
`fn` declares) and `EXPR` is a single expression evaluated as the body.

```lby
fn apply f fn(i64) -> i64 v i64 -> i64
    f(v)

fn main -> i64
    let n i64 = 10
    let add_n fn(i64) -> i64 = fn x i64 -> x + n     # captures n by value
    apply(add_n, 5) + add_n(2)                        # 15 + 12 = 27
```

Design points:

- **Reusing `fn`** keeps the reader's model intact: a closure is "a `fn` with no
  name whose body is the expression after `->`". The parser already parses
  `name type` parameter lists and `->` return arrows for declarations; the lambda
  form reuses that parameter parser and then parses one expression for the body
  instead of an indented block.
- **Parameter types are explicit.** The first increment does *not* infer
  parameter types from context. This matches Lullaby's declaration style (every
  `fn` parameter is typed) and keeps closure typing local and unambiguous —
  important because the value's type must be a fully-known `fn(T) -> R` string,
  which is what `apply`/`parallel_map`/function-typed locals compare against.
  Contextual parameter-type inference (`let f fn(i64)->i64 = fn x -> x + 1`) is a
  clean deferred follow-up.
- **The return type is not written**; it is the type of `EXPR`. The closure's
  *value type* is therefore `fn(param types) -> typeof(EXPR)`, produced in
  canonical string form (`fn(i64) -> i64`) exactly like a name-as-value in
  [[closures_design]], so it string-compares equal to a declared function type.
- **Indentation-only rule preserved.** The body is a single expression on the
  same logical line as `fn ... ->`; no block is opened, so no new indentation
  scope is introduced and the no-brace/no-semicolon rule in
  [core_language_rules.md](core_language_rules.md) is unaffected. A
  multi-statement closure body (an indented block after `->`) is deferred; the
  single-expression form already composes because that expression may itself be
  a call to a named helper.

### Grammar delta

Adding to the expression grammar (formal grammar lives in
`documents/formal_grammar.md`; this is the delta, not a registry edit):

```
closure_expr = "fn" , { param } , "->" , expr ;
param        = ident , type ;
```

`closure_expr` is a new alternative in the primary-expression production. It is
unambiguous with a top-level `fn` *declaration* because declarations only appear
at file top level (after an optional `pub`/`async`), while `closure_expr` only
appears in expression position (the right side of `let`/assignment, a call
argument, a `return` value). A malformed lambda (missing `->`, a non-`name type`
parameter) is reported as a parser diagnostic in the existing `L0216`
malformed-construct family.

## Capture semantics

**What is captured.** The captured set is exactly the closure body's **free
variables**: identifiers the body reads that are neither the closure's own
parameters nor top-level names (functions, enum variants, struct/enum type
names, builtins). Concretely, free variables are the locals of the *enclosing*
function that the body references. In the example above, `x` is a parameter and
`add_n`/`apply`/`n` are candidates; `n` is the only enclosing local read, so the
captured set is `{ n }`.

> **Capture strategy (implemented): frame-by-value.** The delivered
> implementation captures the **whole visible frame by value** — at literal
> evaluation it snapshots *every in-scope local* (`name -> value.clone()`), not
> just the minimal free-variable set. This is a correct by-value snapshot (a
> closure reads only what it references, and each capture clones by the value's
> own rules, so value-semantic collections snapshot and reference-semantic
> handles share) and it keeps the runtime free of a per-node free-variable walk.
> Because the body is a single expression with no assignment statements, captured
> names are inherently read-only, so no `L0441` write-back check is needed.
> **Minimal free-variable capture is a deferred optimization**: it would shrink
> each closure's captured `Vec` to only the names the body references, at the
> cost of a capture-set analysis attached to the node. The observable semantics
> are identical either way.

**By value.** Each free variable is captured **by value**: at the moment the
closure literal evaluates, the current value of each captured local is **cloned**
into the closure's environment (a snapshot). This is the only choice consistent
with Lullaby's existing model:

- Lullaby collections are **value-semantic** (`list`/`map`/`array` are `Vec`-backed
  and cloned on assignment/argument-passing; there are no aliasing references
  except the explicitly reference-semantic `rc<T>`/`Chan`/`Mutex` handles). A
  by-value capture of a `list<i64>` therefore captures an independent snapshot,
  exactly as passing that list to a function would.
- Shared/reference-semantic handles (`Chan`, `Mutex`, `rc<T>`, `Socket`) are
  captured by cloning the *handle*, which — by their existing clone semantics —
  continues to share the *same* underlying resource. This is not a special case:
  it falls straight out of "clone the value", because those handle values clone
  by sharing. So a closure that captures a `Mutex` shares that mutex; a closure
  that captures a `list` snapshots it. The rule is uniform ("capture = clone the
  `Value`"); the sharing behavior is inherited from each value's own clone.

**Capture timing.** Capture happens **when the closure literal is evaluated**
(evaluation-time / eager capture), not when the closure is later called. A
closure built inside a loop iteration captures that iteration's values:

```lby
fn make_adders base i64 -> fn(i64) -> i64
    let f fn(i64) -> i64 = fn x i64 -> x + base
    f                          # captured base at the value it had here
```

Each evaluation of the literal produces a distinct closure value with its own
snapshot.

**Interaction with mutation.** Because capture is by-value snapshot:

- Mutating the enclosing local *after* the closure is created does **not** affect
  the closure's captured copy, and calling the closure does **not** mutate the
  enclosing local. There is no shared mutable cell in this increment.
- Inside the body, a captured variable is **read-only**: the first increment
  forbids assigning to a captured name (see `L0441`). Assigning to the closure's
  own *parameters* is allowed (they are ordinary locals of the closure call).
  This keeps the value model trivial (a snapshot, never written back) and defers
  the harder "mutable capture with write-back" question to a later increment that
  would introduce an explicit shared cell.

This is deliberately the **most conservative correct** semantics: eager, by-value,
read-only capture is exactly analogous to how Lullaby already passes values into
functions, so it introduces no new aliasing and no new lifetime questions.

## Runtime representation

The delivered `Value::Func(String)` is a bare name handle. Capturing closures
extend the function value to optionally carry a captured environment. Two shapes
were considered; the design chooses the second.

**Option A — widen `Value::Func`.** Change the variant to
`Func { target: FuncTarget, env: Vec<(String, Value)> }`. Rejected: it churns
every existing `Value::Func(name)` match arm (there are several in each
interpreter) and forces every plain name-as-value to carry an empty env.

**Option B (chosen) — a distinct `Value::Closure` handle, `Value::Func`
unchanged.** Add a new value alongside `Func`, mirroring how
`Chan`/`Task`/`Future`/`Mutex` are each their own variant:

```rust
// crates/lullaby_runtime/src/lib.rs  (backend-neutral: no body node)
Value::Closure(Closure),

#[derive(Debug, Clone, PartialEq)]
pub struct Closure {
    /// Stable index of this closure's body in each backend's own closure table
    /// (assigned once, on the AST, so the AST runtime and the IR interpreter use
    /// the same id). The body itself is NOT stored in the value.
    id: usize,
    /// The captured environment: one (name, value) per free variable, snapshotted
    /// by value at literal-evaluation time. Cloning the closure clones this Vec,
    /// which — per each Value's own clone — snapshots value-semantic collections
    /// and shares reference-semantic handles.
    captured: Vec<(String, Value)>,
}
```

**Backend-neutral refinement (implemented).** The original draft stored
`body: Arc<ExprKind>` in the value. That cannot work: `Value` is a *single* enum
shared by the AST runtime (which evaluates parser `ExprKind`) and the IR
interpreter (which evaluates `IrExprKind`), and `lullaby_runtime` cannot depend on
`lullaby_ir`. So the closure value carries only a **stable `id` plus the captured
snapshot**, and the *body lives in a per-backend closure table* keyed by that id:
the parser assigns each `fn … -> expr` literal an id on the AST; the AST runtime
builds an `id -> (params, ExprKind body)` table, and IR lowering builds an
`id -> (params, IrExprKind body)` table from the same nodes, so the ids line up.
Invoking `Value::Closure { id, captured }` looks the body up in the current
backend's table, pushes a scope, binds the captured snapshot then the parameters,
and evaluates the body — identical semantics on every interpreter. This is
lambda-lifting by id: the code is shared/immutable per backend, the value is
backend-agnostic, and no `ExprKind`/`IrExprKind` ever crosses into `Value`.

- The captured `Value`s are stored directly, so **cloning a `Value::Closure`
  clones its captured `Vec<(String, Value)>`**, and each element clones by its
  own rules. This is precisely how the by-value / shared-handle split above is
  enforced with zero special-casing — it is `Value`'s existing `Clone`.
- `body: Arc<ExprKind>` follows the shared-handle precedent (`Chan`/`Mutex` hold
  `Arc`-backed state): the *code* is immutable and shared, so an `Arc` clone is
  O(1) and the closure value stays `Clone`. `Value` must remain `Send` for
  [[concurrency_design]]'s `Arc<Program>` threads; `Arc<ExprKind>` and
  `Vec<(String, Value)>` are `Send` as long as `Value` is, which it already is.
- `PartialEq` for `Closure` compares params + captured + body pointer identity
  (or structural body equality); closures are rarely compared, and equality is
  only needed to keep `#[derive(PartialEq)]` on `Value` compiling. `Eq` is
  dropped from `Value` if `f64`-in-capture forces it — it already is not derived
  where floats appear; follow the existing pattern.

**Building the value.** When the interpreter evaluates a `closure_expr`, it:

1. computes the free-variable set (precomputed by semantics and attached to the
   AST/IR node so the runtime does not re-walk — see next section),
2. reads each free variable's current value out of the live `Env` (an existing
   `env.get(name)` clone),
3. constructs `Closure { params, body: Arc::clone(&body), captured }` and yields
   `Value::Closure(closure)`.

**Invoking the value.** Calls already dispatch through function values: in the
AST runtime, `ExprKind::Call { name, args }` looks up `env.get(name)` and, if it
is a `Value::Func(target)`, calls the named function (see
`crates/lullaby_runtime/src/lib.rs` around the `Ok(Value::Func(target))` arm).
Extend that dispatch: if `env.get(name)` is a `Value::Closure(c)`, invoke the
closure instead of a named function:

1. push a fresh `Env` scope (the runtime `Env` is a `Vec<HashMap<String,Value>>`
   scope stack),
2. define the captured bindings into that scope first (so they are visible to the
   body but shadowable by parameters),
3. define each parameter to its evaluated argument,
4. evaluate `body` in that scope,
5. pop the scope and return the result.

A closure passed *as an argument* and then called through a parameter name works
with no extra machinery, because the parameter local simply holds a
`Value::Closure` and the same `Call` dispatch fires. This is why `apply(f, v)`
and `parallel_map(closure, xs)` work unchanged: they receive a function value and
call it; whether it is a `Func` or a `Closure` is resolved at the call site.

**Threading through the three backends (parity).**

- **AST runtime** (`crates/lullaby_runtime`): as above — new `Value::Closure`,
  a `closure_expr` evaluation arm, and a `Value::Closure` case in the existing
  `Call` dispatch (right next to the `Value::Func` case).
- **IR interpreter** (`crates/lullaby_ir`): the IR interpreter mirrors the AST
  runtime one-to-one (it has the same `Value::Func(name.clone())` name-as-value
  arm and the same `Ok(Value::Func(target))` call dispatch). It gains the
  identical `Value::Closure` variant, an IR closure node (see lowering), and the
  same build/invoke logic. Because the IR `Value` is a sibling type, the closure
  struct is duplicated there exactly as `Chan`/`Task`/`Future` already are.
- **Bytecode VM**: the bytecode backend round-trips through the IR interpreter
  (per [[concurrency_design]] and the repository map), so it inherits closure
  execution once the IR node exists and the IR `Value` carries `Closure`. The
  `.lbc` encoder/decoder must serialize the IR closure *node* (params + body +
  the free-variable list), like any other IR expression; the runtime `Closure`
  *value* is never serialized (values are never in the artifact — only code is).

This is the same handle-parity pattern the repo already relies on: one logical
value, duplicated across the AST and IR `Value` enums, executed the same way, so
a single `.lby` fixture returns the same result on all five backend variants.

## Semantics and type-checking

Semantics (`crates/lullaby_semantics`) is where the real work concentrates,
consistent with the project's "type-checker feature, erased at runtime" pattern
used for generics ([[generics_design]]).

**Typing the literal.** For `fn PARAMS -> EXPR`:

1. Open a fresh **lexical scope** whose bindings are the closure parameters (with
   their written types), layered over the enclosing function's scope so the body
   can see enclosing locals.
2. Type-check `EXPR` in that scope to get the body type `R`.
3. The closure's value type is `fn(param types) -> R`, in the same canonical
   string form used by first-class functions, so it unifies structurally with
   any `fn(...) -> ...` expected type (`apply`'s parameter, a function-typed
   `let`, `parallel_map`'s `fn(i64) -> i64`).

**Free-variable resolution.** While checking the body, semantics classifies each
identifier:

- a **closure parameter** → local to the closure, not captured;
- an **enclosing local** (a `let`/parameter in the surrounding function's scope)
  → a **captured free variable**; record its name and type;
- a **top-level name** (function, enum variant, struct/enum type, builtin) →
  resolved globally, **not captured** (functions are called by name / turned into
  `Value::Func`, never snapshotted);
- otherwise → unresolved name (`L0403`-style, surfaced at semantics as an
  unknown-identifier error).

The resolved **captured-variable list** (names, in a deterministic order) is
attached to the AST/IR closure node so the runtime builds the environment without
re-deriving scope — the same "semantics computes it, backends consume it" split
the generics inference uses. The captured *types* feed the value type only
indirectly (through the body type); they are also recorded for diagnostics.

**Scopes involved.** Three nested scopes at a closure literal: (1) the enclosing
function scope (source of captures), (2) the closure-parameter scope (shadows
enclosing names), (3) — at *call time*, at runtime only — the captured-binding
scope plus parameter scope. Semantics validates against (1)+(2); the runtime
reconstructs an equivalent (2)+captured scope on each call.

**Read-only capture enforcement.** An assignment whose target is a captured name
(resolved to an enclosing local, inside a closure body) is rejected — capture is
a snapshot, never written back in this increment (`L0441`). Assigning to a
closure parameter is fine.

**Interaction with `parallel_map`/`spawn`.** `parallel_map(f, xs)` currently
requires `f : fn(i64) -> i64` and runs `f` in a *fresh sibling interpreter* that
resolves `f` by name (per [[concurrency_design]]). A **captured closure** cannot
be resolved by name in a fresh interpreter — but it does not need to be: a
`Value::Closure` is self-contained (it carries its own body + captured env), and
`Value` is already `Send`, so passing a `Value::Closure` across the scoped-thread
boundary and invoking it there is sound *provided its captured values are `Send`*
(they are, being `Value`). The only change `parallel_map` needs is to accept a
`Value::Closure` (not just a named `Value::Func`) as its first argument and call
it via the same closure-invocation path. This is a strict superset of today's
behavior and stays deterministic (results still collected in input order). See
[Interaction with concurrency](#interaction-with-concurrency) for `spawn`.

## Diagnostics

Proposed new codes (semantic family; **do not edit the registry in this
increment** — reserve on delivery):

- **`L0440`** — *Closure captures an unsupported value.* Reserved for the case
  where a captured free variable's type is one the increment forbids from
  crossing into a closure (e.g. a `Socket`, whose per-interpreter integer handle
  is not portable across the runtime boundary the closure may be invoked in — the
  same limitation [[concurrency_design]] notes for sockets across `spawn`). Emit
  when the closure could be moved to another interpreter/thread (e.g. into
  `parallel_map`/`spawn`) and the capture set contains a non-portable handle.
- **`L0441`** — *Assignment to a captured variable inside a closure.* Capture is
  by value (a read-only snapshot) in this increment; the enclosing local cannot
  be mutated through the closure. Fix: compute the new value and return it, or
  rebind a closure-local `let`.
- **`L0442`** — *Closure literal used where a non-function type is expected*, or
  a captured free variable could not be resolved / a body whose type cannot be
  determined. Parallels `L0390` (the first-class-function mismatch code) for the
  lambda form: a closure whose inferred `fn(...) -> R` does not match the expected
  function type, or a malformed capture.

Malformed *syntax* (missing `->`, bad parameter) stays in the existing parser
`L0216` malformed-construct family; the three codes above are semantic.

The exact wording and the final numbers land in `documents/diagnostic_registry.md`
and `crates/lullaby_diagnostics` at implementation time, following the registry's
existing "code | phase | summary | detail | fix" row shape.

## Lowering and backends

**IR lowering.** Closures **stay in the typed IR as a first-class node**; they
are *not* fully erased the way generics are. Generics erase because a type
parameter is just `Value` at runtime with no runtime witness; a closure, by
contrast, needs a runtime witness — the captured environment — so it needs an IR
node. Add an `IrExpr::Closure { params, body, captured }` (the `captured` field
is the semantics-resolved free-variable name list), lowered from the AST
`closure_expr`. The IR lowerer's type re-derivation (`call_return_type` and the
expression-type pass) must type the closure exactly as semantics did
(`fn(params) -> typeof(body)`), the same way the lowerer already re-runs generic
inference and `option`/`result` context inference so IR types match semantics.

The IR closure node is **not** further lowered into a top-level function +
explicit environment struct in this increment (closure conversion / lambda
lifting). It is interpreted directly by the IR interpreter and bytecode VM,
matching how the AST runtime interprets it — this is the parity-preserving,
lowest-risk path. Lambda lifting becomes relevant only for native/WASM codegen
and is deferred with them.

**Native and WASM backends (honest degradation).** The native x86-64 backend and
the WASM backend compile only a scalar/heap subset and already **skip** functions
they cannot lower (recording a reason), so those functions still run on the
interpreters. Closures are **not** in the compiled subset for this increment:

- The native backend lowers all-`i64` scalar + stack-aggregate functions and a
  first string-heap step; it has no notion of a heap-allocated closure object or
  an indirect call through a captured environment.
- The WASM backend lowers a scalar + linear-memory-heap subset with no function
  pointers / `call_indirect` table and no closure object layout.

Therefore any function that **constructs or calls a closure value** is recorded
as *skipped with reason "closures unsupported"* on both compiled backends
(mirroring how `enum`/`match`, `option`/`result`, and `list`/`map` functions are
skipped today), and runs on the interpreters. No new `L03xx` backend-error code
is needed — skipping is the established, non-fatal degradation. When closures are
eventually compiled, the path is standard **closure conversion**: lambda-lift each
closure body to a synthesized top-level function taking an explicit environment
struct, allocate the environment (WASM linear memory / native heap), and call
indirectly through a function table (`call_indirect` on WASM; a code pointer in
the environment on native). That is a separate, sizeable ticket; this increment
is interpreter-first and says so.

## Interaction with concurrency

[[concurrency_design]] records two limitations that capturing closures directly
address, and one that they do not:

- **`spawn` argument shape.** `spawn(f fn(Chan, i64) -> void, ch, v)` is fixed to
  the `(Chan, i64)` argument tuple precisely *because* first-class functions do
  not capture. A capturing closure lets a worker close over the shared state it
  needs — a `Mutex`, a second `Chan`, an accumulator — so the ergonomic
  `spawn(closure)` form (a zero-argument or fixed-shape closure that has already
  captured everything) becomes possible. The primitive `spawn(f, ch, v)` stays;
  `spawn` gains an overload/path that accepts a `Value::Closure` and runs it in
  the detached thread. Soundness: the detached thread runs over the shared
  `Arc<Program>`/`Arc<IrModule>` (per [[concurrency_design]]), and a
  `Value::Closure` is `Send` because its captured `Value`s are `Send` — so the
  closure, with its captured `Mutex`/`Chan` handles (which share underlying state
  on clone), crosses the boundary safely. **This is the concrete unlock**:
  "passing a `Mutex` (or a second channel) into a worker", which today "waits on
  capturing closures", is delivered by capturing that handle into the spawned
  closure.
- **`parallel_map`.** As above, it accepts a `Value::Closure` first argument and
  invokes it in each fresh sibling interpreter, staying order-deterministic.
- **Sockets across threads remain deferred.** A `Socket` is a per-interpreter
  integer index into a runtime-local table, so it cannot cross into a spawned
  worker even inside a closure. Capturing a `Socket` into a closure that is then
  handed to `spawn`/`parallel_map` is exactly the `L0440` case above. Capturing a
  `Socket` into a closure that is invoked in the *same* interpreter is fine;
  only cross-interpreter movement is rejected. Cross-thread socket sharing stays
  a separate follow-up, unchanged by this increment.

Determinism discipline is unchanged: tests over captured closures in concurrent
positions assert on **results**, never on interleaving, exactly as the existing
`run_spawn.lby`/`run_parallel.lby` fixtures do.

## Backends and parity (testing)

A `run_closures.lby` fixture is the parity anchor, combining the capturing
behaviors into a single deterministic `i64`:

```lby
fn apply f fn(i64) -> i64 v i64 -> i64
    f(v)

fn twice f fn(i64) -> i64 v i64 -> i64
    f(f(v))

fn main -> i64
    let n i64 = 10
    let add_n fn(i64) -> i64 = fn x i64 -> x + n     # capture n=10 by value
    let a i64 = apply(add_n, 5)                        # 15
    let b i64 = twice(add_n, 1)                        # 21
    let base i64 = 100
    let bump fn(i64) -> i64 = fn x i64 -> x + base     # capture base=100
    let c i64 = apply(bump, 3)                         # 103
    a + b + c                                          # 15 + 21 + 103 = 139
```

`run_closures.lby` must return the same `139` on the AST interpreter, the IR
interpreter (`--backend ir`), and the bytecode VM (`--backend bytecode`) plus
their optimized variants, via the auto-discovering cross-backend parity harness —
the same bar every prior value-carrying feature (`Value::Func`, `Chan`, `Task`,
`Future`, `Mutex`) had to clear. A capture-then-`parallel_map` fixture and a
capture-then-`spawn` fixture (result-deterministic) extend coverage into the
concurrency paths. Invalid fixtures cover `L0441` (write to a captured name) and
`L0442` (function-type mismatch of a lambda).

## Scope and sequencing

**Production-complete first increment (this ticket):**

- The `fn PARAMS -> EXPR` closure literal with explicit parameter types and a
  single-expression body.
- **Eager, by-value, read-only** capture of enclosing free locals, with the
  uniform "capture = clone the `Value`" rule (snapshots value-semantic
  collections, shares reference-semantic handles).
- `Value::Closure` on the AST and IR `Value` enums; an `IrExpr::Closure` node;
  bytecode serialization of the node; identical build/invoke on all five backend
  variants.
- Semantics: closure typing to `fn(...) -> R`, free-variable resolution, the
  captured-variable list on the node, read-only-capture checking, and diagnostics
  `L0440`/`L0441`/`L0442`.
- `parallel_map` and `spawn` accept a `Value::Closure`.
- Native/WASM: closures are a **skipped** subset (interpreter-only), documented.
- Docs: this file, `documents/repository_map.md`, the diagnostic registry rows,
  and a `run_closures.lby` fixture.

**Deferred (explicit follow-ups):**

- **Contextual parameter-type inference** for lambdas
  (`let f fn(i64)->i64 = fn x -> x + 1`).
- **Multi-statement closure bodies** (an indented block after `->`).
- **Mutable / shared-cell capture** (a closure that writes back to an enclosing
  local via an explicit shared cell), which lifts the `L0441` restriction.
- **Native/WASM closure codegen** via closure conversion (lambda lifting +
  environment structs + `call_indirect`/code-pointer indirect calls).
- **Closures as first-class *typed* struct fields / generic closure types** beyond
  the erased `fn(T) -> R` spelling.
- **Cross-thread `Socket` capture** (tied to the separate socket-sharing ticket).
- A general `spawn` over arbitrary argument tuples subsumed by "spawn a
  zero-argument closure that captured its arguments".

## Why these choices

- **Reuse `fn`, single-expression body, explicit param types.** Zero new
  keyword, the reader's existing `fn`/`name type`/`->` model carries over, and
  the closure's value type is a fully-known `fn(...) -> R` string so it drops
  straight into `apply`/`parallel_map`/function-typed locals with no new type
  machinery. It also keeps the indentation-only rule intact (no new block).
- **Eager, by-value, read-only capture.** This is the *only* capture semantics
  that adds no new aliasing to Lullaby: it is exactly "pass the value in", which
  the language already does everywhere. Value-semantic collections snapshot and
  reference-semantic handles share — both fall out of `Value::clone` with no
  special case — so the rule is one sentence and provably consistent with the
  rest of the language.
- **A distinct `Value::Closure` handle** (not a widened `Func`), following the
  `Chan`/`Task`/`Future`/`Mutex` precedent: it leaves every existing
  `Value::Func` match arm untouched, keeps plain name-as-value free of an empty
  environment, and cleanly carries `Arc<body>` + captured `Vec` while staying
  `Clone`/`Send` for the concurrency backends.
- **IR node, interpreter-first, native/WASM deferred.** Unlike generics, a
  closure needs a runtime witness (its environment), so it cannot be fully
  erased; an IR node interpreted the same on all three backends preserves parity
  at minimal risk, and the compiled backends degrade *honestly* by skipping (the
  established mechanism) until a dedicated closure-conversion ticket lands.
- **Capture is the concurrency unlock.** It is the exact primitive
  [[concurrency_design]] names as the prerequisite for passing shared state
  (`Mutex`, a second `Chan`) into a worker, so delivering it here directly
  advances the concurrency roadmap without changing its determinism discipline.
