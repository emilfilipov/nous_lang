# Concurrency Design (Data Parallelism, then Threads and Channels)

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
Supersedes the deferral in [concurrency_semantics.md](concurrency_semantics.md).

Lullaby's interpreters are real Rust programs, so concurrency ships as runtime
builtins backed by the standard library — no native codegen needed. The first
delivered increment is **data parallelism** via `parallel_map`, which runs on
real OS threads while keeping the interpreter's `&Program` borrow (no
`Arc<Program>` refactor) and producing fully deterministic, ordered results.

## First increment (delivered): `parallel_map`

`parallel_map(f fn(i64) -> i64, args list<i64>) -> list<i64>` evaluates `f(arg)`
for every element of `args` concurrently on separate OS threads and returns the
results in the **same order as `args`**, regardless of thread scheduling.

```lby
fn sq x i64 -> i64
    x * x

fn main -> i64
    let base list<i64> = list_new()
    base = push(base, 1)
    base = push(base, 2)
    base = push(base, 3)
    base = push(base, 4)
    let out list<i64> = parallel_map(sq, base)
    let total i64 = 0
    for i from 0 to 3
        total += get(out, i)
    total                     # 1 + 4 + 9 + 16 = 30
```

### Why this is safe without an `Arc<Program>` refactor

`std::thread::scope` lets spawned threads borrow non-`'static` data. The AST and
IR interpreters keep a `&Program`/`&IrModule` borrow; the builtin spawns a scoped
thread per argument, and each thread:

- borrows the same shared `&Program`/`&IrModule` (no clone, no `Arc`),
- builds a **fresh sibling interpreter** (fresh locals and heap), and
- calls the target function by name with the single argument value.

Heaps are per-thread, so there is **no shared mutable state and no locking**.
`Value` is already `Send` (it holds `String`/`Vec`/numbers, no `Rc`), so the
argument values and results cross threads safely. Results are joined and
collected in **input order**, so output is fully deterministic — which is what
makes `parallel_map` safe for the cross-backend parity harness.

### Constraints (first increment)

- `f` must be an ordinary top-level function value (`fn(i64) -> i64`); its name
  must be resolvable in a fresh interpreter. First-class functions do not capture
  their environment, so nothing is closed over.
- The element type is fixed to `i64` (both the argument list and the result).
- Semantics rejects a wrong arity, a non-`fn(i64) -> i64` first argument, or a
  non-`list<i64>` second argument with diagnostic **L0334**.

### Backends and parity

All three backends run `parallel_map` over the same shared program: the AST
interpreter, the IR interpreter (`--backend ir`), and the bytecode VM
(`--backend bytecode`, which round-trips through the IR interpreter) each build
fresh per-thread interpreters. The `run_parallel.lby` fixture returns the same
deterministic `30` on the AST, IR, and bytecode backends and their optimized
variants.

## Second increment (delivered): `spawn`/`task_join`, channels, and a mutex

**Message passing** (share by communicating) with explicit detached threads and
channels, plus a shared `Mutex` for accumulating counters, is now delivered on
all three backends. It required making the program shareable as `Arc<Program>`
(`Arc<IrModule>` for the IR/bytecode backends) so a detached thread can run
Lullaby independently — the structural change the first increment deferred.

```lby
fn worker ch Chan v i64 -> void
    send(ch, v * v)

fn main -> i64
    let ch Chan = chan_new()
    let t1 Task = spawn(worker, ch, 2)
    let t2 Task = spawn(worker, ch, 3)
    task_join(t1)
    task_join(t2)
    recv(ch) + recv(ch)      # 4 + 9 in some order → 13
```

### Delivered builtins

- `chan_new() -> Chan` — create an unbounded channel carrying `i64` values (a
  single channel element type for this increment; a generic `Chan<T>` follows
  once it is proven). A channel is a **shared** handle: cloning the value shares
  the same underlying queue (reference semantics, like `rc<T>`). Built on
  `std::sync::mpsc` — a cloneable `Sender` plus an `Arc<Mutex<Receiver>>`.
- `send(ch Chan, v i64) -> void` — enqueue a value (never blocks; unbounded).
- `recv(ch Chan) -> i64` — dequeue, blocking until a value is available.
- `try_recv(ch Chan) -> option<i64>` — non-blocking; `some(v)` or `none`.
- `spawn(f fn(Chan, i64) -> void, ch Chan, v i64) -> Task` — run `f(ch, v)` on a
  new detached OS thread. (The signature is fixed to `(Chan, i64)` in this
  increment; a more general `spawn` over arbitrary argument tuples follows with
  capturing closures.)
- `task_join(t Task) -> void` — wait for a spawned thread to finish; a second
  `task_join` on an already-joined handle is a harmless no-op. (Named
  `task_join`, not `join`, because `join` is already the string-list joiner
  builtin.)
- `mutex_new(v i64) -> Mutex` — a shared mutex over one `i64` (`Arc<Mutex<i64>>`,
  shared on clone).
- `mutex_get(m Mutex) -> i64` — lock, read, unlock.
- `mutex_set(m Mutex, v i64) -> void` — lock, write, unlock.
- `mutex_add(m Mutex, delta i64) -> i64` — lock, `v += delta`, return the new
  value, unlock (an atomic read-modify-write so worker threads accumulate
  safely).

