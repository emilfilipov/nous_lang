//! CLI integration tests, part 3 (native x86-64 backend: link-and-run parity,
//! aggregates, strings, RC reclamation, FFI). Split out of tests/cli.rs.

use crate::*;
use std::process::Command;

#[test]
pub(crate) fn native_reports_no_eligible_functions() {
    let scratch = ScratchDir::new("native_reports_no_eligible_functions");
    // `main` uses `to_string(f64)` (dtoa, deferred), so it skips and nothing is
    // eligible for native. (Plain string values are now in the native subset, so
    // the not-eligible example uses the still-deferred float `to_string`.)
    let source = "fn main -> i64\n    len(to_string(1.5))\n";
    let tmp = scratch.join("lullaby_native_none.lby");
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
    let scratch = ScratchDir::new("native_freestanding_direct_pe_runs");
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = scratch.join("lullaby_direct_pe.exe");
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
    let scratch = ScratchDir::new("native_freestanding_direct_pe_heap_runs");
    let fixture = workspace_root().join("tests/fixtures/valid/native_strings.lby");
    let out = scratch.join("lullaby_direct_pe_heap.exe");
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
    let scratch = ScratchDir::new("native_execution_parity_when_linkable");
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = scratch.join("lullaby_native_parity.exe");

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
    let scratch = ScratchDir::new("native_signed_div_overflow_parity_when_l");
    let fixture = workspace_root().join("tests/fixtures/valid/run_div_overflow.lby");
    let out = scratch.join("lullaby_native_div_overflow_parity.exe");

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
    let scratch = ScratchDir::new("native_aggregates_execution_parity_when_");
    let fixture = workspace_root().join("tests/fixtures/valid/native_aggregates.lby");
    let out = scratch.join("lullaby_native_aggregates_parity.exe");

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
    let scratch = ScratchDir::new("native_fat_array_execution_parity_when_l");
    let fixture = workspace_root().join("tests/fixtures/valid/native_fat_array.lby");
    let out = scratch.join("lullaby_native_fat_array_parity.exe");

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
    let scratch = ScratchDir::new("native_fat_array_f64_execution_parity_wh");
    let fixture = workspace_root().join("tests/fixtures/valid/native_fat_array_f64.lby");
    let out = scratch.join("lullaby_native_fat_array_f64_parity.exe");

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
    let scratch = ScratchDir::new("native_many_args_execution_parity_when_l");
    let fixture = workspace_root().join("tests/fixtures/valid/native_many_args.lby");
    let out = scratch.join("lullaby_native_many_args_parity.exe");

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
    let scratch = ScratchDir::new("native_aggregate_boundary_execution_pari");
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
        let out = scratch.join(format!("lullaby_{name}_parity.exe"));

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
    let scratch = ScratchDir::new("native_enum_match_execution_parity_when_");
    for (name, expected) in [
        ("native_enum_option", 49i64),
        ("native_enum_result", 44),
        ("native_enum_user", 206),
    ] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = scratch.join(format!("lullaby_{name}_parity.exe"));

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
    let scratch = ScratchDir::new("native_overflow_arith_execution_parity_w");
    ensure_msvc_env();
    let fixture = workspace_root().join("tests/fixtures/valid/run_overflow_codegen.lby");
    let out = scratch.join("lullaby_native_overflow_codegen.exe");

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
    let scratch = ScratchDir::new("wasm_overflow_arith_execution_parity_wit");
    let fixture = workspace_root().join("tests/fixtures/valid/run_overflow_codegen.lby");
    let out = scratch.join("lullaby_wasm_overflow_codegen.wasm");

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

    let runner = scratch.join("lullaby_wasm_overflow_runner.js");
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
    let scratch = ScratchDir::new("wasm_math_builtins_execution_parity_with");
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_math_builtins.lby");
    let out = scratch.join("lullaby_wasm_math_builtins.wasm");

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

    let runner = scratch.join("lullaby_wasm_math_builtins_runner.js");
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
    let scratch = ScratchDir::new("native_string_collections_execution_pari");
    ensure_msvc_env();
    for (name, expected) in [
        ("native_list_string", 31i64),
        ("native_map_string", 23),
        ("native_result_string", 52),
    ] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = scratch.join(format!("lullaby_{name}_parity.exe"));

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
    let scratch = ScratchDir::new("native_list_execution_parity_when_linkab");
    ensure_msvc_env();
    for name in ["native_list_build", "native_list_value_semantics"] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = scratch.join(format!("lullaby_{name}_parity.exe"));

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
    let scratch = ScratchDir::new("native_list_struct_execution_parity_when");
    ensure_msvc_env();
    let name = "native_list_struct";
    let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
    let out = scratch.join(format!("lullaby_{name}_parity.exe"));

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
    let scratch = ScratchDir::new("native_map_execution_parity_when_linkabl");
    ensure_msvc_env();
    for name in ["native_map_build", "native_map_value_semantics"] {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = scratch.join(format!("lullaby_{name}_parity.exe"));

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
    let scratch = ScratchDir::new("native_control_flow_execution_parity_whe");
    let fixture = workspace_root().join("tests/fixtures/valid/native_control_flow.lby");
    let out = scratch.join("lullaby_native_control_flow_parity.exe");

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
    let scratch = ScratchDir::new("native_strings_execution_parity_when_lin");
    let fixture = workspace_root().join("tests/fixtures/valid/native_strings.lby");
    let out = scratch.join("lullaby_native_strings_parity.exe");

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
    let scratch = ScratchDir::new("native_string_ops_execution_parity_when_");
    let fixture = workspace_root().join("tests/fixtures/valid/native_string_ops.lby");
    let out = scratch.join("lullaby_native_string_ops_parity.exe");

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
    let scratch = ScratchDir::new("native_string_case_execution_parity_when");
    let fixture = workspace_root().join("tests/fixtures/valid/run_string_case.lby");
    let out = scratch.join("lullaby_native_string_case_parity.exe");
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
    let scratch = ScratchDir::new("native_sqrt_execution_parity_when_linkab");
    let fixture = workspace_root().join("tests/fixtures/valid/run_sqrt.lby");
    let out = scratch.join("lullaby_native_sqrt_parity.exe");
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
    let scratch = ScratchDir::new("native_abs_execution_parity_when_linkabl");
    let fixture = workspace_root().join("tests/fixtures/valid/run_abs.lby");
    let out = scratch.join("lullaby_native_abs_parity.exe");
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
    let scratch = ScratchDir::new("native_min_max_execution_parity_when_lin");
    let fixture = workspace_root().join("tests/fixtures/valid/run_minmax.lby");
    let out = scratch.join("lullaby_native_minmax_parity.exe");
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
    let scratch = ScratchDir::new("native_gcd_execution_parity_when_linkabl");
    let fixture = workspace_root().join("tests/fixtures/valid/run_gcd.lby");
    let out = scratch.join("lullaby_native_gcd_parity.exe");
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
    let scratch = ScratchDir::new("native_sign_clamp_execution_parity_when_");
    let fixture = workspace_root().join("tests/fixtures/valid/run_signclamp.lby");
    let out = scratch.join("lullaby_native_signclamp_parity.exe");
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
    let scratch = ScratchDir::new("native_string_build_execution_parity_whe");
    let fixture = workspace_root().join("tests/fixtures/valid/native_string_build.lby");
    let out = scratch.join("lullaby_native_string_build_parity.exe");

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
    let scratch = ScratchDir::new("native_floats_execution_parity_when_link");
    let fixture = workspace_root().join("tests/fixtures/valid/native_floats.lby");
    let out = scratch.join("lullaby_native_floats_parity.exe");

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
    let scratch = ScratchDir::new("native_f32_precision_execution_parity_wh");
    let fixture = workspace_root().join("tests/fixtures/valid/run_f32.lby");
    let out = scratch.join("lullaby_native_f32_parity.exe");

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
    let scratch = ScratchDir::new("native_struct_string_field_execution_par");
    let fixture = workspace_root().join("tests/fixtures/valid/native_struct_string_field.lby");
    let out = scratch.join("lullaby_native_struct_string_field.exe");

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
    let scratch = ScratchDir::new("native_struct_string_field_rc_reclaim_ex");
    let fixture =
        workspace_root().join("tests/fixtures/valid/run_rc_struct_string_field_reclaim.lby");
    let out = scratch.join("lullaby_native_struct_string_rc_reclaim.exe");

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
    let scratch = ScratchDir::new("native_struct_string_field_arena_reclaim");
    let fixture =
        workspace_root().join("tests/fixtures/valid/run_arena_struct_string_field_reclaim.lby");
    let out = scratch.join("lullaby_native_struct_string_arena_reclaim.exe");

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

/// Execution parity for **native monomorphization of heap-`T` user generics** —
/// `Box<string>`, `Pair<string, i64>`, `Opt<string>` each instantiated with a
/// `string` type argument (a one-level heap field/payload after substitution).
/// Exercises construction, string field/payload read, value-semantic copy, and
/// passing/returning a heap-`T` generic value across function boundaries. Every
/// function must compile natively (a monomorphized `Box<string>` has the layout of
/// a hand-written string-field struct), and the `.exe` exit code must equal the
/// interpreter result (mod 256) — value-neutral.
#[test]
pub(crate) fn native_generic_heap_string_execution_parity_when_linkable() {
    let scratch = ScratchDir::new("native_generic_heap_string_execution_par");
    let fixture = workspace_root().join("tests/fixtures/valid/native_generic_heap_string.lby");
    let out = scratch.join("lullaby_native_generic_heap_string.exe");

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
    for name in ["box_len", "rebox", "pair_sum", "opt_len", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively (heap-`T` generic): {emit_out}"
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
            interp, 63,
            "{backend}: heap-`T` generic fixture main computes 63"
        );
    }

    if !out.is_file() {
        eprintln!("no native exe produced; skipping heap-`T` generic run");
        return;
    }
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        63_i32.rem_euclid(256),
        "native heap-`T` generic exit code must equal the interpreter result (mod 256)"
    );
}

