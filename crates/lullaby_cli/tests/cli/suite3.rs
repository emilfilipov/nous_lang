//! CLI integration tests, part 3 (native x86-64 backend: link-and-run parity,
//! aggregates, strings, RC reclamation, FFI). Split out of tests/cli.rs.

use crate::*;
use std::process::Command;

#[test]
pub(crate) fn native_reports_no_eligible_functions() {
    // `main` uses `to_string(f64)` (dtoa, deferred), so it skips and nothing is
    // eligible for native. (Plain string values are now in the native subset, so
    // the not-eligible example uses the still-deferred float `to_string`.)
    let source = "fn main -> i64\n    len(to_string(1.5))\n";
    let tmp = std::env::temp_dir().join("lullaby_native_none.lby");
    std::fs::write(&tmp, source).expect("write temp");
    let output = lullaby()
        .args(["native", "--verbose", tmp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let rendered = format!("{}{}", stdout(&output), stderr(&output));
    assert!(rendered.contains("L0339"), "expected L0339: {rendered}");
    assert!(
        rendered.contains("skipped main"),
        "expected verbose skip reason: {rendered}"
    );
}

/// Direct PE emission (freestanding, NO external linker): `lullaby native
/// --freestanding` writes a runnable `.exe` in-house, skipping `rust-lld`
/// entirely. This test needs neither `rust-lld` nor `kernel32.lib` — that is the
/// whole point — so it runs unconditionally: emit the freestanding fixture, run
/// the produced `.exe`, and assert its exit code equals the interpreter's `main`
/// result (mod 256). Also asserts no object file is written on this path.
#[test]
pub(crate) fn native_freestanding_direct_pe_runs() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_direct_pe.exe");
    let obj = out.with_extension("obj");
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&obj);

    let emit = lullaby()
        .args([
            "native",
            "--freestanding",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let listing = stdout(&emit);
    assert!(
        listing.contains("direct PE, no linker"),
        "expected the direct-PE notice: {listing}"
    );
    assert!(
        out.is_file(),
        "expected a direct-PE exe at {}",
        out.display()
    );
    // The direct path writes no intermediate object file.
    assert!(
        !obj.is_file(),
        "direct PE path must not write an object file"
    );

    // The produced `.exe` begins with the DOS `MZ` magic (a real PE image).
    let bytes = std::fs::read(&out).expect("read direct pe");
    assert_eq!(&bytes[0..2], b"MZ", "PE image DOS magic");

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 39, "fixture main computes 39");

    // Run the in-house `.exe` (no linker was involved) and compare exit codes.
    let exe = Command::new(&out).output().expect("run direct pe exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "direct-PE exit code must equal the interpreter result (mod 256)"
    );
}

/// Direct PE emission for a heap/string program (freestanding): a `.rdata`
/// string constant plus the `.bss` bump heap must map and run correctly from the
/// in-house PE image, with no linker. `native_strings.lby` computes 11
/// (`len("hello") + len("native") + len("")`).
#[test]
pub(crate) fn native_freestanding_direct_pe_heap_runs() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_strings.lby");
    let out = std::env::temp_dir().join("lullaby_direct_pe_heap.exe");
    let _ = std::fs::remove_file(&out);

    let emit = lullaby()
        .args([
            "native",
            "--freestanding",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("direct PE, no linker"),
        "expected the direct-PE notice: {}",
        stdout(&emit)
    );
    assert!(
        out.is_file(),
        "expected a direct-PE exe at {}",
        out.display()
    );

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 11, "fixture main computes 11");

    let exe = Command::new(&out).output().expect("run direct pe heap exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "direct-PE (heap) exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity: link the i64-scalar fixture into a real `.exe`
/// and assert its exit code equals the interpreter's `main` result (mod 256).
/// If `rust-lld` or `kernel32.lib` is unavailable, skip with a message.
#[test]
pub(crate) fn native_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_parity.exe");

    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 39, "fixture main computes 39");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native link+run parity"
        );
        return;
    }

    // The CLI should have produced the `.exe`; run it and compare exit codes.
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// The `i64::MIN / -1` signed-division overflow case must yield `i64::MIN`
/// (wrapping) on every backend, not trap or panic. The three interpreters agree
/// on the fixture's deterministic result (7), and — when linkable — the native
/// `.exe` must exit with the same value. Without the wrapping guard the native
/// `idiv` would raise a hardware #DE and the process would crash instead of
/// exiting 7.
#[test]
pub(crate) fn native_signed_div_overflow_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_div_overflow.lby");
    let out = std::env::temp_dir().join("lullaby_native_div_overflow_parity.exe");

    // All three interpreters agree on 7 (plain-i64 and fixed-width isize paths).
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{backend}: {}", stderr(&run));
        assert_eq!(
            stdout(&run).trim(),
            "7",
            "{backend}: i64::MIN / -1 must wrap to i64::MIN (result 7)"
        );
    }

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native signed-division overflow link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 7,
        "native exit code must equal the interpreter result (7); a trap on \
         i64::MIN / -1 would crash the process instead"
    );
}

