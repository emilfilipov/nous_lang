//! CLI integration tests, part 4 (standard-input builtins: `read_line` and
//! `read_all`). Split out of tests/cli.rs so it does not overlap the native /
//! fuzz / socket suites. Each test feeds a fixed byte string to a compiled
//! `.lby` filter over a piped stdin and asserts the captured stdout on every
//! interpreter backend (`ast`, `ir`, `bytecode`), keeping the checks
//! deterministic.

use crate::*;
use std::io::Write;
use std::process::Stdio;

/// Run `lullaby run --backend <backend> <program>` with `input` written to the
/// child's stdin, then closed, and return the captured process output. Feeding
/// stdin requires a piped spawn (`Command::output` closes stdin immediately), so
/// this drives the child directly.
fn run_with_stdin(backend: &str, program: &std::path::Path, input: &[u8]) -> std::process::Output {
    let mut child = lullaby()
        .args([
            "run",
            "--backend",
            backend,
            program.to_str().expect("program path"),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn lullaby run");
    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(input)
        .expect("write stdin");
    // Dropping the taken stdin handle above closes the pipe, signalling EOF so a
    // `read_line`/`read_all` loop terminates.
    child.wait_with_output().expect("wait for lullaby run")
}

/// The cat-like echo filter (`stdin/echo.lby`) reads stdin line by line with
/// `read_line()` and echoes each line, then returns the line count. With a
/// trailing newline the three input lines are echoed verbatim and `main` returns
/// 3 (printed last by `run`).
#[test]
pub(crate) fn stdin_echo_filter_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/echo.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_with_stdin(backend, &fixture, b"a\nbb\nccc\n");
        assert!(output.status.success(), "{backend}: {output:?}");
        // Each line echoed (trailing newline stripped then re-added by println),
        // then the returned line count 3.
        assert_eq!(stdout(&output), "a\nbb\nccc\n3\n", "{backend}");
    }
}

/// A final line with no trailing newline is still a line: `read_line` returns
/// `some(text)` for it and `none` only at end-of-input, so both lines are echoed
/// and the count is 2.
#[test]
pub(crate) fn stdin_echo_filter_no_trailing_newline_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/echo.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_with_stdin(backend, &fixture, b"x\ny");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output), "x\ny\n2\n", "{backend}");
    }
}

/// Empty stdin: `read_line()` returns `none` immediately, so nothing is echoed
/// and the count is 0. This pins the EOF behavior distinctly from a blank line.
#[test]
pub(crate) fn stdin_echo_filter_empty_input_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/echo.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_with_stdin(backend, &fixture, b"");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output), "0\n", "{backend}");
    }
}

/// The line-count filter (`stdin/line_count.lby`) reports the number of lines. A
/// blank input line counts as a line — `read_line()` yields `some("")` for it,
/// distinct from `none` at EOF — so `"a\n\nc\n"` is three lines. The program
/// prints the count and returns it, so `run` prints it twice.
#[test]
pub(crate) fn stdin_line_count_filter_counts_blank_lines_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/line_count.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_with_stdin(backend, &fixture, b"a\n\nc\n");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output), "3\n3\n", "{backend}");
    }
}

/// Line count over empty stdin is 0 (no lines), the EOF-at-start case.
#[test]
pub(crate) fn stdin_line_count_filter_empty_input_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/line_count.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_with_stdin(backend, &fixture, b"");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output), "0\n0\n", "{backend}");
    }
}

/// `read_all()` slurps the whole of stdin to EOF. The fixture returns the
/// character length of what it read; `"hello\nworld"` is 11 characters (the
/// embedded newline included), which `run` prints as the result.
#[test]
pub(crate) fn stdin_read_all_returns_full_length_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/read_all.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_with_stdin(backend, &fixture, b"hello\nworld");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output).trim(), "11", "{backend}");
    }
}

/// `read_all()` over empty stdin is the empty string (length 0).
#[test]
pub(crate) fn stdin_read_all_empty_input_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/read_all.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_with_stdin(backend, &fixture, b"");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output).trim(), "0", "{backend}");
    }
}

/// A function that uses a stdin builtin is not part of the native i64-scalar
/// subset, so `lullaby native` must skip it cleanly through the existing
/// eligibility gate — reporting `L0339` with a per-function skip reason — rather
/// than crashing or silently producing a wrong result. This pins the graceful
/// native-ineligibility behavior without the native emitter needing to know
/// about stdin at all.
#[test]
pub(crate) fn native_skips_stdin_functions_cleanly() {
    let fixture = workspace_root().join("tests/fixtures/valid/stdin/read_all.lby");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    // No i64-scalar function is eligible, so native reports L0339 and does not
    // succeed — the same clean skip path any non-native builtin (e.g. read_file)
    // takes.
    assert!(
        !output.status.success(),
        "native must not succeed: {output:?}"
    );
    let rendered = format!("{}{}", stdout(&output), stderr(&output));
    assert!(rendered.contains("L0339"), "expected L0339: {rendered}");
    assert!(
        rendered.contains("skipped main"),
        "expected a per-function skip reason for main: {rendered}"
    );
}
