//! CLI integration tests, part 10 (methods on user-defined generic types, stage
//! 4). Split out of tests/cli.rs so it does not overlap the native / fuzz /
//! socket / stdin / map / match / const / generic-struct / generic-enum suites.
//!
//! Stage 4 adds inherent `impl Box<T>` blocks: methods whose signatures and
//! bodies use the type parameter `T` and the `self` receiver. A call resolves the
//! method by the receiver's concrete instantiation, substituting `T` — a method
//! returning `T` on a `Box<i64>` returns `i64`, one taking `T` accepts `i64`.
//! Method dispatch on a generic type is ordinary receiver dispatch at runtime
//! (generics are erased on the interpreters), so these tests pin that a program
//! calling generic-type methods over two instantiations runs byte-for-byte
//! identically on every interpreter backend (`ast`, `ir`, `bytecode`), that the
//! native backend cleanly skips a function using a generic type (reported as
//! native-ineligible via `L0339`, never miscompiled), and that the stage-4
//! semantic negatives are rejected with their dedicated diagnostics.
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

/// Methods on generic types over two instantiations: `Box<i64>` and `Box<bool>`
/// each call `peek` (returns the wrapped `T`) and `rewrap` (takes a `T`, returns
/// a fresh `Box<T>`), and a generic `enum Opt<T>` calls `unwrap_or` (matches
/// `self`, binding the payload as the concrete `T`). `main` evaluates to `151`,
/// identical on every interpreter backend because method dispatch on a generic
/// type is ordinary receiver dispatch under erasure.
#[test]
pub(crate) fn generic_methods_run_identically_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/methods.lby");
    let mut results = Vec::new();
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        results.push(stdout(&output));
    }
    assert_eq!(results[0].trim(), "151", "ast");
    // Byte-for-byte identical across the three interpreters (type erasure keeps
    // one runtime shape per generic type, so method dispatch is identical).
    assert_eq!(results[1], results[0], "ir output differs from ast");
    assert_eq!(results[2], results[0], "bytecode output differs from ast");
}

/// A function that calls a generic-type method is native-ineligible in stage 4
/// (monomorphization on the native backend is a later stage), so the native
/// backend must *cleanly skip* it via the existing `L0339` gate rather than
/// miscompiling. Because `main` uses `Box<i64>`, no function is eligible and the
/// native command surfaces `L0339` as a hard error — a clean diagnostic, never a
/// produced-but-wrong executable.
#[test]
pub(crate) fn generic_methods_cleanly_skip_native() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/methods.lby");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        !output.status.success(),
        "expected the L0339 no-eligible-function gate: {output:?}"
    );
    let errors = stderr(&output);
    assert!(
        errors.contains("L0339"),
        "expected the no-eligible-function skip diagnostic: {errors}"
    );
    // `main` (which uses a generic type) is reported as skipped with a reason that
    // names the generic instantiation, proving it demoted to the interpreter
    // rather than miscompiling.
    assert!(
        errors.contains("skipped main"),
        "expected `main` to be skipped natively: {errors}"
    );
    assert!(
        errors.contains("Box<i64>"),
        "expected the skip reason to name the generic instantiation: {errors}"
    );
}

/// The stage-4 semantic negatives, each rejected with its dedicated diagnostic:
/// a method argument whose type disagrees with the receiver-pinned type parameter
/// (`L0313`), and a call to a method the receiver's generic type does not declare
/// (`L0457`).
#[test]
pub(crate) fn generic_method_negatives_are_rejected() {
    for (fixture_name, code) in [
        ("method_arg_mismatch", "L0313"),
        ("method_undefined", "L0457"),
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