/// Best-effort execution parity for the stack-aggregate subset: native-compile
/// a program that builds a struct and sums a fixed i64 array, then assert the
/// linked `.exe`'s exit code equals the interpreter's `main` result (mod 256).
/// Gated on `rust-lld` + `kernel32.lib` exactly like the scalar parity test.
#[test]
pub(crate) fn native_aggregates_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_aggregates.lby");
    let out = std::env::temp_dir().join("lullaby_native_aggregates_parity.exe");

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // `main` uses only i64 scalars, an all-i64 struct, and a fixed i64 array, so
    // it is eligible for native codegen.
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 43, "aggregates fixture main computes 43");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native aggregates link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for **fat-pointer `array<i64>` parameters**:
/// native-compile a program whose helpers take a read-only `array<i64>` parameter
/// whose length is NOT known at compile time — reading it via `for x in a`, `a[i]`,
/// and `len(a)`, including the `count_frequency_of a array<i64> n, x i64` shape
/// where the length comes from a separate `n` parameter — and assert each such
/// helper compiles natively (as a fat pointer, no longer demoted for "no call site
/// to infer its length from"), the interpreter result agrees across AST/IR/
/// bytecode, and — when linkable — the `.exe` exit code equals the interpreter's
/// `main` result (mod 256). Gated on `rust-lld` + `kernel32.lib` like the other
/// native parity tests; the compile-not-skip and interpreter-truth assertions
/// always run.
#[test]
pub(crate) fn native_fat_array_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_fat_array.lby");
    let out = std::env::temp_dir().join("lullaby_native_fat_array_parity.exe");

    // Make MSVC's `LIB` available (source vcvars64 if unset) so the link+run runs.
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Every fat-pointer-array helper — and `main` — must compile natively: the
    // read-only `array<i64>` parameters are passed as fat pointers, so none is
    // demoted for a missing call-site length.
    let emit_out = stdout(&emit);
    for name in ["sum_array", "count_frequency_of", "max_in", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively (fat-pointer array), got: {emit_out}"
        );
    }
    assert!(
        !emit_out.contains("has no call site to infer its length from"),
        "no fat-pointer-array helper should demote for a missing call-site length: {emit_out}"
    );

    // Interpreter ground truth for `main`, identical across every backend.
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{backend}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(interp, 42, "{backend}: fat-array fixture main computes 42");
    }

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native fat-array link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 42,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for **fat-pointer `array<f64>` parameters**: the
/// same fat-pointer path as the `array<i64>` test but with an `f64` element type,
/// so `a[i]` / `for x in a` load each element through an XMM register. Helpers read
/// a read-only runtime-length `array<f64>` (a for-each comparison count, an indexed
/// sum with a separate length, and a running-max index scan) and return an i64;
/// `main` returns 214. Asserts each helper compiles as a fat pointer and — when
/// linkable — the `.exe` exit code equals the interpreter result.
#[test]
pub(crate) fn native_fat_array_f64_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_fat_array_f64.lby");
    let out = std::env::temp_dir().join("lullaby_native_fat_array_f64_parity.exe");

    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    let emit_out = stdout(&emit);
    for name in ["count_above", "sum_first_over", "max_index", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively (fat-pointer f64 array), got: {emit_out}"
        );
    }
    assert!(
        !emit_out.contains("has no call site to infer its length from"),
        "no fat-pointer-array helper should demote for a missing call-site length: {emit_out}"
    );

    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{backend}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(
            interp, 214,
            "{backend}: fat-array-f64 fixture main computes 214"
        );
    }

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native fat-array-f64 link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 214,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for the native **stack-argument** ABI:
/// native-compile a program whose functions take more than four scalar
/// parameters (six and eight `i64`, plus a mixed int/float six-parameter
/// signature), so their 5th+ arguments are passed on the stack above the shadow
/// space. Assert every such function compiles natively (not skipped), the
/// interpreter result agrees across AST/IR/bytecode, and — when linkable — the
/// `.exe` exit code equals the interpreter's `main` result (mod 256). Sources
/// MSVC's `LIB` (via vcvars64) when unset so the link+run executes.
#[test]
pub(crate) fn native_many_args_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_many_args.lby");
    let out = std::env::temp_dir().join("lullaby_native_many_args_parity.exe");

    // Make MSVC's `LIB` available (source vcvars64 if unset) so the link+run runs.
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Every >4-parameter function — and `main` — must compile natively (the
    // stack-argument ABI keeps them in the native subset, no longer demoted).
    let emit_out = stdout(&emit);
    for name in ["six", "eight", "scale", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively (stack args), got: {emit_out}"
        );
    }
    assert!(
        !emit_out.contains("skipped"),
        "no >4-parameter function should be skipped: {emit_out}"
    );

    // Interpreter ground truth for `main`, identical across every backend.
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{backend}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(interp, 98, "{backend}: many-args fixture main computes 98");
    }

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native many-args link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 98,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for the native **aggregate-boundary** ABI:
/// native-compile programs that pass, return, and mutate scalar-field aggregates
/// (structs, fixed arrays, scalar-payload enums) across function boundaries, then
/// assert each linked `.exe`'s exit code equals the interpreter's `main` result
/// (mod 256). This exercises the hidden-return-pointer / by-pointer-argument /
/// copy-in ABI end to end, including value semantics (a callee that mutates its
/// aggregate parameter must not change the caller's copy). Gated on `rust-lld` +
/// `kernel32.lib` like the other native parity tests; the compile-not-skip and
/// interpreter-truth assertions always run.
#[test]
pub(crate) fn native_aggregate_boundary_execution_parity_when_linkable() {
    for (name, funcs) in [
        (
            "native_aggregate_params",
            &["taxicab", "make_point", "shift", "mutate_local"][..],
        ),
        (
            "native_aggregate_array",
            &["sum_array", "doubled", "mutate_array"][..],
        ),
        (
            "native_aggregate_enum",
            &["classify", "unwrap_or", "direct"][..],
        ),
        (
            "native_aggregate_value_semantics",
            &["clobber_struct", "clobber_array"][..],
        ),
    ] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_{name}_parity.exe"));

        let emit = lullaby()
            .args([
                "native",
                "--verbose",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{}: {}", name, stderr(&emit));
        // Every aggregate-boundary function compiles natively (not skipped): the
        // by-pointer parameter/return ABI is in the native subset.
        for func in funcs {
            assert!(
                stdout(&emit).contains(&format!("compiled {func}")),
                "{name}: expected `{func}` compiled natively: {}",
                stdout(&emit)
            );
        }
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: expected `main` compiled: {}",
            stdout(&emit)
        );
        assert!(
            !stdout(&emit).contains("skipped"),
            "{name}: no aggregate-boundary function should be skipped: {}",
            stdout(&emit)
        );

        // Interpreter ground truth for `main`.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}: {}", name, stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");

        if rust_lld_path().is_none() || !kernel32_available() {
            eprintln!(
                "rust-lld and/or kernel32.lib not available; skipping {name} link+run parity"
            );
            continue;
        }

        assert!(out.is_file(), "expected linked exe at {}", out.display());
        let exe = Command::new(&out).output().expect("run native exe");
        let exit = exe.status.code().expect("native exit code");
        assert_eq!(
            exit,
            (interp.rem_euclid(256)) as i32,
            "{name}: native exit code must equal the interpreter result (mod 256)"
        );
    }
}