### How the `Arc<Program>` share works

A detached (non-scoped) thread outlives the `spawn` call, so it cannot borrow the
caller's stack. The top-level entry point wraps the program in an `Arc<Program>`
(`Arc<IrModule>`). The interpreter keeps its existing `&Program`/`&IrModule`
borrow (from `&*arc`, which the `Arc` outlives) for normal use, **and also**
holds an owned `Arc` clone in a field purely to hand to spawned threads — two
separate handles to the same shared data, **not a self-referential struct**.
`spawn` hands a `.clone()` of that owned `Arc` into the thread closure; inside,
the thread builds a fresh interpreter over `&*arc` (per-thread locals and heap)
and calls the target function. `Value` is already `Send`. The public entry
points (`run_main`, `run_bytecode_main`, `run_main_with_args`, …) wrap the
program in an `Arc` internally, so no existing caller changed. No `unsafe`.

### Determinism and testing (delivered increment)

Detached threads are non-deterministic in *scheduling*, so tests are
*order-independent*: the `run_spawn.lby` fixture spawns N workers that each
`send` one value, `task_join`s them, then `recv`s N values and sums them — the
total is deterministic regardless of completion order. It also exercises the
mutex builtins and combines both into a single deterministic `i64` (`34`),
identical on the AST, IR, and bytecode backends. Never assert on interleaving or
per-message order. (In contrast, `parallel_map` is order-deterministic by
construction.)

### Further deferred work

Generic `Chan<T>`; `select` over multiple channels; and thread-pools.
Because `spawn`'s argument shape is fixed to `(Chan, i64)` and
first-class functions do not yet capture their environment, a worker cannot
receive a `Mutex` (or a second channel) directly — passing shared mutable state
into a worker waits on capturing closures or a more general `spawn`. The `Mutex`
itself is `Send` and shared-on-clone, so it works across threads at the runtime
level today (covered by a runtime test). A concurrent **server** additionally
needs cross-thread socket sharing: socket handles are per-interpreter integer
indexes into a runtime-local table, so a `Socket` cannot currently cross a
`spawn` boundary — sharing sockets across threads is a separate follow-up.

## Third increment (delivered): `async`/`await`

`async`/`await` is delivered on all three backends, built directly on the task
layer above rather than on a new reactor.

```lby
async fn square x i64 -> i64
    x * x

async fn cube x i64 -> i64
    x * x * x

fn main -> i64
    let a Future<i64> = square(6)
    let b Future<i64> = cube(3)
    await a + await b        # 36 + 27 -> 63
```

### Semantics

- **`async fn NAME ... -> T`** declares an asynchronous function. *Calling* it
  does not run it to completion inline; it spawns the body on an OS thread and
  immediately returns a **`Future<T>`** handle. Two `async` calls therefore run
  concurrently.
- **`await EXPR`**, where `EXPR` is a `Future<T>`, blocks the current thread
  until that future completes and evaluates to its `T`. Awaiting the same future
  more than once is not required; each `Future<T>` is awaited to obtain its
  value.
- Results are **deterministic** even though scheduling is not: `square(6)` and
  `cube(3)` may finish in any order, but `await a + await b` is always `63`.
  Tests assert on results, never on interleaving.

### How it maps to the task layer

A `Future<T>` is the value-producing sibling of the existing `Task`: calling an
`async fn` spawns a detached thread (over the same `Arc<Program>`/`Arc<IrModule>`
share used by `spawn`) that runs the function body and stores its `Value` result
in a shared result cell; `await` joins that thread and reads the cell. `Value` is
already `Send`, so arguments and results cross threads safely. The AST runtime,
the IR interpreter, and the bytecode VM all lower `async fn`/`Future`/`await`
this way, so `run_async.lby` returns `63` identically on every backend and
optimized variant.

### Typing and diagnostics

Semantics types an `async fn ... -> T` as producing `Future<T>`; `await e`
requires `e: Future<T>` and has type `T`. Awaiting a non-future (an ordinary
value or a synchronous call), or an `await` whose future type cannot be
resolved, is rejected with **L0344**. The native and WASM scalar backends do not
support `Future`/`await` and skip such functions (they still run on the
interpreters).

### Deferred

A true single-threaded cooperative executor (one OS thread multiplexing many
suspended futures), `select`/`join_all` combinators, async channels,
cancellation, and async I/O integration are deferred; the current model gives
real parallelism and correct results by reusing the thread/task layer, and the
`async`/`await` surface stays stable when a cooperative executor lands beneath
it.

## Why these choices

- **Data parallelism first**: `parallel_map` is the smallest safe concurrency
  surface — real OS-thread parallelism, deterministic output, and no interpreter
  refactor, so it lands without risk to the parity harness.
- **Message passing next**: matches value semantics; no data races by
  construction. Explicit thread arguments work today without capturing closures;
  when capture lands, `spawn(f)` over a captured environment becomes the
  ergonomic form and this stays as the primitive.
- **Scoped threads before `Arc<Program>`**: borrow the program for the duration
  of a `parallel_map` call; only pay for the `Arc` share when detached threads
  actually need it.
