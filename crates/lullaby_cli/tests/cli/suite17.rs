//! CLI integration tests, part 17 — the built-in TEST RUNNER (`lullaby test`),
//! road_to_1_0_stable item B3.
//!
//! The runner discovers every top-level zero-parameter, non-generic `test_*`
//! function returning `void`/`i64`/`bool`, runs each through the AST interpreter,
//! prints `PASS`/`FAIL` per test plus an `N passed, M failed` summary, and exits
//! non-zero when any test fails. The declaration surface (the `test_*` name
//! convention) is specified in `documents/language_surface.md`.
//!
//! The load-bearing property pinned here is **failure isolation for runtime
//! errors**: a test that trips an A5 contract violation (bounds fail,
//! divide-by-zero) must NOT terminate the run. A5's "abort without unwinding"
//! governs the NATIVE tier; the runner executes on the AST interpreter, which
//! surfaces every such violation as an ordinary `RuntimeError` returned by
//! `run_named_function`. `test_runner_survives_contract_violations_mid_suite`
//! proves it by pinning a suite whose 2nd and 4th of 5 tests trip a bounds fail
//! and a divide-by-zero and asserting the other three still report, with a
//! correct `3 passed, 2 failed`.
//!
//! The two shapes that are NOT runtime errors — a stack overflow and a
//! non-terminating test — are now contained too, by running the suite in a child
//! process under a per-test deadline. They are pinned in `suite19.rs`, which is
//! where the isolation mechanism itself is tested; this suite stays focused on
//! the runner's surface and its handling of ordinary runtime errors.

use super::{lullaby, stdout, workspace_root};

/// A suite whose 2nd and 4th of 5 tests trip A5 contract violations (a bounds
/// fail and a divide-by-zero) must report BOTH as ordinary failures, still run
/// and report the other three, and summarize `3 passed, 2 failed` with a
/// non-zero exit. This is the isolation guarantee: a violating test cannot kill
/// the run or truncate the report.
#[test]
fn test_runner_survives_contract_violations_mid_suite() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/contract_abort.lby");
    let output = lullaby()
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    // Every test reports, including the three AFTER the first violation.
    assert!(text.contains("PASS test_a_passes"), "stdout: {text}");
    assert!(text.contains("FAIL test_b_bounds"), "stdout: {text}");
    assert!(text.contains("PASS test_c_passes"), "stdout: {text}");
    assert!(text.contains("FAIL test_d_div"), "stdout: {text}");
    assert!(text.contains("PASS test_e_passes"), "stdout: {text}");

    // Each failure carries its own diagnostic reason.
    assert!(
        text.contains("array index `99` is out of bounds"),
        "bounds diagnostic missing: {text}"
    );
    assert!(
        text.contains("division by zero"),
        "div-by-zero diagnostic missing: {text}"
    );

    assert!(text.contains("3 passed, 2 failed"), "stdout: {text}");
    assert!(!output.status.success(), "must exit non-zero on failure");
}

/// Discovery must select exactly the runnable `test_*` functions: non-test
/// helpers produce no output at all, a `testing_helper` name that starts with
/// `test` but not `test_` is NOT a test, and each unrunnable `test_*` function
/// gets a `skip` line naming the reason.
#[test]
fn test_runner_discovers_only_runnable_test_functions() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/discovery.lby");
    let output = lullaby()
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    assert!(text.contains("PASS test_one"), "stdout: {text}");
    assert!(text.contains("PASS test_two"), "stdout: {text}");
    assert!(text.contains("PASS test_three"), "stdout: {text}");

    // Helpers are not tests and must be entirely absent from the report.
    assert!(!text.contains("helper_adds"), "stdout: {text}");
    assert!(!text.contains("not_a_test"), "stdout: {text}");
    // `testing_helper` starts with `test` but not `test_`: not a test.
    assert!(!text.contains("testing_helper"), "stdout: {text}");

    // Unrunnable `test_*` functions are skipped WITH a reason.
    assert!(
        text.contains("skip test_skipped_has_params"),
        "stdout: {text}"
    );
    assert!(
        text.contains("skip test_skipped_bad_return"),
        "stdout: {text}"
    );

    assert!(text.contains("3 passed, 0 failed"), "stdout: {text}");
    assert!(output.status.success(), "passing suite must exit zero");
}