/// Best-effort execution parity for the native enum + `match` subset:
/// native-compile a program that builds `option`/`result`/user enum locals and
/// matches over them (tag dispatch + scalar payload binding), then assert the
/// linked `.exe`'s exit code equals the interpreter's `main` result (mod 256).
/// Gated on `rust-lld` + `kernel32.lib` like the other native parity tests.
#[test]
pub(crate) fn native_enum_match_execution_parity_when_linkable() {
    for (name, expected) in [
        ("native_enum_option", 49i64),
        ("native_enum_result", 44),
        ("native_enum_user", 206),
    ] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_{name}_parity.exe"));

        let emit = lullaby()
            .args([
                "native",
                "--verbose",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{}", stderr(&emit));
        // `main` (and the helper/match functions) compile natively — the match
        // over a local enum is now in the native subset.
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: expected `main` compiled: {}",
            stdout(&emit)
        );

        // Interpreter ground truth for `main`.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(interp, expected, "{name}: interpreter result");

        if rust_lld_path().is_none() || !kernel32_available() {
            eprintln!(
                "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
                 skipping native enum/match link+run parity for {name}"
            );
            continue;
        }

        assert!(out.is_file(), "expected linked exe at {}", out.display());
        let exe = Command::new(&out).output().expect("run native exe");
        let exit = exe.status.code().expect("native exit code");
        assert_eq!(
            exit,
            (interp.rem_euclid(256)) as i32,
            "{name}: native exit code must equal the interpreter result (mod 256)"
        );
    }
}

/// Overflow-aware fixed-width arithmetic — `checked_*` (`option<T>`),
/// `saturating_*`, and `wrapping_*` for add/sub/mul across narrow (i8/u8/u32/i32)
/// and 64-bit (u64/usize/isize) kinds, signed and unsigned — now compiles on the
/// native backend (previously deferred). `--verbose` proves `main` and its helpers
/// compile natively; the linked `.exe`'s exit code equals the interpreter's `main`
/// result (mod 256). Sources MSVC's `LIB` when unset; gated on `rust-lld` +
/// `kernel32.lib` like the other native parity tests.
#[test]
pub(crate) fn native_overflow_arith_execution_parity_when_linkable() {
    ensure_msvc_env();
    let fixture = workspace_root().join("tests/fixtures/valid/run_overflow_codegen.lby");
    let out = std::env::temp_dir().join("lullaby_native_overflow_codegen.exe");

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Every function — including the overflow-builtin helpers and `main` — compiles
    // natively; none is deferred to the interpreters.
    for func in ["low8", "checked_i8", "checked_usize_mul", "main"] {
        assert!(
            stdout(&emit).contains(&format!("compiled {func}")),
            "expected `{func}` compiled natively: {}",
            stdout(&emit)
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 233, "fixture main result");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native overflow-arith link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// The same overflow-aware arithmetic fixture compiled to WASM: `--verbose` proves
/// every function compiles (none deferred), and — when `node` is available — the
/// module's `main` export returns the interpreter's exact `i64` result.
#[test]
pub(crate) fn wasm_overflow_arith_execution_parity_with_node() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_overflow_codegen.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_overflow_codegen.wasm");

    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    for func in ["low8", "checked_i8", "checked_usize_mul", "main"] {
        assert!(
            stdout(&emit).contains(&format!("compiled {func}")),
            "expected `{func}` compiled on WASM: {}",
            stdout(&emit)
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "233");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM overflow-arith execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_overflow_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM `main` must equal the interpreter result: {out_text}"
    );
}

/// The scalar math builtins compiled to WASM: `--verbose` proves every function
/// compiles (none deferred), and — when `node` is available — the module's `main`
/// export returns the interpreter's exact `i64` result. Exercises `sqrt`/`abs` on
/// `f64` (`f64.sqrt`/`f64.abs`), `abs` on `i64` (incl. `i64::MIN` wrap), the `i64`
/// suite `min`/`max`/`gcd`/`sign`/`clamp` (incl. `gcd(i64::MIN, 0)` and `lo > hi`),
/// all bit-for-bit with the interpreters.
#[test]
pub(crate) fn wasm_math_builtins_execution_parity_with_node() {
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_math_builtins.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_math_builtins.wasm");

    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    for func in ["imin", "f64_checks", "edge_checks", "main"] {
        assert!(
            stdout(&emit).contains(&format!("compiled {func}")),
            "expected `{func}` compiled on WASM: {}",
            stdout(&emit)
        );
    }
    assert!(
        !stdout(&emit).contains("skipped"),
        "no scalar-math function should be skipped: {}",
        stdout(&emit)
    );

    // Interpreter ground truth for `main` (identical on AST/IR/bytecode).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "70");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM math-builtins execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_math_builtins_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM `main` must equal the interpreter result: {out_text}"
    );
}

