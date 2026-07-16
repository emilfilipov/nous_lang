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
//! A third shape is pinned alongside them: a test that **spawns a long-running
//! grandchild**. `sys_status`/`sys_output`/`proc_spawn` are ordinary builtins and
//! nothing gates builtins by entry point, so a test can spawn real processes — and
//! a grandchild outlives a killed child *and* inherits its stderr pipe handle. The
//! first cut of the runner killed only the child and waited on that pipe, so
//! `--timeout 3` actually took 14s, scaling linearly with the grandchild: the
//! deadline bounded nothing. That test asserts a **wall-clock bound**, because the
//! `FAIL` line printed on schedule even while the run was unbounded — the report
//! alone cannot catch it.
//!
//! A fourth shape is pinned here too, and it took three attempts to get right: a
//! test that **forges protocol lines**. Every escape came through the OS rather
//! than the language, which is why each was invisible to a green
//! `cargo test --all`:
//!
//! * the protocol rode on the child's **stderr** behind a nonce in `argv` — but
//!   `warn()` writes to stderr, and a process may read its own command line, so the
//!   nonce was never secret; and
//! * moving it to a private pipe in the **stdin slot** still failed, because
//!   `proc_spawn` spawns with stdin inherited, so a **grandchild** received a
//!   writable handle and `>&0` forged a line — no builtin names a descriptor, the
//!   OS simply hands one over.
//!
//! The child now takes the descriptor out of the stdin slot and reopens that slot
//! onto the null device before running anything, so the channel has no name a
//! program can reach and no slot a child can inherit. Three tests pin it: the
//! end-to-end grandchild forgery (the only kind that would have caught these
//! defects), the direct `println`/`warn` forgery, and the structural condition that
//! no protocol verb reaches stdout/stderr. The last is necessary but NOT
//! sufficient — treating it as sufficient is exactly what let the grandchild route
//! through.
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

use super::{lullaby, stderr, stdout, workspace_root};

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

/// A test that SPAWNS A LONG-RUNNING GRANDCHILD must not outlive the deadline.
///
/// `sys_status`/`sys_output`/`proc_spawn` let a `test_*` function spawn real
/// processes — nothing gates builtins by entry point. A grandchild outlives a
/// killed child AND inherits its stderr pipe handle, so killing only the child
/// left the runner waiting on an EOF the grandchild held: measured at 14s against
/// a `--timeout 3`, tracking the grandchild's lifetime linearly (`ping -n 60` ->
/// 60s). `--timeout N` meant nothing. The fix is to kill the process TREE.
///
/// The wall-clock bound is the assertion that matters — the `FAIL` line alone
/// printed on schedule even while broken, so only elapsed time can catch this.
/// It also pins the tree-kill end-to-end rather than just the deadline:
/// `Command::output()` reads the child's pipes to EOF, so a surviving grandchild
/// holds this test open past the bound even if the runner itself moved on.
#[test]
fn test_runner_timeout_bounds_a_test_that_spawned_a_grandchild() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/spawns_grandchild.lby");
    let started = Instant::now();
    let output = lullaby()
        .arg("test")
        .arg("--timeout")
        .arg("3")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let elapsed = started.elapsed();
    let text = stdout(&output);

    assert!(
        text.contains("FAIL test_a_spawns_a_long_grandchild"),
        "stdout: {text}"
    );
    assert!(text.contains("timed out after 3s"), "stdout: {text}");
    assert!(text.contains("PASS test_b_passes"), "stdout: {text}");
    assert!(text.contains("PASS test_c_passes"), "stdout: {text}");
    assert!(text.contains("2 passed, 1 failed"), "stdout: {text}");
    assert!(!output.status.success(), "must exit non-zero on failure");

    // The load-bearing assertion. The grandchild runs ~15s; the deadline is 3s.
    // A run that waits for the grandchild lands at 14s+, so this bound separates
    // "bounded by our deadline" from "bounded by whatever the test spawned" with
    // room for a loaded machine (two spawns + two compiles) in between.
    assert!(
        elapsed < Duration::from_secs(10),
        "--timeout must bound the run regardless of what a test spawned; took {elapsed:?}"
    );
}

/// A test that FORGES PROTOCOL LINES on stdout and stderr must be completely
/// inert — no truncated run, no phantom PASS.
///
/// `warn()` writes straight to process stderr and `println()` to stdout, and
/// nothing gates builtins by entry point. An earlier design put the protocol on
/// the child's stderr and authenticated it with a per-run nonce passed in argv —
/// unsound, because a process may read its own command line, so the "secret" was
/// handed to the attacker by the OS. A forged `done` truncated the run to a green
/// `0 passed, 0 failed` + exit 0, and a forged `pass` invented a phantom PASS for
/// a failing test.
///
/// The protocol now travels on a private pipe no builtin can write to, so this is
/// closed by construction rather than by secrecy. The exit code is the sharpest
/// assertion: `test_c_fails` genuinely fails, so a run that reports success has
/// been successfully lied to.
#[test]
fn test_runner_ignores_forged_protocol_lines_from_a_test() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/forges_protocol.lby");
    let output = lullaby()
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    // The forged `done` must not truncate: every test still runs and reports.
    assert!(
        text.contains("PASS test_a_forges_protocol"),
        "stdout: {text}"
    );
    assert!(text.contains("PASS test_b_passes"), "stdout: {text}");
    // The forged `pass 2` must not turn a genuinely failing test green.
    assert!(text.contains("FAIL test_c_fails"), "stdout: {text}");
    assert!(text.contains("assertion failed"), "stdout: {text}");

    // The tally is uncorrupted, and the run is NOT silently green.
    assert!(text.contains("2 passed, 1 failed"), "stdout: {text}");
    assert!(
        !text.contains("0 passed, 0 failed"),
        "a forged `done` truncated the run: {text}"
    );
    assert!(
        !output.status.success(),
        "a test forged its way to a green run: {text}"
    );
}

