//! CLI integration tests, part 9 (user-defined generic enums, stage A1). Split
//! out of tests/cli.rs so it does not overlap the native / fuzz / socket / stdin
//! / map / match / const / generic-struct suites. Stage A1 is a single-parameter
//! generic `enum` with a type parameter `T`: the three interpreters run it via
//! type erasure (a generic enum is, at runtime, just a tagged union over dynamic
//! values), so these tests pin that a generic-enum program runs byte-for-byte
//! identically on every interpreter backend (`ast`, `ir`, `bytecode`), that the
//! recursive-generic-enum indirection rule holds (a valid `list`-indirected
//! `Tree<T>` runs; a direct self-recursion is rejected), that the native backend
//! MONOMORPHIZES a scalar-argument generic enum (each concrete instantiation
//! resolves to a scalar-only tag+payload layout, compiles natively, and its
//! `.exe` matches the interpreter; a heap-payload instantiation still cleanly
//! skips), and that the stage-A1 semantic negatives are rejected with their
//! dedicated diagnostics.
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

/// A generic enum instantiated with SCALAR type arguments is now MONOMORPHIZED on
/// the native backend (A1 stage-1 native): each concrete instantiation
/// (`Opt<i64>`, `Opt<bool>`, `Res<i64>`) resolves, after payload substitution, to
/// a scalar-only tag+payload layout byte-identical to the interpreters' erased
/// value, so construction of payload and unit variants plus exhaustive `match`
/// all compile natively — nothing is skipped. Value-neutrality gate: the linked
/// `.exe` exit code must equal the interpreter result (mod 256).
#[test]
pub(crate) fn generic_enum_scalar_compiles_native() {
    ensure_msvc_env();
    let fixture = workspace_root().join("tests/fixtures/valid/generics/opt_res.lby");
    let out = std::env::temp_dir().join("lullaby_generic_enum_scalar_parity.exe");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let emitted = stdout(&output);
    for name in ["wrap", "unwrap_or", "flag_to_int", "res_value", "main"] {
        assert!(
            emitted.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively: {emitted}"
        );
    }
    assert!(
        !emitted.contains("skipped"),
        "no scalar generic-enum function should be skipped: {emitted}"
    );

    // Interpreter ground truth (identical across all three backends, = 141).
    let interp = run_backend("ast", &fixture);
    assert!(interp.status.success(), "{interp:?}");
    let expected: i64 = stdout(&interp).trim().parse().expect("interpreter i64");
    assert_eq!(expected, 141, "opt_res main computes 141");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld/kernel32.lib unavailable; skipping native generic-enum link+run parity"
        );
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = std::process::Command::new(&out)
        .output()
        .expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit as i64,
        expected.rem_euclid(256),
        "native monomorphized generic enum must equal the interpreter's erased value"
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
