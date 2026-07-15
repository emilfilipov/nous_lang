//! CLI integration tests, part 12 (explicit overflow arithmetic on `i64` and the
//! `checked_div`/`checked_rem` builtins). Split out of tests/cli.rs so it does not
//! overlap the native / fuzz / socket / stdin / map / match / const / generics
//! suites.
//!
//! Integer arithmetic wraps by default in Lullaby (a conscious 1.0 decision). The
//! explicit overflow builtins make the choice visible per-operation:
//!
//! - `checked_add`/`checked_sub`/`checked_mul` -> `option<T>` (`none` on overflow),
//!   `saturating_*` -> `T` (clamp to `T`'s bounds), `wrapping_*` -> `T` (the
//!   explicit form of the default). These already covered the fixed-width kinds;
//!   this suite pins the newly-added plain-`i64` support.
//! - `checked_div`/`checked_rem` -> `option<T>` round out the checked family:
//!   `none` on a zero divisor and (for division) the signed `MIN / -1` overflow;
//!   `checked_rem(MIN, -1)` is `some(0)` (defined, matching the default `%`).
//!
//! All overflow builtins resolve identically on the three interpreter backends
//! (`ast`, `ir`, `bytecode`) via the shared `overflow_arith`/`checked_div_rem`
//! runtime helpers. The native and WASM backends do not yet lower these for `i64`
//! (nor the new `checked_div`/`checked_rem` for any width), so a function using one
//! is cleanly skipped via the existing `L0339`/`L0338` no-eligible-function gate —
//! never miscompiled — and runs on the interpreters. The fixtures live at the top
//! level of `tests/fixtures/valid/`, so the `ir_lib_tests` executable-fixture
//! harness and the formatter idempotency sweep also cover them.

use crate::*;

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

/// Assert a valid fixture evaluates to `expected` (its process stdout, trimmed)
/// identically on the `ast`, `ir`, and `bytecode` interpreters.
fn assert_fixture_result(fixture: &str, expected: &str) {
    let path = workspace_root().join("tests/fixtures/valid").join(fixture);
    let mut results = Vec::new();
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &path);
        assert!(output.status.success(), "{backend}: {output:?}");
        results.push(stdout(&output));
    }
    assert_eq!(results[0].trim(), expected, "ast result for {fixture}");
    assert_eq!(
        results[1], results[0],
        "ir output differs from ast for {fixture}"
    );
    assert_eq!(
        results[2], results[0],
        "bytecode output differs from ast for {fixture}"
    );
}

/// `checked_*`/`saturating_*`/`wrapping_*` add/sub/mul on plain `i64`: overflow
/// yields `none`/clamp/wrap, in-range yields `some(v)`/the value. The self-checking
/// fixture evaluates to `297` on every interpreter backend.
#[test]
pub(crate) fn i64_overflow_arithmetic_runs_identically_on_all_backends() {
    assert_fixture_result("run_overflow_i64.lby", "297");
}

/// `checked_div`/`checked_rem` on `i64`: `none` on a zero divisor and the signed
/// `i64::MIN / -1` division overflow; `i64::MIN % -1` is `some(0)`. The fixture
/// evaluates to `13` on every interpreter backend.
#[test]
pub(crate) fn i64_checked_div_rem_runs_identically_on_all_backends() {
    assert_fixture_result("run_overflow_i64_div_rem.lby", "13");
}

/// `checked_div`/`checked_rem` on a fixed-width kind (`i8`): `none` on a zero
/// divisor and the `i8::MIN / -1` overflow (quotient `128` is outside `i8`);
/// `i8::MIN % -1` is `some(0)`. The fixture evaluates to `14` on every interpreter
/// backend.
#[test]
pub(crate) fn sized_checked_div_rem_runs_identically_on_all_backends() {
    assert_fixture_result("run_overflow_sized_div_rem.lby", "14");
}
