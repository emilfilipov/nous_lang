//! CLI integration tests, part 23 (const-sized arrays `array<T, N>`,
//! road_to_1_0_stable A2). A fixed-extent array carries a compile-time extent but
//! erases to the existing length-agnostic `array<T>` representation on every
//! tier, so a program using fixed-extent locals (literal and named-constant `N`)
//! plus a fill literal `[v; k]` runs identically on all three interpreters and,
//! when linkable, on the native backend. The fixture lives in a
//! `const_sized_arrays/` subdirectory so the `ir_lib_tests.rs`
//! whole-`valid`-directory backend-parity harness does not also pick it up.

use crate::*;
use std::process::Command;

/// The fixed-extent fixture returns `10 + 40 + 7 + 4 + 3 = 64` — below a byte, so
/// the native exit code is portable across Windows/Unix truncation.
const FIXTURE: &str = "tests/fixtures/valid/const_sized_arrays/run_const_sized_array.lby";
const EXPECTED: i64 = 64;

/// Every interpreter backend runs the fixed-extent fixture and prints exactly the
/// expected result. This pins that `array<T, N>` (both a named-constant `N` and a
/// literal extent) and the fill literal `[v; k]` behave identically on the AST,
/// IR, and bytecode tiers.
#[test]
pub(crate) fn const_sized_arrays_run_on_all_interpreters() {
    let fixture = workspace_root().join(FIXTURE);
    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output).trim(), EXPECTED.to_string(), "{backend}");
    }
}

/// The fourth tier: a const-sized-array program compiles to a native executable
/// (its extent erased to `array<T>`, which the native i64-scalar/array subset
/// already supports) and, when linkable, its exit code equals the interpreter
/// result — proving the erasure adds no native miscompile surface.
#[test]
pub(crate) fn const_sized_arrays_native_execution_parity_when_linkable() {
    let scratch = ScratchDir::new("const_sized_arrays_native_parity");
    let fixture = workspace_root().join(FIXTURE);
    let out = scratch.join("lullaby_const_sized_array.exe");
    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out"),
            fixture.to_str().expect("fixture"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture")])
        .output()
        .expect("run cli");
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, EXPECTED);

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("linker unavailable; skipping native const-sized-array parity");
        return;
    }
    assert!(out.is_file());
    let exe = Command::new(&out).output().expect("run exe");
    let expected = if cfg!(windows) {
        interp as i32
    } else {
        interp.rem_euclid(256) as i32
    };
    assert_eq!(exe.status.code().expect("exit"), expected);
}
