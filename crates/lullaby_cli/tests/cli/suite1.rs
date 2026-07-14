//! CLI integration tests, part 1 (check/fmt/run-across-backends/example fixtures).
//! Split out of tests/cli.rs; shares its helpers via `use crate::*`.

use crate::*;
use std::process::Command;

#[test]
pub(crate) fn checks_valid_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok:"));
}

#[test]
pub(crate) fn fmt_prints_canonical_source_and_is_idempotent() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_match.lby");
    let first = lullaby()
        .args(["fmt", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(first.status.success(), "{first:?}");
    let formatted = stdout(&first);
    assert!(formatted.contains("match s"), "{formatted}");

    // Formatting an already-canonical fixture is a no-op, and re-formatting the
    // output through a temp file yields identical text.
    let tmp = std::env::temp_dir().join("lullaby_fmt_roundtrip.lby");
    std::fs::write(&tmp, &formatted).expect("write temp");
    let second = lullaby()
        .args(["fmt", tmp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(formatted, stdout(&second));
}

#[test]
pub(crate) fn fmt_check_passes_on_canonical_fixture() {
    // Fixtures are kept canonical, so --check succeeds with no output.
    let fixture = workspace_root().join("tests/fixtures/valid/run_showcase.lby");
    let output = lullaby()
        .args(["fmt", "--check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
pub(crate) fn fmt_check_fails_on_unformatted_input() {
    // A deliberately non-canonical file (extra spacing collapses on format).
    let tmp = std::env::temp_dir().join("lullaby_fmt_check_bad.lby");
    std::fs::write(&tmp, "fn main -> i64\n    1 +  2\n").expect("write temp");
    let output = lullaby()
        .args(["fmt", "--check", tmp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not canonically formatted"),
        "{}",
        stderr(&output)
    );
}

#[test]
pub(crate) fn fmt_rejects_non_lby_extension() {
    let output = lullaby()
        .args(["fmt", "does_not_exist.txt"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("L0001"), "{}", stderr(&output));
}

#[test]
pub(crate) fn checks_valid_fixture_as_json() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.lby");
    let output = lullaby()
        .args([
            "check",
            "--format",
            "json",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        stdout(&output).trim(),
        "{\"status\":\"ok\",\"diagnostics\":[]}"
    );
}

#[test]
pub(crate) fn prints_online_docs_url() {
    let output = lullaby().args(["docs"]).output().expect("run cli");

    assert!(output.status.success(), "{output:?}");
    let stdout = stdout(&output);
    assert!(stdout.contains("docs:"), "{stdout}");
    assert!(stdout.contains("https://lullaby-lang.org"), "{stdout}");
}

#[test]
pub(crate) fn prints_examples_path() {
    let output = lullaby().args(["examples"]).output().expect("run cli");

    assert!(output.status.success(), "{output:?}");
    let stdout = stdout(&output);
    assert!(stdout.contains("examples:"), "{stdout}");
    assert!(stdout.contains("valid"), "{stdout}");
}

#[test]
pub(crate) fn runs_user_facing_valid_examples() {
    let root = workspace_root();
    let examples_dir = root.join("examples/valid");
    let mut examples = std::fs::read_dir(&examples_dir)
        .expect("examples directory")
        .map(|entry| entry.expect("example entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("lby"))
        .collect::<Vec<_>>();
    examples.sort();
    assert!(!examples.is_empty(), "expected user-facing examples");

    for example in examples {
        let output = lullaby()
            .args(["run", example.to_str().expect("example path")])
            .current_dir(&root)
            .output()
            .expect("run example");
        assert!(output.status.success(), "{example:?}\n{output:?}");
    }
    let _ = std::fs::remove_file(root.join("lullaby_example_io.txt"));
}

#[test]
pub(crate) fn runs_standard_streams_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_streams.lby");
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
        let stdout = stdout(&output);
        let stderr = stderr(&output);
        assert!(
            stdout.contains("Hello, world") && stdout.contains("partial line"),
            "{backend} stdout: {stdout}"
        );
        // Warnings go to stderr, not stdout, and are separately observable.
        assert!(stderr.contains("heads up"), "{backend} stderr: {stderr}");
        assert!(!stdout.contains("heads up"), "{backend} stdout: {stdout}");
    }
}

#[test]
pub(crate) fn runs_modulo_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_modulo.lby");
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
        // `17 % 5 = 2`, `-17 % 5 = -2` (sign of dividend), `100 %= 7 -> 2`, and the
        // fizzbuzz score using `%` over 1..=20 is 14. Every backend must agree.
        assert_eq!(
            stdout(&output).replace("\r\n", "\n").trim(),
            "2\n-2\n2\n14",
            "{backend} stdout mismatch"
        );
    }
}

#[test]
pub(crate) fn runs_for_in_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for_in.lby");
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
        // `for x in` over an array sums to 50; over a string's chars counts 5
        // vowels in "education"; over a list sums 10+20+30 = 60.
        assert_eq!(
            stdout(&output).replace("\r\n", "\n").trim(),
            "50\n5\n60",
            "{backend} stdout mismatch"
        );
    }
}

#[test]
pub(crate) fn runs_words_count_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_words_count.lby");
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
        // words -> 4; count(banana,a)=3; count(a.b.c.d,.)=3; empty needle=0;
        // count(aaaa,aa)=2 non-overlapping; all-whitespace words=0.
        // 4*100 + 3*10 + 3 + 0 + 2 + 0 = 435.
        assert_eq!(stdout(&output).trim(), "435", "{backend} stdout mismatch");
    }
}

#[test]
pub(crate) fn runs_grouped_params_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_grouped_params.lby");
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
        // weighted(1,2,3,10) = 60; tag_len("ab","cde",4) = 2+3+4 = 9; total 69.
        // Grouped `a, b, c, scale i64` and `label, suffix string` are exactly the
        // ungrouped forms, so every backend agrees.
        assert_eq!(stdout(&output).trim(), "69", "{backend} stdout mismatch");
    }
}

#[test]
pub(crate) fn runs_sum_reduction_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_sum_reduction.lby");
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
        // Full sum 52; single 10; a[2..5] = 115; empty range 0. The native
        // backend auto-vectorizes this shape; the total must match every backend.
        assert_eq!(
            stdout(&output).replace("\r\n", "\n").trim(),
            "52\n10\n115\n0",
            "{backend} stdout mismatch"
        );
    }
}

#[test]
pub(crate) fn runs_string_ergonomics_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_string_ergonomics.lby");
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
        // `+=`/`s[i]` reverse "abc" -> "cba"; apple<banana<cherry is sorted (1);
        // "zoo"<"abc" is false (no 2); "hello"[1] is 'e' (code 101).
        assert_eq!(
            stdout(&output).replace("\r\n", "\n").trim(),
            "cba\n1\n101",
            "{backend} stdout mismatch"
        );
    }
}

#[test]
pub(crate) fn runs_array_fill_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_array_fill.lby");
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
        // `fib_table(10)=55` via a runtime-sized dp array; two of [3,7,1,9,4] are
        // >= 5; and a 4-element fill of `1` sums to 4.
        assert_eq!(
            stdout(&output).replace("\r\n", "\n").trim(),
            "55\n2\n4",
            "{backend} stdout mismatch"
        );
    }
}

#[test]
pub(crate) fn runs_negation_across_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_negate.lby");
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
        // `-7`, `-(-4)=4`, `-10+3=-7`, and `-2.5 < 0` -> logs `1`. Unary `-` works
        // on floats (a sign-bit flip), which the old `0 - x` desugar could not do.
        assert_eq!(
            stdout(&output).replace("\r\n", "\n").trim(),
            "-7\n4\n-7\n1",
            "{backend} stdout mismatch"
        );
    }
}