/// Best-effort execution parity for the native `string`-carrying growable
/// collections and enums: native-compile a `list<string>` (literal/concat/
/// `to_string` elements, `get`/`len`/`set`/`pop`, a value-semantics probe across a
/// call boundary), a `map<i64, string>` (scalar key, `string` value — `map_set`
/// insert/update-in-place, `map_get` -> `option<string>`, `map_has`, `map_len`),
/// and a `result<i64, string>` matched over both arms plus `option<string>` and a
/// user string-payload enum. A `string` element/value/payload is a single
/// immutable pointer word stored and copied exactly like a scalar (shared, never
/// deep-recursed), so this proves native parity with the WASM string-collection
/// increment. Assert each linked `.exe`'s exit code equals the interpreter's `main`
/// result (mod 256). Sources MSVC's `LIB` when unset; gated on `rust-lld` +
/// `kernel32.lib` like the other native parity tests.
#[test]
pub(crate) fn native_string_collections_execution_parity_when_linkable() {
    ensure_msvc_env();
    for (name, expected) in [
        ("native_list_string", 31i64),
        ("native_map_string", 23),
        ("native_result_string", 52),
    ] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_{name}_parity.exe"));

        let emit = lullaby()
            .args([
                "native",
                "--verbose",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{name}: {}", stderr(&emit));
        // `main` and every helper compile natively — `list<string>`, `map<K,
        // string>`, and string-payload enums are in the native subset now, so
        // nothing is skipped.
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: expected `main` compiled: {}",
            stdout(&emit)
        );
        assert!(
            !stdout(&emit).contains("skipped"),
            "{name}: no string-collection function should be skipped: {}",
            stdout(&emit)
        );

        // Interpreter ground truth for `main` (identical on AST/IR/bytecode).
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{name}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(interp, expected, "{name}: interpreter result");

        if rust_lld_path().is_none() || !kernel32_available() {
            eprintln!(
                "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
                 skipping native string-collection link+run parity for {name}"
            );
            continue;
        }

        assert!(out.is_file(), "expected linked exe at {}", out.display());
        let exe = Command::new(&out).output().expect("run native exe");
        let exit = exe.status.code().expect("native exit code");
        assert_eq!(
            exit,
            (interp.rem_euclid(256)) as i32,
            "{name}: native string-collection exit code must equal the interpreter result (mod 256)"
        );
    }
}

/// Best-effort execution parity for the native growable `list<T>` (scalar element)
/// subset: native-compile programs that build lists via `list_new`/`push` (with
/// capacity-doubling growth), read them with `get`/`len`, and replace/pop
/// value-semantically with `set`/`pop` — including lists crossing function
/// boundaries and an aliasing value-semantics check (`let b = a` then mutating one
/// must not affect the other). Assert each linked `.exe`'s exit code equals the
/// interpreter's `main` result (mod 256). Sources MSVC's `LIB` when unset so the
/// link+run actually executes; gated on `rust-lld` + `kernel32.lib`.
#[test]
pub(crate) fn native_list_execution_parity_when_linkable() {
    ensure_msvc_env();
    for name in ["native_list_build", "native_list_value_semantics"] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_{name}_parity.exe"));

        let emit = lullaby()
            .args([
                "native",
                "--verbose",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{name}: {}", stderr(&emit));
        // `main` (and any list helper function) compiles natively — scalar-element
        // growable lists are in the native subset now, so nothing is skipped.
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: expected `main` compiled: {}",
            stdout(&emit)
        );
        assert!(
            !stdout(&emit).contains("skipped"),
            "{name}: no list function should be skipped: {}",
            stdout(&emit)
        );

        // Interpreter ground truth for `main`.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{name}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");

        if rust_lld_path().is_none() || !kernel32_available() {
            eprintln!(
                "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
                 skipping native list link+run parity for {name}"
            );
            continue;
        }

        assert!(out.is_file(), "expected linked exe at {}", out.display());
        let exe = Command::new(&out).output().expect("run native exe");
        let exit = exe.status.code().expect("native exit code");
        assert_eq!(
            exit,
            (interp.rem_euclid(256)) as i32,
            "{name}: native list exit code must equal the interpreter result (mod 256)"
        );
    }
}