/// RC / free-list reclamation of monomorphized heap-`T` generic values allocated
/// per iteration. `sweep` calls a user function, so it keeps the RC codegen: each
/// iteration builds a `Box<string>` and a `Pair<string, i64>`, and the recursive
/// struct-string drop-glue `rc_dec`s each instantiation's `string` field on the
/// loop edge, freeing the record for reuse. Over 300_001 iterations that is far
/// more than the fixed 1 MiB heap, so a correct exit code equal to the interpreter
/// proves the monomorphized instantiation's string field is reclaimed exactly once
/// (a leak exhausts the heap; a double-free corrupts the free list).
#[test]
pub(crate) fn native_generic_heap_string_rc_reclaim_execution_parity_when_linkable() {
    let scratch = ScratchDir::new("native_generic_heap_string_rc_reclaim_ex");
    let fixture =
        workspace_root().join("tests/fixtures/valid/run_rc_generic_heap_string_reclaim.lby");
    let out = scratch.join("lullaby_native_generic_heap_string_rc_reclaim.exe");

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
        interp, 45_009_527_812,
        "sum of len(b.value)+len(p.key)+p.value over Box<string>/Pair<string,i64> for i in 0..=300000"
    );

    if !out.is_file() {
        eprintln!("no native exe produced; skipping heap-`T` generic RC reclaim run");
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
        "native heap-`T` generic RC-reclaimed loop must complete with the interpreter's \
         result (a crash here means the heap exhausted — reclamation regressed)"
    );
}

