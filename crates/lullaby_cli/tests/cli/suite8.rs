//! CLI integration tests, part 8 (user-defined generic structs, stage 1). Split
//! out of tests/cli.rs so it does not overlap the native / fuzz / socket / stdin
//! / map / match / const suites. Stage 1 is a single-parameter generic `struct`
//! with a scalar type parameter `T`: the three interpreters run it via type
//! erasure (a generic struct is, at runtime, just a struct over dynamic values),
//! so these tests pin that a generic-struct program runs byte-for-byte
//! identically on every interpreter backend (`ast`, `ir`, `bytecode`), that the
//! native backend cleanly skips a function that uses a generic struct (it is
//! reported as native-ineligible via `L0339`, never miscompiled), and that the
//! stage-1 semantic negatives are rejected with their dedicated diagnostics.
//!
//! The positive fixtures live under `tests/fixtures/valid/generics/` and the
//! negatives under `tests/fixtures/invalid/generics/` so the `ir_lib_tests`
//! full-fixture harness and the formatter fixture sweep (both of which scan only
//! the top level of their fixture directory) do not also pick them up.

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

/// Two distinct instantiations of `Box<T>` (`Box<i64>` and `Box<bool>`), a second
/// generic struct `Pair<T>`, positional and named construction, field read with
/// `T` substituted to the concrete type, a value-semantic copy, and passing /
/// returning a generic value through functions. `main` returns
/// `unbox(a)=5 + unbox(copy)=5 + unbox(bumped)=6 + pair_sum(p)=30 + extra=100`
/// = `146`, identical on every interpreter backend.
#[test]
pub(crate) fn generic_struct_runs_identically_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/box_pair.lby");
    let mut results = Vec::new();
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        results.push(stdout(&output));
    }
    assert_eq!(results[0].trim(), "146", "ast");
    // Byte-for-byte identical across the three interpreters (type erasure keeps
    // one runtime shape per generic struct).
    assert_eq!(results[1], results[0], "ir output differs from ast");
    assert_eq!(results[2], results[0], "bytecode output differs from ast");
}

/// A function that uses a generic struct is native-ineligible in stage 1
/// (monomorphization on the native backend is a later stage), so the native
/// backend must *cleanly skip* it via the existing `L0339` gate — report every
/// such function as skipped with a clear "not in the native subset" reason —
/// rather than miscompiling or crashing. Because *no* function is eligible, the
/// native command has nothing to emit and surfaces `L0339` as a hard error (the
/// same gate any all-ineligible program hits); the point is that this is a clean
/// diagnostic, never a produced-but-wrong executable.
#[test]
pub(crate) fn generic_struct_cleanly_skips_native() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/box_pair.lby");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    // The all-ineligible gate reports `L0339` and emits no object — a clean skip,
    // not a crash and not a wrong executable.
    assert!(
        !output.status.success(),
        "expected the L0339 no-eligible-function gate: {output:?}"
    );
    let errors = stderr(&output);
    assert!(
        errors.contains("L0339"),
        "expected the no-eligible-function skip diagnostic: {errors}"
    );
    // Every function that touches a generic struct is reported as skipped with a
    // reason mentioning the generic type spelling, proving it demoted to the
    // interpreter rather than miscompiling.
    for name in ["unbox", "rewrap", "pair_sum", "main"] {
        assert!(
            errors.contains(&format!("skipped {name}")),
            "expected `{name}` to be skipped natively: {errors}"
        );
    }
    assert!(
        errors.contains("Box<i64>"),
        "expected the skip reason to name the generic instantiation: {errors}"
    );
}

/// The stage-1 semantic negatives, each rejected with its dedicated diagnostic:
/// a wrong type-argument arity, using the generic type with no type argument
/// (its parameter left unbound), an ill-typed field access, and a construction
/// whose type parameter cannot be inferred and has no annotation.
#[test]
pub(crate) fn generic_struct_negatives_are_rejected() {
    for (fixture_name, code) in [
        ("wrong_arity", "L0454"),
        ("unbound_type_param", "L0454"),
        ("bad_field_access", "L0371"),
        ("uninferable_type_param", "L0455"),
    ] {
        let fixture = workspace_root()
            .join("tests/fixtures/invalid/generics")
            .join(format!("{fixture_name}.lby"));
        let output = lullaby()
            .args(["check", fixture.to_str().expect("fixture path")])
            .output()
            .expect("check cli");
        assert!(
            !output.status.success(),
            "{fixture_name}: expected rejection, got {output:?}"
        );
        let errors = stderr(&output);
        assert!(
            errors.contains(code),
            "{fixture_name}: expected `{code}` in diagnostics: {errors}"
        );
    }
}
