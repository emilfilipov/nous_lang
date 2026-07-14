# Platform I/O Primitives Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

Builds on [[stdlib_io_boundary.md]] (the intrinsic/stdlib boundary), extends the
current `sys_status`/`sys_output` and `env`/`args` system builtins, and reuses
the `option<T>`/`result<T, E>` conventions from [[option_result_design.md]].
The broad planned surface lives in [[lullaby_input_output.md]]; this document is
the **implementation-grade** design for the four platform-I/O primitives 1.0
needs: a **time/clock**, an **OS randomness** source, **process
spawn/exec/pipe**, and a **non-blocking I/O / poll** primitive. It maps to the
ClickUp `Lullaby` list **"05 IO Errors Syscalls"** (tickets *Time / clock
primitives*, *OS randomness source*, *Process spawn/exec/pipe primitives*,
*Non-blocking I/O / poll primitive*) and to [[roadmap_1_0.md]] Phase 6.

## Design principles

These four subsystems share one contract, inherited from the I/O boundary doc:

- **Every host syscall is a compiler intrinsic**, not stdlib source. The
  semantic checker knows each name with a fixed signature, type-checks it, and
  lowers it through IR; users cannot shadow it. Anything expressible from
  existing intrinsics (e.g. `sleep_millis` built on the monotonic clock) may
  later ship as runtime stdlib `.lby` source, but the primitives below all need
  a syscall, so they are intrinsics.
- **The AST, IR, and bytecode interpreters call the real OS on each backend**,
  exactly as `read_file`/`sys_status` do today. There is no simulated clock,
  seeded RNG, or fake process table. The native backend lowers to the same
  syscalls. This keeps one authoritative implementation and preserves
  cross-backend parity as a *behavioral* property.
- **Programming errors are compile-time** (`L0312` arity, `L0313` argument
  type); **host/resource failures are runtime `resource` diagnostics** carrying
  a span and traceback, or are surfaced as `result`/`option` values where the
  caller is expected to branch. Predicates never throw.