/// Best-effort execution parity for the native MUTABLE-heap collection-element
/// subset: `list<struct>`, `list<list<i64>>`, `map<i64, struct>`, and the
/// `option<struct>` that `map_get` returns. Native-compile a program that builds a
/// `list<Point>` (push/get/set across a boundary), a `list<list<i64>>` (nested
/// deep copy), a `map<i64, Point>` (insert + update-in-place + `map_get` matched),
/// and — CRUCIALLY — a value-semantics probe (`get` a struct element, mutate the
/// retrieved copy, re-`get` and confirm the original list element is unchanged).
/// Assert every function compiles natively (not skipped) and the linked `.exe`'s
/// exit code equals the interpreter's `main` result (96), which proves the
/// recursive per-element deep copy matches the interpreters bit-for-bit. Sources
/// MSVC's `LIB` when unset; gated on `rust-lld` + `kernel32.lib`.
#[test]
pub(crate) fn native_list_struct_execution_parity_when_linkable() {
    ensure_msvc_env();
    let name = "native_list_struct";
    let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
    let out = std::env::temp_dir().join(format!("lullaby_{name}_parity.exe"));

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{name}: {}", stderr(&emit));
    // Every function — including the `list<struct>`, `list<list<i64>>`, and
    // `map<i64, struct>` builders/consumers — compiles natively; nothing is skipped.
    assert!(
        stdout(&emit).contains("compiled main"),
        "{name}: expected `main` compiled: {}",
        stdout(&emit)
    );
    assert!(
        !stdout(&emit).contains("skipped"),
        "{name}: no mutable-heap-element function should be skipped: {}",
        stdout(&emit)
    );

    // Interpreter ground truth for `main` (identical across ast/ir/bytecode).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{name}: {}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native list<struct> link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "{name}: native mutable-heap-element exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for the native growable `map<K, V>` (scalar
/// key/value) subset: native-compile programs that build maps via `map_new`/
/// `map_set` (insert, update-in-place of an existing key, capacity-doubling
/// growth), read them with `map_get` (matching the returned `option<V>`),
/// `map_has`, and `map_len` — including maps crossing function boundaries and an
/// aliasing value-semantics check (`let b = a` then mutating one must not affect
/// the other). Assert each linked `.exe`'s exit code equals the interpreter's
/// `main` result (mod 256), which also proves the native association-list ordering
/// and lookup agree with the interpreters bit-for-bit. Sources MSVC's `LIB` when
/// unset; gated on `rust-lld` + `kernel32.lib` like the other native parity tests.
#[test]
pub(crate) fn native_map_execution_parity_when_linkable() {
    ensure_msvc_env();
    for name in ["native_map_build", "native_map_value_semantics"] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_{name}_parity.exe"));

        let emit = lullaby()
            .args([
                "native",
                "--verbose",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{name}: {}", stderr(&emit));
        // `main` (and every map helper function) compiles natively — scalar-key /
        // scalar-value maps are in the native subset now, so nothing is skipped.
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: expected `main` compiled: {}",
            stdout(&emit)
        );
        assert!(
            !stdout(&emit).contains("skipped"),
            "{name}: no map function should be skipped: {}",
            stdout(&emit)
        );

        // Interpreter ground truth for `main`.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{name}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");

        if rust_lld_path().is_none() || !kernel32_available() {
            eprintln!(
                "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
                 skipping native map link+run parity for {name}"
            );
            continue;
        }

        assert!(out.is_file(), "expected linked exe at {}", out.display());
        let exe = Command::new(&out).output().expect("run native exe");
        let exit = exe.status.code().expect("native exit code");
        assert_eq!(
            exit,
            (interp.rem_euclid(256)) as i32,
            "{name}: native map exit code must equal the interpreter result (mod 256)"
        );
    }
}

/// Best-effort execution parity for the native control-flow subset: native-compile
/// a program whose functions use a `while` loop, `for` sum/product loops, and
/// inter-function calls, then assert the linked `.exe`'s exit code equals the
/// interpreter's `main` result (mod 256). This proves the loop bounds/step
/// semantics, checked-integer arithmetic, and the Win64 call ABI all agree with
/// the interpreters. Gated on `rust-lld` + `kernel32.lib` like the other native
/// parity tests.
#[test]
pub(crate) fn native_control_flow_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_control_flow.lby");
    let out = std::env::temp_dir().join("lullaby_native_control_flow_parity.exe");

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Every function (the two `for` loops, the `while` loop, and the caller that
    // invokes all three plus `main`) is i64-scalar, so all compile natively.
    for name in ["while_sum", "for_sum", "for_product", "combine", "main"] {
        assert!(
            stdout(&emit).contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {}",
            stdout(&emit)
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 31, "control-flow fixture main computes 31");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native control-flow link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for the first native heap step: native-compile a
/// program whose `main` derives an i64 from string constants (the summed byte
/// lengths of string literals), then assert the linked `.exe`'s exit code equals
/// the interpreter's `main` result (mod 256). This proves the `.rdata` string
/// constants, their relocations, the bump heap, and the heap byte scans agree
/// with the interpreter. Gated on `rust-lld` + `kernel32.lib` like the other
/// native parity tests.
#[test]
pub(crate) fn native_strings_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_strings.lby");
    let out = std::env::temp_dir().join("lullaby_native_strings_parity.exe");

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // `main` uses only i64 scalars and `len` over string literals, so it is
    // eligible for native codegen.
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 11, "strings fixture main computes 11");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native strings link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for the index-based native string operations:
/// native-compile a program that uses char-indexed `substring`/`find` (which
/// decode UTF-8 to map char indices to byte offsets), an empty needle, present and
/// absent `find`, and true/false cases of the byte-exact `contains`/`starts_with`/
/// `ends_with` predicates over a multi-byte string ("café", where `é` is 2 bytes),
/// combining them into a deterministic `i64 < 256` from `main`. The `.exe` exit
/// code must equal the interpreter's `main` result (mod 256), proving the native
/// helpers agree with the interpreters bit-for-bit — including the char-vs-byte
/// distinction. Gated on `rust-lld` + `kernel32.lib`; the compile-not-skip and
/// interpreter-truth assertions always run. Sources MSVC's `LIB` (via vcvars64)
/// when unset so the link+run executes.
#[test]
pub(crate) fn native_string_ops_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_string_ops.lby");
    let out = std::env::temp_dir().join("lullaby_native_string_ops_parity.exe");

    // Make MSVC's `LIB` available (source vcvars64 if unset) so the link+run runs.
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Every function — the `substring`/`find`/predicate wrappers, the bool→i64
    // helper, and `main` — must compile natively (not skip to the interpreters).
    let emit_out = stdout(&emit);
    for name in [
        "sub_af",
        "sub_e",
        "sub_full",
        "sub_empty",
        "find_present",
        "find_absent",
        "find_empty",
        "contains_true",
        "contains_false",
        "starts_true",
        "starts_false",
        "ends_true",
        "ends_false",
        "bool_to_i64",
        "main",
    ] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively, got: {emit_out}"
        );
    }

    // Interpreter ground truth for `main` (the joined deterministic total).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 11, "string_ops fixture main computes 11");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native string_ops link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Native `upper`/`lower` (ASCII case fold) execution parity: a byte-wise case fold
