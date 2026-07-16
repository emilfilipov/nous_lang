//! CLI integration tests, part 19 — TEST-RUNNER ISOLATION (`lullaby test`),
//! road_to_1_0_stable item B3.
//!
//! suite17 pins the runner's *surface* (discovery, filtering, ordering, summary)
//! and its containment of ordinary runtime errors. This suite pins the two
//! failure shapes that are NOT runtime errors and therefore escape the `Result`
//! the interpreter returns:
//!
//! * a **stack overflow** faults on the guard page and terminates the process
//!   running the test — there is no unwinding to catch (which is exactly why
//!   Rust's own libtest can `catch_unwind` a panic and this runner cannot); and
//! * a **non-terminating** test never returns at all.
//!
//! Both used to take the runner down with them: the overflow aborted it with no
//! summary, and the hang ran until someone killed it. They were untestable for
//! precisely that reason — a pin for either would have killed this test binary or
//! stalled CI forever. The runner now runs the suite in a child process under a
//! per-test deadline, so both are contained and both are pinnable, which is what
//! these tests do.
//!
//! The load-bearing assertion in each is the same: the killer test is reported as
//! an ordinary failure, **every other test still runs**, and the summary is still
//! correct with a non-zero exit.
//!
//! Exit codes here are deliberately NOT pinned to a value: a stack overflow is
//! `0xC00000FD` (`STATUS_STACK_OVERFLOW`) on Windows and 127 (or a signal) on
//! POSIX. These tests assert the *observable* — the run survives and reports —
//! never a platform-specific number.

use std::time::{Duration, Instant};

use super::{lullaby, stdout, workspace_root};

/// A test that OVERFLOWS THE STACK (unbounded recursion) as the 2nd of 4 must be
/// reported as a failure, must not prevent the other three from running, and must
/// still produce a correct `3 passed, 1 failed` summary with a non-zero exit.
///
/// Before subprocess isolation this aborted the runner outright: tests 3 and 4
/// never ran and no summary was printed.
#[test]
fn test_runner_survives_a_stack_overflowing_test() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/stack_overflow.lby");
    let output = lullaby()
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    // The overflowing test is reported as an ordinary failure...
    assert!(text.contains("FAIL test_b_overflows"), "stdout: {text}");
    assert!(
        text.contains("terminated abnormally"),
        "the failure must name abnormal termination as the reason: {text}"
    );
    // ...and, crucially, the tests AFTER it still ran.
    assert!(text.contains("PASS test_a_passes"), "stdout: {text}");
    assert!(text.contains("PASS test_c_passes"), "stdout: {text}");
    assert!(text.contains("PASS test_d_passes"), "stdout: {text}");

    // The summary exists at all (it did not, before) and is correct.
    assert!(text.contains("3 passed, 1 failed"), "stdout: {text}");
    assert!(!output.status.success(), "must exit non-zero on failure");
}

/// A NON-TERMINATING test as the 2nd of 4 must trip the per-test deadline, be
/// reported as a timeout failure, and leave the other three to run and summarize.
///
/// The explicit short `--timeout` is what keeps this test cheap: without a
/// deadline anywhere (the old behavior) this fixture would hang CI forever, and
/// with only the 60s default it would cost a minute. It is also the pin that a
/// hanging test *cannot* stall CI: the assertion below bounds the wall clock.
#[test]
fn test_runner_survives_a_non_terminating_test() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/infinite_loop.lby");
    let started = Instant::now();
    let output = lullaby()
        .arg("test")
        .arg("--timeout")
        .arg("2")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let elapsed = started.elapsed();
    let text = stdout(&output);

    assert!(
        text.contains("FAIL test_b_never_terminates"),
        "stdout: {text}"
    );
    assert!(
        text.contains("timed out after 2s"),
        "the failure must name the timeout as the reason: {text}"
    );
    assert!(text.contains("PASS test_a_passes"), "stdout: {text}");
    assert!(text.contains("PASS test_c_passes"), "stdout: {text}");
    assert!(text.contains("PASS test_d_passes"), "stdout: {text}");
    assert!(text.contains("3 passed, 1 failed"), "stdout: {text}");
    assert!(!output.status.success(), "must exit non-zero on failure");

    // A hanging test cannot stall CI: the deadline bounds the whole run. The
    // ceiling is generous (a loaded machine still spawns two children and
    // compiles twice) but far below "forever", which is the property at issue.
    assert!(
        elapsed < Duration::from_secs(45),
        "the timeout must bound the run; took {elapsed:?}"
    );
}

