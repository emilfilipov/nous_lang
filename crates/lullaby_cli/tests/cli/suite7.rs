//! CLI integration tests, part 7 (named compile-time constants). Split out of
//! tests/cli.rs so it does not overlap the native / fuzz / socket / stdin / map /
//! match suites. Every reference to a `const` is folded to its literal value
//! during semantic analysis, so the backends never see a `const`; these tests
//! pin that the folded programs run byte-for-byte identically across every
//! interpreter backend (`ast`, `ir`, `bytecode`) and — for the all-i64 program —
//! that the native backend compiles the folded literals and the linked `.exe`
//! exits with the same result mod 256. The fixtures live under
//! `tests/fixtures/valid/const/` so the `ir_lib_tests` full-fixture harness (which
//! only scans the top level of `tests/fixtures/valid`) does not also pick them up.

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

/// A constant used in expressions, a constant defined from another constant
/// (`DOUBLED = MAX_LEN * 2`), a constant used across multiple functions, and a
/// bitwise constant expression (`MASK = (1 << 4) - 1`). All-i64, so it folds to
/// literals and stays native-eligible. `main` returns
/// `scaled(2) + DOUBLED + masked(255)` = `385 + 256 + 15` = `656` on every
/// interpreter backend.
#[test]
pub(crate) fn const_scalars_fold_identically_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/const/scalars.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output).trim(), "656", "{backend}");
    }
}

/// Constants of every scalar kind (`i64`/`f64`/`bool`/`string`/`char`), each used
/// across multiple functions, with `DOUBLED` defined from another constant. The
/// program prints `hi` then `ready` and returns
/// `len("hi") + DOUBLED + char_code('A')` = `2 + 256 + 65` = `323`. The full
/// stdout must be byte-for-byte identical on every interpreter backend.
#[test]
pub(crate) fn const_mixed_scalar_kinds_identical_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/const/mixed.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(backend, &fixture);
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output), "hi\nready\n323\n", "{backend}");
    }
}

/// The all-i64 constant program is native-eligible: a folded constant is just a
/// literal, so the native backend compiles every function (no skips) and the
/// produced `.exe` exits with the interpreter result. Windows preserves the full
/// 32-bit process exit code, so the direct-PE executable exits with `656` (the
/// same value the interpreters return). This proves the fold is backend-agnostic
/// — the native emitter never learns about `const`. Gated on the native
/// toolchain being reachable; skipped gracefully otherwise.
#[test]
pub(crate) fn const_scalars_native_matches_interpreter() {
    ensure_msvc_env();
    if !kernel32_available() {
        eprintln!("kernel32.lib not reachable; skipping const native link+run");
        return;
    }
    let fixture = workspace_root().join("tests/fixtures/valid/const/scalars.lby");
    let exe = std::env::temp_dir().join("lullaby_const_scalars.exe");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            exe.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    // Every function compiles natively (folded constants are literals, so nothing
    // is skipped for a `const`-related reason).
    for name in ["scaled", "masked", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled natively: {listing}"
        );
    }

    // Run the produced executable; on Windows the full 32-bit exit code is
    // preserved, so it equals the interpreter result (656) exactly.
    let run = std::process::Command::new(&exe)
        .output()
        .expect("run native exe");
    assert_eq!(
        run.status.code(),
        Some(656),
        "native exit code must equal the interpreter result (656)"
    );
}