/// over the ASCII strings the native subset builds matches the interpreters'
/// `to_uppercase`/`to_lowercase` exactly. Includes `upper(lower(x))` (whose inner
/// result is a fresh temporary reclaimed through `str_read_own`).
#[test]
pub(crate) fn native_string_case_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_string_case.lby");
    let out = std::env::temp_dir().join("lullaby_native_string_case_parity.exe");
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 17829, "upper/lower fixture main computes 17829");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native string-case parity");
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    let expected = if cfg!(windows) {
        interp as i32
    } else {
        interp.rem_euclid(256) as i32
    };
    assert_eq!(
        exit, expected,
        "native upper/lower must match the interpreters"
    );
}

/// Native `sqrt` (a single SSE2 `sqrtsd`) execution parity: `sqrt(a*a+b*b)` and
/// `sqrt(2.0)` compiled to machine code must agree bit-for-bit with the
/// interpreters' `f64::sqrt`. Also exercises f64 parameters routed through XMM.
#[test]
pub(crate) fn native_sqrt_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_sqrt.lby");
    let out = std::env::temp_dir().join("lullaby_native_sqrt_parity.exe");
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "sqrt-using main must compile natively: {}",
        stdout(&emit)
    );

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 11, "sqrt fixture main computes 11");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native sqrt parity");
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        interp.rem_euclid(256) as i32,
        "native sqrt must match the interpreters' f64::sqrt"
    );
}

/// Native `abs(f64)` (an SSE2 in-register sign-bit clear) execution parity:
/// `abs(-7.5)` and `abs(a-b)` compiled to machine code must agree bit-for-bit
/// with the interpreters' `f64::abs`. Also exercises f64 parameters routed
/// through XMM (the `dist` helper).
#[test]
pub(crate) fn native_abs_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_abs.lby");
    let out = std::env::temp_dir().join("lullaby_native_abs_parity.exe");
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main") && stdout(&emit).contains("compiled dist"),
        "abs-using main and dist must compile natively: {}",
        stdout(&emit)
    );

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 11, "abs fixture main computes 11");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native abs parity");
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        interp.rem_euclid(256) as i32,
        "native abs must match the interpreters' f64::abs"
    );
}

/// Native `min`/`max` on `i64` (a branchless signed `cmp` + `cmov`) execution
/// parity: `min`/`max` and a `clampi` helper built from them (including negative
/// operands) compiled to machine code must agree with the interpreters' i64
/// `min`/`max`. Exercises i64 parameters across a function boundary too.
#[test]
pub(crate) fn native_min_max_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_minmax.lby");
    let out = std::env::temp_dir().join("lullaby_native_minmax_parity.exe");
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main") && stdout(&emit).contains("compiled clampi"),
        "min/max-using main and clampi must compile natively: {}",
        stdout(&emit)
    );

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 109, "min/max fixture main computes 109");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native min/max parity");
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        interp.rem_euclid(256) as i32,
        "native min/max must match the interpreters' i64::min/i64::max"
    );
}