/// Isolation must not cost determinism: the stack-overflow suite — whose report
/// is assembled across TWO child processes, since the first one dies — must still
/// print byte-identical output across runs, in source-declaration order.
#[test]
fn test_runner_crash_recovery_output_is_deterministic_and_ordered() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/stack_overflow.lby");
    let run = || {
        stdout(
            &lullaby()
                .arg("test")
                .arg(&fixture)
                .output()
                .expect("run lullaby test"),
        )
    };
    assert_eq!(run(), run(), "crash-recovery output must be deterministic");

    let first = run();
    let order: Vec<&str> = first
        .lines()
        .filter_map(|line| line.strip_prefix("PASS ").or(line.strip_prefix("FAIL ")))
        .map(|line| line.split(':').next().unwrap_or(line).trim())
        .collect();
    assert_eq!(
        order,
        vec![
            "test_a_passes",
            "test_b_overflows",
            "test_c_passes",
            "test_d_passes",
        ],
        "results must stay in source order across the resumed batch: {first}"
    );
}

/// `--filter` composes with crash recovery: selecting only the overflowing test
/// reports just it, as a failure, with the filtered-out count.
#[test]
fn test_runner_filter_can_select_a_crashing_test() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/stack_overflow.lby");
    let output = lullaby()
        .arg("test")
        .arg("--filter")
        .arg("overflows")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    assert!(text.contains("FAIL test_b_overflows"), "stdout: {text}");
    assert!(!text.contains("test_a_passes"), "stdout: {text}");
    assert!(
        text.contains("0 passed, 1 failed, 3 filtered out"),
        "stdout: {text}"
    );
    assert!(!output.status.success(), "must exit non-zero");
}

/// `--timeout` is rejected with usage when it has no value, is non-numeric, is
/// repeated, or is given to a command other than `test`.
#[test]
fn test_runner_timeout_rejects_malformed_use() {
    let fixture = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");

    // Missing value.
    let output = lullaby()
        .arg("test")
        .arg("--timeout")
        .output()
        .expect("run lullaby test");
    assert!(!output.status.success(), "missing value must fail");

    // Non-numeric value.
    let output = lullaby()
        .arg("test")
        .arg("--timeout")
        .arg("soon")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    assert!(!output.status.success(), "non-numeric timeout must fail");

    // Repeated.
    let output = lullaby()
        .arg("test")
        .arg("--timeout")
        .arg("5")
        .arg("--timeout")
        .arg("6")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    assert!(!output.status.success(), "repeated timeout must fail");

    // Not a `test` flag.
    let output = lullaby()
        .arg("run")
        .arg("--timeout")
        .arg("5")
        .arg(&fixture)
        .output()
        .expect("run lullaby run");
    assert!(!output.status.success(), "--timeout is test-only");
}

/// A passing suite still passes under isolation, and `--timeout` does not
/// interfere with a test that terminates normally well inside the deadline.
#[test]
fn test_runner_passing_suite_is_unaffected_by_the_deadline() {
    let fixture = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");
    let output = lullaby()
        .arg("test")
        .arg("--timeout")
        .arg("30")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);
    assert!(text.contains("4 passed, 0 failed"), "stdout: {text}");
    assert!(output.status.success(), "passing suite must exit zero");
}
