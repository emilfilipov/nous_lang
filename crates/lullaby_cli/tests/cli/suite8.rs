//! CLI integration tests, part 8 (user-defined generic structs, stage 1). Split
//! out of tests/cli.rs so it does not overlap the native / fuzz / socket / stdin
//! / map / match / const suites. Stage 1 is a single-parameter generic `struct`
//! with a scalar type parameter `T`: the three interpreters run it via type
//! erasure (a generic struct is, at runtime, just a struct over dynamic values),
//! so these tests pin that a generic-struct program runs byte-for-byte
//! identically on every interpreter backend (`ast`, `ir`, `bytecode`), that the
//! native backend MONOMORPHIZES a scalar-argument generic struct (each concrete
//! instantiation resolves to a scalar-only layout byte-identical to the erased
//! value, so it compiles natively and its `.exe` matches the interpreter), and
//! that the stage-1 semantic negatives are rejected with their dedicated
//! diagnostics.
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

/// A generic struct instantiated with SCALAR type arguments is now MONOMORPHIZED
/// on the native backend (A1 stage-1 native): each concrete instantiation
/// (`Box<i64>`, `Box<bool>`, `Pair<i64>`) resolves, after type-argument
/// substitution, to a scalar-only layout byte-identical to the interpreters'
/// erased value, so every function compiles natively — none is skipped. This is
/// the value-neutrality gate: the linked `.exe` exit code must equal the
/// interpreter result (mod 256). Gated on the link toolchain.
#[test]
pub(crate) fn generic_struct_scalar_compiles_native() {
    ensure_msvc_env();
    let fixture = workspace_root().join("tests/fixtures/valid/generics/box_pair.lby");
    let out = std::env::temp_dir().join("lullaby_generic_struct_scalar_parity.exe");
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
    // Every function that touches a scalar generic-struct instantiation compiles
    // natively; nothing is skipped.
    let emitted = stdout(&output);
    for name in ["unbox", "rewrap", "pair_sum", "main"] {
        assert!(
            emitted.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively: {emitted}"
        );
    }
    assert!(
        !emitted.contains("skipped"),
        "no scalar generic-struct function should be skipped: {emitted}"
    );

    // Interpreter ground truth (identical across all three backends, = 146).
    let interp = run_backend("ast", &fixture);
    assert!(interp.status.success(), "{interp:?}");
    let expected: i64 = stdout(&interp).trim().parse().expect("interpreter i64");
    assert_eq!(expected, 146, "box_pair main computes 146");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld/kernel32.lib unavailable; skipping native generic-struct link+run parity"
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
        "native monomorphized generic struct must equal the interpreter's erased value"
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
