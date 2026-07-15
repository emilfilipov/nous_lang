# Lullaby Concurrency Model Design — Actors + Intra-Actor Async

**Status:** Design proposal, 2026-07-14. **Stage 1 delivered, 2026-07-15**
(actor core + `spawn` + fire-and-forget `tell`, on the AST interpreter — see
"Stage 1 delivery" below). This document proposes the concrete, buildable shape
of Lullaby's **safe-tier concurrency model**: real parallelism via **actors on a
thread pool**, with **structured `async`/`await` for concurrency *within* a
single actor**.

## Stage 1 delivery (2026-07-15)

Stage 1 of §5.2 — the **actor core** — is implemented and test-locked. Delivered
surface, exactly as the decided syntax in
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md):

- **`actor Name`** blocks with a `state` section (private fields, `name Type`
  like a struct), an optional **`init <params>`** constructor, and one or more
  **`on <handler> <params>`** message handlers. A handler with no `-> T` is a
  fire-and-forget **tell** handler. (A handler *may* be declared with `-> T` — a
  reply/`ask` handler — and its body is type-checked, but `ask` is stage 2, so
  `tell`ing one is rejected.) The three inner section words (`state`, `init`,
  `on`) are **contextual** — recognized only inside an `actor` block — so
  existing code using them as identifiers is unaffected; `actor` and `tell` are
  keywords, and `spawn` stays the contextual form `spawn NAME(...)` (the
  delivered thread `spawn(...)` builtin is unchanged).
- **`spawn NAME(args)`** constructs an actor (zero-initializing `state`, then
  running `init`), schedules it, and yields a typed handle **`Actor<Name>`**. The
  handle is sendable, so actors can address one another.
- **`tell handle.handler(args)`** enqueues a fire-and-forget message and returns
  `void` immediately.
- **Semantics:** the `actor` block, the `Actor<Name>` handle type, `spawn`, and
  `tell` are fully type-checked; **sendability** is enforced (a non-atomic
  `rc<T>`/`ref<T>`/`ptr<T>` message argument is rejected — `L0353` — which is
  what keeps per-actor RC non-atomic); actor `state` is private (no external
  read/write — `L0354`). Diagnostics: `L0348` (actor declaration), `L0349`
  (`spawn`), `L0352` (`tell`), `L0353` (sendability), `L0354` (state privacy /
  actor-as-value).
- **Runtime (AST interpreter):** a real mailbox + single-threaded **cooperative,
  deterministic** scheduler. `spawn` builds an actor with its own state; `tell`
  enqueues; every outstanding message is drained **run-to-completion, one at a
  time (FIFO)** before `main` returns (a graceful drain), so a `tell` with an
  observable side effect produces the same output on every run. One message runs
  at a time and each actor's `state` is touched only by its own handlers, so the
  state is a single-writer resource with no data races.
- **Backends:** actors run on the **AST interpreter only** this stage. The
  IR interpreter and bytecode VM **reject** an actor program with a dedicated
  `L0355` (never silently diverging); the native and WASM backends **cleanly
  skip** it (`L0339`/`L0338`) and it runs on the interpreter — no miscompile.
- **Deferred to later stages (not built at stage 1):** `ask`/`Future`/`await`
  request-reply (stage 2, **now delivered** — see below); move/`shared` message
  semantics + `copy` and the use-after-send analysis (stage 3); supervision/
  failure (stage 4); back-pressure, `try_tell`, `join_all`/`select` (stage 5);
  native/WASM actor codegen (stage 6); and the IR and bytecode interpreters (an
  actor program currently rejects there rather than running).

## Stage 2 delivery (2026-07-16)

Stage 2 of §5.2 — **request-reply** — is implemented and test-locked on the AST
interpreter, exactly as the §1.2–§1.4 surface specifies. Delivered surface:

- **`ask TARGET.HANDLER(args)`** — a request-reply send to an `Actor<T>` handle
  whose handler is declared with a reply type (`on name params -> R`). It
  enqueues a request carrying a one-shot reply slot and evaluates to a
  **`Future<R>`**. `ask` is a keyword, mirroring `tell`; a handler *replies* by
  the ordinary block-value rule — the reply is the handler body's final
  expression, of the declared type `R` (no separate `reply` keyword this stage).
- **`await FUTURE`** — resolves a `Future<R>` to its `R`. The existing `await`
  (from the thread-spawning `async fn` substrate) is reused: it now also accepts
  an actor request-reply future. `await ask c.value()` inline and a stored
  `let f Future<i64> = ask c.value()` then `await f` both work; a `Future<R>` is
  a first-class value (stored in a local, passed, returned).
- **Semantics:** `ask` requires a reply (`-> R`) handler and `tell` a
  fire-and-forget one; the opposite pairing is `L0352`. `ask` arguments obey the
  same sendability rule as `tell` arguments, and a reply handler's `-> R` reply
  type is checked sendable **at the handler declaration** (so a reply can never
  smuggle a non-atomic `rc`/`ref`/`ptr` back to the asker) — both `L0353`. `await`
  typing is unchanged (`Future<R>` → `R`).
- **Runtime (AST interpreter) — fulfillment, ordering, deadlock.** The stage-1
  deterministic single-threaded mailbox is extended with one-shot **reply slots**
  and a **non-reentrant run-to-completion** turn model. `await f` repeatedly runs
  the next *deliverable* message — the earliest queued message whose target actor
  is not already mid-turn — until `f`'s reply slot is filled, then takes it.
  Ordering guarantee: because dispatch is FIFO over deliverable messages on one
  thread, the turn sequence (and every reply and side effect) is identical on
  every run; results are deterministic and tests assert on results, never
  interleavings. An actor is *busy* for the whole span of a turn including nested
  `await`s, so it never runs two turns at once — its `state` stays single-writer.
  **Deadlock:** if an `await` could only be satisfied by re-entering a busy actor
  (an actor asking itself, or a mutual `ask` cycle), no message is deliverable and
  the slot can never fill; this is reported as a clean, deterministic runtime
  error **`L0356`** rather than a hang.
