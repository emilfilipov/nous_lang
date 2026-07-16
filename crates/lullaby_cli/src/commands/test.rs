//! `lullaby test` — the built-in language-level test runner.

use std::path::PathBuf;

use lullaby_runtime::run_named_function;

use crate::args::OutputMode;
use crate::compile::{SourceMode, compile};
use crate::diagnostics::format_reports;

/// Run the language-level test suite in a `.lby` source file. The source is
/// validated as a LIBRARY (no `main` required), then every top-level function
/// whose name starts with `test_`, takes zero parameters, is non-generic, and
/// returns `void`/`i64`/`bool` is run through the AST interpreter. A test passes
/// if it returns without a runtime error and fails if it produces one (an
/// `assert(false)` throw, or any other runtime error). Prints one line per test
/// plus a summary and exits non-zero if any test failed.
///
/// `filter` is the optional `--filter <substring>` name filter: when present,
/// only `test_*` functions whose name contains that (case-sensitive) substring
/// are considered. Filtering happens BEFORE the runnability checks, so filtering
/// down to one test never emits `skip` lines about unrelated ones. A filter that
/// matches nothing is reported explicitly (and is not an error) so a typo in the
/// substring is visible rather than looking like an empty, passing suite.
///
/// Tests run in source-declaration order, which is deterministic across runs.
///
/// A test that fails with a RUNTIME ERROR — including an A5 contract violation
/// (bounds fail, divide-by-zero) — does NOT terminate the run: A5's
/// abort-without-unwinding applies to the NATIVE tier, whereas this runner
/// executes on the AST interpreter, which surfaces every such violation as an
/// ordinary `RuntimeError` returned by `run_named_function`. The runner reports
/// it as a normal failure and continues. See `tests/cli/suite17.rs`.
///
/// KNOWN GAPS (not runtime errors, so they escape that `Result`): a test that
/// OVERFLOWS THE STACK aborts this whole process (no summary), and a
/// NON-TERMINATING test hangs it (there is no per-test timeout). Containing both
/// needs a subprocess per test plus a deadline — funded follow-up work tracked in
/// `documents/road_to_1_0_stable.md` B3. Do not describe isolation as total.
pub(crate) fn test_file(
    path: PathBuf,
    mode: OutputMode,
    filter: Option<String>,
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
    let mut names = Vec::new();
    let mut filtered_out = 0usize;
    for function in &compiled.checked.program.functions {
        if !function.name.starts_with("test_") {
            continue;
        }
        // Apply `--filter` before the runnability checks below, so narrowing to
        // one test does not print `skip` lines about tests the user excluded.
        if let Some(substring) = filter.as_deref()
            && !function.name.contains(substring)
        {
            filtered_out += 1;
            continue;
        }
        // Skip test-named functions that cannot be run as a zero-argument entry
        // point, noting why so the surface stays discoverable.
        if !function.params.is_empty() {
            println!(
                "skip {}: takes parameters (test functions must take zero parameters)",
                function.name
            );
            continue;
        }
        if !function.type_params.is_empty() {
            println!("skip {}: is generic", function.name);
            continue;
        }
        if !matches!(function.return_type.name.as_str(), "void" | "i64" | "bool") {
            println!(
                "skip {}: returns `{}` (expected void, i64, or bool)",
                function.name, function.return_type.name
            );
            continue;
        }
        names.push(function.name.clone());
    }

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

    let mut passed = 0usize;
    let mut failed = 0usize;
    for name in &names {
        match run_named_function(&compiled.checked.program, name) {
            Ok(_) => {
                passed += 1;
                println!("PASS {name}");
            }
            Err(error) => {
                failed += 1;
                println!("FAIL {name}: {}", error.message);
                if verbose {
                    for frame in &error.traceback {
                        match frame.span {
                            Some(span) => println!(
                                "    at {} ({}:{})",
                                frame.function, span.line, span.column
                            ),
                            None => println!("    at {}", frame.function),
                        }
                    }
                }
            }
        }
    }

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