#[test]
pub(crate) fn rejects_modulo_on_float() {
    let fixture = workspace_root().join("tests/fixtures/invalid/modulo_on_float.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0307"));
}

#[test]
pub(crate) fn rejects_user_facing_invalid_examples() {
    let examples_dir = workspace_root().join("examples/invalid");
    let mut examples = std::fs::read_dir(&examples_dir)
        .expect("invalid examples directory")
        .map(|entry| entry.expect("example entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("lby"))
        .collect::<Vec<_>>();
    examples.sort();
    assert!(
        !examples.is_empty(),
        "expected invalid user-facing examples"
    );

    for example in examples {
        let output = lullaby()
            .args(["check", example.to_str().expect("example path")])
            .output()
            .expect("check invalid example");
        assert!(
            !output.status.success(),
            "expected invalid example to fail: {example:?}"
        );
    }
}

#[test]
pub(crate) fn runs_arithmetic_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn runs_arithmetic_fixture_with_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "ir",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn runs_arithmetic_fixture_with_bytecode_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "bytecode",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn runs_inferred_let_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_inferred_let.lby");
    for args in [
        vec!["run", fixture.to_str().expect("fixture path")],
        vec![
            "run",
            "--backend",
            "ir",
            fixture.to_str().expect("fixture path"),
        ],
        vec![
            "run",
            "--backend",
            "bytecode",
            fixture.to_str().expect("fixture path"),
        ],
    ] {
        let output = lullaby().args(args).output().expect("run cli");

        assert!(output.status.success(), "{output:?}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
    }
}

/// Functions declared without a `-> T` clause infer their return type from the
/// body (and `main` infers `i64`); the fixture computes 21 on all interpreters.
#[test]
pub(crate) fn runs_inferred_return_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_inferred_return.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(b) = backend {
            args.push("--backend".to_string());
            args.push(b.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(output.status.success(), "{backend:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "21",
            "backend {backend:?}"
        );
    }
}

/// Inferred return types flow through to native codegen: the exit code equals
/// the interpreter result (21) when the platform can link.
#[test]
pub(crate) fn native_inferred_return_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_inferred_return.lby");
    let out = std::env::temp_dir().join("lullaby_native_inferred_return.exe");
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
    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("linker unavailable; skipping native inferred-return parity");
        return;
    }
    assert!(out.is_file());
    let exe = Command::new(&out).output().expect("run exe");
    assert_eq!(exe.status.code().expect("exit"), 21);
}

/// String interpolation `"a=${expr}"` desugars to a `to_string`/`+`
/// concatenation and produces the same string on all three interpreter backends.
#[test]
pub(crate) fn runs_string_interpolation_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_string_interpolation.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(b) = backend {
            args.push("--backend".to_string());
            args.push(b.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(output.status.success(), "{backend:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "n=7 sq=49 big=0",
            "backend {backend:?}"
        );
    }
}

/// An unterminated `${` in a string interpolation is a parse error (`L0207`).
#[test]
pub(crate) fn rejects_unterminated_interpolation() {
    let fixture = workspace_root().join("tests/fixtures/invalid/interpolation_unterminated.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0207"), "{}", stderr(&output));
}

/// A function whose inferred return type is (mutually) recursive must be
/// annotated: `L0439`.
#[test]
pub(crate) fn rejects_recursive_inferred_return() {
    let fixture = workspace_root().join("tests/fixtures/invalid/inferred_return_recursive.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0439"), "{}", stderr(&output));
}

/// Inline conditional (`THEN if COND else ELSE`) — nested, in a `let`, in a
/// function body, and driving the result — computes 115 identically on all three
/// interpreter backends.
#[test]
pub(crate) fn runs_conditional_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_conditional.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(b) = backend {
            args.push("--backend".to_string());
            args.push(b.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(output.status.success(), "{backend:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "115",
            "backend {backend:?}"
        );
    }
}

/// The SSE2-vectorized element-wise map (`c[i]=a[i]±b[i]`) is bit-for-bit
/// identical to the scalar loop: when linkable, the native `.exe` exit equals the
/// interpreter result (560 mod 256).
#[test]
pub(crate) fn native_simd_map_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_simd_map.lby");
    let out = std::env::temp_dir().join("lullaby_native_simd_map.exe");
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
    assert_eq!(interp, 560);

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("linker unavailable; skipping native SIMD-map parity");
        return;
    }
    assert!(out.is_file());
    let exe = Command::new(&out).output().expect("run exe");
    // Windows preserves the full 32-bit process exit code; Unix truncates the
    // wait status to the low 8 bits. The value (560) is chosen above a byte so
    // this parity check actually exercises the wide native result.
    let expected = if cfg!(windows) {
        interp as i32
    } else {
        interp.rem_euclid(256) as i32
    };
    assert_eq!(exe.status.code().expect("exit"), expected);
}

/// The SSE2-vectorized bitwise reductions (`acc = acc ^ a[i]`, `& |`) and bitwise
/// element-wise maps (`c[i] = a[i] ^ b[i]`), plus scalar `i64` bitwise operators,
/// are bit-for-bit identical to the interpreters. When linkable, the native `.exe`
/// exit equals the interpreter result (35, chosen below a byte so the check is
/// portable across Windows/Unix exit-code truncation).
#[test]
pub(crate) fn native_bitwise_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_simd_bitwise.lby");
    let out = std::env::temp_dir().join("lullaby_native_bitwise.exe");
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
    assert_eq!(interp, 35);

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("linker unavailable; skipping native bitwise parity");
        return;
    }
    assert!(out.is_file());
    let exe = Command::new(&out).output().expect("run exe");
    assert_eq!(exe.status.code().expect("exit"), interp as i32);
}

/// Scalar `i64` bitwise codegen edge cases (`~` one's complement, arithmetic
/// `-8 >> 1 == -4`, shift-amount masking `1 << 65 == 2` / `1 << -63 == 2`, and the
/// C-like precedence chain) compile natively and match the interpreters bit-for-bit.
/// The `run_bitwise.lby` fixture returns 586; this exercises the tricky operators
/// the SIMD fixture does not.
#[test]
pub(crate) fn native_scalar_bitwise_edge_cases_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_bitwise.lby");
    let out = std::env::temp_dir().join("lullaby_native_scalar_bitwise.exe");
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
    assert_eq!(interp, 586);

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("linker unavailable; skipping native scalar-bitwise parity");
        return;
    }
    assert!(out.is_file());
    let exe = Command::new(&out).output().expect("run exe");
    // 586 exceeds a byte: Windows keeps the full exit code, Unix truncates.
    let expected = if cfg!(windows) {
        interp as i32
    } else {
        interp.rem_euclid(256) as i32
    };
    assert_eq!(exe.status.code().expect("exit"), expected);
}

/// The inline conditional desugars to a plain `if` statement, so the native
/// backend compiles it; when the platform can link, the `.exe` exit code equals
/// the interpreter result (115 mod 256).
#[test]
pub(crate) fn native_conditional_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_conditional.lby");
    let out = std::env::temp_dir().join("lullaby_native_conditional_parity.exe");

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
    assert_eq!(interp, 115, "fixture main computes 115");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld and/or kernel32.lib not available; skipping native link+run parity");
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, (interp.rem_euclid(256)) as i32);
}

/// A function whose final expression is a bare tail `if`/`else` (no trailing
/// `return`) must return the branch value natively, not the fallthrough `0`.
/// Regression test for a miscompile where a `xor rax,rax` safety epilogue
/// clobbered the tail `if`'s result.
#[test]
pub(crate) fn native_tail_if_return_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_tail_if.lby");
    let out = std::env::temp_dir().join("lullaby_native_tail_if_parity.exe");

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
        interp, 300,
        "classify(15)+classify(3)=300, total>250 so main=300"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld and/or kernel32.lib not available; skipping native tail-if parity");
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
        "native tail-if must return the branch value, not 0"
    );
}