- **Tiers.** Request-reply runs on the **AST interpreter only**, like stage 1.
  Because `ask` reuses the `tell` message-send AST node, the IR/bytecode backends
  reject an `ask` program with the same **`L0355`** gate, native/WASM cleanly skip
  it (**`L0339`/`L0338`**, program-declares-actors), and a `no-runtime` module
  rejects it with **`L0441`** — all inherited, no new tier plumbing.
- **New diagnostic:** `L0356` (request-reply deadlock). Reused: `L0352`
  (send-form mismatch now covers `ask`↔`tell`), `L0353` (now also the reply
  path).
- **Deferred to later stages (unchanged):** move/`shared` + `copy` and
  use-after-send (stage 3); supervision/failure (stage 4); `join_all`/`select`,
  back-pressure/`try_tell` (stage 5); native/WASM actor codegen (stage 6). A
  `Future<R>` is awaited **once** (one-shot); collection/`select`/`race` over many
  futures is stage 5. Cross-actor `async`/`await` fan-out within a turn works
  today via the cooperative mailbox drive (a handler may hold several outstanding
  asks and await them in sequence); the structured `join_all`/`select`
  combinators remain stage 5.

The rest of this document is the original design proposal (the full model); the
above is the slice that is live today.

Canonical language rules: see [core_language_rules.md](core_language_rules.md).
The decided direction and its rationale are fixed in
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md) — this
document designs *within* that decision and does not re-litigate it. Read that
file for the memory model (arena-first, `ref`/RC secondary, raw pointers under
`unsafe`) and the two-tier (safe vs freestanding) identity.

## Relationship to existing concurrency docs (reconciliation)

Two documents already exist and describe a **lower-level substrate**, not the
1.0 safe-tier model this proposal defines:

- [concurrency_design.md](concurrency_design.md) documents runtime-level
  builtins already delivered on the interpreters: `parallel_map`, detached
  `spawn`/`task_join`, `Chan` channels, a `Mutex`, and a thread-spawning
  `async`/`await` over `Future<T>`. These are **primitives**, not the safe
  surface. This proposal treats them as the substrate the actor runtime is built
  on and as the "advanced / escape" API, and reframes intra-actor `async`/`await`
  as a **cooperative** model (see §1.4 and the reconciliation note in §7).
- [concurrency_semantics.md](concurrency_semantics.md) is the older gating note
  that stubbed threading behind `L0212`. It is superseded for the safe-tier
  surface by this proposal; its "structured lifetime / error propagation /
  determinism-for-tests" principles are carried forward here.

Neither of those files is edited by this proposal. A follow-up doc pass (a
dedicated documentation sub-agent) must reconcile them, plus
[roadmap_1_0.md](roadmap_1_0.md), [repository_map.md](repository_map.md), and
[atomics_design.md](atomics_design.md), once the owner accepts a direction here.

## Why actors (one paragraph, not a re-litigation)

Per the canonical decision: actors keep Lullaby's **non-atomic per-actor
reference counting**, its **no-borrow-checker** design, and give
**data-race-freedom by construction**. Each actor owns an isolated, single-
threaded heap/arena; messages are values moved or copied across actor
boundaries; there is no shared mutable state. This means the queued arena + RC
memory work needs **no rework** for concurrency (each actor is effectively a
single-threaded program). The escape hatch is immutable data shared by pointer.
The freestanding `no-runtime` tier has no actor runtime and instead exposes raw
concurrency primitives under `unsafe`.

---

## 1. Surface syntax

All forms below are **indentation-only** — no braces, no semicolons, no colons
after conditions — matching the current implemented surface (`fn add a, b i64 ->
i64`, block `if`/`while`/`for`, `match ... ->`, `struct`/`enum`,
`result<T,E>`/`option<T>`/`?`, `region`, `ref`/`rc`/`ptr`/`unsafe`,
space-separated type annotations).

### 1.1 Declaring an actor

An actor bundles **private state** (isolated heap) with a set of **message
handlers**. It reads like a `struct` with behavior — a shape Lullaby users and
LLMs already know.

```lby
actor Counter
    state
        count i64

    init start i64
        count = start

    on increment by i64
        count += by

    on value -> i64
        count
```

- `state` block: the actor's private fields, declared `name Type` exactly like
  `struct` fields. Only the actor's own handlers may touch them — no external
  reads or writes, ever. This is the single-writer invariant that removes data
  races.
- `init <params>`: the constructor, run once on the spawning caller's request
  before the actor starts consuming messages. It initializes `state`.
- `on <handler> <params> [-> T]`: a message handler.
  - No `-> T`: a **tell** handler (fire-and-forget). Returns `void` to the world.
  - With `-> T`: an **ask** handler; the trailing expression (or explicit
    `reply e`) is the reply value of type `T`.
- Handler bodies are ordinary Lullaby blocks: `if`/`while`/`for`/`match`, local
  `let`, `region`, calls to free functions, etc.

**OWNER DECISION NEEDED — actor declaration form.**