/// Native inline `gcd(a, b)` on `i64` execution parity: several cases (negative
/// operands, a zero operand, coprime inputs) and a `reduce_num` helper built on
/// `gcd` must agree with the interpreters' `gcd_i64` (Euclid on the unsigned
/// magnitudes). Exercises i64 parameters across a function boundary too.
#[test]
pub(crate) fn native_gcd_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_gcd.lby");
    let out = std::env::temp_dir().join("lullaby_native_gcd_parity.exe");
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main") && stdout(&emit).contains("compiled reduce_num"),
        "gcd-using main and reduce_num must compile natively: {}",
        stdout(&emit)
    );

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 135, "gcd fixture main computes 135");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native gcd parity");
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        interp.rem_euclid(256) as i32,
        "native gcd must match the interpreters' gcd_i64"
    );
}

/// Native `sign`/`clamp` on `i64` execution parity: `sign` (positive, negative,
/// zero) and `clamp` (above, below, in-range, and a `lo > hi` case through a
/// `clip` helper) compiled to machine code must agree with the interpreters'
/// `i64::signum` and the `clamp` branch semantics.
#[test]
pub(crate) fn native_sign_clamp_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_signclamp.lby");
    let out = std::env::temp_dir().join("lullaby_native_signclamp_parity.exe");
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main") && stdout(&emit).contains("compiled clip"),
        "sign/clamp-using main and clip must compile natively: {}",
        stdout(&emit)
    );

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 158, "sign/clamp fixture main computes 158");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native sign/clamp parity");
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        interp.rem_euclid(256) as i32,
        "native sign/clamp must match the interpreters"
    );
}

/// Best-effort execution parity for first-class native heap `string` values:
/// native-compile a program that builds strings by concatenation (`+`), converts
/// integers/bools with `to_string`, passes a string to a helper that returns its
/// `len`, and derives a deterministic `i64 < 256`. The `.exe` exit code must equal
/// the interpreter's `main` result (mod 256), proving native string literals,
/// concatenation, `to_string`, `len`, and string params/returns agree with the
/// interpreters bit-for-bit. Gated on `rust-lld` + `kernel32.lib`; the
/// compile-not-skip and interpreter-truth assertions always run. Sources MSVC's
/// `LIB` (via vcvars64) when unset so the link+run executes.
#[test]
pub(crate) fn native_string_build_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_string_build.lby");
    let out = std::env::temp_dir().join("lullaby_native_string_build_parity.exe");

    // Make MSVC's `LIB` available (source vcvars64 if unset) so the link+run runs.
    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // All three functions (`greeting` returns a string, `measure` takes a string,
    // `main` builds/concats/converts strings) must compile natively, not skip.
    for func in ["compiled greeting", "compiled measure", "compiled main"] {
        assert!(
            stdout(&emit).contains(func),
            "expected `{func}`: {}",
            stdout(&emit)
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 17, "string_build fixture main computes 17");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native string_build link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for the native float subset: native-compile a
/// program whose `main` uses `f64`/`f32` arithmetic, all six comparisons, and the
/// `to_f32`/`to_f64` conversions (but keeps an all-i64 signature), then assert the
/// linked `.exe`'s exit code equals the interpreter's `main` result (mod 256).
/// This proves SSE scalar float codegen — including single-precision f32 rounding
/// and the NaN-aware ordered compares — agrees bit-for-bit with the interpreter.
/// Gated on `rust-lld` + `kernel32.lib` like the other native parity tests.
#[test]
pub(crate) fn native_floats_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_floats.lby");
    let out = std::env::temp_dir().join("lullaby_native_floats_parity.exe");

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // `main` keeps an all-i64 signature and uses only float locals, so it is
    // eligible for native codegen despite the f64/f32 internals.
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 9, "floats fixture main computes 9");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native floats link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native exit code must equal the interpreter result (mod 256)"
    );
}

/// Best-effort execution parity for the `run_f32.lby` fixture specifically: it
/// exercises f32 precision loss (2^24 + 1 rounding back to 2^24) alongside f64,
/// which only agrees with the interpreter if the native f32 ops are done in
/// single precision. Gated like the other native parity tests.
#[test]
pub(crate) fn native_f32_precision_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_f32.lby");
    let out = std::env::temp_dir().join("lullaby_native_f32_parity.exe");

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled main"),
        "expected `main` compiled: {}",
        stdout(&emit)
    );

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(interp, 3, "run_f32 fixture main computes 3");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native f32 precision link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "native f32 exit code must equal the interpreter result (mod 256)"
    );
}

/// Functional parity for a **`struct` with a `string` field** (a one-level
/// heap-typed field). `native_struct_string_field.lby` builds a `Rec { name string,
/// id i64 }`, reads the string field, derives lengths, passes the struct by value
/// across function boundaries, and rebuilds it (value semantics: a copy shares the
/// immutable string pointer). Every function must compile natively (not skip), the
/// interpreter result agrees across AST/IR/bytecode, and — when a native exe is
/// produced (direct PE needs no linker) — the exit code equals the interpreter's
/// `main` result (31).
#[test]
pub(crate) fn native_struct_string_field_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_struct_string_field.lby");
    let out = std::env::temp_dir().join("lullaby_native_struct_string_field.exe");

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let emit_out = stdout(&emit);
    for name in ["label", "name_len", "relabel", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively (string-field struct): {emit_out}"
        );
    }

    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{backend}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(
            interp, 31,
            "{backend}: struct-string fixture main computes 31"
        );
    }

    if !out.is_file() {
        eprintln!("no native exe produced; skipping struct-string-field run");
        return;
    }
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        31_i32.rem_euclid(256),
        "native struct-string exit code must equal the interpreter result (mod 256)"
    );
}