/// Arena reclamation of monomorphized heap-`T` generic values. Each `*_part`
/// helper is a provably-local heap-using LEAF that builds a heap-`T` generic value
/// (`Box<string>`, `Pair<string, i64>`, `Opt<string>`) kept local, so it routes
/// through a function-scoped arena and rewinds the bump pointer on return. `main`
/// calls each 300_001 times; every call's region — including the instantiation's
/// string record — is reclaimed at return, so the process completes in the fixed
/// 1 MiB heap. The `Opt<string>` leaf proves the enum-payload string is reclaimed
/// too (an enum local has no per-temp RC drop-glue, so ONLY the arena rewind bounds
/// it). `rc_free` no-ops in arena mode, so drop-glue and rewind coexist without a
/// double-free. The arena analogue of the RC reclaim test above.
#[test]
pub(crate) fn native_generic_heap_string_arena_reclaim_execution_parity_when_linkable() {
    let scratch = ScratchDir::new("native_generic_heap_string_arena_reclaim");
    let fixture =
        workspace_root().join("tests/fixtures/valid/run_arena_generic_heap_string_reclaim.lby");
    let out = scratch.join("lullaby_native_generic_heap_string_arena_reclaim.exe");

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
        interp, 45_014_216_718,
        "sum of box_part(i)+pair_part(i)+opt_part(i) for i in 0..=300000"
    );

    if !out.is_file() {
        eprintln!("no native exe produced; skipping heap-`T` generic arena reclaim run");
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
        "native heap-`T` generic arena-reclaimed loop must complete with the interpreter's \
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
    let scratch = ScratchDir::new("native_string_utf8_foreach_parity_when_l");
    let fixture = workspace_root().join("tests/fixtures/valid/native_string_utf8_foreach.lby");
    let out = scratch.join("lullaby_native_string_utf8_foreach_parity.exe");

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

/// Best-effort execution parity for **native monomorphization of user-defined
/// generic types with SCALAR type arguments** (A1 stage-1 native). The fixture
/// declares generic structs (`Box<T>`, `Pair<K, V>`) and generic enums (`Opt<T>`,
/// `Either<L, R>`) and instantiates each with scalar arguments (`i64`/`bool`/`f64`),
/// exercising construction, field read, value-semantic field write, `match`,
/// value-semantic copy, and passing/returning generic values across boundaries.
/// Every function must compile natively (monomorphization resolves each
/// instantiation to a concrete scalar-only layout). The value-neutrality gate: the
/// three interpreters must agree AND the native `.exe` exit code must equal that
/// result (mod 256) — a monomorphized `Box<i64>` is byte-identical to the erased
/// `Box<i64>` the interpreters run. Gated on `rust-lld` + `kernel32.lib`.
#[test]
pub(crate) fn native_generic_scalar_execution_parity_when_linkable() {
    let scratch = ScratchDir::new("native_generic_scalar_execution_parity_w");
    ensure_msvc_env();
    let fixture = workspace_root().join("tests/fixtures/valid/native_generic_scalar.lby");
    let out = scratch.join("lullaby_native_generic_scalar_parity.exe");

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
    // Every generic instantiation resolves to a scalar-only layout, so all
    // functions compile natively and nothing is skipped.
    let emit_out = stdout(&emit);
    for name in [
        "unbox",
        "rewrap",
        "pair_score",
        "opt_or",
        "flag_or",
        "either_to_i",
        "f64box_hit",
        "main",
    ] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile natively (scalar generic monomorphization), got: {emit_out}"
        );
    }
    assert!(
        !emit_out.contains("skipped"),
        "no scalar generic function should be skipped: {emit_out}"
    );

    // Interpreter ground truth, identical across every backend (generics erase, so
    // a divergence here is itself a finding independent of native).
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
                "{backend}: generic-scalar interpreters must agree ({prev} vs {interp})"
            ),
        }
    }
    let expected = expected.expect("at least one interpreter run");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib (via the LIB env var) not available; \
             skipping native generic-scalar link+run parity"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit as i64,
        expected.rem_euclid(256),
        "native monomorphized generic value must equal the interpreter's erased value \
         (exit {exit} vs interpreter {expected})"
    );
}

