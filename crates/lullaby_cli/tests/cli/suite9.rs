//! CLI integration tests, part 9 (user-defined generic enums, stage A1). Split
//! out of tests/cli.rs so it does not overlap the native / fuzz / socket / stdin
//! / map / match / const / generic-struct suites. Stage A1 is a single-parameter
//! generic `enum` with a type parameter `T`: the three interpreters run it via
//! type erasure (a generic enum is, at runtime, just a tagged union over dynamic
//! values), so these tests pin that a generic-enum program runs byte-for-byte
//! identically on every interpreter backend (`ast`, `ir`, `bytecode`), that the
//! recursive-generic-enum indirection rule holds (a valid `list`-indirected
//! `Tree<T>` runs; a direct self-recursion is rejected), that the native backend
//! cleanly skips a function that uses a generic enum (reported as
//! native-ineligible via `L0339`, never miscompiled), and that the stage-A1
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

/// Two distinct instantiations of `Opt<T>` (`Opt<i64>` and `Opt<bool>`), a second
/// generic enum `Res<T>`, payload-carrying (`present(v)`) and unit (`absent`)
/// construction, exhaustive `match` with `T` substituted to the concrete type,
/// and passing / returning a generic enum value through functions. `main`
/// evaluates to `141`, identical on every interpreter backend.
#[test]
pub(crate) fn generic_enum_runs_identically_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/opt_res.lby");
    let mut results = Vec::new();
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        results.push(stdout(&output));
    }
    assert_eq!(results[0].trim(), "141", "ast");
    // Byte-for-byte identical across the three interpreters (type erasure keeps
    // one runtime shape per generic enum).
    assert_eq!(results[1], results[0], "ir output differs from ast");
    assert_eq!(results[2], results[0], "bytecode output differs from ast");
}

/// A recursive generic enum whose recursion passes through a `list<Tree<T>>`
/// indirection is legal (no `L0456`) and runs identically on every interpreter
/// backend. `main` sums the leaves of an `i64` tree to `17`.
#[test]
pub(crate) fn recursive_generic_enum_through_indirection_runs() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/tree_indirection.lby");
    let mut results = Vec::new();
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        results.push(stdout(&output));
    }
    assert_eq!(results[0].trim(), "17", "ast");
    assert_eq!(results[1], results[0], "ir output differs from ast");
    assert_eq!(results[2], results[0], "bytecode output differs from ast");
}

/// A function that uses a generic enum is native-ineligible in stage A1
/// (monomorphization on the native backend is a later stage), so the native
/// backend must *cleanly skip* it via the existing `L0339` gate — report every
/// such function as skipped with a clear "not in the native subset" reason —
/// rather than miscompiling or crashing. Because *no* function is eligible, the
/// native command has nothing to emit and surfaces `L0339` as a hard error; the
/// point is that this is a clean diagnostic, never a produced-but-wrong
/// executable.
#[test]
pub(crate) fn generic_enum_cleanly_skips_native() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/opt_res.lby");
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
    // Every function that touches a generic enum is reported as skipped with a
    // reason mentioning the generic type spelling, proving it demoted to the
    // interpreter rather than miscompiling.
    for name in ["wrap", "unwrap_or", "flag_to_int", "res_value", "main"] {
        assert!(
            errors.contains(&format!("skipped {name}")),
            "expected `{name}` to be skipped natively: {errors}"
        );
    }
    assert!(
        errors.contains("Opt<i64>"),
        "expected the skip reason to name the generic instantiation: {errors}"
    );
}

/// The stage-A1 semantic negatives, each rejected with its dedicated diagnostic:
/// a wrong type-argument arity (`L0454`), a unit variant whose type parameter
/// cannot be inferred and has no annotation (`L0455`), a directly recursive
/// generic enum that is infinitely sized (`L0456`, the recursion-through-
/// indirection rule), and a payload argument whose type disagrees with the
/// annotation-pinned type parameter (`L0381`).
#[test]
pub(crate) fn generic_enum_negatives_are_rejected() {
    for (fixture_name, code) in [
        ("enum_wrong_arity", "L0454"),
        ("enum_uninferable", "L0455"),
        ("enum_direct_recursion", "L0456"),
        ("enum_payload_mismatch", "L0381"),
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
