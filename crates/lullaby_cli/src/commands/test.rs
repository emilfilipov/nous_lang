//! `lullaby test` — the built-in language-level test runner.

use std::path::PathBuf;
use std::time::Duration;

use lullaby_parser::Program;

use crate::args::OutputMode;
use crate::commands::test_isolate::run_isolated;
use crate::compile::{SourceMode, compile};
use crate::diagnostics::format_reports;

/// Discover the runnable tests in a validated program, in deterministic order:
/// source-declaration order within a file, loader merge order across modules.
///
/// A test is a top-level function whose name starts with `test_`, takes zero
/// parameters, is non-generic, and returns `void`/`i64`/`bool`.
///
/// `filter` is applied BEFORE the runnability checks, so narrowing to one test
/// never emits `skip` lines about tests the user excluded. Returns the selected
/// names and how many `test_*` functions the filter excluded.
///
/// `emit_skips` prints a `skip <name>: <reason>` line for each unrunnable
/// `test_*` function. The isolated child passes `false`: the parent already
/// printed them, and both sides must agree on the resulting indices.
pub(crate) fn discover_tests(
    program: &Program,
    filter: Option<&str>,
    emit_skips: bool,
) -> (Vec<String>, usize) {
    let mut names = Vec::new();
    let mut filtered_out = 0usize;
    for function in &program.functions {
        if !function.name.starts_with("test_") {
            continue;
        }
        if let Some(substring) = filter
            && !function.name.contains(substring)
        {
            filtered_out += 1;
            continue;
        }
        // Skip test-named functions that cannot be run as a zero-argument entry
        // point, noting why so the surface stays discoverable.
        if !function.params.is_empty() {
            if emit_skips {
                println!(
                    "skip {}: takes parameters (test functions must take zero parameters)",
                    function.name
                );
            }
            continue;
        }
        if !function.type_params.is_empty() {
            if emit_skips {
                println!("skip {}: is generic", function.name);
            }
            continue;
        }
        if !matches!(function.return_type.name.as_str(), "void" | "i64" | "bool") {
            if emit_skips {
                println!(
                    "skip {}: returns `{}` (expected void, i64, or bool)",
                    function.name, function.return_type.name
                );
            }
            continue;
        }
        names.push(function.name.clone());
    }
    (names, filtered_out)
}

/// Run the language-level test suite in a `.lby` source file. The source is
/// validated as a LIBRARY (no `main` required), then every discovered `test_*`
/// function (see [`discover_tests`]) is run on the AST interpreter. A test passes
/// if it returns without a runtime error and fails if it produces one (an
/// `assert(false)` throw, or any other runtime error). Prints one line per test
/// plus a summary and exits non-zero if any test failed.
///
/// `filter` is the optional `--filter <substring>` name filter; a filter that
/// matches nothing is reported explicitly (and is not an error) so a typo in the
/// substring is visible rather than looking like an empty, passing suite.
///
/// Tests run in deterministic order and their results are reported in that same
/// order regardless of how many child processes it took to produce them.
///
/// # Isolation
///
/// The tests run in a CHILD PROCESS (see [`crate::commands::test_isolate`]), so
/// no test can take the runner down:
///
/// * a **runtime error** — `assert(false)`, `throw`, an A5 contract violation
///   (bounds fail, divide-by-zero), `L0423` — is returned by the interpreter as
///   an ordinary `RuntimeError` and reported as a `FAIL`;
/// * a **stack overflow** kills the child, which the parent observes as abnormal
///   termination and reports as that test's `FAIL`; and
/// * a **non-terminating** test trips the per-test deadline (`--timeout`), is
///   killed, and is reported as that test's `FAIL`.
///
/// In every case the remaining tests still run and the summary is still correct.
/// A test that wedges the machine outside the child (a killed child's own
/// grandchildren, or an exhausted global resource) is still outside what a
/// process boundary can contain.
pub(crate) fn test_file(
    path: PathBuf,
    mode: OutputMode,
    filter: Option<String>,
    timeout_secs: u64,
) -> Result<(), String> {
    let compiled = match compile(&path, SourceMode::Library) {
        Ok(compiled) => compiled,
        Err(failure) => {
            return Err(format_reports(
                &failure.reports,
                mode,
                failure.source.as_deref(),
            ));
        }
    };

    let verbose = mode == OutputMode::Verbose;
    let (names, filtered_out) = discover_tests(&compiled.checked.program, filter.as_deref(), true);

    if names.is_empty() {
        match filter.as_deref() {
            // Distinguish a mistyped filter from a genuinely empty suite: both
            // print `0 passed, 0 failed`, so without this the two look identical.
            Some(substring) if filtered_out > 0 => println!(
                "no tests matched filter `{substring}` ({filtered_out} test(s) filtered out)"
            ),
            Some(substring) => println!("no tests matched filter `{substring}`"),
            None => {
                println!("no tests found (define functions named `test_*` with zero parameters)")
            }
        }
        println!("0 passed, 0 failed");
        return Ok(());
    }

    // `--timeout 0` disables the deadline: a non-terminating test then hangs the
    // runner again, so it is strictly opt-in.
    let timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs));
    // Prints `PASS`/`FAIL` per test as results stream in.
    let tally = run_isolated(&path, &names, filter.as_deref(), verbose, timeout)?;
    let (passed, failed) = (tally.passed, tally.failed);

    if filtered_out > 0 {
        println!("{passed} passed, {failed} failed, {filtered_out} filtered out");
    } else {
        println!("{passed} passed, {failed} failed");
    }
    if failed > 0 {
        // Non-zero exit without an extra diagnostic line: the per-test output and
        // summary already report the failures.
        return Err(String::new());
    }
    Ok(())
}