/// Closures (native scalar subset: integer AND float captures/parameters/returns,
/// any parameter count, direct non-escaping call) compile natively and run
/// byte-identically to the interpreters. Each fixture takes the DEFAULT native
/// command's direct-PE path (no external linker — the closure heap block is
/// allocated in-house), so this needs neither `rust-lld` nor `kernel32.lib`. The
/// `reclaim`/`float_reclaim` fixtures allocate a closure per iteration across many
/// iterations; their correct result (rather than a heap-exhaustion trap) proves the
/// arena per-iteration sub-region reclaims each closure block — bounded heap. A float
/// capture does not change that argument: the block's shape is unchanged (one raw
/// word per capture), only what the words hold.
///
/// The float/multi-parameter fixtures pin the two ABI hazards of the hidden env
/// pointer, which shifts every parameter to effective position `i + 1`:
///
/// - `mixed_capture`, `combo`, `float_spill` — a float parameter's register is fixed
///   by POSITION (a float at position 2 takes `xmm2`), not by how many floats precede
///   it. Injecting the sequential-XMM bug (reading `xmm{pos-1}`) makes all three fail.
///   Note a SINGLE-float-parameter closure cannot pin this: the caller stages that
///   argument through `xmm0`, so a callee wrongly reading `xmm0` still sees the right
///   value. These fixtures interleave classes so the coincidence breaks.
/// - `four_params`, `six_params`, `float_spill`, `combo` — a 4th parameter is the 5th
///   argument and spills to the stack. Injecting a wrong spill displacement (`40`
///   instead of `48`) makes exactly these four fail.
#[test]
pub(crate) fn native_closures_direct_pe_run_parity() {
    let scratch = ScratchDir::new("native_closures_direct_pe_run_parity");
    let fixtures = [
        "native_closure_scalar",
        "native_closure_multi_capture",
        "native_closure_loop",
        "native_closure_reclaim",
        // Scalar completeness: floats and past-the-register-file parameter counts.
        "native_closure_float_capture",
        "native_closure_mixed_capture",
        "native_closure_four_params",
        "native_closure_six_params",
        "native_closure_float_spill",
        "native_closure_f32",
        "native_closure_combo",
        "native_closure_float_reclaim",
        // Closures stage 3a: a closure passed as a NON-ESCAPING higher-order
        // argument to a callee that calls it. `run_closures` uses the closure both
        // as a HOF argument (`apply(add_n, 5)`) and as a direct call (`add_n(2)`);
        // the others pin a non-capturing closure argument, a callee that invokes its
        // fn parameter multiple times, and a FLOAT closure argument (the callee
        // returns i64, since a float-RETURNING user call is orthogonally deferred).
        "run_closures",
        "native_hof_noncapture",
        "native_hof_multi_call",
        "native_hof_float_arg",
    ];
    for name in fixtures {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = scratch.join(format!("lullaby_{name}.exe"));
        let obj = out.with_extension("obj");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&obj);

        let emit = lullaby()
            .args([
                "native",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{name}: {}", stderr(&emit));
        let listing = stdout(&emit);
        assert!(
            listing.contains("direct PE, no linker"),
            "{name}: closure build must take the direct-PE path: {listing}"
        );
        assert!(
            !obj.is_file(),
            "{name}: direct-PE path must not write an object"
        );

        // Interpreter ground truth for `main`, from ALL THREE engines. Native is
        // compared against a value every interpreter agrees on, so this cannot pass
        // by matching one engine that is itself wrong.
        let mut interp: Option<i64> = None;
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
            assert!(run.status.success(), "{name} ({backend}): {}", stderr(&run));
            let value: i64 = stdout(&run)
                .trim()
                .parse()
                .unwrap_or_else(|e| panic!("{name} ({backend}): interpreter i64: {e}"));
            match interp {
                None => interp = Some(value),
                Some(first) => assert_eq!(
                    value, first,
                    "{name}: interpreter divergence — {backend} computes {value}, \
                     the first engine computed {first}"
                ),
            }
        }
        let interp = interp.expect("at least one backend ran");

        // Run the in-house `.exe` and compare exit codes (all fixtures return < 256).
        let exe = Command::new(&out).output().expect("run direct pe exe");
        let exit = exe.status.code().expect("native exit code");
        assert_eq!(
            exit,
            interp.rem_euclid(256) as i32,
            "{name}: native closure exit must equal the interpreter result"
        );
    }
}