| Option | Shape | Trade-offs |
| :-- | :-- | :-- |
| **A. Dedicated `actor` block** (above) | `actor Name` + `state` + `on ...` | Terse; one construct maps 1:1 to the runtime concept; greppable; LLM-obvious. New keyword surface (`actor`, `state`, `on`). **Recommended.** |
| B. `struct` + `impl Actor` trait | reuse `struct` for state, methods tagged as handlers via a trait | Fewer new keywords; but handlers are indistinguishable from ordinary methods, blurring the "crosses a thread boundary" boundary — the exact hazard actors exist to make visible. |
| C. Actor-as-function with an explicit receive loop | `fn counter ... loop { recv ... }` | Maximally minimal; but hand-rolled receive loops are error-prone, hide the mailbox, and are hard for tiny models to emit correctly. |

**Recommendation: Option A.** A dedicated `actor` block makes the concurrency
boundary syntactically explicit (the whole point of the model), reuses the
`state`/field grammar users know, and gives the compiler a clean node to type-
check sendability and reply types against. The three new keywords (`actor`,
`state`, `on`) are cheap and self-documenting.

### 1.2 Spawning an actor

Spawning constructs an actor, schedules it on the pool, and returns a **typed
handle** — an address, not the actor's memory. The handle is the only way to
reach the actor.

```lby
fn main -> i64
    let c Actor<Counter> = spawn Counter(0)
    tell c.increment(5)
    tell c.increment(3)
    let n i64 = await ask c.value()
    n                                  # 8
```

- `spawn Expr(args)` runs `init` with `args`, places the actor on the scheduler,
  and yields `Actor<T>`.
- `Actor<T>` is a lightweight, **sendable** reference (it can itself be put in a
  message so actors can address one another). It is *not* the actor's heap — it
  carries no RC into the actor's private memory.

**OWNER DECISION NEEDED — spawn keyword vs construction form.**

- **A. `spawn Counter(0)` → `Actor<Counter>`** (recommended): the `spawn` keyword
  reads as "start a concurrent thing", is greppable, and mirrors the existing
  `spawn` builtin's mental model.
- B. `actor Counter(0)` (construction implies spawning): terser, but overloads
  the `actor` keyword between *declaration* and *instantiation*, and hides that a
  scheduler slot and mailbox are being allocated.

**Recommendation: Option A** — keep spawning an explicit verb.

### 1.3 Sending: `tell` (fire-and-forget) and `ask` (request-reply)

Sends must be **visibly distinct from local calls** — a message crosses a heap
and thread boundary and may be copied/moved, so it must not masquerade as an
ordinary method call.

```lby
# tell: enqueue a message, do not wait. Type is void.
tell c.increment(5)

# ask: enqueue a request, get back a Future<T> for the reply.
let f Future<i64> = ask c.value()
let n i64 = await f

# ...or await inline:
let n2 i64 = await ask c.value()
```

- `tell target.handler(args)` — enqueues onto `target`'s mailbox and returns
  immediately (subject to back-pressure, §2.4). Type `void`. Only valid for a
  handler declared without `-> T`.
- `ask target.handler(args)` — enqueues a request carrying a reply address;
  evaluates to `Future<T>` where `T` is the handler's reply type. Only valid for
  a handler declared with `-> T`.
- `await` (see §1.4) resolves a `Future<T>` to its `T`.

**OWNER DECISION NEEDED — send syntax.**

| Option | tell / ask | Trade-offs |
| :-- | :-- | :-- |
| **A. Keyword prefixes** `tell` / `ask` (above) | `tell c.increment(5)` / `ask c.value()` | Reads in English; unmistakably distinct from local calls; greppable; LLM-easy. Two more keywords. **Recommended.** |
| B. Send operators `<-` / `<?` | `c.increment(5) <- 5` / `c <? value()` | Terser; but symbol-soup, hard for tiny models, and `<-`/`<?` collide visually with comparison operators. |
| C. Erlang bang `c ! increment(5)` | `c ! increment(5)` | Idiomatic in actor languages; but `!` is **reserved for the planned error-throw token** (`!0x4c`, see [lullaby_error_handling.md](lullaby_error_handling.md)) — a direct clash. Rejected. |

**Recommendation: Option A.** `tell`/`ask` keep sends greppable and English-
readable, avoid the `!` clash, and make the (a)synchrony of each call site
explicit. This is the single most important LLM-legibility choice in the model.

### 1.4 `async` / `await` for intra-actor concurrency

An actor processes **one message at a time** (run-to-completion per turn — this
is what makes its `state` a single-writer resource with no locks). But a single
handler often needs to fan out several `ask`s, or perform async I/O, without
pinning its worker thread while it blocks. `async`/`await` provides *structured
concurrency inside one turn*.

```lby
actor Aggregator
    state
        db Actor<Db>
        cache Actor<Cache>

    on report id i64 -> Report
        # Fire two asks concurrently, then await both. The worker thread is
        # released while both replies are outstanding — no OS thread is blocked.
        let a Future<Row>   = ask db.row(id)
        let b Future<Meta>  = ask cache.meta(id)
        build_report(await a, await b)
```

Structured combinators (proposed, in the concurrency stdlib module):

```lby
on totals ids list<i64> -> i64
    # join_all awaits a list of futures, preserving order, within this turn.
    let rows list<Row> = join_all(map(fn(i) -> ask db.row(i), ids))
    sum_amounts(rows)
```

- `await e` where `e: Future<T>` suspends the **current handler turn** and
  yields the actor's worker thread back to the pool; when the awaited reply
  arrives the turn is resumed on a pool thread. Type of `await e` is `T`.
- Multiple `Future`s may be outstanding *within one turn* (`a` and `b` above run
  concurrently). This is the "concurrency within an actor".
- `join_all(futures) -> list<T>` and `select(futures) -> (index, T)` are the
  structured combinators; both are scoped to the current turn (no future escapes
  the handler that created it — structured concurrency).