/// A wholly passing suite exits zero with the exact summary.
#[test]
fn test_runner_passes_demo_suite_with_zero_exit() {
    let fixture = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");
    let output = lullaby()
        .arg("test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);
    assert!(text.contains("4 passed, 0 failed"), "stdout: {text}");
    assert!(output.status.success(), "passing suite must exit zero");
}

/// Test order is source-declaration order and is byte-identical across runs.
/// Pinned against the mixed pass/fail fixture so ordering is proven while
/// failures are interleaved, not just on an all-passing suite.
#[test]
fn test_runner_ordering_is_deterministic_across_runs() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/contract_abort.lby");
    let run = || {
        stdout(
            &lullaby()
                .arg("test")
                .arg(&fixture)
                .output()
                .expect("run lullaby test"),
        )
    };
    let first = run();
    let second = run();
    assert_eq!(first, second, "test-runner output must be deterministic");

    // Source-declaration order, explicitly.
    let order: Vec<&str> = first
        .lines()
        .filter_map(|line| line.strip_prefix("PASS ").or(line.strip_prefix("FAIL ")))
        .map(|line| line.split(':').next().unwrap_or(line).trim())
        .collect();
    assert_eq!(
        order,
        vec![
            "test_a_passes",
            "test_b_bounds",
            "test_c_passes",
            "test_d_div",
            "test_e_passes",
        ],
        "stdout: {first}"
    );
}

/// `--filter <substring>` runs only the matching tests, reports how many it
/// filtered out, and stays silent about the rest.
#[test]
fn test_runner_filter_selects_matching_tests() {
    let fixture = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");
    let output = lullaby()
        .arg("test")
        .arg("--filter")
        .arg("arith")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    assert!(text.contains("PASS test_arith"), "stdout: {text}");
    assert!(!text.contains("test_strings"), "stdout: {text}");
    assert!(!text.contains("test_option"), "stdout: {text}");
    assert!(
        text.contains("1 passed, 0 failed, 3 filtered out"),
        "stdout: {text}"
    );
    assert!(output.status.success(), "must exit zero");
}

/// A filter can select a failing test alone; the exit code still reflects it.
#[test]
fn test_runner_filter_can_select_a_failing_test() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/contract_abort.lby");
    let output = lullaby()
        .arg("test")
        .arg("--filter")
        .arg("bounds")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    assert!(text.contains("FAIL test_b_bounds"), "stdout: {text}");
    assert!(!text.contains("test_a_passes"), "stdout: {text}");
    assert!(
        text.contains("0 passed, 1 failed, 4 filtered out"),
        "stdout: {text}"
    );
    assert!(!output.status.success(), "must exit non-zero");
}

/// A filter matching nothing is reported explicitly (not silently "0 passed"),
/// so a mistyped substring is visible. It is not an error: exit stays zero.
#[test]
fn test_runner_filter_matching_nothing_is_reported() {
    let fixture = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");
    let output = lullaby()
        .arg("test")
        .arg("--filter")
        .arg("no_such_test")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    assert!(
        text.contains("no tests matched filter `no_such_test`"),
        "stdout: {text}"
    );
    assert!(text.contains("0 passed, 0 failed"), "stdout: {text}");
    assert!(output.status.success(), "an empty match is not an error");
}

/// `--filter` is rejected with usage when it has no value, is repeated, is
/// empty, or is given to a command other than `test`.
#[test]
fn test_runner_filter_rejects_malformed_use() {
    let fixture = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");

    // Missing value.
    let output = lullaby()
        .arg("test")
        .arg("--filter")
        .output()
        .expect("run lullaby test");
    assert!(!output.status.success(), "missing value must fail");

    // Empty value.
    let output = lullaby()
        .arg("test")
        .arg("--filter")
        .arg("")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    assert!(!output.status.success(), "empty filter must fail");

    // Repeated.
    let output = lullaby()
        .arg("test")
        .arg("--filter")
        .arg("a")
        .arg("--filter")
        .arg("b")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    assert!(!output.status.success(), "repeated filter must fail");

    // Not a `test` flag.
    let output = lullaby()
        .arg("run")
        .arg("--filter")
        .arg("a")
        .arg(&fixture)
        .output()
        .expect("run lullaby run");
    assert!(!output.status.success(), "--filter is test-only");
}

/// A `test_*` function that fails an `assert` reports the assertion message and
/// exits non-zero, and `--verbose` adds a traceback under the failure.
#[test]
fn test_runner_verbose_adds_traceback_to_failures() {
    let fixture = workspace_root().join("tests/fixtures/test_runner/contract_abort.lby");
    let output = lullaby()
        .arg("test")
        .arg("--verbose")
        .arg("--filter")
        .arg("bounds")
        .arg(&fixture)
        .output()
        .expect("run lullaby test");
    let text = stdout(&output);

    assert!(text.contains("FAIL test_b_bounds"), "stdout: {text}");
    assert!(
        text.contains("    at test_b_bounds"),
        "verbose traceback missing: {text}"
    );
    assert!(!output.status.success(), "must exit non-zero");
}
