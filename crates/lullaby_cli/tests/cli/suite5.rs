//! CLI integration tests, part 5 (map iteration builtins: `map_keys` and
//! `map_values`). Split out of tests/cli.rs so it does not overlap the native /
//! fuzz / socket / stdin suites. Each test runs a pure, deterministic `.lby`
//! program that builds a `map`, prints its keys then its values in the map's
//! insertion order, and asserts the captured stdout is byte-for-byte identical
//! on every interpreter backend (`ast`, `ir`, `bytecode`). This pins the
//! insertion-order iteration guarantee (Lullaby's `map` is the insertion-ordered
//! `OrderedMap`) across the backends, complementing the semantic type-check tests
//! and the five-variant parity coverage of `run_map_iter.lby`.

use crate::*;

/// Run `lullaby run --backend <backend> <program>` and return the captured
/// output. The map-iteration fixtures are pure (no stdin, no I/O), so a plain
/// `Command::output` suffices.
fn run_backend(backend: &str, program: &std::path::Path) -> std::process::Output {
    lullaby()
        .args([
            "run",
            "--backend",
            backend,
            program.to_str().expect("program path"),
        ])
        .output()
        .expect("run lullaby")
}

/// `map_keys`/`map_values` over a `map<string, i64>` return the keys and values
/// in **insertion order**, not sorted or hash order. The fixture inserts
/// `banana`, `apple`, `cherry`, then re-sets `apple` to a new value; re-setting an
/// existing key keeps its original position, so the keys stay `banana, apple,
/// cherry` and the values are `2, 10, 3`. `main` returns the key count (3), which
/// `run` prints last.
#[test]
pub(crate) fn map_keys_values_insertion_order_string_keys_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/map_iter/string_keys.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(
            stdout(&output),
            "banana\napple\ncherry\n2\n10\n3\n3\n",
            "{backend}"
        );
    }
}

/// The same builtins over a `map<i64, string>` (integer keys, string values):
/// keys `30, 10, 20` and values `thirty, ten, twenty` in insertion order, with
/// the count (3) returned and printed last. This pins that the key/value element
/// types flow through correctly for both orderings of scalar-vs-string.
#[test]
pub(crate) fn map_keys_values_insertion_order_int_keys_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/map_iter/int_keys.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(
            stdout(&output),
            "30\n10\n20\nthirty\nten\ntwenty\n3\n",
            "{backend}"
        );
    }
}

/// An empty map yields empty key/value lists: `len(map_keys(m))` and
/// `len(map_values(m))` are both 0, so the print loop never runs and `main`
/// returns 0 (printed once by `run`). This pins the empty-map edge case.
#[test]
pub(crate) fn map_keys_values_empty_map_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/map_iter/empty.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output), "0\n", "{backend}");
    }
}

/// A function that iterates a `map` (via `map_keys`/`map_values`) is not part of
/// the native i64-scalar subset, so `lullaby native` must skip it cleanly through
/// the existing eligibility gate — reporting `L0339` with a per-function skip
/// reason naming the unsupported map type — rather than crashing or silently
/// producing a wrong result. This pins the graceful native-ineligibility behavior
/// without the native emitter needing to know about map iteration at all.
#[test]
pub(crate) fn native_skips_map_iteration_functions_cleanly() {
    let fixture = workspace_root().join("tests/fixtures/valid/map_iter/string_keys.lby");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    // No i64-scalar function is eligible (main iterates a map), so native reports
    // L0339 and does not succeed — the same clean skip path any non-native heap
    // builtin takes.
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