/// `parse_i64(s) -> result<i64, string>` compiles to native and runs with the
/// same result as the interpreters: `ok(n)` / `err(message)` matched, the error
/// path builds the exact `` cannot parse `{text}` as i64 `` record (so `len(m)`
/// agrees), overflow is an `err`, and `i64::MAX` parses without a false overflow.
#[test]
pub(crate) fn native_parse_i64_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_parse_i64.lby");
    let out = std::env::temp_dir().join("lullaby_native_parse_i64_parity.exe");

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
        interp, 62,
        "40 + 2 + (-5) + is_max(1) + overflow_ok(0) + badlen(24) = 62"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld and/or kernel32.lib not available; skipping native parse_i64 parity");
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
        "native parse_i64 result must match interpreter"
    );
}

/// `split(text, sep) -> array<string>` (plus `array<string>` indexing, `len`,
/// `join`, and passing/returning it across functions) compiles to native and runs
/// with the same result as the interpreters. The `array<string>` is a heap
/// `list<string>`-layout block; `split` slices fields (empty fields for
/// leading/trailing/consecutive separators), and `join` reverses it.
#[test]
pub(crate) fn native_split_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_split.lby");
    let out = std::env::temp_dir().join("lullaby_native_split_parity.exe");

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
    assert_eq!(interp, 342134, "split/index/len/join fold");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld and/or kernel32.lib not available; skipping native split parity");
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
    assert_eq!(exit, expected, "native split result must match interpreter");
}

/// RC drop insertion (memory model stage 2) reclaims per-iteration string
/// allocations. The `to_string(i) + "…"` idiom allocates THREE records each pass —
/// the `to_string(i)` temp, the literal temp, and the concat result bound to `s` —
/// all reclaimed (the two operands by the ownership-aware `concat_own`, `s` by the
/// loop-body drop). Over 200k iterations that is far more than the fixed 1 MiB heap,
/// so a correct exit code equal to the interpreter's result proves reclamation; a
/// heap-exhaustion crash would mean it regressed.
#[test]
pub(crate) fn native_rc_string_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_rc_string_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_rc_reclaim.exe");

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
        interp, 3_088_906,
        "sum of len(to_string(i)+\"!!!!!!!!!!\") for i in 0..=200000"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld and/or kernel32.lib not available; skipping native RC reclaim parity");
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
        "native RC-reclaimed loop must complete with the interpreter's result \
         (a crash here means the heap exhausted — reclamation regressed)"
    );
}

/// Arena-first memory (stage 1) reclamation. `build_len` is a provably-local,
/// heap-using LEAF function (scalar return, no user calls, no loop): it allocates a
/// string per call that stays local, so it routes its allocations through a
/// function-scoped arena and rewinds the bump pointer on return. `main` calls it in
/// a 200_001-iteration loop, allocating far more than the fixed 1 MiB heap in
/// aggregate; because each call's region is reclaimed at return, the process
/// completes with the interpreter's exact result. WITHOUT the arena, `build_len`
/// (which has no loop, so no per-iteration RC drops) would leak every call and
/// exhaust the heap — so a correct exit code proves per-call arena reset. This is
/// the arena analogue of `native_rc_string_reclaim_execution_parity_when_linkable`.
#[test]
pub(crate) fn native_arena_string_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arena_string_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_arena_reclaim.exe");

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
        interp, 3_088_906,
        "sum of build_len(i) = len(to_string(i)+\"!!!!!!!!!!\") for i in 0..=200000"
    );

    // The direct-PE path needs no linker, so this runs unconditionally on a COFF
    // host. (It also links via rust-lld when available; either way the produced
    // exe must run.)
    if !out.is_file() {
        eprintln!("no native exe produced; skipping arena reclaim run");
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
        "native arena-reclaimed loop must complete with the interpreter's result \
         (a crash here means the heap exhausted — the per-call arena reset regressed)"
    );
}

/// Arena-first memory (stage 2) LOOP sub-region reclamation. `sum_scratch` is a
/// scalar-returning LEAF whose `for` loop allocates per-iteration scratch that
/// stays LOCAL (a fresh `string` read only by `len`, accumulating a SCALAR
/// `total`). Its heap is confined to the iteration, so the loop gets a
/// per-iteration bump-pointer rewind (a sub-region), and `sum_scratch` routes
/// through the arena. Over 300_000 iterations it allocates ~11 MB of records in
/// aggregate — far more than the fixed 1 MiB heap — yet completes with the
/// interpreter's exact result because each iteration's scratch is reclaimed at the
/// back-edge. WITHOUT the per-iteration reset the loop would exhaust the heap and
/// trap, so a correct exit code proves the sub-region reset. This is the loop
/// analogue of `native_arena_string_reclaim_execution_parity_when_linkable`.
#[test]
pub(crate) fn native_arena_loop_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arena_loop_scratch_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_arena_loop_reclaim.exe");

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
        interp, 4_688_895,
        "sum of len(to_string(i)+\"!!!!!!!!!!\") for i in 1..=300000"
    );

    if !out.is_file() {
        eprintln!("no native exe produced; skipping arena loop reclaim run");
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
        "native arena loop-reclaimed run must complete with the interpreter's result \
         (a crash here means the heap exhausted — the per-iteration sub-region reset regressed)"
    );
}

/// Heap-overflow safety (Part B). `grow` is an escaping accumulator
/// (`acc = acc + "0123456789"` where `acc` outlives the iteration): its heap can
/// NEVER be reclaimed, so a long loop fills the fixed 1 MiB bump heap. Before the
/// guard the allocator wrote past the region and the process access-violated
/// (segfaulted). Now `__lullaby_alloc` bounds-checks the bump and traps with `ud2`
/// on exhaustion — a DEFINED illegal-instruction abort (`STATUS_ILLEGAL_INSTRUCTION`
/// = `0xC000001D`), a distinct, non-zero exit status — instead of corrupting
/// memory. This asserts the clean, defined failure (the safety proof). Runs
/// unconditionally: the direct-PE path needs no linker.
#[test]
pub(crate) fn native_heap_overflow_traps_cleanly() {
    let dir = std::env::temp_dir().join("lullaby_native_overflow");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let src = dir.join("overflow.lby");
    let out = dir.join("overflow.exe");
    std::fs::write(
        &src,
        concat!(
            "fn grow n i64 -> i64\n",
            "    let acc string = \"\"\n",
            "    for i from 0 to n\n",
            "        acc = acc + \"0123456789\"\n",
            "    len(acc)\n\n",
            "fn main -> i64\n",
            "    grow(1000000)\n",
        ),
    )
    .expect("write overflow source");

    let emit = lullaby()
        .args([
            "native",
            "-o",
            out.to_str().expect("out path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // `grow` must be native-compiled (it exercises the native allocator), not
    // skipped to the interpreter — otherwise the heap guard would never run.
    let emit_stdout = stdout(&emit) + &stderr(&emit);
    assert!(
        !emit_stdout.to_lowercase().contains("grow: "),
        "grow must be native-compiled so the heap guard runs, not skipped: {emit_stdout}"
    );

    // The produced exe is a Windows PE, so only run+assert it on a Windows host
    // (the default `x86_64-pc-windows-msvc` target is not runnable elsewhere).
    if !cfg!(windows) {
        eprintln!("non-Windows host; skipping heap-overflow native run");
        return;
    }
    if !out.is_file() {
        eprintln!("no native exe produced; skipping heap-overflow run");
        return;
    }
    let exe = Command::new(&out).output().expect("run native exe");
    // The `ud2` heap-exhaustion trap surfaces as STATUS_ILLEGAL_INSTRUCTION. The key
    // property is a DEFINED trap: never a silent success (exit 0), and never an
    // out-of-bounds write / access violation (0xC0000005).
    let exit = exe
        .status
        .code()
        .expect("native exit code (Windows returns NTSTATUS)");
    assert_eq!(
        exit, 0xC000_001Du32 as i32,
        "heap exhaustion must trap cleanly with STATUS_ILLEGAL_INSTRUCTION (0xC000001D), \
         not segfault / access-violate (0xC0000005) or write out of bounds; got {exit:#010x}"
    );
}

/// RC call-argument reclamation: `len(<fresh temp>)` frees the temporary it reads.
/// `len(to_string(i))` in a loop allocates a `to_string` record each iteration that
/// `len` reads and (before this) leaked; over 300k iterations (~10 MB) in the fixed
/// 1 MiB heap, completing with the interpreter's result proves the ownership-aware
/// `len` helper reclaims it.
#[test]
pub(crate) fn native_rc_len_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_rc_len_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_rc_len_reclaim.exe");

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
        interp, 1_688_896,
        "sum of len(to_string(i)) for i in 0..=300000"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib not available; skipping native RC len-reclaim parity"
        );
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
        "len(fresh temp) must reclaim the read temporary (a crash means it leaked)"
    );
}

