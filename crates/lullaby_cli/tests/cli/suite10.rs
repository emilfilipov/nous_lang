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
//! native backend now COMPILES inherent-method dispatch (each `recv.method(args)`
//! call resolves to a monomorphized instance function per receiver instantiation,
//! so the produced `.exe` exits with the same value the interpreters compute), and
//! that the stage-4 semantic negatives are rejected with their dedicated
//! diagnostics. A method whose receiver/param/return falls outside the native
//! subset still skips cleanly (`L0339`) rather than miscompiling.
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

/// Native inherent-method dispatch (A1): a program whose `main` calls methods on
/// generic types — `Box<i64>.peek`/`rewrap`, `Box<bool>.peek`, `Opt<i64>.unwrap_or`
/// — now COMPILES natively. Each `recv.method(args)` resolves to a monomorphized
/// instance function (`self` passed by the existing aggregate ABI, copy-in value
/// semantics), so the produced `.exe` exits with the interpreters' `151` (mod 256).
/// The verbose listing reports the mangled instance functions as compiled, and the
/// direct-PE path runs it with no linker.
#[test]
pub(crate) fn generic_methods_compile_native() {
    let fixture = workspace_root().join("tests/fixtures/valid/generics/methods.lby");
    let out = std::env::temp_dir().join("lullaby_generic_methods.exe");
    let _ = std::fs::remove_file(&out);
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
    assert!(
        output.status.success(),
        "expected native method dispatch to compile: {output:?}"
    );
    let listing = stdout(&output);
    // `main` and the monomorphized method instances are compiled `.text` symbols.
    assert!(listing.contains("compiled main"), "listing: {listing}");
    assert!(
        listing.contains("$mth$Box_i64_$peek"),
        "expected the Box<i64>::peek instance to compile: {listing}"
    );
    assert!(
        listing.contains("$mth$Opt_i64_$unwrap_or"),
        "expected the Opt<i64>::unwrap_or instance to compile: {listing}"
    );
    assert!(out.is_file(), "expected a native exe at {}", out.display());

    // Native exit code must equal the interpreter result (151) mod 256.
    let exe = Command::new(&out).output().expect("run native methods exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 151,
        "native inherent-method dispatch must exit with the interpreter result (151)"
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