/// Deferred closure shapes still skip cleanly to the interpreters (`L0339`), never
/// miscompiled, and each still runs correctly on the interpreters.
///
/// These are the **skip pins**: they fix the boundary of the native closure subset in
/// place. Scalar completeness (floats, any parameter count) widened what compiles, so
/// each escape hatch is re-pinned HERE with a float-capturing closure — proving the
/// widening did not accidentally admit an escaping or heap shape along with the
/// floats. What remains deferred:
///
/// - a heap capture (`string`; `list`/`map`/aggregate resolve the same way),
/// - a returned/escaping closure,
/// - a closure whose body calls a user/`extern` function,
/// - a mutable/rebound closure local,
/// - a closure bound from a factory result rather than a direct literal,
/// - a higher-order callee that is NOT call-only — it reads its fn parameter as a
///   value (`native_hof_leaky_skip`) or passes it onward (`native_hof_onward_skip`,
///   the documented single-level frontier of closures stage 3a).
///
/// A NON-escaping higher-order argument (`apply(f, x)`) is no longer here — it
/// compiles and is pinned by `native_closures_direct_pe_run_parity`. `native_closure_float_hof`
/// stays a skip because it needs a float-RETURNING user call, an orthogonally
/// deferred feature, not because of the higher-order argument itself.
#[test]
pub(crate) fn native_closure_deferred_shapes_skip() {
    // (fixture, interpreter result)
    let skips = [
        ("native_closure_string_capture", 42_i64),
        ("run_closures_returned", 134),
        // The same escape hatches, re-pinned with FLOAT-capturing closures.
        ("native_closure_float_hof", 61),
        ("native_closure_float_body_call", 62),
        ("native_closure_float_rebind", 63),
        ("native_closure_factory_bound", 64),
        // Closures stage 3a refusal boundaries: a callee whose fn parameter escapes
        // (read as a value, or passed onward) is not a higher-order parameter, so
        // both it and the caller demote cleanly and still run on the interpreters.
        ("native_hof_leaky_skip", 12),
        ("native_hof_onward_skip", 42),
    ];
    for (name, expected) in skips {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));

        // `lullaby native` fails cleanly with L0339 and lists `main` as skipped.
        let native = lullaby()
            .args([
                "native",
                "--verbose",
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !native.status.success(),
            "{name}: a deferred closure program must not compile natively"
        );
        let rendered = format!("{}{}", stdout(&native), stderr(&native));
        assert!(
            rendered.contains("L0339"),
            "{name}: expected L0339: {rendered}"
        );
        assert!(
            rendered.contains("skipped main"),
            "{name}: expected `main` in the skip listing: {rendered}"
        );

        // It still runs correctly on the interpreters.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{name}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(interp, expected, "{name}: interpreter computes {expected}");
    }
}
