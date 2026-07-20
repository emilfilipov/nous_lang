//! Shared execution scaffolding for every interpreter tier (AST, IR, bytecode):
//! the large evaluation stack the interpreters run on, and the uniform call-depth
//! bound that turns unbounded recursion into a clean diagnostic instead of a host
//! stack overflow.
//!
//! # Why the interpreters run on a dedicated thread
//!
//! The three interpreters are recursive tree-walkers: evaluating a Lullaby
//! function call descends through several nested Rust stack frames (statement →
//! expression → call dispatch → `invoke_function` → statement …). A moderately
//! deep but perfectly well-defined recursive program therefore consumed the host
//! thread's default stack and aborted the whole process with
//! `STATUS_STACK_OVERFLOW` (`0xC00000FD`) — a hard, uncatchable OS abort, not a
//! Lullaby diagnostic — where the native backend ran the same program correctly.
//! Worse, the three tiers have different Rust frame footprints, so they overflowed
//! at *different* depths: above a few hundred frames they disagreed with each other
//! and with native, which silently blinded the differential fuzzers (they treat the
//! interpreters as the oracle) to any program recursing past that ceiling.
//!
//! [`run_on_interpreter_stack`] runs an interpreter's whole evaluation on a freshly
//! spawned thread with a large ([`INTERPRETER_STACK_SIZE`]) stack, so every tier can
//! recurse far past the old ceiling and agree with native on deep recursion. It is
//! applied uniformly at every interpreter *entry point* (the `run_main*` /
//! `run_named_function` functions of all three tiers), which is also the in-process
//! path the fuzzers invoke — so the oracle now covers deep recursion for free.
//!
//! # Why there is also a depth bound
//!
//! A bigger stack only moves the ceiling; a genuinely unbounded recursion would
//! still eventually exhaust even a gigabyte stack and abort the process — and again
//! at a tier-dependent depth. [`INTERPRETER_RECURSION_LIMIT`] is a single shared
//! bound, checked identically by all three interpreters at each user-function /
//! closure invocation, that raises the catchable [`recursion_limit_error`]
//! (`L0466`) *before* the large stack can overflow. The bound is deliberately far
//! below the depth at which the shallowest tier (the AST interpreter, which has the
//! largest per-call frame) would exhaust [`INTERPRETER_STACK_SIZE`], so the
//! diagnostic always wins the race against the OS abort. The result: a terminating
//! recursion returns the same value on every tier; an unbounded one ends with the
//! same clean `L0466` on every interpreter tier, never a bare host abort that
//! differs by tier.

use crate::RuntimeError;

/// The stack size, in bytes, of the dedicated thread every interpreter tier runs
/// its evaluation on. 2 GiB of address space is *reserved* here; physical memory is
/// committed lazily by the OS only as the stack actually grows, so an ordinary
/// shallow program pays essentially nothing for it. It is sized generously so that
/// [`INTERPRETER_RECURSION_LIMIT`] frames of the most stack-hungry tier fit with a
/// wide safety margin **in an unoptimized `debug` build**, where per-call frames are
/// several times larger than in `release` — the worst case, and the one the test
/// suite runs under (see the module docs).
pub const INTERPRETER_STACK_SIZE: usize = 2 << 30; // 2 GiB

/// The maximum Lullaby call depth (nested user-function / closure invocations) any
/// interpreter tier will enter before raising [`recursion_limit_error`]. Shared by
/// all three tiers so they agree exactly on where unbounded recursion stops.
///
/// Chosen to sit far below the depth at which the AST interpreter — the tier with
/// the largest per-call Rust frame — would exhaust [`INTERPRETER_STACK_SIZE`] in a
/// `debug` build (measured: the interpreters clear 20 000 frames on this stack with
/// multiple-fold headroom even unoptimized), so the clean `L0466` diagnostic always
/// wins the race against the OS stack overflow, on both `debug` and `release`. It is
/// simultaneously deep enough — comparable to the native backend's own call-stack
/// capacity — that every realistic terminating recursion runs to completion and the
/// differential fuzzers can exercise recursion two orders of magnitude past the
/// few-hundred-frame ceiling that used to crash the interpreters. Beyond it the
/// interpreter refuses cleanly while native may keep going: an acceptable
/// correct-or-refuse divergence, not a silent wrong answer.
pub const INTERPRETER_RECURSION_LIMIT: usize = 20_000;

/// The error every interpreter raises when a program's call depth reaches
/// [`INTERPRETER_RECURSION_LIMIT`]. `L0466` is a catchable runtime error (an
/// enclosing `try`/`catch` can observe it) rather than a host-process abort, and is
/// identical across the AST, IR, and bytecode tiers.
pub fn recursion_limit_error() -> RuntimeError {
    RuntimeError::new(
        "L0466",
        format!(
            "recursion limit exceeded: a Lullaby call nested more than {INTERPRETER_RECURSION_LIMIT} \
             frames deep on the interpreter. This bound turns unbounded recursion into a clean \
             diagnostic instead of a host stack overflow; if the recursion is intended to go this \
             deep, rework it iteratively (a loop with an explicit stack) or compile with \
             `lullaby native`, which uses the operating system's own call stack"
        ),
    )
}

/// Run `f` — an interpreter tier's entire evaluation — on a freshly spawned thread
/// with an [`INTERPRETER_STACK_SIZE`] stack, returning its result. A panic inside
/// `f` (an interpreter bug, or a deliberate `panic!`) is propagated to the caller
/// unchanged via [`std::panic::resume_unwind`], so this wrapper is transparent to
/// panic behavior: the process still aborts on an interpreter panic exactly as it
/// did before, only now the panic surfaces on the calling thread.
///
/// A scoped thread is used so `f` may borrow non-`'static` data from the caller
/// (the interpreter entry points build their runtime over a borrowed program); the
/// scope join happens before `run_on_interpreter_stack` returns, so nothing escapes.
pub fn run_on_interpreter_stack<F, T>(f: F) -> T
where
    F: FnOnce() -> T + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(INTERPRETER_STACK_SIZE)
            .name("lullaby-interp".to_string())
            .spawn_scoped(scope, f)
            .expect("spawn interpreter evaluation thread")
            .join()
            .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
    })
}

/// Spawn a detached interpreter thread with an [`INTERPRETER_STACK_SIZE`] stack,
/// returning its [`std::thread::JoinHandle`]. Used for the concurrency builtins
/// (`spawn`, `async fn`) whose worker runs a fresh interpreter on its own thread:
/// that worker must have the same large stack as the main evaluation, or a deeply
/// recursive `async`/`spawn`ed body would overflow the OS thread's default stack —
/// exactly the host abort the depth bound and large stack exist to prevent.
pub fn spawn_interpreter_thread<F, T>(f: F) -> std::thread::JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(INTERPRETER_STACK_SIZE)
        .name("lullaby-interp".to_string())
        .spawn(f)
        .expect("spawn interpreter worker thread")
}