/// RC / free-list reclamation of a **struct with a `string` field** allocated per
/// iteration. `sweep` calls a user function, so it is NOT arena-routed and keeps the
/// RC codegen: each iteration constructs `Rec(to_string(i) + "…", i)` (allocating one
/// string record — the concat operands are reclaimed by the ownership-aware concat)
/// and the borrow-only struct temp's `string` field is `rc_dec`'d on the loop edge,
/// freeing the record onto the free list for the next iteration to reuse. Over
/// 300_001 iterations that is far more than the fixed 1 MiB heap, so a correct exit
/// code equal to the interpreter's result proves the recursive drop reclaims exactly
/// once (no leak — a leak exhausts the heap; no double-free — that corrupts the free
/// list). The struct is stack-flattened, so only the string field is heap.
#[test]
pub(crate) fn native_struct_string_field_rc_reclaim_execution_parity_when_linkable() {
    let fixture =
        workspace_root().join("tests/fixtures/valid/run_rc_struct_string_field_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_struct_string_rc_reclaim.exe");

    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(
        interp, 45_004_838_906,
        "sum of len(r.name)+r.id over Rec(to_string(i)+\"!!!!!!!!!!\", i) for i in 0..=300000"
    );

    if !out.is_file() {
        eprintln!("no native exe produced; skipping struct-string RC reclaim run");
        return;
    }
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    let expected = if cfg!(windows) {
        interp as i32
    } else {
        interp.rem_euclid(256) as i32
    };
    assert_eq!(
        exit, expected,
        "native struct-string RC-reclaimed loop must complete with the interpreter's \
         result (a crash here means the heap exhausted — reclamation regressed)"
    );
}

/// Arena reclamation of a **struct with a `string` field**. `build_len` is a
/// provably-local heap-using LEAF (scalar return, no user calls, no loop) that
/// constructs a `Rec` with a fresh `string` field kept local (read only via `len`),
/// so it routes through a function-scoped arena and rewinds the bump pointer on
/// return. `main` calls it 300_001 times; each call's region — including the struct's
/// string record — is reclaimed at return, so the process completes in the fixed
/// 1 MiB heap despite allocating far more in aggregate. `rc_free` no-ops in arena
/// mode, so the recursive drop-glue and the arena rewind coexist without double-free.
/// This is the arena analogue of the RC reclaim test above.
#[test]
pub(crate) fn native_struct_string_field_arena_reclaim_execution_parity_when_linkable() {
    let fixture =
        workspace_root().join("tests/fixtures/valid/run_arena_struct_string_field_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_struct_string_arena_reclaim.exe");

    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
    assert_eq!(
        interp, 45_004_838_906,
        "sum of build_len(i) over Rec(to_string(i)+\"!!!!!!!!!!\", i) for i in 0..=300000"
    );

    if !out.is_file() {
        eprintln!("no native exe produced; skipping struct-string arena reclaim run");
        return;
    }
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    let expected = if cfg!(windows) {
        interp as i32
    } else {
        interp.rem_euclid(256) as i32
    };
    assert_eq!(
        exit, expected,
        "native struct-string arena-reclaimed loop must complete with the interpreter's \
         result (a crash here means the per-call arena reset regressed)"
    );
}

/// Best-effort execution parity for **native `for c in s` over multi-byte UTF-8**.
/// The `for c in s` loop is lowered to a forward byte cursor (O(N)) that decodes
/// each code point in place; this fixture iterates strings whose characters span
/// all four UTF-8 widths (ASCII, 2-byte `é`, 3-byte `☕`/`日本語`, 4-byte `🎉`) and
/// sums their `char_code`s, so any decode error — a wrong scalar value, a
/// miscounted iteration, or a desynced cursor — diverges from the interpreters
/// (which use Rust's real UTF-8 decoding as ground truth). Asserts `sum_codes`,
/// `count_chars`, and `main` compile natively and — when linkable — the `.exe`
/// exit code equals the interpreter result.
#[test]
pub(crate) fn native_string_utf8_foreach_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_string_utf8_foreach.lby");
    let out = std::env::temp_dir().join("lullaby_native_string_utf8_foreach_parity.exe");

    ensure_msvc_env();

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // The byte-cursor `for c in s` loops must all compile natively (a regression
    // that demoted them would hide the decode path behind the interpreter).
    let emit_out = stdout(&emit);
    for name in ["sum_codes", "count_chars", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively (byte-cursor string loop), got: {emit_out}"
        );
    }

    // Interpreter ground truth (real UTF-8 decoding), identical across every backend.
    let mut expected: Option<i64> = None;
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{backend}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        match expected {
            None => expected = Some(interp),
            Some(prev) => assert_eq!(
                prev, interp,
                "{backend}: UTF-8 foreach interpreters must agree ({prev} vs {interp})"
            ),
        }
    }
    let expected = expected.expect("at least one interpreter run");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native UTF-8 foreach link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit as i64,
        expected.rem_euclid(256),
        "native byte-cursor `for c in s` must decode multi-byte UTF-8 exactly like the \
         interpreter (exit {exit} vs interpreter {expected})"
    );
}