/// RC call-argument reclamation for the two-string builtins: `split("…", ",")` with
/// LITERAL arguments frees both fresh-temp arguments after `split` reads them (via
/// the ownership-aware indirect-call helper), on top of the recursive `array<string>`
/// drop and `split`'s internal-temp reclaim. Over 100k iterations this is far more
/// than the 1 MiB heap, so completing with the interpreter's result proves the
/// argument temporaries are reclaimed (this case previously crashed).
#[test]
pub(crate) fn native_rc_split_literal_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_rc_split_literal_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_rc_split_literal_reclaim.exe");

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
    assert_eq!(interp, 1_600_016, "(4 + 4*3) * 100001 iterations");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld/kernel32.lib unavailable; skipping native split-literal reclaim parity"
        );
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
        "fresh-temp arguments to split must be reclaimed (a crash means they leaked)"
    );
}

/// RC call-argument reclamation for the string-read ops: `substring(to_string(i),
/// 0, 1)` frees its fresh `to_string(i)` source after reading it (via
/// `__lullaby_str_read_own`), and `len` of the resulting substring frees that too —
/// so 300k iterations (~10 MB, both temporaries per pass) complete in the 1 MiB
/// heap with the interpreter's result.
#[test]
pub(crate) fn native_rc_readop_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_rc_readop_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_rc_readop_reclaim.exe");

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
        interp, 300_001,
        "len(substring(to_string(i),0,1)) == 1 for i in 0..=300000"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native readop reclaim parity");
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
        "a fresh-temp source to substring/trim/repeat/char_at must be reclaimed"
    );
}

/// RC recursive drop reclaims a uniquely-owned `array<string>` loop temporary (a
/// `split` result): each iteration allocates a `list<string>` block plus its string
/// elements plus `split`'s internal `rest` slices, all reclaimed (elements+block by
/// `__lullaby_drop_string_array`, the internal temps by `split` itself). Over 100k
/// iterations that far exceeds the 1 MiB heap, so a correct exit code proves the
/// recursive drop works (a crash would mean a leak or a double-free). The separator
/// and text are loop-invariant borrowed locals, so the only allocations are the
/// reclaimed per-iteration split records.
#[test]
pub(crate) fn native_rc_array_string_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_rc_array_string_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_rc_array_reclaim.exe");

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
    assert_eq!(interp, 1_600_016, "(4 + 4*3) * 100001 iterations");

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib not available; skipping native array reclaim parity"
        );
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
        "native array<string> recursive drop must reclaim block + elements each iteration"
    );
}

/// RC stage 2, `continue` early-exit edge: a uniquely-owned per-iteration string is
/// dropped on the `continue` edge, not only the fallthrough back-edge. The fixture
/// allocates `to_string(i) + "…"` each pass and, on every EVEN iteration, `continue`s
/// BEFORE reaching the fallthrough drop — so those 150k iterations are reclaimed
/// solely by the new continue-edge drop. Over 300k iterations (~8 MB of records) in
/// the fixed ~1 MiB heap, completing with the interpreter's result proves the
/// continue-edge drop reclaims (a heap-exhaustion crash would mean it leaked; a
/// free-list-corrupting double-free would crash or return the wrong value).
#[test]
pub(crate) fn native_rc_string_continue_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_rc_string_continue_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_rc_string_continue_reclaim.exe");

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
        interp, 4_838_906,
        "sum of len(to_string(i)+\"!!!!!!!!!!\") for i in 0..=300000, plus 1 per odd i"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld and/or kernel32.lib not available; skipping continue-reclaim parity");
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
        "native `continue`-edge drop must reclaim the owned string each iteration \
         (a crash here means the even-iteration temporaries leaked or double-freed)"
    );
}

/// RC stage 2, `break` early-exit edge (small-loop no-double-free proof): the loop
/// allocates a fresh string per pass and `break`s at `i == 4`. Iterations 0..4 fall
/// through (fallthrough drop); `i == 4` breaks (break-edge drop). Five strings, each
/// dropped exactly once — a double-free on the break edge would corrupt the free
/// list and crash or return the wrong value, so a correct exit code proves the
/// break-edge drop is balanced.
#[test]
pub(crate) fn native_rc_string_break_reclaim_execution_parity_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_rc_string_break_reclaim.lby");
    let out = std::env::temp_dir().join("lullaby_native_rc_string_break_reclaim.exe");

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
        interp, 25,
        "5 iterations of len(to_string(i)+\"xxxx\") for i in 0..=4"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld and/or kernel32.lib not available; skipping break-reclaim parity");
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
        "native `break`-edge drop must reclaim the owned string exactly once (no double-free)"
    );
}

/// An inline-conditional condition must be `bool` (`L0305`, shared with `if`).
#[test]
pub(crate) fn rejects_conditional_non_bool_condition() {
    let fixture =
        workspace_root().join("tests/fixtures/invalid/conditional_condition_not_bool.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0305"), "{}", stderr(&output));
}

/// The two branches of an inline conditional must have the same type (`L0435`).
#[test]
pub(crate) fn rejects_conditional_branch_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/conditional_branch_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0435"), "{}", stderr(&output));
}

/// An inline conditional over an aggregate/heap result type is rejected with a
/// clear diagnostic (`L0436`); an `if` statement selects those instead.
#[test]
pub(crate) fn rejects_conditional_aggregate_result() {
    let fixture = workspace_root().join("tests/fixtures/invalid/conditional_aggregate_result.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0436"), "{}", stderr(&output));
}

/// `string + char` / `string += char` concatenate the char as a one-character
/// string, identically on all three interpreter backends (fixture prints HI!?).
#[test]
pub(crate) fn runs_string_char_concat_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_string_char_concat.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(b) = backend {
            args.push("--backend".to_string());
            args.push(b.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(output.status.success(), "{backend:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "HI!?",
            "backend {backend:?}"
        );
    }
}

/// `VALUE in COLLECTION` — char-in-string, substring-in-string, and list
/// membership — computes 5110 identically on all three interpreter backends.
#[test]
pub(crate) fn runs_in_operator_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_in_operator.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(b) = backend {
            args.push("--backend".to_string());
            args.push(b.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(output.status.success(), "{backend:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "5110",
            "backend {backend:?}"
        );
    }
}