**OWNER DECISION NEEDED — turn model while awaiting (reentrancy).**

The sharp question: while a handler is suspended at an `await`, may the actor
begin processing the **next** message from its mailbox?

- **A. Run-to-completion, non-reentrant (recommended).** The actor does **not**
  start another message until the current turn (including all its `await`s)
  finishes. `state` is therefore never mutated by a second handler mid-turn — the
  simplest possible correctness story, no re-entrancy hazards, trivially data-
  race-free. Cost: an actor that spends a turn awaiting a slow reply does not
  make progress on other messages meanwhile (throughput, not safety, is the
  cost). The worker *thread* is still released to the pool, so other **actors**
  keep running — only this one actor is logically busy.
- B. Reentrant (interleave other messages at `await` points). Higher per-actor
  throughput, but `state` can change across an `await`, reintroducing exactly the
  "shared mutable state under you" hazard actors exist to remove; correctness now
  depends on the author reasoning about interleavings. Rejected for the default.

**Recommendation: Option A** — non-reentrant run-to-completion. It preserves the
by-construction safety guarantee. If a specific actor needs to keep serving
cheap queries while a slow operation is outstanding, the idiom is to **spawn a
child actor** for the slow work and `tell` it, not to make the parent reentrant.

**OWNER DECISION NEEDED — reconciling with the existing thread-spawning
`async`.** [concurrency_design.md](concurrency_design.md) currently implements
`async fn`/`await` by **spawning an OS thread per call**. This proposal's
intra-actor `async`/`await` is **cooperative** (suspend the turn, release the
worker, resume on reply — no new OS thread). Recommendation: keep the *surface*
(`async`/`await`/`Future<T>`) stable and swap the *mechanism* to the cooperative
executor beneath actors, exactly as that doc already anticipates ("the
`async`/`await` surface stays stable when a cooperative executor lands beneath
it"). The thread-spawning version remains available as the low-level substrate /
advanced API.

### 1.5 A complete example program

```lby
# A tiny concurrent word-count pipeline: a reader actor tells lines to a bank
# of counter actors (sharded by hash), then asks each for its subtotal.

actor Shard
    state
        counts map<string, i64>

    init
        counts = map_new()

    on add word string
        let cur i64 = map_get_or(counts, word, 0)
        map_set(counts, word, cur + 1)

    on total -> i64
        let mut sum i64 = 0
        for kv in map_pairs(counts)
            sum += kv.value
        sum

fn shard_for word string, n i64 -> i64
    hash_string(word) % n

fn main -> i64
    let n i64 = 4
    let shards list<Actor<Shard>> = list_new()
    for i from 0 to n - 1
        shards = push(shards, spawn Shard())

    for word in words_of_input()
        let s Actor<Shard> = get(shards, shard_for(word, n))
        tell s.add(word)

    # Ask every shard for its subtotal, concurrently, then sum the replies.
    let subtotals list<Future<i64>> = list_new()
    for i from 0 to n - 1
        subtotals = push(subtotals, ask get(shards, i).total())
    sum_i64(join_all(subtotals))
```

---

## 2. Semantics

### 2.1 Mailbox / queue model

- Each actor owns exactly one **mailbox**: a FIFO queue of pending messages.
  `tell` and `ask` enqueue; the actor's turn loop dequeues one at a time.
- Message ordering: **per-sender FIFO is guaranteed** (messages from actor A to
  actor B are delivered and processed in the order A sent them). No global
  ordering across different senders is promised — that is inherently non-
  deterministic and tests must not assert on it (carrying forward the
  determinism-for-tests discipline from `concurrency_design.md`: assert on
  results, never on interleaving).
- A message is `(handler_id, payload, optional reply_address)`. `ask` messages
  carry a one-shot reply address (the `Future`'s fulfillment slot).

### 2.2 Scheduling on a thread pool

- A fixed-size **worker pool** (default: number of hardware threads) runs a
  work-stealing loop. Each ready actor (mailbox non-empty, not currently
  running) is a schedulable unit.
- **An actor runs on at most one worker at a time** — this is the invariant that
  makes its heap single-threaded. Different actors run on different workers in
  parallel; a single actor never runs concurrently with itself.
- A worker picks a ready actor, runs **one turn** (one message to completion,
  including cooperative `await` suspensions, per §1.4 Option A), then releases
  the actor (re-queuing it if its mailbox is still non-empty) and steals more
  work. This is fair and starvation-free with a simple round-robin/steal policy.
- **Blocking calls** (synchronous file/socket I/O) inside a turn would block a
  pool worker. Policy: such calls are either (a) routed through async I/O that
  integrates with `await`, or (b) run on a separate blocking-I/O thread pool so
  they never starve compute workers. Recommend a dedicated blocking pool for
  synchronous syscalls; async I/O integration is a post-1.0 refinement.

### 2.3 Message ownership: move vs copy vs immutable-share

This is where value semantics + arenas make the model cheap and race-free (see
§3 for the memory-model mechanics). The rule set:

- **Small / scalar / `repr`-sized values** (`i64`, `bool`, `char`, small
  `struct`s of scalars) — **copied** by value into the recipient's arena. This
  is what value semantics already does; nothing new.
- **Owned aggregates** (`list`, `map`, `string`, large `struct`s) — **moved**:
  the value is transferred to the recipient and the **sender loses access**
  (enforced statically, see §3.3). Zero deep-copy; the payload is re-homed into
  the recipient's arena. Move is the default for owned aggregates because it is
  both the fastest and the safest (no aliasing survives the send).
- **Immutable-share** (the escape hatch) — a value explicitly marked shareable
  (`shared<T>`, §3.4) is passed **by pointer**; both actors may read it, neither
  may mutate it. Used for large read-only data (config, lookup tables) that would
  be wasteful to copy per message.

**OWNER DECISION NEEDED — default for owned aggregates: move vs deep-copy.**

- **A. Move by default (recommended).** Sender loses access; zero-copy;
  statically enforced linear use at the send site. Fastest and race-free. Cost: a
  slightly stronger static analysis (the value must be provably not-used-after-
  send), which ties into the escape analysis already planned for arenas.
- B. Deep-copy by default. Sender keeps its value; simpler analysis. Cost: O(size)
  copy on every send of a large payload — a real performance tax in a message-
  heavy program, and the copy is usually pointless because the sender rarely
  needs the value afterward.

**Recommendation: Option A (move by default)**, with an explicit `copy e` at the
send site when the sender genuinely needs to keep the value:
`tell logger.line(copy msg)`. This gives the fast path by default and makes the
rare keep-a-copy case explicit and visible.

### 2.4 What types are sendable

A type is **sendable** iff every value it can hold is safe to hand to another
actor. Concretely:

- **Sendable:** all scalars; `string`; `array<T>`/`list<T>`/`map<K,V>` of
  sendable elements; `struct`/`enum` whose fields/payloads are all sendable;
  `option<T>`/`result<T,E>` of sendable types; `Actor<T>` handles;
  `shared<T>` (immutable-share, §3.4).
- **Not sendable:** `ref<T>` (an actor-local alias/borrow — meaningless in
  another heap); `ptr<T>` (raw pointer — freestanding/`unsafe` only); a plain
  `rc<T>` (per-actor, non-atomic refcount — must not be touched by two threads).
  Sending a non-sendable type is a **compile error** (proposed diagnostic
  **L0350** "type `X` is not sendable across an actor boundary", with a note
  pointing at the offending field/payload).
- Rationale: this is Lullaby's analogue of Rust's `Send`, but it is a
  *structural, compiler-derived* property (no user-written trait), keeping the
  surface terse. The single load-bearing rule is: **non-atomic `rc` and
  actor-local `ref`/`ptr` never cross the boundary** — which is exactly what
  keeps per-actor RC non-atomic (§3.5).

### 2.5 Back-pressure

Unbounded mailboxes are a memory-safety hazard (a fast producer can OOM the
process). Default: **bounded mailboxes with credit-based back-pressure.**

- Each mailbox has a capacity (default proposed: a small constant, e.g. 1024
  messages; tunable per actor at `spawn`).
- When a mailbox is full, `tell` **suspends the sending turn** (cooperatively —
  the sender is itself an actor, so this is an `await` on mailbox capacity, not
  an OS-thread block) until space frees. This propagates back-pressure up the
  pipeline naturally.
- `try_tell target.handler(args) -> bool` is the non-blocking variant: returns
  `false` if the mailbox is full, letting the sender shed load explicitly.

**OWNER DECISION NEEDED — bounded (with blocking `tell`) vs unbounded default.**

- **A. Bounded + back-pressure (recommended):** bounds memory by construction;
  matches the safety-first identity; `tell` can suspend. Slight surprise that a
  "fire-and-forget" `tell` can wait — mitigated by `try_tell` for load-shedding.
- B. Unbounded default: `tell` never waits (simplest mental model), but a
  producer/consumer imbalance silently grows memory without limit — contrary to
  the memory-safety pillar.

**Recommendation: Option A**, bounded with `try_tell` escape hatch. Memory
safety is the moat; an unbounded queue quietly violates it.

### 2.6 Actor lifecycle: spawn, stop, failure

- **Spawn:** `spawn Expr(args)` runs `init`, allocates the mailbox and heap/arena,
  registers with the scheduler, returns `Actor<T>`.
- **Stop (graceful):** an actor stops when (a) it calls `stop` inside a handler
  (finish the current turn, then terminate), or (b) its handle count drops such
  that it is unreachable *and* its mailbox is empty (optional GC-of-actors — see
  open questions), or (c) its supervisor stops it. On stop, the actor's arena is
  bulk-freed (the arena model makes teardown a single reset), and any outstanding
  `ask` futures to it are failed with a "recipient stopped" error.
- **Failure (crash):** a handler that hits an unrecoverable fault (a bounds-check
  panic, an explicit unrecoverable error, an `assert`) **crashes the actor**, not
  the process. Its arena is reclaimed; its supervisor is notified. This is the
  "let it crash" model (§2.7).

### 2.7 Error and supervision behavior

Consistent with [lullaby_error_handling.md](lullaby_error_handling.md): expected,
recoverable failures are modeled **in types** with `result<T,E>`/`option<T>` and
the `?` operator — a handler that can fail *in an expected way* returns
`result<T,E>` and the caller `ask`s and matches on it. **Unexpected** faults
(bounds violation, invariant break, `assert` failure) use the **let-it-crash**
supervision model rather than unwinding through user code.

```lby
# Expected failure: modeled as result, surfaced to the asker.
on withdraw amount i64 -> result<i64, string>
    if amount > balance
        return err("insufficient funds")
    balance -= amount
    ok(balance)
```

Supervision:

- Every actor has a **supervisor** — by default the actor that spawned it (a
  supervision *tree*, rooted at `main`).
- When a supervised actor crashes, its supervisor receives a **failure
  notification** (a system message) and applies a **strategy**:
  - `restart` — discard the crashed actor's state, re-run `init`, keep the same
    `Actor<T>` handle valid (queued messages after the crash point are
    preserved; the in-flight message that caused the crash is dropped).
  - `stop` — terminate the child; fail its outstanding `ask` futures.
  - `escalate` — the supervisor itself fails, propagating up the tree.
- Proposed spawn-site syntax for choosing a strategy (default `stop`):

```lby
let w Actor<Worker> = spawn Worker() supervise restart
```

**OWNER DECISION NEEDED — default supervision strategy + restart backoff.**

- **A. Default `stop`, opt-in `restart` (recommended):** predictable — a crash
  terminates the child and surfaces the failure to its `ask`ers immediately;
  resilience (`restart`) is an explicit, considered choice. Simplest to reason
  about for a 1.0.
- B. Default `restart` (Erlang-style resilience by default): more fault-tolerant
  out of the box, but silent restart loops (a deterministically-crashing actor
  restarting forever) are a real footgun without a backoff/limit policy.

**Recommendation: Option A**, default `stop`. If `restart` is chosen for an
actor, pair it with a restart limit (e.g. "N restarts within T seconds then
escalate") so a poison message cannot spin forever. Getting supervision *policy*
right is worth an explicit owner call — hence the flag.

- **`ask` to a crashed/stopped actor** resolves the `Future` to a failure
  (`result::err` with a "recipient unavailable" `E`, so it composes with `?` and
  `match`), never a hang. This is essential for liveness.

---

## 3. Interaction with the memory model

This model is chosen precisely because it composes with the arena-first memory
model (see [execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md))
with **no rework**.

### 3.1 Per-actor heap / arena

Each actor is a single-threaded program: it gets its own **function/loop
implicit arenas** and may open explicit `region` blocks inside handlers, exactly
as sequential code does. Because only one worker touches an actor at a time,
**none of the arena machinery needs synchronization** — no atomic bump pointer,
no locked reset. The actor's top-level arena is reset when the actor stops or is
restarted (a single bulk free for the whole actor lifetime's non-escaping data).

### 3.2 A message crossing the boundary

When actor A sends a payload to actor B, the payload must move from A's arena to
B's arena (they are different memory regions). Given value semantics:

- **Copy path** (small/scalar values): the value is copied into B's arena at
  delivery — the same copy value semantics already performs on assignment/return.
- **Move path** (owned aggregates, default per §2.3): the payload's backing
  storage is **re-homed** into B's arena and A's binding is statically dead after
  the send. Implementation options: (a) copy-into-B-then-invalidate-A (simple,
  one copy, always correct), or (b) true zero-copy handoff of the allocation when
  A's arena and B's arena can share an allocator page ownership transfer
  (optimization, later). Ship (a) first; (b) is a perf refinement.
- The move/copy happens **at delivery into B's turn**, so B's handler always sees
  the payload living in B's own arena — no cross-arena pointers ever exist inside
  a handler. This is the property that keeps handlers ordinary sequential code.

### 3.3 Why static move-checking is cheap here

The "sender loses access after a move-send" check is a **local, conservative
use-after-send analysis** — the same flavor as the local escape analysis already
planned for arenas (explicitly *not* Tofte–Talpin inference, to protect the
~7 ms fast-compile edge). At a `tell`/`ask` that moves an owned aggregate, the
compiler marks the sender's binding dead; a later use is a compile error
(proposed **L0351** "value moved into an actor message is used after send", with
the fix: `copy` it at the send site or restructure). Default-deny: if the
analysis cannot prove non-use, it requires an explicit `copy` rather than
guessing.

### 3.4 Immutable-share: `shared<T>` (the escape hatch)

For large read-only data that many actors need (a parsed config, a static lookup
table), copying per message is wasteful. `shared<T>` is an **immutable**,
by-pointer share:

```lby
let table shared<LookupTable> = share(build_table())
tell w1.use(table)          # both sends pass the same pointer,
tell w2.use(table)          # no copy — table is read-only in every actor
```

- A `shared<T>` is **deeply immutable** — no actor can mutate through it (a
  mutation attempt is a compile error). Immutability is what makes by-pointer
  sharing race-free without locks.
- Lifetime/reclamation of the shared object is the one place refcounting *could*
  become cross-thread. See §3.5 for why the common path still avoids atomics, and
  the OWNER DECISION on how `shared<T>` is reclaimed.

### 3.5 Why this needs no atomic RC

The per-actor `rc<T>` refcount is only ever touched by the actor's single worker
thread, because **`rc<T>` is not sendable** (§2.4) — it can never be aliased into
a second actor. Therefore per-actor RC stays **non-atomic**, which is the entire
reason actors were chosen over shared-memory threads. The only object reachable
from two threads is `shared<T>`, and it is immutable, so its *contents* need no
synchronization. The single remaining question is reclaiming the `shared<T>`
object itself:

**OWNER DECISION NEEDED — how `shared<T>` is reclaimed.**

- **A. Global immutable region, freed at program exit (recommended for 1.0).**
  `share(...)` allocates into a process-global immutable arena; nothing is freed
  until the program ends. Zero refcount traffic, zero atomics, dead-simple, and
  correct for the overwhelmingly common case (config/tables that live for the
  program's lifetime). Cost: a long-running program that creates and discards
  many distinct shared objects leaks them until exit.
- B. Atomic refcount on `shared<T>` only. Reclaims eagerly, but reintroduces
  atomic RC traffic — on the shared path *only*, never on the per-actor path.
  More runtime complexity; a small perf cost per share/drop.
- C. A separate concurrent reclaimer for the shared region (post-1.0).

**Recommendation: Option A for 1.0**, with the `shared<T>` surface kept stable so
B or C can land underneath later if long-running-server workloads demand eager
reclamation. This keeps 1.0 free of *all* atomic refcounting while preserving the
escape hatch.

---

## 4. Two-tier interaction

### 4.1 Safe tier (default) — the full actor model

Everything in §§1–3: `actor`/`state`/`on`, `spawn`, `tell`/`ask`, intra-actor
`async`/`await`/`join_all`/`select`, bounded mailboxes + back-pressure,
supervision, sendability checking, move/copy/`shared` message semantics. Assumes
the minimal safe-tier runtime (allocator backing arenas, RC helpers, panic→
supervisor, and the **actor scheduler**). This is the safe-tier identity called
out in the canonical doc's coupling notes.

### 4.2 Freestanding tier (`no-runtime`) — raw primitives under `unsafe`

The freestanding tier has **no scheduler, no mailboxes, no actors, no `Future`,
no supervision** — the canonical doc is explicit that "the freestanding tier
drops actors and RC and exposes raw concurrency primitives under `unsafe`", and
that there is **no hidden allocation and no hidden control flow** (a mailbox
enqueue or a refcount op is unacceptable in an interrupt handler). Instead it
exposes, all under `unsafe`:

- **Raw thread / CPU spin-up** intrinsics appropriate to the target (e.g. bring
  up an AP core), not a managed pool.
- **Atomics and memory-ordering intrinsics** — see
  [atomics_design.md](atomics_design.md); load/store/CAS/fetch-add with explicit
  ordering. These are the building blocks a freestanding author uses to hand-roll
  whatever synchronization they need.
- **Spinlocks / raw mutexes** built from those atomics (library code the author
  writes or pulls from a freestanding support module), with no allocator
  dependency.
- **Volatile / MMIO** and **interrupt-handler calling conventions** already on the
  freestanding checklist — the actual hardware concurrency edge (interrupts,
  memory-mapped device queues) lives here.

The relationship mirrors the memory model: the safe-tier actor runtime is *built
on* freestanding primitives (a scheduler is atomics + threads + queues), just as
safe-tier arenas are built on the static-buffer arena primitive. A kernel written
in Lullaby stays arena-safe for most of its logic and drops to these raw
concurrency primitives only at the hardware edge.

### 4.3 The existing low-level builtins

The delivered `spawn`/`task_join`/`Chan`/`Mutex`/thread-`async` builtins
(`concurrency_design.md`) sit **between** the two tiers conceptually: they are
the raw thread/channel substrate the safe-tier actor runtime is implemented on
top of, and they remain available as an advanced/escape API for code that wants
explicit threads and channels rather than actors. The recommended, safe,
1.0-blessed surface is actors; the raw builtins are documented as advanced.

---

## 5. Implementation sketch

**No code is written by this proposal.** This is the subsystem-impact map and a
staged, production-complete increment plan (mirroring the arena staging in
`execution_tiers_and_1_0_scope.md`). Every stage is independently shippable,
fully tested (including negative cases), deterministic (assert on results not
interleavings), and doc-complete before it lands — per the Production Quality
Standard.

### 5.1 Subsystems touched

- **Lexer:** new keywords `actor`, `state`, `on`, `tell`, `ask`, `spawn`,
  `share`, `shared`, `supervise`, `reply`, `stop` (and combinators `join_all`,
  `select` as stdlib names, not keywords). Reserve them with `L0211`-style
  "planned syntax" rejection until each stage lands, so partial rollout never
  silently mis-parses.
- **Parser / AST:** `ActorDecl` (name, `state` fields, `init`, list of `on`
  handlers); handler node (name, params, optional reply type, body); send
  expressions (`Tell`, `Ask` carrying target + handler + args); `spawn`
  expression with optional `supervise` clause; `await`/`share`/`copy`
  expressions. `formal_grammar.md` gets the new productions.
- **Semantics:** actor-type registration; `Actor<T>`/`Future<T>`/`shared<T>` type
  constructors; handler signature checking; reply-type checking (`ask` target
  must have `-> T`; `tell` target must not); **sendability** derivation (L0350);
  **use-after-send** move analysis (L0351), folded into the arena escape pass;
  `await` typing (reuse/extend L0344 from the existing async work); back-pressure
  and supervision are runtime, not type, concerns.
- **IR:** lower `actor`/handlers into a dispatch structure (a per-actor handler
  table + a state struct); lower `tell`/`ask` into `msg_build` + `mailbox_enqueue`
  (+ reply-slot alloc for `ask`); lower `await` into cooperative suspend/resume
  points; lower `spawn` into `actor_create`. Region-enter/reset ops already exist;
  actor turns reuse them.
- **Runtime scheduler (new subsystem):** worker thread pool, per-actor mailbox
  (bounded ring + overflow policy), run-to-completion turn loop, cooperative
  suspension for `await`, work-stealing, supervision tree, blocking-I/O pool.
  Built on the existing `spawn`/thread substrate (`Arc<Program>` share is already
  in place).
- **Interpreters (AST/IR/bytecode):** implement the model end-to-end first (they
  are real Rust programs, so the scheduler is ordinary Rust — the fastest path to
  a correct, testable model, exactly as `parallel_map`/`spawn` landed there
  first).
- **Native backend:** actors lower to calls into runtime-library helpers
  (`__lullaby_actor_create`, `__lullaby_mailbox_enqueue`,
  `__lullaby_actor_yield`, …), following the same "native calls shared runtime
  helpers" pattern the arena work uses (`__lullaby_arena_alloc`). The scheduler
  itself is a runtime library, not emitted per program. Native is a **later
  stage** — parity, not the leading edge.
- **Freestanding tier:** no scheduler; instead the raw atomics/threads intrinsics
  (§4.2), landing with the broader `no-runtime` tier work, not with the safe-tier
  actor stages.

### 5.2 Staged, production-complete increment plan

1. **Actor core (interpreters).** `actor`/`state`/`init`/`on` (tell-only),
   `spawn`, `tell`, bounded mailbox, run-to-completion turn loop on a real worker
   pool. Messages restricted to **copyable** payloads (scalars + value structs).
   Sendability check (L0350). Deterministic result-based fixtures across AST/IR/
   bytecode. *This is the foundation — everything else builds on the turn loop.*
2. **Request-reply.** `ask`, reply typing (`on ... -> T`, `reply`), `Future<T>`,
   `await` (cooperative suspend/resume within a turn), `ask`-to-stopped resolves
   to a failure value. Reconcile intra-actor `async`/`await` onto the cooperative
   executor (surface unchanged from existing async).
3. **Move & share semantics.** Move-by-default for owned aggregates with use-
   after-send analysis (L0351), `copy` opt-out, and `shared<T>`/`share` immutable
   sharing (global immutable region). Ties into the arena escape pass.
4. **Supervision & failure.** Supervision tree, `supervise stop|restart|escalate`,
   restart limits/backoff, crash-isolates-the-actor (arena reclaim on crash),
   failure surfaced to `ask`ers as a `result::err`.
5. **Structured intra-actor concurrency.** `join_all`, `select`, blocking-I/O
   pool integration; back-pressure `try_tell`.
6. **Native parity.** Lower actors/`tell`/`ask`/`await`/`spawn` to runtime-library
   calls in the native backend; the scheduler ships as a runtime library.
7. **Freestanding raw primitives** (with the `no-runtime` tier epic, not here):
   atomics/threads/spinlocks under `unsafe`; no scheduler.
8. **Post-1.0 refinements.** True single-thread cooperative executor tuning,
   zero-copy allocation handoff on move, eager `shared<T>` reclamation, actor
   naming/registry, distributed transport. Surfaces stay stable.

Stages 1–6 are the 1.0 safe-tier concurrency deliverable; stage 7 belongs to the
freestanding epic; stage 8 is explicitly above the 1.0 line (spot optimization /
convenience only, per the canonical doc).

---

## 6. Open questions / risks for the owner

1. **Actor garbage / leak.** When is an actor with no live `Actor<T>` handles and
   an empty mailbox reclaimed? Options: explicit `stop` only (simplest, but leaks
   unreferenced actors), supervisor-driven teardown, or handle-count tracking
   (adds a cross-thread refcount on handles — mild atomic cost). Recommend
   **explicit `stop` + supervisor teardown for 1.0**; revisit handle-GC if it
   proves a footgun.
2. **`shared<T>` reclamation** (§3.5): global-immutable-region-until-exit is
   recommended for 1.0; long-running servers that churn shared objects may need
   eager reclamation (atomic RC or concurrent reclaimer) sooner than post-1.0.
3. **Blocking syscalls in a turn** (§2.2): dedicated blocking pool vs full async
   I/O integration. A blocking pool is enough for 1.0; async I/O is a larger,
   later effort that must not stall the compile-speed or safety goals.
4. **Fairness / priority:** is plain round-robin work-stealing enough, or do some
   messages (system/failure notifications) need priority lanes? Recommend a
   two-lane mailbox (system messages ahead of user messages) so supervision stays
   responsive under load.
5. **Determinism for the test harness:** actor scheduling is non-deterministic;
   all cross-backend fixtures must assert on **results**, never interleavings
   (carried from `concurrency_design.md`). Risk: authors writing order-dependent
   tests. Mitigation: a documented "result-only" testing rule and, optionally, a
   single-worker deterministic scheduler mode for CI reproduction.
6. **Move analysis strength** (§3.3): the conservative use-after-send pass must
   stay cheap to protect the fast-compile edge. Risk: it rejects some safe
   programs (default-deny). Mitigation: `copy` is always an escape hatch, and the
   diagnostic (L0351) names the exact fix.
7. **Keyword budget:** `actor`/`state`/`on`/`tell`/`ask`/`spawn`/`share`/`shared`/
   `supervise`/`reply`/`stop` is a meaningful addition to a deliberately tiny
   keyword set. Confirm the owner is comfortable with this surface, or whether
   some (e.g. `reply`, `stop`) should be builtins/methods rather than keywords.
8. **Reconciling `concurrency_design.md` / `concurrency_semantics.md`**: this
   proposal repositions the existing raw builtins as substrate and reframes
   `async`/`await` as cooperative. That reconciliation (and updates to
   `roadmap_1_0.md`, `repository_map.md`, `atomics_design.md`) is a follow-up doc
   pass, not part of this proposal.

---

## 7. Summary of OWNER DECISION NEEDED forks

| # | Decision | Recommendation |
| :-- | :-- | :-- |
| 1 | Actor declaration form | **Dedicated `actor` block** (`state` + `on`) |
| 2 | Spawn form | **`spawn Counter(0)` → `Actor<T>`** |
| 3 | Send syntax | **Keyword `tell` / `ask`** (avoids `!` error-token clash) |
| 4 | Turn model while awaiting | **Non-reentrant run-to-completion** |
| 5 | Reconcile existing thread-spawning `async` | **Keep surface, swap to cooperative executor** |
| 6 | Owned-aggregate message default | **Move by default**, explicit `copy` to keep |
| 7 | Mailbox bounding | **Bounded + back-pressure**, `try_tell` to shed |
| 8 | Default supervision strategy | **`stop` by default**, opt-in `restart` w/ limit |
| 9 | `shared<T>` reclamation | **Global immutable region until exit** (1.0) |

Each fork is designed so the *surface* stays stable if the owner later reverses
the underlying mechanism — the model can ship on the recommendations and evolve
beneath them.