- **Non-determinism is designed for, not against.** Time and randomness produce
  different values on every run and every backend by definition. The parity
  harness therefore checks **structural invariants** (see
  [Parity and testing](#parity-and-testing)); exact-value behavior is covered by
  CLI round-trip tests, not cross-backend value comparison.

## 1. Time and clock

Two distinct clocks with different guarantees; conflating them is the classic
time bug, so they are separate builtins.

- **Monotonic clock** — for measuring *elapsed time* (durations, timeouts,
  benchmarks). Never runs backwards, unaffected by NTP steps or the user
  changing the wall clock. Its zero point is arbitrary, so only *differences*
  between two reads are meaningful.
- **Wall clock** — for *timestamps* (log lines, file times, "now"). Anchored to
  the Unix epoch (1970-01-01T00:00:00Z), but may jump forward or backward when
  the system clock is adjusted.

### Builtin surface

| Builtin | Signature | Backing syscall | Meaning |
| :-- | :-- | :-- | :-- |
| `mono_nanos()` | `-> i64` | `Instant::now()` vs. process-start baseline | Nanoseconds since an arbitrary fixed baseline; monotonic non-decreasing. |
| `mono_millis()` | `-> i64` | same, divided | Milliseconds since the same baseline. |
| `wall_nanos()` | `-> i64` | `SystemTime::now()` since `UNIX_EPOCH` | Nanoseconds since the Unix epoch (UTC). |
| `wall_millis()` | `-> i64` | same, divided | Milliseconds since the Unix epoch (UTC). |
| `sleep_millis(ms i64)` | `i64 -> void` | `thread::sleep` | Blocks the current thread for at least `ms` milliseconds; a negative or zero `ms` returns immediately. |

Return type is `i64` throughout. `i64` nanoseconds is chosen deliberately: it
overflows only after ~292 years, needs no new numeric type, and matches the
existing all-`i64` native codegen subset so the clocks are native-eligible.
A `duration` alias / struct is intentionally deferred — durations are just the
difference of two monotonic reads (`i64` nanos), and adding a nominal type now
would bloat the primitive without adding safety the type checker can enforce.

The runtime holds a single monotonic baseline `Instant` captured when the
interpreter is constructed (mirroring how `sockets`/`args` are per-runtime
state), so `mono_nanos()` is `now.duration_since(baseline).as_nanos()`
saturated into `i64`. `wall_*` reads `SystemTime::now()` and, on the rare
pre-epoch clock, clamps the elapsed duration to `0` rather than erroring — wall
reads never throw.

### `.lby` examples

```lullaby
fn main -> void
    let start i64 = mono_nanos()
    do_work()
    let elapsed_ns i64 = mono_nanos() - start
    println("elapsed ms: " + to_string(elapsed_ns / 1000000))

fn stamp -> string
    "at epoch-ms " + to_string(wall_millis())
```

### Backend and parity semantics

All four backends read the real OS clock; the native backend lowers `mono_*` to
`QueryPerformanceCounter` (Windows) / `clock_gettime(CLOCK_MONOTONIC)`
(POSIX) and `wall_*` to `GetSystemTimePreciseAsFileTime` / `clock_gettime(
CLOCK_REALTIME)`, converting to the same `i64`-nanos-since-baseline / -epoch
contract. `sleep_millis` lowers to `Sleep` / `nanosleep`. Because clock values
are inherently non-deterministic and time-varying, **parity fixtures are
structural** (see [Parity and testing](#parity-and-testing)), never
value-comparing a timestamp across backends.

### Diagnostics (proposed — not yet in the registry)

Clock reads do not fail on any supported platform, so they need no new runtime
codes; arity/type misuse is the existing `L0312`/`L0313`. One proposed code:

- **`L0430` (resource)** — "clock read failed": reserved for the pathological
  platform where `clock_gettime` returns an error (e.g. a sandbox denying the
  syscall). Interpreters map the OS error to this categorized `resource`
  diagnostic with a span; it should be effectively unreachable in practice.

## 2. OS randomness

A single cryptographically-usable random-bytes builtin backed by the OS CSPRNG.

| Builtin | Signature | Backing syscall |
| :-- | :-- | :-- |
| `os_random(len i64)` | `i64 -> result<list<byte>, string>` | `getrandom(2)` (Linux) / `getentropy` (BSD/macOS) / `BCryptGenRandom` (Windows), with a `/dev/urandom` read as the portable fallback |

`os_random(n)` returns `ok(bytes)` where `bytes` is a `list<byte>` of exactly
`n` cryptographically-strong random bytes, or `err(message)` if the OS entropy
source is unavailable. It reuses the existing `list<byte>` collection type and
the `result` conventions from [[option_result_design.md]], so no new value kind
is needed.

Rules that make this **cryptographically usable, not a toy PRNG**:

- It is **never a seeded, reproducible PRNG.** There is deliberately no
  `seed()`/`srand()` builtin and no way to make `os_random` deterministic. Every
  call draws fresh entropy from the OS. A user who wants a reproducible
  simulation PRNG must build one in Lullaby source seeded from `os_random` — the
  language primitive only ever yields real entropy.
- A `len` of `0` returns `ok([])`; a **negative `len` is a programming error**
  surfaced as `err("os_random length must be non-negative")` (not a panic).
- Large requests are read in a loop until satisfied (the Linux `getrandom`
  syscall caps a single read at 256 bytes), so any `len` up to a sane ceiling
  succeeds; requests above an implementation ceiling (e.g. 64 MiB) return `err`
  rather than allocating unboundedly.

### `.lby` example

```lullaby
fn token_16 -> result<list<byte>, string>
    os_random(16)

fn main -> void
    match os_random(32)
        ok(bytes) -> println("got " + to_string(len(bytes)) + " random bytes")
        err(msg) -> warn("entropy unavailable: " + msg)
```

### Backend and parity semantics

Every interpreter backend calls the OS CSPRNG directly (via a thin, dependency
light wrapper mirroring the `getrandom` crate's platform selection, or that
crate if the workspace later admits the dependency). The native backend lowers
to the same syscall (`BCryptGenRandom` / `getrandom`). Output is
non-deterministic by construction, so parity is **structural**, never
value-comparing bytes across backends.

### Diagnostics (proposed)

- **`L0431` (resource)** — "OS entropy source unavailable": the *internal* error
  category the runtime uses when the CSPRNG syscall itself fails
  (e.g. `/dev/urandom` missing in a broken chroot). Note the *ordinary* failure
  path is the returned `err(string)` value, not a thrown diagnostic; `L0431` is
  reserved for the case where even reporting an `err` is impossible and the
  runtime must raise a categorized resource error with a span.

## 3. Process spawn / exec / pipe

Today `sys_status`/`sys_output` are **fire-and-forget**: they launch a child,
block to completion, and return only the exit code or captured stdout. That
covers "run a tool, read its output" but cannot stream to a long-running child,
interleave reads and writes, or kill a runaway process. This subsystem adds a
**live child-process handle** alongside — not replacing — the existing builtins.

### Handle type and builtin surface

A new opaque value kind `Value::Process(handle)` indexes a per-runtime
`processes` table, exactly mirroring how `Value::Socket(handle)` indexes
`sockets` today (a spawned `std::process::Child` is not `Clone`, so it must live
behind an integer handle). The Lullaby-visible type spelling is `process`.

| Builtin | Signature | Behavior |
| :-- | :-- | :-- |
| `proc_spawn(program string, argv list<string>)` | `string, list<string> -> result<process, string>` | Spawn `program` with `argv`, inheriting the parent environment, stdin/stdout/stderr set to pipes. No shell is invoked. `ok(handle)` on success, `err` if the program is missing/not executable. |
| `proc_spawn_env(program string, argv list<string>, env list<string>)` | `string, list<string>, list<string> -> result<process, string>` | As above but replaces the child environment with the given `"KEY=VALUE"` entries. |
| `proc_write_stdin(p process, data string)` | `process, string -> result<i64, string>` | Write `data` to the child's stdin pipe; returns the byte count written. |
| `proc_close_stdin(p process)` | `process -> result<void, string>` | Close (EOF) the child's stdin so a filter child can finish. |
| `proc_read_stdout(p process)` | `process -> result<string, string>` | Read currently-available stdout (blocking until at least some bytes or EOF); empty string at EOF. |
| `proc_read_stderr(p process)` | `process -> result<string, string>` | As above for stderr. |
| `proc_wait(p process)` | `process -> result<i64, string>` | Block until the child exits; returns its exit code (or `-1` if terminated by signal without a code, matching `sys_status`). |
| `proc_kill(p process)` | `process -> result<void, string>` | Force-terminate the child (`TerminateProcess` / `SIGKILL`). Killing an already-exited child is a no-op `ok`. |

`sys_status`/`sys_output` remain the ergonomic shortcut for the common
"run-and-collect" case and are **unchanged**; they are now specifiable as thin
wrappers over `proc_spawn` + drain + `proc_wait`, but stay intrinsics so
existing programs and native codegen are untouched. The relationship:
`sys_status(p, a)` ≈ `proc_spawn` then `proc_wait`; `sys_output(p, a)` ≈
`proc_spawn` then drain stdout then `proc_wait`.

Handle discipline mirrors sockets exactly: a stale/closed `process` handle
passed to any `proc_*` builtin is a runtime error (the socket doc's
"closed or invalid handle" pattern), and dropping the runtime reaps outstanding
children so a program cannot leak zombies.

### `.lby` example

```lullaby
fn upper(input string) -> result<string, string>
    match proc_spawn("tr", ["a-z", "A-Z"])
        err(m) -> err(m)
        ok(child) ->
            proc_write_stdin(child, input)
            proc_close_stdin(child)
            let out result<string, string> = proc_read_stdout(child)
            proc_wait(child)
            out
```

### Backend and parity semantics

Interpreters back the table with `std::process::Child` and its piped stdio; the
native backend lowers to `CreateProcess`/`CreatePipe` (Windows) and
`posix_spawn`/`pipe`/`waitpid` (POSIX). Process *exit codes* for a **fixed,
deterministic child** (e.g. spawning the platform `true`/`false` or an echo of a
literal) are stable and *are* value-compared across backends in the parity
harness; anything time- or environment-dependent is only checked structurally
(e.g. "a spawned child yields some exit code and its handle round-trips").

### Diagnostics (proposed)

The ordinary failure path is the returned `err(string)`. New categorized runtime
codes for the cases where an `err` cannot be produced or the misuse is
structural:

- **`L0432` (resource)** — "process spawn failed": reserved internal category
  when the spawn syscall fails so hard no `err` value can be constructed
  (the everyday missing-executable case already surfaces as `err`, extending
  today's `L0416` command-launch family).
- **`L0433` (runtime)** — "invalid or closed process handle": a `proc_*` builtin
  received a `process` handle that was never spawned or already reaped
  (mirrors the socket stale-handle error).

## 4. Non-blocking I/O / poll

**Status.** The std-only, portable *core* of this subsystem is **delivered**: a
socket can be switched into non-blocking mode with `set_nonblocking(sock Socket,
enabled bool) -> result<i64, string>`, and non-blocking accept/read/recv builtins
surface a would-block condition as `ok(none)` instead of blocking —
`tcp_accept_nb(listener) -> result<option<Socket>, string>`,
`tcp_read_nb(conn, max i64) -> result<option<string>, string>`, and
`udp_recv_nb(sock) -> result<option<string>, string>`. These run on the AST, IR,
and bytecode interpreters (the backends holding live OS handles) and behave
identically on Windows/Linux/macOS through `std`. They let a single thread drive
many sockets by polling with a short backoff between empty passes — the correct
std-only floor for an event loop, and the 1.0 spanning-set answer for
non-blocking I/O. See `documents/standard_library.md` (Networking).

The **readiness selector below (`poll_*`) is the deliberate follow-up**: it parks
the thread until a socket is ready (epoll/kqueue/IOCP), avoiding poll-with-backoff
for high-fan-out servers. It requires platform syscalls or an external crate, so
it sits outside the std-only spanning set and is scheduled after 1.0. The rest of
this section is the design for that future primitive.

The honest scope: a **minimal, correct, cross-platform readiness primitive**,
not an async runtime. It answers one question efficiently — *of these handles,
which are ready to read/write right now (or within a timeout)?* — so a single
thread can drive many sockets (high-concurrency servers, websocket fan-out)
without a thread per connection. Coroutines, futures scheduling, and an
`await`-driven executor are **out of scope here** and tracked under the
concurrency roadmap; this primitive is the syscall floor they would later build
on.

### Handle / event model

A `poller` is an opaque handle (`Value::Poller(handle)` in a per-runtime
`pollers` table, same pattern as sockets/processes) wrapping the OS readiness
object: **epoll** (Linux), **kqueue** (BSD/macOS), and **IOCP**/`WSAPoll` on
Windows behind one interface. Registered interests are keyed by the existing
`socket` handle (the only pollable resource for 1.0; file/pipe registration is a
later extension), and readiness is reported as a small bitset.

| Builtin | Signature | Behavior |
| :-- | :-- | :-- |
| `poll_new()` | `-> result<poller, string>` | Create an OS readiness set (epoll/kqueue/IOCP). |
| `poll_add(p poller, s socket, interest i64)` | `poller, socket, i64 -> result<void, string>` | Register `s` for readable (`1`), writable (`2`), or both (`3`). |
| `poll_remove(p poller, s socket)` | `poller, socket -> result<void, string>` | Deregister `s`. |
| `poll_wait(p poller, timeout_ms i64)` | `poller, i64 -> result<list<pollevent>, string>` | Block up to `timeout_ms` (negative = infinite, `0` = immediate) and return the ready events; empty list on timeout. |
| `poll_close(p poller)` | `poller -> result<void, string>` | Release the readiness set. |

A `pollevent` is a built-in struct `{ sock socket, readable bool, writable bool }`
so the caller matches ready sockets against its own connection table. Interest
flags are plain `i64` bitmasks (`1` read, `2` write) to stay inside the current
type surface; named constants can be provided later as stdlib.

### `.lby` example (single-thread event loop skeleton)

```lullaby
fn serve(listener socket) -> result<void, string>
    let p poller = expect(poll_new())
    poll_add(p, listener, 1)
    loop
        let events list<pollevent> = expect(poll_wait(p, 1000))
        for i in 0..len(events)
            let ev pollevent = events[i]
            if ev.sock == listener and ev.readable
                let client socket = expect(tcp_accept(listener))
                poll_add(p, client, 1)
            else
                handle_ready(p, ev)
```

(`expect` here denotes unwrapping a `result`; the exact spelling follows
whatever `result`-unwrap helper the prelude settles on.)

### Backend and parity semantics

Interpreters implement the poller over the OS primitive directly (via a thin
platform layer, or the `mio`/`polling` crate if the workspace admits it); the
native backend lowers to the same syscalls. Readiness is inherently
timing-dependent, so the parity harness checks it **structurally**: with two
connected in-process sockets, after writing to one, a `poll_wait` reports the
peer readable; a poll with an empty set and a `0` timeout returns an empty list.
Exact event ordering and wakeup timing are **not** value-compared across
backends.

### Diagnostics (proposed)

- **`L0434` (resource)** — "poll syscall failed": creating the readiness set or
  a `poll_wait` failed at the OS level in a way that cannot be returned as
  `err` (e.g. `epoll_create` EMFILE where even error reporting is degraded).
- **`L0435` (runtime)** — "invalid or closed poller handle": a `poll_*` builtin
  received a handle that was never created or already closed (socket
  stale-handle pattern).

The everyday failures (registering a closed socket, a socket the OS rejects)
surface as the returned `err(string)`, consistent with the socket builtins.

## Parity and testing

Because all four subsystems are non-deterministic, the cross-backend parity
harness asserts **structural invariants**, and exact-value behavior is covered
by **CLI round-trip tests** (which run a real program end to end on one backend
and check its output), never by comparing two backends' values.

Structural parity fixtures (run on AST, IR, and bytecode and asserted equal
*as booleans*):

- **Time:** two successive `mono_nanos()` reads are **non-decreasing**
  (`t1 >= t0`); an elapsed duration `mono_nanos() - start` is **`>= 0`**;
  `wall_millis()` is a large positive value (post-2020 epoch sanity), not that a
  specific timestamp matches.
- **Randomness:** `os_random(n)` yields a list of **length exactly `n`**; two
  successive calls of the same length **differ** (proving it is not a constant
  or a fixed seed); `os_random(0)` is `ok([])`; a negative length is `err`.
- **Process:** spawning a fixed deterministic child (platform `true`/`false` or
  an echo of a literal) yields a **stable exit code / stdout** that *is*
  value-compared across backends; a spawned handle round-trips through
  `proc_wait`; a stale handle errors.
- **Poll:** with two connected in-process sockets, writing to one makes the
  peer report **readable** within a bounded wait; an empty poller with a `0`
  timeout returns an **empty** ready list.

CLI round-trip tests (single backend, real values) cover: a program that times a
loop and prints a plausible elapsed range; a program that prints the length of
`os_random`'s output; a filter pipeline via `proc_spawn`/stdin/stdout; and a
tiny `poll`-driven echo server driven by a real `tcp_connect` client (mirroring
the existing `http_server_round_trip_on_all_backends` pattern). This keeps the
non-deterministic primitives honestly tested without ever pretending two
backends produce identical clocks, entropy, or wakeup timing.

## Fit with the I/O boundary

Every builtin here is an **intrinsic** by the boundary rule in
[[stdlib_io_boundary.md]] (each needs a host syscall or privileged runtime
state). They extend three families already in that document and the runtime:

- clock/random/poll join the standard-stream and file intrinsics as new syscall
  intrinsics;
- `proc_*` generalizes the existing `sys_status`/`sys_output` command family
  from fire-and-forget into a live handle, reusing the `Value::Socket` opaque
  handle pattern (`Value::Process`, `Value::Poller`);
- all use the `result<T, string>`/`option<T>` conventions and per-runtime handle
  tables already established for sockets, so no new value-representation
  machinery is introduced beyond the three opaque handle kinds.

When the dotted `io.*` namespace and the module system land, only the *spelling*
of these names changes (`io.mono_nanos`, `os.random`, `proc.spawn`,
`io.poll_new`); the boundary, signatures, error model, and parity contract
defined here are unaffected.

## Scope and sequencing

**1.0-critical (must ship for the "any program" half of 1.0):**

1. **Time/clock** — smallest, zero-dependency, unblocks timeouts, benchmarks,
   logging, and the `poll` timeout argument. Ship first.
2. **OS randomness** — small and self-contained; unblocks tokens, nonces, TLS
   material, and any Lullaby-level PRNG. Ship alongside time.
3. **Process spawn/exec/pipe** — required for real toolchain/orchestration
   programs and shells; larger surface (handle table, pipes, wait/kill) but a
   direct extension of the existing command builtins.

**Can trail (1.0-desirable, not blocking):**

4. **Non-blocking I/O / poll** — the largest and most platform-divergent
   (epoll/kqueue/IOCP), and the socket subsystem it multiplexes already works
   with a thread-per-connection or bounded-accept model (see the existing
   `http_server` example). It makes servers *scalable*, not *possible*, so it
   can land after the first three without blocking the 1.0 "any program" claim.
   File/pipe pollability and named interest constants are explicit follow-ups.

Within each subsystem the delivery order is: semantic signatures + `L####`
registry entries -> runtime intrinsic (all interpreters) with structural parity
fixtures -> native lowering -> CLI round-trip test -> registry +
this doc + [[repository_map.md]] updated together, per the contributor guide.

## Why these choices

- **Two clocks, not one.** Monotonic-for-durations vs. wall-for-timestamps is
  the single most important correctness distinction in time APIs; exposing one
  clock would guarantee the classic "duration went negative after an NTP step"
  bug. `i64` nanos avoids a new numeric type and stays native-eligible.
- **OS CSPRNG only, never a seeded PRNG.** Making the language primitive always
  cryptographically strong means a user can never accidentally use a predictable
  RNG for security. Reproducible simulation RNGs are a *library* concern built
  *on top of* real entropy, not a language primitive.
- **A live process handle beside the existing shortcuts.** Keeping
  `sys_status`/`sys_output` unchanged preserves every existing program and the
  native codegen path, while `proc_*` adds streaming/kill for the cases the
  shortcuts cannot express — reusing the proven `Value::Socket` handle pattern
  rather than inventing new machinery.
- **A readiness primitive, honestly minimal.** A correct cross-platform
  `poll`/`epoll`/`kqueue`/`IOCP` floor is what real event loops need and is
  achievable now; a full async runtime is a separate, larger design. Scoping to
  readiness (not completion-based async everywhere) keeps the primitive small,
  correct, and portable, and leaves the async executor as a future layer built
  on this floor.
- **Structural parity for non-determinism.** Value-comparing clocks, entropy, or
  wakeup timing across backends would be flaky and wrong; asserting invariants
  (non-decreasing, length-exact, differs-on-repeat, ready-after-write) tests the
  contract that actually matters and keeps the harness deterministic.
