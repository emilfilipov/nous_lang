# Concurrency Primitive Semantics

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This note specifies the intended runtime semantics for concurrency primitives
and decides which are in the alpha subset versus documented stubs. The broader
planned surface (thread pools, subprocess piping, async I/O) lives in
[lullaby_input_output.md](lullaby_input_output.md). This document is the
concurrency semantics deliverable those tickets require.

## Design Stance

Concurrency is **not in the alpha executable subset.** The alpha runtime is a
single-threaded AST/IR/bytecode interpreter, and shipping unsound threading
primitives would violate the project's memory-safety goal before lifetime and
sharing analysis exist. Instead:

- The primitives below have **defined semantics** here so the surface is stable.
- Any concurrency keyword or builtin that appears in source is **rejected with a
  clear diagnostic** rather than silently accepted or partially executed.

## Alpha Subset vs Stubs

| Primitive | Intended semantics | Alpha status |
| :-- | :-- | :-- |
| `spawn_thread(fn, args)` | Start `fn` on a new OS thread; returns a thread handle. | Stub — rejected |
| `wait(thread)` | Block until the thread finishes; propagate its result or error. | Stub — rejected |
| `create_mutex()` / `lock` / `unlock` | Mutual-exclusion handle guarding a critical section; unlock on scope exit. | Stub — rejected |
| `async fn` | A function whose call returns a task instead of a value. | Stub — rejected |
| `await expr` | Suspend until the task completes; yield its value or propagate its error. | Stub — rejected |
| `spawn_task(fn, args)` | Schedule `fn` on the async executor; returns a task handle. | Stub — rejected |
| `await_all(tasks)` | Await a collection of tasks; return their values in order. | Stub — rejected |

"Rejected" means the parser/semantic layer emits a diagnostic (reserved code
`L0212` "concurrency primitive is not available in this release", in the same
family as `L0211` planned-syntax rejection) with a source span and a note that
the primitive is planned. This keeps the acceptance guarantee: **unsupported
concurrency features produce clear diagnostics, never partial behavior.**

## Intended Semantics (for when the subset opens)

- **Memory sharing.** Threads may share only values that are provably safe to
  share; enforcement depends on the pointer/reference and lifetime analysis
  tickets. Until those land, no sharing is permitted, which is another reason the
  primitives stay stubbed.
- **Structured lifetime.** A spawned thread/task must be joined (`wait`/`await`)
  within the scope that spawned it; an un-joined handle at scope exit is a
  compile-time error. This keeps cleanup ordering deterministic and matches the
  region/lifetime model.
- **Error propagation.** A thread/task that fails carries its `N####` runtime
  error to the joining site; `wait`/`await` re-raise it with the original span
  plus a "raised in worker" traceback frame.
- **Cancellation.** Not modeled in the first cut; a joined worker always runs to
  completion. Cooperative cancellation is a later addition.
- **Determinism for tests.** The first executor is a deterministic, single-worker
  scheduler so async tests are reproducible before real parallelism is enabled.

## Sequencing

1. Land pointer/reference + lifetime analysis (prerequisite for safe sharing).
2. Open `spawn_thread`/`wait` with the no-sharing rule and structured join.
3. Add `create_mutex` and guarded sharing once sharing analysis exists.
4. Add `async`/`await`/`spawn_task`/`await_all` on the deterministic executor.
5. Add real multi-worker scheduling and cancellation last.

## Non-Goals For This Note

- Actual thread/executor implementation (this note is semantics + gating only).
- Lock-free data structures, atomics, and memory-ordering primitives.
- Distributed or cross-process concurrency.
