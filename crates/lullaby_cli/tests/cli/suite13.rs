//! CLI integration tests, part 13 — safe-tier failure semantics (decision A5).
//!
//! The safe-tier guarantee (see `documents/lullaby_error_handling.md` and
//! `documents/execution_tiers_and_1_0_scope.md`) splits runtime failure into two
//! disjoint families:
//!
//!   * A **contract / memory-safety violation** (index out of bounds, `pop` of an
//!     empty list, divide-by-zero) is a *bug*. It **aborts** the program with a
//!     clear `L####` diagnostic and a non-zero exit; it does **not** unwind and is
//!     **not** catchable by `try`/`catch`.
//!   * A **modeled / expected failure** flows through `result`/`?`/`throw`/`catch`
//!     and is **recoverable** — the program keeps running to a normal exit.
//!
//! These tests pin BOTH halves and, crucially, assert the same behavior on all
//! three interpreters (`ast`, `ir`, `bytecode`) so no backend can silently drift
//! (e.g. swallow an abort, or return a wrong value instead of aborting).

use crate::*;

/// The three interpreter backends `lullaby run` accepts.
const BACKENDS: [&str; 3] = ["ast", "ir", "bytecode"];

/// Run a fixture through `lullaby run --backend <backend>` and return the output.
fn run_backend(fixture: &str, backend: &str) -> std::process::Output {
    let path = workspace_root().join(fixture);
    lullaby()
        .args([
            "run",
            "--backend",
            backend,
            path.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli")
}

/// Assert a fixture ABORTS with `code` on every interpreter backend: non-zero
/// exit and the diagnostic code on stderr, never a success and never a different
/// code. This is the abort half of the A5 guarantee, checked for consistency.
fn assert_aborts_on_all_backends(fixture: &str, code: &str) {
    for backend in BACKENDS {
        let output = run_backend(fixture, backend);
        let stderr = stderr(&output);
        assert!(
            !output.status.success(),
            "[{backend}] {fixture} should abort, but it exited 0. stderr: {stderr}"
        );
        assert!(
            stderr.contains(code),
            "[{backend}] {fixture} should abort with {code}. stderr: {stderr}"
        );
    }
}

/// Assert a fixture COMPLETES normally (exit 0) on every interpreter backend —
/// the recoverable half of the A5 guarantee. Returns nothing; a modeled failure
/// handled by `catch`/`?` must not be turned into an abort.
fn assert_recovers_on_all_backends(fixture: &str) {
    for backend in BACKENDS {
        let output = run_backend(fixture, backend);
        assert!(
            output.status.success(),
            "[{backend}] {fixture} should recover and exit 0. stderr: {}",
            stderr(&output)
        );
    }
}

// -- Abort family: contract violations abort with a clear diagnostic ----------

#[test]
pub(crate) fn array_index_out_of_bounds_aborts_l0413_on_all_backends() {
    assert_aborts_on_all_backends(
        "tests/fixtures/invalid/array_index_out_of_bounds.lby",
        "L0413",
    );
}

#[test]
pub(crate) fn list_get_out_of_bounds_aborts_l0413_on_all_backends() {
    assert_aborts_on_all_backends("tests/fixtures/invalid/list_get_out_of_bounds.lby", "L0413");
}

#[test]
pub(crate) fn list_set_out_of_bounds_aborts_l0413_on_all_backends() {
    assert_aborts_on_all_backends("tests/fixtures/invalid/list_set_out_of_bounds.lby", "L0413");
}

#[test]
pub(crate) fn pop_empty_list_aborts_l0413_on_all_backends() {
    assert_aborts_on_all_backends("tests/fixtures/invalid/pop_empty_list.lby", "L0413");
}

// -- Native parity: a bounds violation TRAPS in native code, never corrupts ----
//
// The three interpreters abort out-of-range list `get`/`set`/`pop` with `L0413`
// (above). Native `list<T>` get/set/pop now bounds-check the index against the
// list's `len` header and, on violation, emit the same `ud2` bounds-trap array
// indexing uses — a DEFINED illegal-instruction abort (`STATUS_ILLEGAL_INSTRUCTION`
// = `0xC000001D`), never a silent wrong value (exit 0) and never a heap-corrupting
// out-of-bounds read/write (which could access-violate `0xC0000005` or worse,
// succeed with garbage). This is the native half of the A5 safe-tier guarantee.
//
// These emit the fixture native and run it, asserting the clean, defined trap.
// The programs use only the bump heap (no C-runtime import), so they take the
// direct-PE path and need no linker; the run+assert is Windows-only (the default
// `x86_64-pc-windows-msvc` exe is not runnable elsewhere).

/// Emit `fixture` native, run the produced exe, and assert it traps cleanly with
/// `STATUS_ILLEGAL_INSTRUCTION` (`0xC000001D`) — the defined `ud2` bounds-trap —
/// rather than exiting 0 or access-violating. On a non-Windows host the run is
/// skipped (the exe is a Windows PE).
fn assert_native_list_op_traps(fixture: &str, tag: &str) {
    let src = workspace_root().join(fixture);
    let out = std::env::temp_dir().join(format!("lullaby_native_{tag}.exe"));
    let _ = std::fs::remove_file(&out);

    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        emit.status.success(),
        "native emit for {fixture} failed: {}",
        stderr(&emit)
    );

    if !cfg!(windows) {
        eprintln!("non-Windows host; skipping native list-op trap run for {fixture}");
        return;
    }
    assert!(
        out.is_file(),
        "expected a native exe at {} (main must be native-eligible so the bounds check is emitted)",
        out.display()
    );
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe
        .status
        .code()
        .expect("native exit code (Windows returns NTSTATUS)");
    assert_eq!(
        exit, 0xC000_001Du32 as i32,
        "out-of-range native list `{tag}` must trap cleanly with STATUS_ILLEGAL_INSTRUCTION \
         (0xC000001D), not corrupt memory / access-violate (0xC0000005) or silently \
         succeed with a wrong value; got {exit:#010x}"
    );
}

#[test]
pub(crate) fn native_list_get_out_of_bounds_traps() {
    assert_native_list_op_traps(
        "tests/fixtures/invalid/list_get_out_of_bounds.lby",
        "list_get_oob",
    );
}

#[test]
pub(crate) fn native_list_set_out_of_bounds_traps() {
    assert_native_list_op_traps(
        "tests/fixtures/invalid/list_set_out_of_bounds.lby",
        "list_set_oob",
    );
}

#[test]
pub(crate) fn native_pop_empty_list_traps() {
    assert_native_list_op_traps("tests/fixtures/invalid/pop_empty_list.lby", "pop_empty");
}

#[test]
pub(crate) fn divide_by_zero_aborts_l0404_on_all_backends() {
    assert_aborts_on_all_backends("tests/fixtures/invalid/div_by_zero.lby", "L0404");
}

/// A contract violation is NOT catchable: wrapping a divide-by-zero in a
/// `try`/`catch` must still abort with the same `L0404` — only user `throw`s are
/// recoverable. This is the key "no unwinding through a safety abort" assertion.
#[test]
pub(crate) fn abort_is_not_catchable_by_try_catch_on_all_backends() {
    // A `try` body whose divisor is a runtime zero: the catch handler must NOT
    // run, and the program must abort with the div-by-zero diagnostic.
    let source = concat!(
        "fn main -> i64\n",
        "    let zero i64 = len(\"\")\n",
        "    try\n",
        "        10 / zero\n",
        "    catch message\n",
        "        999\n",
    );
    let (dir, base) = fs_temp_dir("a5_uncatchable");
    let path = format!("{base}/uncatchable.lby");
    std::fs::write(&path, source).expect("write temp source");
    for backend in BACKENDS {
        let output = lullaby()
            .args(["run", "--backend", backend, &path])
            .output()
            .expect("run cli");
        let stderr = stderr(&output);
        let stdout = stdout(&output);
        assert!(
            !output.status.success(),
            "[{backend}] a caught div-by-zero must still abort. stderr: {stderr}"
        );
        assert!(
            stderr.contains("L0404"),
            "[{backend}] expected L0404 (division by zero) to escape the catch. stderr: {stderr}"
        );
        // The catch handler's sentinel value must never be produced.
        assert!(
            !stdout.contains("999"),
            "[{backend}] the catch handler ran on a safety abort — it must not. stdout: {stdout}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// An UNCAUGHT `throw` aborts with `L0420` on every backend — the boundary of the
/// recoverable model: `throw` is catchable, but if nothing catches it the program
/// terminates with a clear diagnostic rather than continuing.
#[test]
pub(crate) fn uncaught_throw_aborts_l0420_on_all_backends() {
    assert_aborts_on_all_backends("tests/fixtures/invalid/uncaught_throw.lby", "L0420");
}

// -- Recoverable family: modeled failures keep running -------------------------

/// A caught `throw` and a `?`-propagated `none` both let the program run to a
/// clean exit — proving contract-violation aborts and modeled failures are truly
/// distinct paths. Lullaby has no forced "unwrap" that panics on `none`.
#[test]
pub(crate) fn recoverable_throw_and_question_mark_complete_on_all_backends() {
    assert_recovers_on_all_backends("tests/fixtures/valid/run_recoverable_not_abort.lby");
}

/// The existing `?`-propagation fixture (result + option, success and failure
/// paths folded via `match`) stays a clean exit on every backend — a `none`/`err`
/// flowing through `?` is recovered, never an abort.
#[test]
pub(crate) fn error_propagation_stays_recoverable_on_all_backends() {
    assert_recovers_on_all_backends("tests/fixtures/valid/run_error_propagation.lby");
}