/// `in` requires a `string`/`list<T>` collection with a matching value type,
/// else `L0437`.
#[test]
pub(crate) fn rejects_in_incompatible_operands() {
    for name in ["in_unsupported_collection", "in_value_type_mismatch"] {
        let fixture = workspace_root().join(format!("tests/fixtures/invalid/{name}.lby"));
        let output = lullaby()
            .args(["check", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(!output.status.success(), "{name}: {output:?}");
        assert!(
            stderr(&output).contains("L0437"),
            "{name}: {}",
            stderr(&output)
        );
    }
}

/// String slicing `s[i:j]` / `s[i:]` / `s[:j]` / `s[:]` desugars to `substring`,
/// identically on all three interpreter backends (fixture -> hello|world|hello|rld).
#[test]
pub(crate) fn runs_string_slice_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_string_slice.lby");
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(b) = backend {
            args.push("--backend".to_string());
            args.push(b.to_string());
        }
        args.push(fixture.to_str().expect("fixture path").to_string());
        let output = lullaby().args(&args).output().expect("run cli");
        assert!(output.status.success(), "{backend:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello|world|hello|rld",
            "backend {backend:?}"
        );
    }
}

/// A slice requires a `string` target and `i64` bounds, else `L0438`.
#[test]
pub(crate) fn rejects_slice_incompatible_operands() {
    for name in ["slice_non_string_target", "slice_non_int_bound"] {
        let fixture = workspace_root().join(format!("tests/fixtures/invalid/{name}.lby"));
        let output = lullaby()
            .args(["check", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(!output.status.success(), "{name}: {output:?}");
        assert!(
            stderr(&output).contains("L0438"),
            "{name}: {}",
            stderr(&output)
        );
    }
}

/// `string + <non-char scalar>` is still a type error (`L0307`); only
/// string+string and string+char concatenate.
#[test]
pub(crate) fn rejects_string_plus_int_concat() {
    let fixture = workspace_root().join("tests/fixtures/invalid/string_plus_int.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0307"), "{}", stderr(&output));
}

#[test]
pub(crate) fn runs_parallel_map_fixture_on_all_backends() {
    // `parallel_map` runs `sq` on separate OS threads and returns the mapped
    // list in input order, so the sum is a deterministic 30 (1+4+9+16) on every
    // backend.
    let fixture = workspace_root().join("tests/fixtures/valid/run_parallel.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "30",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_sizeof_fixture_on_all_backends() {
    // `run_sizeof.lby` sums `size_of`/`align_of` of several scalars, a fixed
    // `array<i64>`, and a C-packed struct plus two `offset_of` field offsets.
    // The deterministic total is 116 on the AST, IR, and bytecode interpreters:
    // s1 = 8+4+1+4 = 17, s2 = 1+8+4 = 13, a1 = 8+2 = 10, arrsz = 3*8 = 24,
    // msz = 24+8 = 32, offs = 4+16 = 20 -> 116.
    let fixture = workspace_root().join("tests/fixtures/valid/run_sizeof.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "116",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_ptr_cast_fixture_on_all_backends() {
    // `run_ptr_cast.lby` round-trips a raw pointer through `ptr_to_int` /
    // `int_to_ptr`, then `volatile_store`s 99 and `volatile_load`s it back,
    // yielding a deterministic 99 on the AST, IR, and bytecode interpreters.
    let fixture = workspace_root().join("tests/fixtures/valid/run_ptr_cast.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "99",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_list_map_fixture_on_all_backends() {
    // `list_map` squares [1,2,3,4] -> [1,4,9,16] (sum 30) and doubles them via
    // a closure -> [2,4,6,8] (sum 20); `list_reduce` folds each. The
    // deterministic total is 50 on the AST, IR, and bytecode interpreters.
    let fixture = workspace_root().join("tests/fixtures/valid/run_list_map.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "50",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_list_higher_order_fixture_on_all_backends() {
    // `list_filter` keeps the small elements [1,2,3] (sum 6, named predicate)
    // and the big elements [4,5,6] (sum 15, closure predicate); `list_reduce`
    // folds each with a two-argument closure. The deterministic total is 21 on
    // the AST, IR, and bytecode interpreters.
    let fixture = workspace_root().join("tests/fixtures/valid/run_list_higher_order.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "21",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_sort_by_fixture_on_all_backends() {
    // `sort_by` orders [3,1,2,1] descending (closure `b - a`) -> [3,2,1,1] and
    // ascending (named `ascending`) -> [1,1,2,3]. top=3, bottom=1, mid=1, so the
    // deterministic total is 311 on the AST, IR, and bytecode interpreters. The
    // duplicated `1`s exercise the stable-sort guarantee.
    let fixture = workspace_root().join("tests/fixtures/valid/run_sort_by.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "311",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_sort_types_fixture_on_all_backends() {
    // `sort` orders a `list<i64>` (smallest 2), a `list<f64>` (2.5 lands at
    // index 1), and a `list<string>` (apple index 0, cherry index 2). The
    // deterministic total is 212 on the AST, IR, and bytecode interpreters.
    let fixture = workspace_root().join("tests/fixtures/valid/run_sort_types.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "212",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_spawn_channel_mutex_fixture_on_all_backends() {
    // Four detached `spawn`ed workers each `send(ch, v * v)`; `main` joins them
    // and sums the four received squares (order-independent â†’ 30), then a mutex
    // loop adds 1 four times (â†’ 4). The deterministic total is 34 on every
    // backend, proving spawn/channels/mutex work on AST, IR, and bytecode.
    let fixture = workspace_root().join("tests/fixtures/valid/run_spawn.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "34",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_atomic_memory_orderings_fixture_on_all_backends() {
    // Exercises the ordering-taking atomic surface and a fence: a `release`
    // store, `acquire`/`relaxed`/`seq_cst` loads, a `relaxed` fetch-and-add, an
    // `acq_rel`/`acquire` compare-and-swap, a `seq_cst` swap, a `relaxed`
    // fetch-and-sub, and a `seq_cst` fence, then sums the observed values.
    // Single-threaded, so the memory ordering never changes the value: the
    // deterministic total is 300 on AST, IR, and bytecode, proving the
    // `MemoryOrder` enum and every `atomic_*_ordered`/`fence` builtin type-check
    // and run identically across backends.
    let fixture = workspace_root().join("tests/fixtures/valid/run_atomic_orderings.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "300",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn runs_non_blocking_socket_fixture_on_all_backends() {
    // Exercises the non-blocking socket surface deterministically, with no
    // timing race: a TCP listener and a UDP socket are bound to ephemeral
    // loopback ports (`127.0.0.1:0`), put into non-blocking mode with
    // `set_nonblocking` (ok = 100 each), then probed with no pending peer.
    // `tcp_accept_nb` and `udp_recv_nb` must surface would-block as `ok(none)`
    // immediately (accept none = 1, recv none = 10) rather than block or error,
    // so the total is a fixed 211 (100 + 1 + 100 + 10) on AST, IR, and bytecode.
    let fixture = workspace_root().join("tests/fixtures/valid/run_socket_nonblocking.lby");
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
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "211",
            "{backend} result"
        );
    }
}

#[test]
pub(crate) fn compiles_fixture_to_bytecode_artifact_and_runs_it() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/valid/run_arithmetic.lby");
    let artifact = root.join("target/run_arithmetic.lbc");
    let _ = std::fs::remove_file(&artifact);

    let compile = lullaby()
        .args([
            "compile",
            "--optimize",
            "full",
            "-o",
            artifact.to_str().expect("artifact path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("compile cli");

    assert!(compile.status.success(), "{compile:?}");
    assert!(stdout(&compile).contains("compiled:"), "{compile:?}");
    let artifact_text = std::fs::read_to_string(&artifact).expect("artifact");
    assert!(artifact_text.contains("\"format\": \"lullaby-bytecode\""));
    assert!(artifact_text.contains("\"version\": 5"));
    assert!(artifact_text.contains("\"metadata\""));
    assert!(artifact_text.contains("\"function_table\""));
    assert!(artifact_text.contains("\"memory_operations\""));
    assert!(artifact_text.contains("\"instructions\""));

    let run = lullaby()
        .args(["run", artifact.to_str().expect("artifact path")])
        .output()
        .expect("run artifact cli");

    assert!(run.status.success(), "{run:?}");
    assert_eq!(stdout(&run).trim(), "42");
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn builds_fixture_to_bytecode_artifact_and_runs_it() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/valid/run_arithmetic.lby");
    let artifact = root.join("target/build_arithmetic.lbc");
    let _ = std::fs::remove_file(&artifact);

    let build = lullaby()
        .args([
            "build",
            "--optimize",
            "full",
            "-o",
            artifact.to_str().expect("artifact path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("build cli");

    assert!(build.status.success(), "{build:?}");
    assert!(stdout(&build).contains("compiled:"), "{build:?}");
    let artifact_text = std::fs::read_to_string(&artifact).expect("artifact");
    assert!(artifact_text.contains("\"format\": \"lullaby-bytecode\""));
    assert!(artifact_text.contains("\"version\": 5"));
    assert!(artifact_text.contains("\"instructions\""));

    let inspect = lullaby()
        .args(["inspect", artifact.to_str().expect("artifact path")])
        .output()
        .expect("inspect artifact cli");
    assert!(inspect.status.success(), "{inspect:?}");
    assert!(stdout(&inspect).contains("format: lullaby-bytecode"));

    let run = lullaby()
        .args(["run", artifact.to_str().expect("artifact path")])
        .output()
        .expect("run artifact cli");

    assert!(run.status.success(), "{run:?}");
    assert_eq!(stdout(&run).trim(), "42");
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn inspects_bytecode_artifact() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/valid/run_store.lby");
    let artifact = root.join("target/inspect_memory.lbc");
    let _ = std::fs::remove_file(&artifact);

    let compile = lullaby()
        .args([
            "compile",
            "-o",
            artifact.to_str().expect("artifact path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("compile cli");

    assert!(compile.status.success(), "{compile:?}");

    let inspect = lullaby()
        .args(["inspect", artifact.to_str().expect("artifact path")])
        .output()
        .expect("inspect cli");

    assert!(inspect.status.success(), "{inspect:?}");
    let inspect_stdout = stdout(&inspect);
    assert!(
        inspect_stdout.contains("format: lullaby-bytecode"),
        "{inspect_stdout}"
    );
    assert!(inspect_stdout.contains("version: 5"), "{inspect_stdout}");
    assert!(inspect_stdout.contains("entry: main"), "{inspect_stdout}");
    assert!(inspect_stdout.contains("functions:"), "{inspect_stdout}");
    assert!(
        inspect_stdout.contains("memory operations: 4"),
        "{inspect_stdout}"
    );

    let verbose = lullaby()
        .args([
            "inspect",
            "--verbose",
            artifact.to_str().expect("artifact path"),
        ])
        .output()
        .expect("inspect verbose cli");

    assert!(verbose.status.success(), "{verbose:?}");
    let verbose_stdout = stdout(&verbose);
    assert!(
        verbose_stdout.contains("memory operation: #0 main allocate"),
        "{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("memory operation: #1 main store"),
        "{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("memory operation: #2 main load"),
        "{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("memory operation: #3 main deallocate"),
        "{verbose_stdout}"
    );

    let json = lullaby()
        .args([
            "inspect",
            "--format",
            "json",
            artifact.to_str().expect("artifact path"),
        ])
        .output()
        .expect("inspect json cli");

    assert!(json.status.success(), "{json:?}");
    let json_stdout = stdout(&json);
    assert!(json_stdout.contains("\"status\":\"ok\""), "{json_stdout}");
    assert!(
        json_stdout.contains("\"format\":\"lullaby-bytecode\""),
        "{json_stdout}"
    );
    assert!(json_stdout.contains("\"functions\""), "{json_stdout}");
    assert!(
        json_stdout.contains("\"memory_operations\""),
        "{json_stdout}"
    );
    assert!(
        json_stdout.contains("\"kind\":\"allocate\""),
        "{json_stdout}"
    );
    assert!(json_stdout.contains("\"sequence\":0"), "{json_stdout}");
    assert!(json_stdout.contains("\"kind\":\"store\""), "{json_stdout}");
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn rejects_invalid_bytecode_artifact() {
    let root = workspace_root();
    let artifact = root.join("target/invalid_artifact.lbc");
    std::fs::write(
        &artifact,
        "{\"format\":\"not-lullaby\",\"version\":1,\"entry\":\"main\",\"module\":{\"functions\":[]}}",
    )
    .expect("write invalid artifact");

    let output = lullaby()
        .args(["run", artifact.to_str().expect("artifact path")])
        .output()
        .expect("run artifact cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0601 [bytecode error]"), "{stderr}");
    assert!(
        stderr.contains("unsupported bytecode artifact format"),
        "{stderr}"
    );
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn rejects_planned_unsupported_syntax_with_dedicated_diagnostic() {
    for fixture_name in ["unsupported_module.lby", "unsupported_catch.lby"] {
        let fixture = workspace_root()
            .join("tests/fixtures/invalid")
            .join(fixture_name);
        let output = lullaby()
            .args(["check", fixture.to_str().expect("fixture path")])
            .output()
            .expect("check cli");

        let stderr = stderr(&output);
        assert!(!output.status.success(), "{fixture_name}: {output:?}");
        assert!(
            stderr.contains("L0211 [parser error]"),
            "{fixture_name}: {stderr}"
        );
        assert!(
            stderr.contains("planned and is not supported"),
            "{fixture_name}: {stderr}"
        );
    }
}

#[test]
pub(crate) fn runs_multi_file_module_program_across_backends() {
    // `main.lby` imports `geometry.lby` and uses its `pub` `Point`/`dot`. The
    // merged program must run identically on every backend.
    let entry = workspace_root().join("tests/fixtures/valid/modules/main.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                entry.to_str().expect("entry path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output).trim(), "25", "{backend}");
    }
}

#[test]
pub(crate) fn rejects_cross_module_private_use_with_l0392() {
    let entry = workspace_root().join("tests/fixtures/invalid/modules_private/main.lby");
    let output = lullaby()
        .args(["check", entry.to_str().expect("entry path")])
        .output()
        .expect("check cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0392 [loader error]"), "{stderr}");
    assert!(stderr.contains("priv_helper"), "{stderr}");
}

#[test]
pub(crate) fn rejects_duplicate_module_name_with_l0391() {
    let entry = workspace_root().join("tests/fixtures/invalid/modules_duplicate/main.lby");
    let output = lullaby()
        .args(["check", entry.to_str().expect("entry path")])
        .output()
        .expect("check cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0391 [loader error]"), "{stderr}");
    assert!(stderr.contains("shared"), "{stderr}");
}

#[test]
pub(crate) fn rejects_import_cycle_with_l0393() {
    let entry = workspace_root().join("tests/fixtures/invalid/modules_cycle/a.lby");
    let output = lullaby()
        .args(["check", entry.to_str().expect("entry path")])
        .output()
        .expect("check cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0393 [loader error]"), "{stderr}");
}

#[test]
pub(crate) fn reports_invalid_bytecode_artifact_with_verbose_guidance() {
    let root = workspace_root();
    let artifact = root.join("target/invalid_artifact_verbose.lbc");
    std::fs::write(
        &artifact,
        "{\"format\":\"not-lullaby\",\"version\":1,\"entry\":\"main\",\"module\":{\"functions\":[]}}",
    )
    .expect("write invalid artifact");

    let output = lullaby()
        .args([
            "run",
            "--verbose",
            artifact.to_str().expect("artifact path"),
        ])
        .output()
        .expect("run artifact cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0601 [bytecode error]"), "{stderr}");
    assert!(stderr.contains("Problem:"), "{stderr}");
    assert!(stderr.contains("Root cause:"), "{stderr}");
    assert!(stderr.contains("Suggested fix:"), "{stderr}");
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn reports_invalid_bytecode_artifact_as_json() {
    let root = workspace_root();
    let artifact = root.join("target/invalid_artifact_json.lbc");
    std::fs::write(
        &artifact,
        "{\"format\":\"not-lullaby\",\"version\":1,\"entry\":\"main\",\"module\":{\"functions\":[]}}",
    )
    .expect("write invalid artifact");

    let output = lullaby()
        .args([
            "run",
            "--format",
            "json",
            artifact.to_str().expect("artifact path"),
        ])
        .output()
        .expect("run artifact cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0601\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"bytecode\""), "{stderr}");
    assert!(stderr.contains("\"root_cause\":"), "{stderr}");
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn reports_missing_bytecode_instructions_as_json() {
    let root = workspace_root();
    let artifact = root.join("target/missing_instructions_artifact_json.lbc");
    std::fs::write(
        &artifact,
        "{\"format\":\"lullaby-bytecode\",\"version\":4,\"metadata\":{\"producer\":\"test\",\"target\":\"lullaby-vm\",\"payload\":\"instruction-bytecode\"},\"entry\":\"main\",\"function_table\":[],\"module\":{\"functions\":[{\"name\":\"main\",\"params\":[],\"return_type\":{\"name\":\"i64\"},\"span\":{\"line\":1,\"column\":1}}]}}",
    )
    .expect("write missing instructions artifact");

    let output = lullaby()
        .args([
            "run",
            "--format",
            "json",
            artifact.to_str().expect("artifact path"),
        ])
        .output()
        .expect("run artifact cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0601\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"bytecode\""), "{stderr}");
    assert!(stderr.contains("missing field"), "{stderr}");
    assert!(stderr.contains("instructions"), "{stderr}");
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn reports_invalid_bytecode_instruction_contract_as_json() {
    let root = workspace_root();
    let artifact = root.join("target/invalid_instruction_artifact_json.lbc");
    std::fs::write(
        &artifact,
        "{\"format\":\"lullaby-bytecode\",\"version\":5,\"metadata\":{\"producer\":\"test\",\"target\":\"lullaby-vm\",\"payload\":\"instruction-bytecode\"},\"entry\":\"main\",\"function_table\":[{\"name\":\"main\",\"params\":[],\"return_type\":{\"name\":\"i64\"}}],\"module\":{\"functions\":[{\"name\":\"main\",\"params\":[],\"return_type\":{\"name\":\"i64\"},\"instructions\":[{\"Break\":{\"line\":1,\"column\":1}}],\"span\":{\"line\":1,\"column\":1}}]}}",
    )
    .expect("write invalid instruction artifact");

    let output = lullaby()
        .args([
            "run",
            "--format",
            "json",
            artifact.to_str().expect("artifact path"),
        ])
        .output()
        .expect("run artifact cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0601\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"bytecode\""), "{stderr}");
    assert!(
        stderr.contains("instruction `break` outside loop"),
        "{stderr}"
    );
    let _ = std::fs::remove_file(artifact);
}

#[test]
pub(crate) fn reports_compile_write_failure_as_json() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/valid/run_arithmetic.lby");
    let missing_dir = root.join("target/lullaby_missing_compile_dir");
    let artifact = missing_dir.join("run_arithmetic.lbc");
    let _ = std::fs::remove_dir_all(&missing_dir);

    let output = lullaby()
        .args([
            "compile",
            "--format",
            "json",
            "-o",
            artifact.to_str().expect("artifact path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("compile cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0003\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"resource\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
pub(crate) fn runs_logic_fixture_with_optimized_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "ir",
            "--optimize",
            "constant-fold",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}

#[test]
pub(crate) fn runs_logic_fixture_with_optimized_bytecode_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "bytecode",
            "--optimize",
            "constant-fold",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}

#[test]
pub(crate) fn runs_arithmetic_fixture_with_full_optimized_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "ir",
            "--optimize",
            "full",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn runs_arithmetic_fixture_with_full_optimized_bytecode_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "bytecode",
            "--optimize",
            "full",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn rejects_optimizer_for_ast_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lby");
    let output = lullaby()
        .args([
            "run",
            "--optimize",
            "constant-fold",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0502 [optimizer error]"), "{stderr}");
    assert!(
        stderr.contains("--backend ir or --backend bytecode"),
        "{stderr}"
    );
}

#[test]
pub(crate) fn reports_optimizer_backend_mismatch_with_verbose_guidance() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lby");
    let output = lullaby()
        .args([
            "run",
            "--verbose",
            "--optimize",
            "constant-fold",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0502 [optimizer error]"), "{stderr}");
    assert!(stderr.contains("Problem:"), "{stderr}");
    assert!(stderr.contains("Root cause:"), "{stderr}");
    assert!(stderr.contains("Suggested fix:"), "{stderr}");
    assert!(
        stderr.contains("usage: lullaby run --backend ir|bytecode"),
        "{stderr}"
    );
}

#[test]
pub(crate) fn reports_optimizer_backend_mismatch_as_json() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lby");
    let output = lullaby()
        .args([
            "run",
            "--format",
            "json",
            "--optimize",
            "constant-fold",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0502\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"optimizer\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
pub(crate) fn runs_memory_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_memory.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn runs_memory_fixture_with_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_memory.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "ir",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn runs_store_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_store.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
pub(crate) fn runs_while_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_while.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4");
}

#[test]
pub(crate) fn runs_loop_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_loop.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "5");
}

#[test]
pub(crate) fn runs_logic_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}

#[test]
pub(crate) fn runs_for_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
pub(crate) fn runs_for_step_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for_step.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9");
}

#[test]
pub(crate) fn runs_array_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_array.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
pub(crate) fn runs_file_io_fixture() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/valid/run_file_io.lby");
    let output_path = root.join("target/lullaby_fixture_io.txt");
    let _ = std::fs::remove_file(&output_path);

    let output = lullaby()
        .current_dir(&root)
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "alpha beta");
    assert_eq!(
        std::fs::read_to_string(&output_path).expect("written fixture file"),
        "alpha beta"
    );
    let _ = std::fs::remove_file(output_path);
}

#[test]
pub(crate) fn rejects_forbidden_braces() {
    let fixture = workspace_root().join("tests/fixtures/invalid/brace.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0102 [lexer error]"), "{stderr}");
    assert!(
        stderr.contains("curly braces are not block delimiters"),
        "{stderr}"
    );
}

#[test]
pub(crate) fn reports_forbidden_braces_with_verbose_context() {
    let fixture = workspace_root().join("tests/fixtures/invalid/brace.lby");
    let output = lullaby()
        .args([
            "check",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0102 [lexer error]"), "{stderr}");
    assert!(stderr.contains("Source:"), "{stderr}");
    assert!(stderr.contains("Problem:"), "{stderr}");
    assert!(stderr.contains("Root cause:"), "{stderr}");
    assert!(stderr.contains("Suggested fix:"), "{stderr}");
}

#[test]
pub(crate) fn reports_forbidden_braces_as_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/brace.lby");
    let output = lullaby()
        .args([
            "check",
            "--diagnostic-format",
            "json",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"status\":\"error\""), "{stderr}");
    assert!(stderr.contains("\"code\":\"L0102\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"lexer\""), "{stderr}");
    assert!(
        stderr.contains("\"span\":{\"line\":2,\"column\":5}"),
        "{stderr}"
    );
    assert!(stderr.contains("\"root_cause\":"), "{stderr}");
}

#[test]
pub(crate) fn rejects_missing_indented_body() {
    let fixture = workspace_root().join("tests/fixtures/invalid/missing_indented_body.lby");
    let output = lullaby()
        .args([
            "check",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0205 [parser error]"), "{stderr}");
    assert!(stderr.contains("Root cause:"), "{stderr}");
}

#[test]
pub(crate) fn rejects_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0303 [semantic error]"), "{stderr}");
    assert!(stderr.contains("L0301 [semantic error]"), "{stderr}");
}

#[test]
pub(crate) fn rejects_non_exhaustive_match() {
    let fixture = workspace_root().join("tests/fixtures/invalid/nonexhaustive_match.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("L0384 [semantic error]"),
        "{}",
        stderr(&output)
    );
}

#[test]
pub(crate) fn rejects_uninferable_none() {
    let fixture = workspace_root().join("tests/fixtures/invalid/uninferable_none.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("L0386 [semantic error]"),
        "{}",
        stderr(&output)
    );
}

#[test]
pub(crate) fn rejects_duplicate_enum_variant() {
    let fixture = workspace_root().join("tests/fixtures/invalid/duplicate_enum_variant.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("L0380 [semantic error]"),
        "{}",
        stderr(&output)
    );
}

#[test]
pub(crate) fn reports_type_mismatch_as_ordered_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/type_mismatch.lby");
    let output = lullaby()
        .args([
            "check",
            "--format",
            "json",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    let n0303 = stderr.find("\"code\":\"L0303\"").expect("L0303");
    let n0301 = stderr.find("\"code\":\"L0301\"").expect("L0301");
    assert!(n0303 < n0301, "{stderr}");
    assert!(stderr.contains("\"function\":\"main\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
pub(crate) fn check_allows_library_style_source_without_main() {
    let fixture = workspace_root().join("tests/fixtures/invalid/missing_main.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("ok:"), "{output:?}");
}

#[test]
pub(crate) fn compile_requires_zero_argument_main_entrypoint() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/invalid/missing_main.lby");
    let artifact = root.join("target/missing_main.lbc");
    let _ = std::fs::remove_file(&artifact);

    let output = lullaby()
        .args([
            "compile",
            "-o",
            artifact.to_str().expect("artifact path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0329 [semantic error]"), "{stderr}");
    assert!(stderr.contains("zero-argument `main`"), "{stderr}");
    assert!(!artifact.exists(), "{artifact:?}");
}

#[test]
pub(crate) fn run_rejects_main_with_parameters_as_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/main_with_parameter.lby");
    let output = lullaby()
        .args([
            "run",
            "--format",
            "json",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0329\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"semantic\""), "{stderr}");
    assert!(stderr.contains("\"function\":\"main\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
pub(crate) fn rejects_assignment_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/assignment_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0314"));
}

#[test]
pub(crate) fn rejects_break_outside_loop() {
    let fixture = workspace_root().join("tests/fixtures/invalid/break_outside_loop.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0317"));
}

#[test]
pub(crate) fn rejects_logical_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/logical_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0320"));
}

#[test]
pub(crate) fn rejects_ordering_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/ordering_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0327"));
}

#[test]
pub(crate) fn rejects_for_range_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_range_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0321"));
}

#[test]
pub(crate) fn rejects_for_zero_step_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_zero_step.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0411"));
}

#[test]
pub(crate) fn rejects_array_literal_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_literal_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0324"));
}

#[test]
pub(crate) fn rejects_array_index_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0326"));
}

#[test]
pub(crate) fn rejects_array_index_out_of_bounds_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0413"));
}

#[test]
pub(crate) fn reports_runtime_error_with_verbose_traceback() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.lby");
    let output = lullaby()
        .args(["run", "--verbose", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0413 [runtime error]"), "{stderr}");
    assert!(stderr.contains("Traceback:"), "{stderr}");
    assert!(stderr.contains("in `main`"), "{stderr}");
    assert!(stderr.contains("Suggested fix:"), "{stderr}");
}

#[test]
pub(crate) fn reports_runtime_error_as_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.lby");
    let output = lullaby()
        .args([
            "run",
            "--format",
            "json",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0413\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"runtime\""), "{stderr}");
    assert!(
        stderr.contains("\"traceback\":[{\"function\":\"main\""),
        "{stderr}"
    );
}

#[test]
pub(crate) fn rejects_store_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/store_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0328"));
}

#[test]
pub(crate) fn rejects_use_after_free_at_compile_time() {
    let fixture = workspace_root().join("tests/fixtures/invalid/use_after_free.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0350"), "{}", stderr(&output));
}

#[test]
pub(crate) fn rejects_store_after_dealloc_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/store_after_dealloc.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0406"));
}

#[test]
pub(crate) fn rejects_missing_file_with_structured_resource_error() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/invalid/read_missing_file.lby");
    let _ = std::fs::remove_file(root.join("target/lullaby_missing_file.txt"));

    let output = lullaby()
        .current_dir(root)
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("L0414 [resource error]"), "{stderr}");
}

#[test]
pub(crate) fn reports_missing_file_resource_error_as_json() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/invalid/read_missing_file.lby");
    let _ = std::fs::remove_file(root.join("target/lullaby_missing_file.txt"));

    let output = lullaby()
        .current_dir(root)
        .args([
            "run",
            "--format",
            "json",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("\"code\":\"L0414\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"resource\""), "{stderr}");
    assert!(stderr.contains("\"root_cause\":"), "{stderr}");
}

#[test]
pub(crate) fn rejects_extra_positionals() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.lby");
    let output = lullaby()
        .args([
            "check",
            fixture.to_str().expect("fixture path"),
            "extra.lby",
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("usage: lullaby check"), "{stderr}");
}

#[test]
pub(crate) fn rejects_file_builtin_argument_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/read_file_path_type.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0313"));
}

#[test]
pub(crate) fn rejects_write_file_content_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/write_file_content_type.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0313"));
}

#[test]
pub(crate) fn rejects_system_builtin_argument_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/sys_args_type.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0313"));
}