/// A test that forges protocol lines THROUGH A GRANDCHILD must be inert too.
///
/// This is the end-to-end pin, and it is the one that matters: it is the only
/// test here that would have caught any of this feature's three blocking defects,
/// all of which passed a fully green `cargo test --all`. Each escaped through the
/// **OS**, not the language — a grandchild outliving the deadline, `argv` handing
/// over a nonce, and here process inheritance handing over a descriptor.
///
/// The protocol pipe's write end is given to the test process in its stdin slot,
/// and `proc_spawn` spawns with stdin **inherited** — so a grandchild receives a
/// writable handle to the channel and `>&0` injects a forged line. No builtin
/// names a descriptor; the OS hands one over regardless. Unfixed, this produced a
/// green run for a suite containing `assert(false)` (`3 passed, 0 failed`, exit 0)
/// or a corrupted tally (`5 passed, 1 failed` from three tests).
///
/// Note this defeats validating `done` against `last_reported`: the forger reports
/// every index first, so `done` then completes the batch legitimately. Only the
/// channel's unreachability makes the protocol trustworthy — which is why the fix
/// reopens the stdin slot onto the null device rather than patching a spawn site.
#[test]
fn test_runner_ignores_protocol_forged_through_a_spawned_grandchild() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/forges_via_spawn.lby");
    let output = lullaby()
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    // A genuinely failing test must still be reported as failing...
    assert!(text.contains("FAIL test_c_fails"), "stdout: {text}");
    assert!(text.contains("assertion failed"), "stdout: {text}");
    // ...and must never appear as a phantom PASS.
    assert!(
        !text.contains("PASS test_c_fails"),
        "a forged `pass` invented a PASS for a failing test: {text}"
    );

    // The tally is exactly the three real tests — not inflated by forged results.
    assert!(
        text.contains("2 passed, 1 failed"),
        "the tally was corrupted by forged protocol lines: {text}"
    );
    assert!(
        !output.status.success(),
        "a test forged its way to a green run through a grandchild: {text}"
    );
}

/// A test's `read_line` sees a clean EOF, not an error.
///
/// The child reopens its stdin slot onto the null device after taking the protocol
/// descriptor out of it, so stdin behaves exactly as it did when the runner passed
/// `Stdio::null()`. Pinned because the reclaim-only design left a *write* handle in
/// the slot, and a stdin-reading test then failed with `Access is denied`
/// (`L0419`) — harmless but a real behavior change that the reopen removes.
#[test]
fn test_runner_gives_tests_a_readable_null_stdin() {
    let dir = std::env::temp_dir().join("lullaby_cli_test_stdin_eof");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let fixture = dir.join("stdin_eof.lby");
    std::fs::write(
        &fixture,
        "fn test_a_reads_stdin -> void\n    \
         match read_line()\n        \
         some(s) -> assert(false)\n        \
         none -> assert(true)\n",
    )
    .expect("write fixture");

    let output = lullaby()
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    assert!(
        text.contains("PASS test_a_reads_stdin"),
        "read_line must see a clean EOF (`none`): {text}"
    );
    assert!(text.contains("1 passed, 0 failed"), "stdout: {text}");
    assert!(output.status.success(), "stdout: {text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A NECESSARY condition for the two tests above, pinned directly: the protocol is
/// not carried on the child's stdout or stderr.
///
/// This catches a regression that puts the protocol back on a stream a Lullaby
/// program can `println`/`warn` onto. It is deliberately NOT the whole guarantee —
/// believing it was is what let the grandchild forgery through. A test reaches the
/// channel by two distinct routes: **writing** to a stream it shares (what this
/// pins), and **inheriting** a descriptor from the process that holds it (what
/// `test_runner_ignores_protocol_forged_through_a_spawned_grandchild` pins). Both
/// are needed; neither implies the other.
#[test]
fn test_runner_protocol_never_touches_the_shared_streams() {
    let fixture = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");
    // Drive the internal batch child directly, with its stdin (the protocol slot)
    // pointed at nothing, and capture both shared streams.
    let output = lullaby()
        .arg("__run-test-batch")
        .arg(&fixture)
        .arg("0")
        .arg("0")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run the internal batch child");

    let streams = format!("{}{}", stdout(&output), stderr(&output));
    for verb in ["start ", "pass ", "fail ", "done "] {
        assert!(
            !streams.contains(verb),
            "protocol verb `{verb}` leaked onto a stream a test can write to \
             (`println`/`warn` reach stdout/stderr). This is ONE of two routes to \
             the channel — the other is inheriting the descriptor from the process \
             that holds it, pinned by \
             `test_runner_ignores_protocol_forged_through_a_spawned_grandchild`. \
             streams: {streams}"
        );
    }
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