#[test]
pub(crate) fn run_passes_trailing_program_args_to_args_builtin() {
    // `lullaby run <file.lby> alpha beta` exposes ["alpha", "beta"] through the
    // `args()` builtin on every backend, so the program observes 2 arguments.
    let mut prog = std::env::temp_dir();
    prog.push("lullaby_cli_args_count.lby");
    std::fs::write(&prog, "fn main -> i64\n    len(args())\n").expect("write program");

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
                "alpha",
                "beta",
            ])
            .output()
            .expect("run cli");

        assert!(output.status.success(), "{backend}: {output:?}");
        assert_eq!(stdout(&output).trim(), "2", "{backend}");
    }

    // With no trailing arguments, `args()` is an empty list.
    let output = lullaby()
        .args(["run", prog.to_str().expect("program path")])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(stdout(&output).trim(), "0");

    let _ = std::fs::remove_file(&prog);
}

#[test]
pub(crate) fn run_env_builtin_reads_process_environment() {
    // `env(name)` reads a variable the child process is given, and reports a
    // definitely-unset variable as `none`.
    let mut prog = std::env::temp_dir();
    prog.push("lullaby_cli_env_read.lby");
    std::fs::write(
        &prog,
        "fn main -> string\n    match env(\"LULLABY_CLI_ENV_TEST\")\n        some(v) -> v\n        none -> \"MISSING\"\n",
    )
    .expect("write program");

    let set = lullaby()
        .args(["run", prog.to_str().expect("program path")])
        .env("LULLABY_CLI_ENV_TEST", "present")
        .output()
        .expect("run cli");
    assert!(set.status.success(), "{set:?}");
    assert_eq!(stdout(&set).trim(), "present");

    let unset = lullaby()
        .args(["run", prog.to_str().expect("program path")])
        .env_remove("LULLABY_CLI_ENV_TEST")
        .output()
        .expect("run cli");
    assert!(unset.status.success(), "{unset:?}");
    assert_eq!(stdout(&unset).trim(), "MISSING");

    let _ = std::fs::remove_file(&prog);
}
