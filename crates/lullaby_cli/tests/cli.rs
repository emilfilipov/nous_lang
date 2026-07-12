use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn lullaby() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lullaby"))
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Whether `haystack` contains `needle` as a contiguous byte subslice.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn checks_valid_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok:"));
}

#[test]
fn fmt_prints_canonical_source_and_is_idempotent() {
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
fn fmt_check_passes_on_canonical_fixture() {
    // Fixtures are kept canonical, so --check succeeds with no output.
    let fixture = workspace_root().join("tests/fixtures/valid/run_showcase.lby");
    let output = lullaby()
        .args(["fmt", "--check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn fmt_check_fails_on_unformatted_input() {
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
fn fmt_rejects_non_lby_extension() {
    let output = lullaby()
        .args(["fmt", "does_not_exist.txt"])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("L0001"), "{}", stderr(&output));
}

#[test]
fn checks_valid_fixture_as_json() {
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
fn prints_offline_docs_path() {
    let output = lullaby().args(["docs"]).output().expect("run cli");

    assert!(output.status.success(), "{output:?}");
    let stdout = stdout(&output);
    assert!(stdout.contains("docs:"), "{stdout}");
    assert!(stdout.contains("index.html"), "{stdout}");
}

#[test]
fn prints_examples_path() {
    let output = lullaby().args(["examples"]).output().expect("run cli");

    assert!(output.status.success(), "{output:?}");
    let stdout = stdout(&output);
    assert!(stdout.contains("examples:"), "{stdout}");
    assert!(stdout.contains("valid"), "{stdout}");
}

#[test]
fn runs_user_facing_valid_examples() {
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
fn runs_standard_streams_across_backends() {
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
fn runs_modulo_across_backends() {
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
fn runs_for_in_across_backends() {
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
fn runs_sum_reduction_across_backends() {
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
fn runs_string_ergonomics_across_backends() {
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
fn runs_array_fill_across_backends() {
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
fn runs_negation_across_backends() {
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
fn rejects_modulo_on_float() {
    let fixture = workspace_root().join("tests/fixtures/invalid/modulo_on_float.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0307"));
}

#[test]
fn rejects_user_facing_invalid_examples() {
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
fn runs_arithmetic_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_arithmetic_fixture_with_ir_backend() {
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
fn runs_arithmetic_fixture_with_bytecode_backend() {
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
fn runs_inferred_let_fixture_on_all_backends() {
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

/// Inline conditional (`THEN if COND else ELSE`) — nested, in a `let`, in a
/// function body, and driving the result — computes 115 identically on all three
/// interpreter backends.
#[test]
fn runs_conditional_fixture_on_all_backends() {
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

/// The inline conditional desugars to a plain `if` statement, so the native
/// backend compiles it; when the platform can link, the `.exe` exit code equals
/// the interpreter result (115 mod 256).
#[test]
fn native_conditional_execution_parity_when_linkable() {
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

/// An inline-conditional condition must be `bool` (`L0305`, shared with `if`).
#[test]
fn rejects_conditional_non_bool_condition() {
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
fn rejects_conditional_branch_type_mismatch() {
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
fn rejects_conditional_aggregate_result() {
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
fn runs_string_char_concat_fixture_on_all_backends() {
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
fn runs_in_operator_fixture_on_all_backends() {
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
fn rejects_in_incompatible_operands() {
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
fn runs_string_slice_fixture_on_all_backends() {
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
fn rejects_slice_incompatible_operands() {
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
fn rejects_string_plus_int_concat() {
    let fixture = workspace_root().join("tests/fixtures/invalid/string_plus_int.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0307"), "{}", stderr(&output));
}

#[test]
fn runs_parallel_map_fixture_on_all_backends() {
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
fn runs_sizeof_fixture_on_all_backends() {
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
fn runs_ptr_cast_fixture_on_all_backends() {
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
fn runs_list_map_fixture_on_all_backends() {
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
fn runs_list_higher_order_fixture_on_all_backends() {
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
fn runs_sort_by_fixture_on_all_backends() {
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
fn runs_sort_types_fixture_on_all_backends() {
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
fn runs_spawn_channel_mutex_fixture_on_all_backends() {
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
fn runs_atomic_memory_orderings_fixture_on_all_backends() {
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
fn runs_non_blocking_socket_fixture_on_all_backends() {
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
fn compiles_fixture_to_bytecode_artifact_and_runs_it() {
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
fn builds_fixture_to_bytecode_artifact_and_runs_it() {
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
fn inspects_bytecode_artifact() {
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
fn rejects_invalid_bytecode_artifact() {
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
fn rejects_planned_unsupported_syntax_with_dedicated_diagnostic() {
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
fn runs_multi_file_module_program_across_backends() {
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
fn rejects_cross_module_private_use_with_l0392() {
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
fn rejects_duplicate_module_name_with_l0391() {
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
fn rejects_import_cycle_with_l0393() {
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
fn reports_invalid_bytecode_artifact_with_verbose_guidance() {
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
fn reports_invalid_bytecode_artifact_as_json() {
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
fn reports_missing_bytecode_instructions_as_json() {
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
fn reports_invalid_bytecode_instruction_contract_as_json() {
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
fn reports_compile_write_failure_as_json() {
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
fn runs_logic_fixture_with_optimized_ir_backend() {
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
fn runs_logic_fixture_with_optimized_bytecode_backend() {
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
fn runs_arithmetic_fixture_with_full_optimized_ir_backend() {
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
fn runs_arithmetic_fixture_with_full_optimized_bytecode_backend() {
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
fn rejects_optimizer_for_ast_backend() {
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
fn reports_optimizer_backend_mismatch_with_verbose_guidance() {
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
fn reports_optimizer_backend_mismatch_as_json() {
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
fn runs_memory_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_memory.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_memory_fixture_with_ir_backend() {
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
fn runs_store_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_store.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_while_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_while.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4");
}

#[test]
fn runs_loop_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_loop.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "5");
}

#[test]
fn runs_logic_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}

#[test]
fn runs_for_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn runs_for_step_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for_step.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9");
}

#[test]
fn runs_array_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_array.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn runs_file_io_fixture() {
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
fn rejects_forbidden_braces() {
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
fn reports_forbidden_braces_with_verbose_context() {
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
fn reports_forbidden_braces_as_json() {
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
fn rejects_missing_indented_body() {
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
fn rejects_type_mismatch() {
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
fn rejects_non_exhaustive_match() {
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
fn rejects_uninferable_none() {
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
fn rejects_duplicate_enum_variant() {
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
fn reports_type_mismatch_as_ordered_json() {
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
fn check_allows_library_style_source_without_main() {
    let fixture = workspace_root().join("tests/fixtures/invalid/missing_main.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("ok:"), "{output:?}");
}

#[test]
fn compile_requires_zero_argument_main_entrypoint() {
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
fn run_rejects_main_with_parameters_as_json() {
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
fn rejects_assignment_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/assignment_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0314"));
}

#[test]
fn rejects_break_outside_loop() {
    let fixture = workspace_root().join("tests/fixtures/invalid/break_outside_loop.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0317"));
}

#[test]
fn rejects_logical_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/logical_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0320"));
}

#[test]
fn rejects_ordering_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/ordering_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0327"));
}

#[test]
fn rejects_for_range_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_range_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0321"));
}

#[test]
fn rejects_for_zero_step_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_zero_step.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0411"));
}

#[test]
fn rejects_array_literal_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_literal_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0324"));
}

#[test]
fn rejects_array_index_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0326"));
}

#[test]
fn rejects_array_index_out_of_bounds_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0413"));
}

#[test]
fn reports_runtime_error_with_verbose_traceback() {
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
fn reports_runtime_error_as_json() {
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
fn rejects_store_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/store_type_mismatch.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0328"));
}

#[test]
fn rejects_use_after_free_at_compile_time() {
    let fixture = workspace_root().join("tests/fixtures/invalid/use_after_free.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("L0350"), "{}", stderr(&output));
}

#[test]
fn rejects_store_after_dealloc_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/store_after_dealloc.lby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0406"));
}

#[test]
fn rejects_missing_file_with_structured_resource_error() {
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
fn reports_missing_file_resource_error_as_json() {
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
fn rejects_extra_positionals() {
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
fn rejects_file_builtin_argument_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/read_file_path_type.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0313"));
}

#[test]
fn rejects_write_file_content_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/write_file_content_type.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0313"));
}

#[test]
fn rejects_system_builtin_argument_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/sys_args_type.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("L0313"));
}

#[test]
fn run_passes_trailing_program_args_to_args_builtin() {
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
fn run_env_builtin_reads_process_environment() {
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

/// A fresh temp directory for a file-system test, using forward slashes so the
/// path can be embedded in a `.lby` string literal on every platform (Windows
/// accepts `/` in `std::fs` paths). The directory is recreated empty.
fn fs_temp_dir(test_name: &str) -> (std::path::PathBuf, String) {
    let dir = std::env::temp_dir().join(format!("lullaby_cli_{test_name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let lby = dir.to_string_lossy().replace('\\', "/");
    (dir, lby)
}

#[test]
fn run_write_bytes_read_bytes_round_trip_on_all_backends() {
    // Write raw bytes, read them back, and reconstruct their numeric sum. The
    // program is deterministic and each backend runs against its own file.
    for backend in ["ast", "ir", "bytecode"] {
        let (dir, base) = fs_temp_dir(&format!("bytes_{backend}"));
        let path = format!("{base}/data.bin");
        let source = format!(
            "fn main -> i64\n    \
             let data list<byte> = list_new()\n    \
             data = push(data, byte(72))\n    \
             data = push(data, byte(105))\n    \
             data = push(data, byte(33))\n    \
             write_bytes(\"{path}\", data)\n    \
             let back list<byte> = read_bytes(\"{path}\")\n    \
             byte_val(get(back, 0)) + byte_val(get(back, 1)) + byte_val(get(back, 2)) + len(back)\n"
        );
        let prog = dir.join("prog.lby");
        std::fs::write(&prog, source).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // 72 + 105 + 33 + 3 == 213
        assert_eq!(stdout(&output).trim(), "213", "{backend}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn run_read_lines_and_file_size_on_all_backends() {
    for backend in ["ast", "ir", "bytecode"] {
        let (dir, base) = fs_temp_dir(&format!("lines_{backend}"));
        let path = format!("{base}/notes.txt");
        // Seed the file from the harness (a `.lby` string literal cannot hold a
        // raw newline). "a\nbb\nccc" is 8 bytes and three lines.
        std::fs::write(dir.join("notes.txt"), "a\nbb\nccc").expect("seed file");
        let source = format!(
            "fn main -> i64\n    \
             let lines list<string> = read_lines(\"{path}\")\n    \
             let size i64 = file_size(\"{path}\")\n    \
             len(lines) * 100 + size\n"
        );
        let prog = dir.join("prog.lby");
        std::fs::write(&prog, source).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // 3 lines * 100 + 8 bytes == 308
        assert_eq!(stdout(&output).trim(), "308", "{backend}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn run_directory_builtins_on_all_backends() {
    for backend in ["ast", "ir", "bytecode"] {
        let (dir, base) = fs_temp_dir(&format!("dirs_{backend}"));
        let sub = format!("{base}/nested");
        let file = format!("{sub}/one.txt");
        // Create a directory, put one file in it, list it, then tear it down.
        let source = format!(
            "fn flag b bool -> i64\n    if b\n        1\n    else\n        0\n\n\
             fn main -> i64\n    \
             make_dir(\"{sub}\")\n    \
             write_file(\"{file}\", \"x\")\n    \
             let is_d bool = is_dir(\"{sub}\")\n    \
             let is_f bool = is_file(\"{file}\")\n    \
             let entries list<string> = list_dir(\"{sub}\")\n    \
             remove_file(\"{file}\")\n    \
             remove_dir(\"{sub}\")\n    \
             let gone bool = is_dir(\"{sub}\")\n    \
             flag(is_d) * 1000 + flag(is_f) * 100 + len(entries) * 10 + flag(gone)\n"
        );
        let prog = dir.join("prog.lby");
        std::fs::write(&prog, source).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // is_dir=1 -> 1000, is_file=1 -> 100, 1 entry -> 10, gone=false -> 0 == 1110
        assert_eq!(stdout(&output).trim(), "1110", "{backend}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn runs_socket_fixture_on_all_backends() {
    // The auto-run socket fixture is deterministic: `tcp_connect("127.0.0.1", 1)`
    // is a guaranteed connection refusal (port 1 is virtually always closed), so
    // the `match` takes the `err` arm and returns `1` on every backend without any
    // external server or real I/O.
    let fixture = workspace_root().join("tests/fixtures/valid/run_socket.lby");
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
        assert_eq!(stdout(&output).trim(), "1", "{backend} result");
    }
}

#[test]
fn tcp_client_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A real TCP round-trip driven from the test as the SERVER. The Lullaby
    // program is the client: it connects, writes a request, reads the reply, and
    // returns the reply length. The Rust listener replies "pong!" (5 bytes) to
    // every accepted connection, once per backend.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _addr) = listener.accept().expect("accept");
            let mut buffer = [0u8; 64];
            let _read = stream.read(&mut buffer).expect("server read");
            stream.write_all(b"pong!").expect("server write");
            stream.flush().expect("server flush");
        }
    });

    let program = format!(
        "fn main -> i64\n    \
         let outcome result<Socket, string> = tcp_connect(\"127.0.0.1\", {port})\n    \
         match outcome\n        \
         ok(conn) -> handle(conn)\n        \
         err(message) -> 0 - 1\n\n\
         fn handle conn Socket -> i64\n    \
         let sent result<i64, string> = tcp_write(conn, \"ping\")\n    \
         let reply result<string, string> = tcp_read(conn)\n    \
         tcp_close(conn)\n    \
         match reply\n        \
         ok(text) -> len(text)\n        \
         err(message) -> 0 - 2\n"
    );
    let prog = std::env::temp_dir().join("lullaby_tcp_client.lby");
    std::fs::write(&prog, program).expect("write program");

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The reply "pong!" is 5 bytes long.
        assert_eq!(stdout(&output).trim(), "5", "{backend} reply length");
    }

    server.join().expect("server thread");
    let _ = std::fs::remove_file(&prog);
}

#[test]
fn tcp_server_round_trip() {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    // A real TCP round-trip where the Lullaby program is the SERVER: it listens on
    // a fixed loopback port, accepts one connection, reads the request, echoes it
    // back with a prefix, and exits. The Rust test connects as the client.
    //
    // Pick an ephemeral port up front by binding and dropping, then reuse it. This
    // is a small race window but adequate for a single-shot loopback test.
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("addr").port()
    };

    let program = format!(
        "fn main -> i64\n    \
         let bound result<Socket, string> = tcp_listen(\"127.0.0.1\", {port})\n    \
         match bound\n        \
         ok(listener) -> serve(listener)\n        \
         err(message) -> 0 - 1\n\n\
         fn serve listener Socket -> i64\n    \
         let accepted result<Socket, string> = tcp_accept(listener)\n    \
         match accepted\n        \
         ok(conn) -> echo(conn)\n        \
         err(message) -> 0 - 2\n\n\
         fn echo conn Socket -> i64\n    \
         let request result<string, string> = tcp_read(conn)\n    \
         match request\n        \
         ok(text) -> reply(conn, text)\n        \
         err(message) -> 0 - 3\n\n\
         fn reply conn Socket text string -> i64\n    \
         let sent result<i64, string> = tcp_write(conn, \"echo:\" + text)\n    \
         tcp_close(conn)\n    \
         match sent\n        \
         ok(count) -> count\n        \
         err(message) -> 0 - 4\n"
    );
    let prog = std::env::temp_dir().join("lullaby_tcp_server.lby");
    std::fs::write(&prog, program).expect("write program");

    // Run the Lullaby server in a background thread so the test can connect to it.
    let prog_path = prog.clone();
    let server = std::thread::spawn(move || {
        lullaby()
            .args([
                "run",
                "--backend",
                "ast",
                prog_path.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli")
    });

    // Retry the connect briefly while the Lullaby server binds and starts listening.
    let mut stream = None;
    for _ in 0..50 {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
    let mut stream = stream.expect("connect to lullaby server");
    stream.write_all(b"hi").expect("client write");
    stream.flush().expect("client flush");
    let mut reply = String::new();
    stream.read_to_string(&mut reply).expect("client read");
    assert_eq!(reply, "echo:hi", "server echo reply");

    let output = server.join().expect("server thread");
    assert!(output.status.success(), "lullaby server: {output:?}");
    // "echo:hi" is 7 bytes, the byte count returned by tcp_write.
    assert_eq!(stdout(&output).trim(), "7", "server tcp_write byte count");
    let _ = std::fs::remove_file(&prog);
}

/// Probe whether UDP loopback datagrams actually flow in this environment. Some
/// sandboxes and host firewalls silently drop loopback UDP, which would make a
/// real round-trip hang or fail through no fault of the code under test. Returns
/// `true` only if a datagram sent to a bound loopback socket is received back
/// within a short timeout.
fn udp_loopback_available() -> bool {
    use std::net::UdpSocket;
    use std::time::Duration;

    let Ok(rx) = UdpSocket::bind("127.0.0.1:0") else {
        return false;
    };
    let Ok(addr) = rx.local_addr() else {
        return false;
    };
    if rx
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return false;
    }
    let Ok(tx) = UdpSocket::bind("127.0.0.1:0") else {
        return false;
    };
    if tx.send_to(b"probe", addr).is_err() {
        return false;
    }
    let mut buffer = [0u8; 8];
    rx.recv_from(&mut buffer).is_ok()
}

#[test]
fn udp_round_trip_on_all_backends() {
    use std::net::UdpSocket;
    use std::time::Duration;

    // Skip cleanly where UDP loopback is unavailable (sandbox/firewall): the
    // round-trip would otherwise hang or fail on the environment, not the code.
    if !udp_loopback_available() {
        eprintln!(
            "skipping udp_round_trip_on_all_backends: UDP loopback is unavailable in this environment"
        );
        return;
    }

    // A real UDP round-trip: the Lullaby program binds a UDP socket, sends a
    // datagram to a Rust-side UDP socket, then receives the Rust reply and returns
    // its length. A fresh Rust responder socket is used per backend so datagrams
    // never cross runs.
    let program_template = |responder_port: u16| {
        format!(
            "fn main -> i64\n    \
             let bound result<Socket, string> = udp_bind(\"127.0.0.1\", 0)\n    \
             match bound\n        \
             ok(sock) -> exchange(sock, {responder_port})\n        \
             err(message) -> 0 - 1\n\n\
             fn exchange sock Socket responder i64 -> i64\n    \
             let sent result<i64, string> = udp_send_to(sock, \"ping\", \"127.0.0.1\", responder)\n    \
             let reply result<string, string> = udp_recv(sock)\n    \
             match reply\n        \
             ok(text) -> len(text)\n        \
             err(message) -> 0 - 2\n"
        )
    };

    for backend in ["ast", "ir", "bytecode"] {
        let responder = UdpSocket::bind("127.0.0.1:0").expect("responder bind");
        let responder_port = responder.local_addr().expect("addr").port();
        // A generous read timeout means a lost datagram surfaces as a failed
        // assertion below rather than hanging the responder thread forever.
        responder
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("responder read timeout");

        let handler = std::thread::spawn(move || {
            let mut buffer = [0u8; 64];
            if let Ok((_len, sender)) = responder.recv_from(&mut buffer) {
                let _ = responder.send_to(b"pong-udp", sender);
            }
        });

        let program = program_template(responder_port);
        let prog = std::env::temp_dir().join(format!("lullaby_udp_{backend}.lby"));
        std::fs::write(&prog, program).expect("write program");

        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The reply "pong-udp" is 8 bytes long.
        assert_eq!(stdout(&output).trim(), "8", "{backend} udp reply length");

        handler.join().expect("responder thread");
        let _ = std::fs::remove_file(&prog);
    }
}

#[test]
fn http_get_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A real HTTP/1.1 GET round-trip driven from the test as the SERVER. The
    // minimal server replies "hello" (5 bytes) with a `Content-Length` header and
    // `Connection: close` to every request, once per backend. The Lullaby program
    // is the client: it takes the port as a program argument via `args()`, builds
    // the URL, `http_get`s it, and returns the response body length (5).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _addr) = listener.accept().expect("accept");
            let mut buffer = [0u8; 1024];
            let _read = stream.read(&mut buffer).expect("server read");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                )
                .expect("server write");
            stream.flush().expect("server flush");
        }
    });

    // `args()` yields `list<string>`; `get(args(), 0)` is the port passed on the
    // command line. The URL is assembled with `string` concatenation.
    let program = concat!(
        "fn main -> i64\n    ",
        "let port string = get(args(), 0)\n    ",
        "let url string = \"http://127.0.0.1:\" + port + \"/\"\n    ",
        "let outcome result<string, string> = http_get(url)\n    ",
        "match outcome\n        ",
        "ok(body) -> len(body)\n        ",
        "err(message) -> 0 - 1\n",
    );
    let prog = std::env::temp_dir().join("lullaby_http_get.lby");
    std::fs::write(&prog, program).expect("write program");
    let port_arg = port.to_string();

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
                &port_arg,
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The body "hello" is 5 bytes long.
        assert_eq!(stdout(&output).trim(), "5", "{backend} body length");
    }

    server.join().expect("server thread");
    let _ = std::fs::remove_file(&prog);
}

#[test]
fn http_post_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A real HTTP/1.1 POST round-trip: the minimal server reads the request,
    // parses `Content-Length`, drains the request body, and replies with the body
    // byte count rendered as the response body. The Lullaby program posts a fixed
    // body and returns the length of the response body (which is the decimal
    // digits of the request body length). The request body is "payload" (7 bytes),
    // so the response body is "7" (1 byte) and the Lullaby program returns 1.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
    let port = listener.local_addr().expect("addr").port();

    let server = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _addr) = listener.accept().expect("accept");
            let mut raw = Vec::new();
            let mut buffer = [0u8; 1024];
            // Read until the header terminator, then keep reading the declared body.
            loop {
                let read = stream.read(&mut buffer).expect("server read");
                if read == 0 {
                    break;
                }
                raw.extend_from_slice(&buffer[..read]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let length = text
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    if raw.len() >= body_start + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw);
            let header_end = text.find("\r\n\r\n").expect("header terminator");
            let length = text
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length:")
                        .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            let body = &raw[header_end + 4..header_end + 4 + length];
            let reply_body = body.len().to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                reply_body.len(),
                reply_body
            );
            stream.write_all(response.as_bytes()).expect("server write");
            stream.flush().expect("server flush");
        }
    });

    let program = concat!(
        "fn main -> i64\n    ",
        "let port string = get(args(), 0)\n    ",
        "let url string = \"http://127.0.0.1:\" + port + \"/\"\n    ",
        "let outcome result<string, string> = http_post(url, \"payload\")\n    ",
        "match outcome\n        ",
        "ok(body) -> len(body)\n        ",
        "err(message) -> 0 - 1\n",
    );
    let prog = std::env::temp_dir().join("lullaby_http_post.lby");
    std::fs::write(&prog, program).expect("write program");
    let port_arg = port.to_string();

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                prog.to_str().expect("program path"),
                &port_arg,
            ])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{backend}: {output:?}");
        // The request body "payload" is 7 bytes, so the response body is "7"
        // (1 byte) and the Lullaby program returns 1.
        assert_eq!(stdout(&output).trim(), "1", "{backend} echoed length");
    }

    server.join().expect("server thread");
    let _ = std::fs::remove_file(&prog);
}

/// End-to-end HTTP/1.1 round-trip where the Lullaby program is the SERVER,
/// written in pure Lullaby (`examples/valid/http_server/server.lby`) on top of
/// the socket builtins plus `tcp_shutdown`. A Rust `TcpStream` HTTP client
/// sends a real request and reads the full response to EOF, asserting the
/// status line and body â€” proving a graceful teardown delivers the buffered
/// response (no "Empty reply"). Runs on every backend.
#[test]
fn http_server_round_trip_on_all_backends() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    // Send one HTTP request over a fresh connection and read the whole response
    // to EOF (the server sends `Connection: close` and shuts down its write half).
    fn request(port: u16, path: &str) -> String {
        // Retry the connect briefly while the Lullaby server binds and listens.
        let mut stream = None;
        for _ in 0..100 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        let mut stream = stream.expect("connect to lullaby http server");
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("client write");
        stream.flush().expect("client flush");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("client read to EOF");
        response
    }

    let server_path = workspace_root().join("examples/valid/http_server/server.lby");

    for backend in ["ast", "ir", "bytecode"] {
        // Pick a free port, then release it so the Lullaby server can bind it.
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
            probe.local_addr().expect("addr").port()
        };

        // Serve two requests: one for `/` and one for an unknown path.
        let path = server_path.clone();
        let port_arg = port.to_string();
        let server = std::thread::spawn(move || {
            lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    path.to_str().expect("server path"),
                    &port_arg,
                    "2",
                ])
                .output()
                .expect("run cli")
        });

        // Known route: expect a 200 with the server's greeting body.
        let ok_response = request(port, "/");
        let status_line = ok_response.lines().next().unwrap_or_default();
        assert_eq!(
            status_line, "HTTP/1.1 200 OK",
            "{backend} status line for /: {ok_response:?}"
        );
        assert!(
            ok_response.ends_with("Hello from Lullaby!"),
            "{backend} greeting body for /: {ok_response:?}"
        );
        assert!(
            ok_response.contains("Content-Length: 19"),
            "{backend} content-length for /: {ok_response:?}"
        );

        // Unknown route: expect a 404.
        let missing_response = request(port, "/does-not-exist");
        let missing_status = missing_response.lines().next().unwrap_or_default();
        assert_eq!(
            missing_status, "HTTP/1.1 404 Not Found",
            "{backend} status line for unknown path: {missing_response:?}"
        );

        let output = server.join().expect("server thread");
        assert!(
            output.status.success(),
            "{backend} lullaby server: {output:?}"
        );
    }
}

// -- WebAssembly backend (scalar subset) -------------------------------------

/// Whether `node` is available on this machine (its result runs the emitted
/// `.wasm` for execution parity). Returns `false` if `node --version` cannot run.
fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn wasm_emits_module_and_lists_functions() {
    // The scalar fixture: an arithmetic function, a recursive `if` function, a
    // bool-returning comparison, a `for`-loop function, plus a `main` the
    // interpreter uses for ground truth. Every function is in the scalar subset.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_scalars.wasm");
    let output = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    for name in ["add", "fib", "is_even", "sum_to", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // The emitted file starts with the WASM magic + version 1.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert_eq!(
        &bytes[0..8],
        &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00],
        "wasm header"
    );
}

#[test]
fn wasm_reports_no_eligible_functions() {
    // A file whose only function uses a type outside the supported WASM value set
    // (strings/structs/arrays/enums, scalar-/string-/struct-/nested-list-element
    // `list`s, maps with a scalar or `string` value or a `struct` value, and enums
    // with scalar/`string`/one-level-mutable payloads are now supported): a map
    // whose VALUE is itself a map — `map<i64, map<i64, i64>>` — nests a collection
    // the backend does not lay out, so nothing is eligible and the WASM backend
    // reports L0338. `wasm` reuses the executable pipeline, which requires `main`;
    // make `main` itself return that type so nothing is eligible and the emitter
    // reports L0338 rather than compiling anything.
    let source = "fn main -> map<i64, map<i64, i64>>\n    map_new()\n";
    let tmp = std::env::temp_dir().join("lullaby_wasm_none.lby");
    std::fs::write(&tmp, source).expect("write temp");
    let output = lullaby()
        .args(["wasm", "--verbose", tmp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let rendered = format!("{}{}", stdout(&output), stderr(&output));
    assert!(rendered.contains("L0338"), "expected L0338: {rendered}");
    assert!(
        rendered.contains("skipped main"),
        "expected verbose skip reason: {rendered}"
    );
}

#[test]
fn wasm_execution_parity_with_node() {
    // Emit the module, then (if `node` is available) instantiate it and assert
    // each exported function matches the interpreter's ground truth.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_parity.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Interpreter ground truth for `main` (which calls the others).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "152");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM execution parity");
        return;
    }

    // A tiny JS runner: print several exported results. i64 params/returns are
    // BigInt in JS, so pass `10n` and stringify the BigInt result.
    let runner = std::env::temp_dir().join("lullaby_wasm_runner.js");
    // The module imports the host functions `env.log_i64`, `env.console_log`, and
    // `env.dom_set_text`, so instantiation must supply all three even though these
    // scalar functions do not call them.
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const lines=[\
             'add='+e.add(20n,22n).toString(),\
             'fib='+e.fib(10n).toString(),\
             'is_even10='+e.is_even(10n).toString(),\
             'is_even55='+e.is_even(55n).toString(),\
             'sum='+e.sum_to(10n).toString(),\
             'main='+e.main().toString()\
           ];\
           process.stdout.write(lines.join(';'));\
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
    // Arithmetic function.
    assert!(out_text.contains("add=42"), "{out_text}");
    // Recursive function with `if`.
    assert!(out_text.contains("fib=55"), "{out_text}");
    // Bool-returning comparison exports as i32 0/1.
    assert!(out_text.contains("is_even10=1"), "{out_text}");
    assert!(out_text.contains("is_even55=0"), "{out_text}");
    // `for`-loop function.
    assert!(out_text.contains("sum=55"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
}

#[test]
fn wasm_log_import_execution_parity_with_node() {
    // The linear-memory step: a program whose exported function calls the
    // `wasm_log` host import with several computed values. The generated JS
    // harness supplies `env.log_i64`, capturing each call into an array, then
    // asserts the captured sequence equals what the interpreter computes.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_log.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_log.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // The emitted module exports `memory` (the linear memory) — a quick check on
    // the raw bytes independent of any runtime.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );

    // Interpreter ground truth. `main` calls `emit()` (which logs 4, 10, 42) and
    // then returns 36, which the CLI prints as the final line — drop that so we
    // compare only the `wasm_log` side-effect sequence.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let mut interp_lines: Vec<String> = stdout(&run)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let interp_return = interp_lines.pop();
    let interp_logged = interp_lines;
    assert_eq!(interp_logged, vec!["4", "10", "42"]);
    assert_eq!(interp_return.as_deref(), Some("36"));

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM host-import execution parity");
        return;
    }

    // The harness provides `env.log_i64`, capturing each call into `logged`,
    // then calls the exported `emit` and prints the captured BigInts.
    let runner = std::env::temp_dir().join("lullaby_wasm_log_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const logged=[];\
         const imports={{env:{{log_i64:(x)=>logged.push(x.toString()),console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           r.instance.exports.emit();\
           process.stdout.write(logged.join(';'));\
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
    let captured: Vec<String> = String::from_utf8_lossy(&node.stdout)
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        captured, interp_logged,
        "WASM host-log call sequence must equal the interpreter's"
    );
}

#[test]
fn wasm_heap_types_execution_parity_with_node() {
    // The heap-types step: a program that builds a string, a struct (with a field
    // mutation), and a fixed array (with an indexed write and a `for`-loop read),
    // all laid out in linear memory. Each exported function's WASM result must
    // match the interpreter, and the emitted `memory` must hold the interned
    // string literal.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_heap.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_heap.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // The emitted module exports `memory` and seeds the `"hello"` literal into
    // its Data section — a raw-bytes check independent of any runtime.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );
    assert!(
        contains_subslice(&bytes, b"hello"),
        "string literal seeded into the data section"
    );

    // Interpreter ground truth for `main` (which calls every heap function).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "133");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM heap-types execution parity");
        return;
    }

    // The runner instantiates the module (a no-op `env.log_i64`), calls each
    // export, and additionally reads the interned
    // `[char_len i32][byte_len i32][utf8]` string layout straight out of `memory`
    // at the reserved base (offset 16): char count at +0, byte count at +4, bytes
    // at +8.
    let runner = std::env::temp_dir().join("lullaby_wasm_heap_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const slen=dv.getInt32(16,true);\
           const sblen=dv.getInt32(20,true);\
           const sbytes=new Uint8Array(e.memory.buffer).slice(24,24+sblen);\
           const lines=[\
             'greet_len='+e.greet_len().toString(),\
             'point_sum='+e.point_sum(3n,4n).toString(),\
             'point_mutated='+e.point_mutated(1n).toString(),\
             'array_probe='+e.array_probe().toString(),\
             'main='+e.main().toString(),\
             'str='+Buffer.from(sbytes).toString()+'/'+slen\
           ];\
           process.stdout.write(lines.join(';'));\
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
    // `len` on a string literal read from linear memory.
    assert!(out_text.contains("greet_len=5"), "{out_text}");
    // Struct field reads.
    assert!(out_text.contains("point_sum=7"), "{out_text}");
    // Struct field mutation.
    assert!(out_text.contains("point_mutated=12"), "{out_text}");
    // Array literal, indexed write, `for`-loop indexed read, and array `len`.
    assert!(out_text.contains("array_probe=109"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
    // The interned string layout in `memory` decodes back to the literal.
    assert!(out_text.contains("str=hello/5"), "{out_text}");
}

#[test]
fn wasm_string_concat_execution_parity_with_node() {
    // Runtime string concatenation (`a + b` on two `string` values) compiles to
    // WASM: each function allocates a fresh `[char_len][byte_len][utf8]` record and
    // copies both operands' byte ranges. The fixture exercises direct concat, a
    // chained `a + b + c`, and concatenation through a helper function, returning
    // deterministic `i64` char counts via `len(...)`. Every export's WASM result
    // must match the interpreter bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_string_concat.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_string_concat.wasm");
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

    // Every function — including the ones doing runtime `+` on strings — compiles
    // to WASM (none is skipped/demoted to the interpreters).
    let emit_out = stdout(&emit);
    for name in [
        "concat_two",
        "concat_three",
        "simple_len",
        "chained_len",
        "helper_len",
        "deep_len",
        "main",
    ] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile to WASM, got: {emit_out}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "33");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM string-concat execution parity");
        return;
    }

    // Instantiate under node, call each export, and additionally decode a
    // concatenated record built at runtime straight out of `memory` (char count at
    // +0, byte count at +4, bytes at +8) to prove the layout round-trips.
    let runner = std::env::temp_dir().join("lullaby_wasm_string_concat_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const u8=new Uint8Array(e.memory.buffer);\
           const ptr=e.concat_two(16,16);\
           const cl=dv.getInt32(ptr,true);\
           const bl=dv.getInt32(ptr+4,true);\
           const s=Buffer.from(u8.slice(ptr+8,ptr+8+bl)).toString();\
           const lines=[\
             'simple_len='+e.simple_len().toString(),\
             'chained_len='+e.chained_len().toString(),\
             'helper_len='+e.helper_len().toString(),\
             'deep_len='+e.deep_len().toString(),\
             'main='+e.main().toString(),\
             'rec='+s+'/'+cl+'/'+bl\
           ];\
           process.stdout.write(lines.join(';'));\
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
    assert!(out_text.contains("simple_len=6"), "{out_text}");
    assert!(out_text.contains("chained_len=6"), "{out_text}");
    assert!(out_text.contains("helper_len=10"), "{out_text}");
    assert!(out_text.contains("deep_len=11"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
    // `concat_two("", "")` over the two `[16..]` records at the reserved base —
    // both point at the same interned first literal — concatenates its bytes and
    // sums its headers, so the runtime-built record decodes correctly. The first
    // interned literal is `foo` (from `simple_len`), so `concat_two(16, 16)`
    // yields `foofoo` with char count 6 and byte count 6.
    assert!(out_text.contains("rec=foofoo/6/6"), "{out_text}");
}

#[test]
fn wasm_string_ops_execution_parity_with_node() {
    // Index-based string operations compile to WASM: char-indexed `substring`/`find`
    // (which decode UTF-8 to map char indices to byte offsets) and byte-exact
    // `contains`/`starts_with`/`ends_with`. The fixture exercises a multi-byte
    // ("café", where `é` is 2 bytes) string across edge indices, present/absent
    // `find`, an empty needle, and true/false cases of every predicate, combining
    // them into a deterministic `i64` from `main` plus string-returning `substring`
    // exports the node runner decodes. Every export's WASM result must match the
    // interpreter bit-for-bit — including the char-vs-byte distinction.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_string_ops.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_string_ops.wasm");
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

    // Every function compiles to WASM (none is skipped/demoted to the interpreters).
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
            "expected `{name}` to compile to WASM, got: {emit_out}"
        );
    }

    // Interpreter ground truth for `main` (the joined deterministic total).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "11");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM string-ops execution parity");
        return;
    }

    // Instantiate under node, call each export, and decode every `substring` record
    // straight out of `memory` (char count at +0, byte count at +4, bytes at +8).
    // The decoded text and headers must match the interpreters' `builtin_substring`
    // — critically, `substring("café", 3, 4)` is the multi-byte `é` (char_len 1,
    // byte_len 2), proving the char->byte mapping.
    let runner = std::env::temp_dir().join("lullaby_wasm_string_ops_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const u8=new Uint8Array(e.memory.buffer);\
           function dec(ptr){{const cl=dv.getInt32(ptr,true);const bl=dv.getInt32(ptr+4,true);\
             const s=Buffer.from(u8.slice(ptr+8,ptr+8+bl)).toString();return s+'/'+cl+'/'+bl;}}\
           const lines=[\
             'sub_af='+dec(e.sub_af()),\
             'sub_e='+dec(e.sub_e()),\
             'sub_full='+dec(e.sub_full()),\
             'sub_empty='+dec(e.sub_empty()),\
             'find_present='+e.find_present().toString(),\
             'find_absent='+e.find_absent().toString(),\
             'find_empty='+e.find_empty().toString(),\
             'contains_true='+e.contains_true().toString(),\
             'contains_false='+e.contains_false().toString(),\
             'starts_true='+e.starts_true().toString(),\
             'starts_false='+e.starts_false().toString(),\
             'ends_true='+e.ends_true().toString(),\
             'ends_false='+e.ends_false().toString(),\
             'main='+e.main().toString()\
           ];\
           process.stdout.write(lines.join(';'));\
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
    // Char-indexed substring slices, including a multi-byte char (char_len != byte_len).
    assert!(out_text.contains("sub_af=af/2/2"), "{out_text}");
    assert!(out_text.contains("sub_e=\u{e9}/1/2"), "{out_text}");
    assert!(out_text.contains("sub_full=caf\u{e9}/4/5"), "{out_text}");
    assert!(out_text.contains("sub_empty=/0/0"), "{out_text}");
    // `find` returns a CHAR index (present), -1 (absent), 0 (empty needle).
    assert!(out_text.contains("find_present=2"), "{out_text}");
    assert!(out_text.contains("find_absent=-1"), "{out_text}");
    assert!(out_text.contains("find_empty=0"), "{out_text}");
    // Byte-exact predicates, true and false cases.
    assert!(out_text.contains("contains_true=1"), "{out_text}");
    assert!(out_text.contains("contains_false=0"), "{out_text}");
    assert!(out_text.contains("starts_true=1"), "{out_text}");
    assert!(out_text.contains("starts_false=0"), "{out_text}");
    assert!(out_text.contains("ends_true=1"), "{out_text}");
    assert!(out_text.contains("ends_false=0"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
}

#[test]
fn wasm_to_string_execution_parity_with_node() {
    // `to_string(x)` compiles to WASM for integer/bool/char/byte/string arguments,
    // building `[char_len][byte_len][utf8]` records identical to the interpreters'
    // `Value::Display`. Floats are DEFERRED (no float `to_string` appears in the
    // fixture). The fixture exercises signed/unsigned/`i64::MIN`/`u64::MAX`/zero
    // integers, fixed-width kinds, `bool`, `byte`, ASCII + multi-byte `char`, and
    // the string identity, returning a deterministic joined `i64` length from
    // `main` plus per-type `string`-returning exports the node runner decodes.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_to_string.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_to_string.wasm");
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

    // Every function compiles to WASM (none is skipped/demoted to the interpreters).
    let emit_out = stdout(&emit);
    for name in [
        "i64_text",
        "i64_min_text",
        "u64_text",
        "fixed_text",
        "bool_text",
        "byte_text",
        "char_of",
        "char_text",
        "string_id",
        "main",
    ] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile to WASM, got: {emit_out}"
        );
    }

    // Interpreter ground truth for `main` (the joined char count of the bundle).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "78");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM to_string execution parity");
        return;
    }

    // Instantiate under node, call each string-returning export, and decode its
    // record straight out of `memory` (char count at +0, byte count at +4, bytes at
    // +8). The decoded text must match the interpreters' `to_string` bit-for-bit,
    // including `i64::MIN`, `u64::MAX`, a byte magnitude passed as a parameter, and
    // a 2-byte UTF-8 char (char_len = 1, byte_len = 2).
    let runner = std::env::temp_dir().join("lullaby_wasm_to_string_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const u8=new Uint8Array(e.memory.buffer);\
           const dec=(ptr)=>{{\
             const cl=dv.getInt32(ptr,true);\
             const bl=dv.getInt32(ptr+4,true);\
             const s=Buffer.from(u8.slice(ptr+8,ptr+8+bl)).toString();\
             return s+'/'+cl+'/'+bl;\
           }};\
           const lines=[\
             'i64='+dec(e.i64_text()),\
             'i64min='+dec(e.i64_min_text()),\
             'u64='+dec(e.u64_text()),\
             'fixed='+dec(e.fixed_text()),\
             'bool='+dec(e.bool_text()),\
             'byte='+dec(e.byte_text(200)),\
             'char='+dec(e.char_of(233)),\
             'chars='+dec(e.char_text()),\
             'sid='+dec(e.string_id()),\
             'main='+e.main().toString()\
           ];\
           process.stdout.write(lines.join(';'));\
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
    // Signed decimal with a negative and zero; all ASCII, so char_len == byte_len.
    assert!(out_text.contains("i64=42,0,-7/7/7"), "{out_text}");
    // `i64::MIN` prints its full negative magnitude (20 chars incl. the `-`).
    assert!(
        out_text.contains("i64min=-9223372036854775808/20/20"),
        "{out_text}"
    );
    // `to_u64(0 - 1)` is `u64::MAX` — the unsigned magnitude, not `-1`.
    assert!(
        out_text.contains("u64=18446744073709551615/20/20"),
        "{out_text}"
    );
    // `i8` wraps to -128; `u32` prints its magnitude.
    assert!(
        out_text.contains("fixed=-128|4000000000/15/15"),
        "{out_text}"
    );
    assert!(out_text.contains("bool=true,false/10/10"), "{out_text}");
    // `byte(200)` passed via the parameter prints decimal 200.
    assert!(out_text.contains("byte=200/3/3"), "{out_text}");
    // A 2-byte UTF-8 scalar (é = U+00E9): one char, two bytes.
    assert!(out_text.contains("char=é/1/2"), "{out_text}");
    // ASCII + 2-byte char: two chars, three bytes.
    assert!(out_text.contains("chars=Aé/2/3"), "{out_text}");
    // `to_string(string)` is the identity record.
    assert!(out_text.contains("sid=kept/4/4"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
}

#[test]
fn wasm_value_if_execution_parity_with_node() {
    // A value-producing tail `if`/`elif`/`else` (each branch yields the function's
    // result value) now compiles to WASM: the `if` emits a typed block so the
    // branch value is left on the stack. Previously such functions were skipped.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_value_if.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_value_if.wasm");
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
    let emit_out = stdout(&emit);
    for name in ["sign_of", "abs_or_zero", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` (value-producing `if`) to compile to WASM, got: {emit_out}"
        );
    }

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "145");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM value-if execution parity");
        return;
    }
    let runner = std::env::temp_dir().join("lullaby_wasm_value_if_runner.js");
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
    assert!(out_text.contains("main=145"), "{out_text}");
}

#[test]
fn wasm_aggregate_args_execution_parity_with_node() {
    // Aggregates across call boundaries: a `main -> i64` that passes a struct to a
    // function reading its fields, receives a struct another function returns, and
    // takes+returns a fixed array — plus a value-semantics probe where a callee
    // mutates its struct/array PARAMETER and the caller's copy stays unchanged.
    // Every aggregate argument is deep-copied at the call site, so the WASM result
    // must equal the interpreter's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_aggregate_args.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_aggregate_args.wasm");
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
    // Every function — the struct/array takers and returners, the value-semantics
    // mutator, and `main` — compiles to WASM (none skipped).
    let listing = stdout(&emit);
    for name in [
        "sum_point",
        "make_point",
        "first_of",
        "bump",
        "mutate_point",
        "main",
    ] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // Interpreter ground truth for `main` (which drives every case).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "150");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM aggregate-args execution parity");
        return;
    }

    // Instantiate and compare `main()` to the interpreter. The value-semantics of
    // the deep copies are baked into `main`: `arr_untouched` (1, not 101) proves
    // `bump` did not mutate the caller's array, and `caller_unchanged` (11, not
    // 1998) proves `mutate_point` did not mutate the caller's struct.
    let runner = std::env::temp_dir().join("lullaby_wasm_aggregate_args_runner.js");
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
        "WASM aggregate-args `main` must equal the interpreter (value semantics): {out_text}"
    );
}

#[test]
fn wasm_nested_aggregate_args_execution_parity_with_node() {
    // The recursive deep-copy path: aggregates nested inside aggregates crossing
    // call boundaries — a struct holding a struct, and an array of arrays. When a
    // callee mutates a nested field/element of its parameter, the caller's copy
    // must be untouched, which requires the copy-on-pass to recurse into nested
    // mutable aggregates. `main` returns the interpreter-checked total.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_aggregate_nested.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_aggregate_nested.wasm");
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
    let listing = stdout(&emit);
    for name in ["outer_total", "wreck", "rows_sum", "wreck_rows", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // Interpreter ground truth.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "32");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM nested-aggregate execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_aggregate_nested_runner.js");
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
        "WASM nested-aggregate `main` must equal the interpreter (recursive value semantics): {out_text}"
    );
}

#[test]
fn wasm_fixed_width_integers_execution_parity_with_node() {
    // The fixed-width integer step: three fixtures whose `main` returns `i64` but
    // whose bodies exercise the width-normalized operations (wrapping arithmetic,
    // signedness-correct comparison/division, bitwise/shift, `~`, and the
    // `to_<T>`/`to_i64` conversions). Each compiles to WASM now, and each exported
    // `main` must equal the interpreter's ground truth bit-for-bit.
    let cases: [(&str, &str); 4] = [
        ("run_int_widths", "2147483649"),
        ("run_int_widths_wide", "7"),
        ("run_bitwise_widths", "410"),
        // `i64::MIN / -1` must wrap to `i64::MIN` (result 7) rather than trap the
        // WASM `i64.div_s`, on both the plain-i64 and fixed-width signed paths.
        ("run_div_overflow", "7"),
    ];

    // Emit each module and confirm `main` compiled (not skipped).
    let mut wasm_paths: Vec<(String, std::path::PathBuf, String)> = Vec::new();
    for (name, expected) in cases {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_wasm_{name}.wasm"));
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
        assert!(emit.status.success(), "{}: {}", name, stderr(&emit));
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: `main` should compile to WASM, got: {}",
            stdout(&emit)
        );

        // Interpreter ground truth.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}: {}", name, stderr(&run));
        assert_eq!(stdout(&run).trim(), expected, "{name} interpreter result");

        wasm_paths.push((name.to_string(), out, expected.to_string()));
    }

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM fixed-width execution parity");
        return;
    }

    // A runner that instantiates each module and prints `name=main()`. `main`
    // returns `i64`, which is a BigInt in JS.
    for (name, out, expected) in &wasm_paths {
        let runner = std::env::temp_dir().join(format!("lullaby_wasm_{name}_runner.js"));
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
            "{name} node failed: {}",
            String::from_utf8_lossy(&node.stderr)
        );
        let out_text = String::from_utf8_lossy(&node.stdout);
        assert!(
            out_text.contains(&format!("main={expected}")),
            "{name}: WASM `main` must equal the interpreter ({expected}), got: {out_text}"
        );
    }
}

#[test]
fn wasm_float_execution_parity_with_node() {
    // The float step: two fixtures whose `main` returns `i64` but whose bodies
    // exercise `f32`/`f64` arithmetic, comparisons, and the `to_f32`/`to_f64`
    // conversions. Each compiles to WASM now (single-precision `f32.*` ops keep
    // f32 bit-identical to the interpreter), and each exported `main` must equal
    // the interpreter's ground truth.
    let cases: [(&str, &str); 2] = [("run_f32", "3"), ("native_floats", "9")];

    let mut wasm_paths: Vec<(String, std::path::PathBuf, String)> = Vec::new();
    for (name, expected) in cases {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_wasm_{name}.wasm"));
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
        assert!(emit.status.success(), "{}: {}", name, stderr(&emit));
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: `main` should compile to WASM, got: {}",
            stdout(&emit)
        );

        // Interpreter ground truth.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}: {}", name, stderr(&run));
        assert_eq!(stdout(&run).trim(), expected, "{name} interpreter result");

        wasm_paths.push((name.to_string(), out, expected.to_string()));
    }

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM float execution parity");
        return;
    }

    // A runner that instantiates each module and prints `main=main()`. `main`
    // returns `i64`, which is a BigInt in JS.
    for (name, out, expected) in &wasm_paths {
        let runner = std::env::temp_dir().join(format!("lullaby_wasm_{name}_runner.js"));
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
            "{name} node failed: {}",
            String::from_utf8_lossy(&node.stderr)
        );
        let out_text = String::from_utf8_lossy(&node.stdout);
        assert!(
            out_text.contains(&format!("main={expected}")),
            "{name}: WASM `main` must equal the interpreter ({expected}), got: {out_text}"
        );
    }
}

#[test]
fn wasm_enum_match_execution_parity_with_node() {
    // The enum + match step: a program whose `main` returns `i64` but whose body
    // exercises enum construction and `match` over the built-in `option<i64>`,
    // `result<i64, i64>` (scalar payloads), and a small user enum with a scalar
    // payload plus a wildcard arm, including a call returning `option<i64>` that
    // the caller matches. Each function compiles to WASM now (tag-based enum
    // records in linear memory), and the exported `main` must equal the
    // interpreter's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_enum_match.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_enum_match.wasm");
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
    // Every function (including the enum-payload matchers and constructors) must
    // COMPILE to WASM, not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["unwrap_or", "divide", "describe", "area", "pick", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "144");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM enum-match execution parity");
        return;
    }

    // Instantiate the module (no-op host imports) and call the exported `main`,
    // which threads every enum construction and match through linear memory.
    let runner = std::env::temp_dir().join("lullaby_wasm_enum_match_runner.js");
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
        "WASM enum+match `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_list_build_execution_parity_with_node() {
    // The growable `list<T>` step: a program that builds a scalar-element list via
    // `list_new`/`push` (crossing the initial capacity to trigger a grow+copy),
    // reads it with `get`/`len`, replaces an element with `set`, and drops the last
    // with `pop`. Each function compiles to WASM now (a `[len][cap][slots]` block
    // in linear memory), and the exported `main` must equal the interpreter's
    // ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_build.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_build.wasm");
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
    // Both functions must COMPILE to WASM (the list ops lower to linear memory),
    // not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["build", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "5879");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list-build execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_build_runner.js");
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
        "WASM list-build `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_list_value_semantics_execution_parity_with_node() {
    // The list value-semantics step: assigning a list to another binding shares an
    // `i32` pointer, but every mutating op (`push`/`set`) deep-copies first and a
    // list crossing a call boundary is deep-copied, so mutating one binding is
    // never observable through another. `main` probes an aliased binding, a
    // push-derived list, a set-derived list, and a callee that pushes to its
    // parameter; the WASM result must equal the interpreter's.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_value_semantics.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_value_semantics.wasm");
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
    let verbose = stdout(&emit);
    for func in ["mutate", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "334211");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list value-semantics parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_value_semantics_runner.js");
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
        "WASM list value-semantics `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_list_struct_and_nested_and_map_struct_execution_parity_with_node() {
    // Mutable-heap collection ELEMENTS/VALUES: a `list<struct>` (push structs, read a
    // field, `set` an element), a `list<list<i64>>` (one level of mutable nesting,
    // summed through nested `get`s), and a `map<i64, struct>` (`map_set`/`map_get`
    // returning `option<struct>`/`map_len`). CRUCIALLY it includes a value-semantics
    // probe: `get(ps, 2)` returns a struct that is mutated (`.x`/`.y` set to 1000/
    // 2000), then `get(ps, 2)` again must still read the ORIGINAL element — proving
    // `get` returns a deep copy (the interpreters' `values[i].clone()`), so the
    // mutable-aggregate element deep-copy matches the interpreters bit-for-bit. Each
    // function compiles to WASM (the collection's element/value deep-copy recurses
    // into the struct/nested list), and the exported `main` must equal the
    // interpreter's ground truth.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_struct.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_struct.wasm");
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
    // Every function must COMPILE to WASM (the mutable-heap element/value deep-copy
    // recursion lowers to linear memory), not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in [
        "point_sum",
        "grow_probe",
        "build_points",
        "nested_sum",
        "build_nested",
        "map_point_value",
        "build_map",
        "main",
    ] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "503411108");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list<struct>/map<K,struct> parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_struct_runner.js");
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
        "WASM list<struct>/nested/map<K,struct> `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_map_build_execution_parity_with_node() {
    // The growable `map<K, V>` step: a program that builds a scalar-key,
    // scalar-value map via `map_new`/`map_set` (inserting several keys plus an
    // in-place update, and crossing the initial capacity to trigger a grow+copy),
    // reads it with `map_get` (matching the returned `option<V>`), `map_has`, and
    // `map_len`. Each function compiles to WASM now (a `[len][cap][(k,v) pairs]`
    // block in linear memory), and the exported `main` must equal the
    // interpreter's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_map_build.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_map_build.wasm");
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
    // All functions must COMPILE to WASM (the map ops lower to linear memory),
    // not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["build", "lookup", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "5999509");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM map-build execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_map_build_runner.js");
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
        "WASM map-build `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_map_value_semantics_execution_parity_with_node() {
    // The map value-semantics step: assigning a map to another binding shares an
    // `i32` pointer, but every mutating op (`map_set`) deep-copies first and a map
    // crossing a call boundary is deep-copied, so mutating one binding is never
    // observable through another. `main` probes an aliased binding, an insert-
    // derived map, an update-derived map, and a callee that inserts into its
    // parameter; the WASM result must equal the interpreter's.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_map_value_semantics.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_map_value_semantics.wasm");
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
    let verbose = stdout(&emit);
    for func in ["probe", "value_of", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "2231100");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM map value-semantics parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_map_value_semantics_runner.js");
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
        "WASM map value-semantics `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_map_string_key_execution_parity_with_node() {
    // The `map<string, V>` step: a `string`-KEYED map compiles now. The lookup
    // compares keys by CONTENT (equal `byte_len` and identical UTF-8 bytes), not
    // pointer identity, exactly like the interpreters' `Value` equality — so a key
    // built by concatenation (`"a" + "b"`, a fresh string object) is the SAME key
    // as a separately-built literal `"ab"`. The fixture builds a `map<string, i64>`
    // and a `map<string, string>`, sets keys via concatenated/`to_string` strings,
    // updates an existing key (proving content equality overwrites, not appends),
    // and reads with `map_get`/`map_has`/`map_len`. The exported `main` must equal
    // the interpreters' ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_map_string_key.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_map_string_key.wasm");
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
    // Every function must COMPILE to WASM (the string-keyed map ops lower to linear
    // memory with a content-equality scan), not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["build_scores", "score", "build_labels", "label_len", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`, cross-checked across all three backends
    // (AST/IR/bytecode) so the WASM result is compared to a value every interpreter
    // agrees on.
    let mut interp_main = String::new();
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
        assert!(run.status.success(), "{}", stderr(&run));
        let got = stdout(&run).trim().to_string();
        if interp_main.is_empty() {
            interp_main = got;
        } else {
            assert_eq!(
                got, interp_main,
                "`{backend}` interpreter disagreed on map<string, _> ground truth"
            );
        }
    }
    assert_eq!(interp_main, "325634");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM map string-key execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_map_string_key_runner.js");
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
        "WASM map string-key `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_list_string_and_map_string_execution_parity_with_node() {
    // The `string`-element/value step: a `list<string>` (built with `push` of
    // literal, concatenated, and `to_string` strings, read with `get`/`len`, and
    // passed to helpers) and a `map<i64, string>` (built with `map_set`, read with
    // `map_get` matching the returned `option<string>`, plus `map_has`/`map_len`).
    // A `string` element/value is an `i32` pointer stored in one slot exactly like
    // a scalar and — because strings are immutable — is SHARED (not deep-recursed)
    // on the value-semantic deep copy. `grow_probe` pushes to its list parameter
    // and `main` re-reads the caller's list length to prove the caller is
    // unaffected. All functions must compile to WASM and the exported `main` must
    // equal each interpreter backend's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_string.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_string.wasm");
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
    // Every function must COMPILE to WASM (list<string>/map<i64,string> lower to
    // linear memory), not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in [
        "total_len",
        "grow_probe",
        "build_words",
        "build_names",
        "name_len",
        "main",
    ] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`, identical on all three interpreter
    // backends (AST/IR/bytecode).
    let mut interp_main = String::new();
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
        assert!(run.status.success(), "{}", stderr(&run));
        let result = stdout(&run).trim().to_string();
        if interp_main.is_empty() {
            interp_main = result.clone();
        }
        assert_eq!(
            result, interp_main,
            "backend `{backend}` must match the other interpreter backends"
        );
    }
    assert_eq!(interp_main, "13444740");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list<string>/map<string> parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_string_runner.js");
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
        "WASM list<string>/map<string> `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
fn wasm_js_dom_interop_execution_parity_with_node() {
    // The JS/DOM interop step: a program whose exported function calls the
    // `console_log(s)` and `dom_set_text(id, text)` host imports with computed
    // strings. The generated JS harness supplies `env.console_log` and
    // `env.dom_set_text`, decodes each (ptr, len) string out of `memory`, and
    // captures them; the captured strings must equal what the interpreter prints,
    // and the exported `main` must equal the interpreter's `main`.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_interop.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_interop.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // The emitted module exports `memory` and seeds the interop string literals.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );
    assert!(
        contains_subslice(&bytes, b"console_log") && contains_subslice(&bytes, b"dom_set_text"),
        "module imports the JS/DOM host functions"
    );

    // Interpreter ground truth. `main` calls `ui()` (which logs two console lines
    // and two dom lines) then returns 22, printed as the final line. Split the
    // side-effect lines from the return value.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let mut interp_lines: Vec<String> = stdout(&run)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let interp_return = interp_lines.pop();
    // console_log prints the string; dom_set_text prints `id=text`.
    assert_eq!(
        interp_lines,
        vec!["ready", "idle", "status=ready", "count=42"]
    );
    assert_eq!(interp_return.as_deref(), Some("22"));

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM JS/DOM interop execution parity");
        return;
    }

    // The harness decodes each string from the `(ptr, len)` host operands — `ptr`
    // points directly at the first UTF-8 byte and `len` is the byte length — so
    // it slices `[ptr, ptr + len)`, captures console/dom calls, and prints the
    // whole-program `main`.
    let runner = std::env::temp_dir().join("lullaby_wasm_interop_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const logs=[];const doms=[];let mem;\
         const dec=(ptr,len)=>Buffer.from(new Uint8Array(mem.buffer).slice(ptr,ptr+len)).toString();\
         const imports={{env:{{\
           log_i64:()=>{{}},\
           console_log:(p,l)=>logs.push(dec(p,l)),\
           dom_set_text:(ip,il,tp,tl)=>doms.push(dec(ip,il)+'='+dec(tp,tl))\
         }}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           mem=r.instance.exports.memory;\
           const main=r.instance.exports.main().toString();\
           process.stdout.write('logs='+logs.join('|')+';doms='+doms.join('|')+';main='+main);\
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
    // The captured `console_log` sequence equals the interpreter's stdout lines.
    assert!(out_text.contains("logs=ready|idle"), "{out_text}");
    // The captured `dom_set_text` `id=text` sequence equals the interpreter's.
    assert!(
        out_text.contains("doms=status=ready|count=42"),
        "{out_text}"
    );
    // Whole-program `main` matches the interpreter.
    assert!(out_text.contains("main=22"), "{out_text}");
}

// -- Full-stack web demo (WASM frontend + Lullaby HTTP backend, shared module) -

/// Every file of the full-stack example checks: the shared domain module, the
/// WASM frontend, the HTTP backend, and the copied `http` framework module.
#[test]
fn fullstack_example_files_check() {
    let dir = workspace_root().join("examples/valid/fullstack");
    for file in ["shared.lby", "frontend.lby", "backend.lby", "http.lby"] {
        let output = lullaby()
            .args(["check", dir.join(file).to_str().expect("file path")])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{file}: {output:?}");
    }
}

/// The frontend compiles to a real `.wasm` module (shared module included), and
/// — when `node` is present — instantiating it with capturing
/// `env.console_log` / `env.dom_set_text` imports renders the shared labels and
/// the exported `main` returns the summed shared priority score. The interpreter
/// is the ground truth for both.
#[test]
fn fullstack_frontend_wasm_matches_shared_logic() {
    let fixture = workspace_root().join("examples/valid/fullstack/frontend.lby");
    let out = std::env::temp_dir().join("lullaby_fullstack_frontend.wasm");
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
    // The frontend entry and the imported shared logic all compiled.
    let listing = stdout(&emit);
    for name in ["main", "render", "classify", "priority_score"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // Valid WASM: the `\0asm` magic header plus the exported memory and the two
    // JS/DOM host imports the shared frontend uses.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(bytes.starts_with(b"\0asm"), "wasm magic header");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );
    assert!(
        contains_subslice(&bytes, b"console_log") && contains_subslice(&bytes, b"dom_set_text"),
        "module imports the JS/DOM host functions"
    );

    // Interpreter ground truth: two console/dom lines per rendered task, then the
    // summed shared priority score (quick=1 + detailed=3 + empty=0 = 4).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let mut interp_lines: Vec<String> = stdout(&run)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let interp_return = interp_lines.pop();
    assert_eq!(
        interp_lines,
        vec![
            "quick",
            "task_a=quick",
            "detailed",
            "task_b=detailed",
            "empty",
            "task_c=empty",
        ]
    );
    assert_eq!(interp_return.as_deref(), Some("4"));

    if !node_available() {
        eprintln!("node not found on PATH; skipping full-stack frontend WASM parity");
        return;
    }

    // Instantiate under node with capturing host imports and assert the rendered
    // shared labels and the exported score match the interpreter.
    let runner = std::env::temp_dir().join("lullaby_fullstack_frontend_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const logs=[];const doms=[];let mem;\
         const dec=(ptr,len)=>Buffer.from(new Uint8Array(mem.buffer).slice(ptr,ptr+len)).toString();\
         const imports={{env:{{\
           log_i64:()=>{{}},\
           console_log:(p,l)=>logs.push(dec(p,l)),\
           dom_set_text:(ip,il,tp,tl)=>doms.push(dec(ip,il)+'='+dec(tp,tl))\
         }}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           mem=r.instance.exports.memory;\
           const main=r.instance.exports.main().toString();\
           process.stdout.write('logs='+logs.join('|')+';doms='+doms.join('|')+';main='+main);\
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
    // The shared classification labels rendered to the console and the DOM.
    assert!(out_text.contains("logs=quick|detailed|empty"), "{out_text}");
    assert!(
        out_text.contains("doms=task_a=quick|task_b=detailed|task_c=empty"),
        "{out_text}"
    );
    // The summed shared priority score matches the interpreter.
    assert!(out_text.contains("main=4"), "{out_text}");
}

/// Drive the full-stack backend as a real HTTP client on all three backends and
/// assert the `/classify` body comes from the shared domain module (the same
/// label/score the frontend renders for the sample title).
#[test]
fn fullstack_shared_logic_round_trip() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn request(port: u16, path: &str) -> String {
        let mut stream = None;
        for _ in 0..100 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        let mut stream = stream.expect("connect to lullaby backend");
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("client write");
        stream.flush().expect("client flush");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("client read to EOF");
        response
    }

    let backend_path = workspace_root().join("examples/valid/fullstack/backend.lby");

    for backend in ["ast", "ir", "bytecode"] {
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
            probe.local_addr().expect("addr").port()
        };

        // Serve two requests: the shared `/classify` route and an unknown path.
        let path = backend_path.clone();
        let port_arg = port.to_string();
        let server = std::thread::spawn(move || {
            lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    path.to_str().expect("backend path"),
                    &port_arg,
                    "2",
                ])
                .output()
                .expect("run cli")
        });

        // The shared route: 200 with the classification body for the sample title
        // "Write the design document" (detailed, score 3, valid), computed by the
        // shared module — the same values the WASM frontend renders.
        let classify = request(port, "/classify");
        let status_line = classify.lines().next().unwrap_or_default();
        assert_eq!(
            status_line, "HTTP/1.1 200 OK",
            "{backend} status line for /classify: {classify:?}"
        );
        assert!(
            classify.contains("label=detailed"),
            "{backend} shared label for /classify: {classify:?}"
        );
        assert!(
            classify.contains("score=3"),
            "{backend} shared score for /classify: {classify:?}"
        );
        assert!(
            classify.contains("valid=true"),
            "{backend} shared validity for /classify: {classify:?}"
        );
        assert!(
            classify.contains("title=Write the design document"),
            "{backend} sample title for /classify: {classify:?}"
        );

        // Unknown route still 404s through the shared router seed.
        let missing = request(port, "/does-not-exist");
        let missing_status = missing.lines().next().unwrap_or_default();
        assert_eq!(
            missing_status, "HTTP/1.1 404 Not Found",
            "{backend} status line for unknown path: {missing:?}"
        );

        let output = server.join().expect("server thread");
        assert!(
            output.status.success(),
            "{backend} lullaby backend: {output:?}"
        );
    }
}

// -- Native x86-64 backend (i64-scalar subset, link-to-exe) ------------------

/// Locate `rust-lld.exe` under the rustc sysroot (mirrors the CLI's discovery).
/// `None` if rustc or the linker cannot be found.
fn rust_lld_path() -> Option<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let lld = PathBuf::from(sysroot).join("lib/rustlib/x86_64-pc-windows-msvc/bin/rust-lld.exe");
    lld.is_file().then_some(lld)
}

/// Locate a toolchain executable on `PATH` (e.g. `llvm-pdbutil`) for optional,
/// gracefully-skipped real-toolchain checks. Tries the bare name and `.exe`.
fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for candidate in [dir.join(name), dir.join(format!("{name}.exe"))] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether `kernel32.lib` is reachable via the `LIB` environment variable.
fn kernel32_available() -> bool {
    std::env::var("LIB").ok().is_some_and(|lib| {
        lib.split(';')
            .any(|dir| !dir.is_empty() && PathBuf::from(dir.trim()).join("kernel32.lib").is_file())
    })
}

/// Emit + verbose-list the native object for the i64-scalar fixture. This part
/// always runs: it exercises the emitter and CLI wiring regardless of linking.
#[test]
fn native_emits_object_and_lists_functions() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_list.exe");
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
    let listing = stdout(&output);
    for name in ["add", "fib", "sum_to", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // The object file is always written (the reliable floor) and starts with the
    // AMD64 COFF machine magic (0x8664, little-endian).
    let obj = out.with_extension("obj");
    let bytes = std::fs::read(&obj).expect("read native object");
    assert_eq!(&bytes[0..2], &[0x64, 0x86], "COFF AMD64 machine");
}

/// `lullaby native --target x86_64-unknown-linux-gnu` writes a relocatable ELF64
/// object beginning with the ELF magic. On this Windows host the object is not
/// linked or run (deferred to the native platform / Phase 9 CI); the CLI reports
/// exactly that.
#[test]
fn native_target_linux_emits_elf_object() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_linux.o");
    let output = lullaby()
        .args([
            "native",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    assert!(
        listing.contains("x86_64-unknown-linux-gnu (ELF64)"),
        "reports the ELF target: {listing}"
    );
    assert!(
        listing.contains("Phase 9") || listing.contains("deferred"),
        "reports link+run deferral: {listing}"
    );
    assert!(
        !listing.contains("native exe:"),
        "must not link an exe on this host: {listing}"
    );

    let bytes = std::fs::read(&out).expect("read ELF object");
    assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
    assert_eq!(bytes[4], 2, "ELFCLASS64");
}

/// `lullaby native --target x86_64-apple-darwin` writes a relocatable Mach-O
/// x86-64 object beginning with the Mach-O magic, also without linking.
#[test]
fn native_target_macos_emits_macho_object() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_macos.o");
    let output = lullaby()
        .args([
            "native",
            "--target",
            "x86_64-apple-darwin",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));

    let bytes = std::fs::read(&out).expect("read Mach-O object");
    // MH_MAGIC_64 (0xFEEDFACF) little-endian.
    assert_eq!(&bytes[0..4], &[0xCF, 0xFA, 0xED, 0xFE], "Mach-O magic");
}

/// `lullaby native --target aarch64-unknown-linux-gnu` writes a real aarch64
/// ELF64 object: the ELF magic, `EM_AARCH64` (183), the compiled scalar
/// functions, and the aarch64-specific link/run notice (not the x86-64 "deferred"
/// notice). This structural part always runs.
#[test]
fn native_target_aarch64_emits_elf_object() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_aarch64.o");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            "--target",
            "aarch64-unknown-linux-gnu",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    assert!(
        listing.contains("aarch64-unknown-linux-gnu (ELF64)"),
        "reports the aarch64 ELF target: {listing}"
    );
    assert!(
        listing.contains("aarch64 ELF object emitted"),
        "reports the aarch64 link/run notice: {listing}"
    );
    for name in ["add", "fib", "sum_to", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    let bytes = std::fs::read(&out).expect("read aarch64 ELF object");
    assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
    assert_eq!(bytes[4], 2, "ELFCLASS64");
    assert_eq!(
        u16::from_le_bytes([bytes[18], bytes[19]]),
        183,
        "e_machine = EM_AARCH64"
    );
}

/// Locate the LLVM cross-linker `ld.lld` shipped with the Rust toolchain — this
/// is `rust-lld` in gnu (ELF) flavor. `None` if it cannot be found.
fn ld_lld_path() -> Option<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let lld =
        PathBuf::from(sysroot).join("lib/rustlib/x86_64-pc-windows-msvc/bin/gcc-ld/ld.lld.exe");
    lld.is_file().then_some(lld)
}

/// Whether Docker with working arm64 (QEMU) emulation is available: probe with a
/// throwaway `linux/arm64` container, exactly as the task describes.
fn docker_arm64_available() -> bool {
    Command::new("docker")
        .args(["run", "--rm", "--platform", "linux/arm64", "alpine", "true"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// End-to-end AArch64 verification: emit the aarch64 ELF, link it with the
/// cross-linker (`ld.lld -m aarch64linux`) into an arm64 executable, run it under
/// Docker's arm64 (QEMU) emulation, and assert the process exit code equals the
/// interpreter's `run` result mod 256. Gated on Docker+arm64 and `ld.lld` being
/// available; skipped gracefully otherwise (like the node-gated WASM parity
/// tests). This is the real link+run proof that the AArch64 machine code is
/// correct, not just structurally well-formed.
#[test]
fn native_aarch64_links_and_runs_under_docker() {
    let Some(lld) = ld_lld_path() else {
        eprintln!("ld.lld not found in the Rust sysroot; skipping AArch64 link+run");
        return;
    };
    if !docker_arm64_available() {
        eprintln!("Docker arm64 emulation unavailable; skipping AArch64 link+run");
        return;
    }
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");

    // The expected exit code is the interpreter's `run` result mod 256.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run interpreter");
    assert!(run.status.success(), "{}", stderr(&run));
    let result: i64 = stdout(&run)
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .next_back()
        .expect("interpreter prints an integer result");
    let expected_code = result.rem_euclid(256) as i32;

    // Fresh working directory for the object + linked executable.
    let dir = std::env::temp_dir().join("lullaby_aarch64_run");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    let obj = dir.join("prog.o");
    let exe = dir.join("prog");

    // 1. Emit the aarch64 ELF object.
    let emit = lullaby()
        .args([
            "native",
            "--target",
            "aarch64-unknown-linux-gnu",
            "-o",
            obj.to_str().expect("obj path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("emit aarch64 object");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // 2. Link it into an arm64 executable with the cross-linker.
    let link = Command::new(&lld)
        .args([
            "-m",
            "aarch64linux",
            "-o",
            exe.to_str().expect("exe path"),
            obj.to_str().expect("obj path"),
        ])
        .output()
        .expect("run ld.lld");
    assert!(
        link.status.success(),
        "ld.lld failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );

    // 3. Run under arm64 emulation. Windows bind mounts do not carry the exec
    //    bit, so copy the binary and mark it executable before running it.
    let mount = format!("{}:/w", dir.display());
    let run_exe = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            "linux/arm64",
            "-v",
            &mount,
            "busybox",
            "sh",
            "-c",
            "cp /w/prog /prog && chmod +x /prog && /prog",
        ])
        .output()
        .expect("docker run arm64");

    // 4. The container exit code must equal the interpreter result mod 256.
    let code = run_exe.status.code().expect("container exit code");
    assert_eq!(
        code,
        expected_code,
        "aarch64 exit {code} must equal interpreter result {result} mod 256 ({expected_code}); docker stderr: {}",
        String::from_utf8_lossy(&run_exe.stderr)
    );
}

/// Whether Docker can run a native `linux/amd64` container (no QEMU needed).
fn docker_amd64_available() -> bool {
    Command::new("docker")
        .args(["run", "--rm", "--platform", "linux/amd64", "alpine", "true"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// End-to-end x86-64 ELF verification: emit the Linux x86-64 ELF, link it with
/// `ld.lld -m elf_x86_64`, run it under a native `linux/amd64` Docker container,
/// and assert the process exit code equals the interpreter's `run` result mod
/// 256. This proves the x86-64 ELF machine code + freestanding `exit`-syscall
/// entry actually execute on Linux, not merely that the object is well-formed.
/// Gated on Docker + `ld.lld`; skipped gracefully otherwise.
#[test]
fn native_elf_x86_64_links_and_runs_under_docker() {
    let Some(lld) = ld_lld_path() else {
        eprintln!("ld.lld not found in the Rust sysroot; skipping x86-64 ELF link+run");
        return;
    };
    if !docker_amd64_available() {
        eprintln!("Docker linux/amd64 unavailable; skipping x86-64 ELF link+run");
        return;
    }
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run interpreter");
    assert!(run.status.success(), "{}", stderr(&run));
    let result: i64 = stdout(&run)
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .next_back()
        .expect("interpreter prints an integer result");
    let expected_code = result.rem_euclid(256) as i32;

    let dir = std::env::temp_dir().join("lullaby_elf_x86_64_run");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create work dir");
    let obj = dir.join("prog.o");
    let exe = dir.join("prog");
    let emit = lullaby()
        .args([
            "native",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-o",
            obj.to_str().expect("obj path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("emit x86-64 ELF object");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let link = Command::new(&lld)
        .args([
            "-m",
            "elf_x86_64",
            "-o",
            exe.to_str().expect("exe path"),
            obj.to_str().expect("obj path"),
        ])
        .output()
        .expect("run ld.lld");
    assert!(
        link.status.success(),
        "ld.lld failed: {}",
        String::from_utf8_lossy(&link.stderr)
    );
    let mount = format!("{}:/w", dir.display());
    let run_exe = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--platform",
            "linux/amd64",
            "-v",
            &mount,
            "busybox",
            "sh",
            "-c",
            "cp /w/prog /prog && chmod +x /prog && /prog",
        ])
        .output()
        .expect("docker run amd64");
    let code = run_exe.status.code().expect("container exit code");
    assert_eq!(
        code,
        expected_code,
        "x86-64 ELF exit {code} must equal interpreter result {result} mod 256 ({expected_code}); docker stderr: {}",
        String::from_utf8_lossy(&run_exe.stderr)
    );
}

/// An unknown `--target` triple is rejected with `L0347` and no object is
/// produced.
#[test]
fn native_unknown_target_is_rejected() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let output = lullaby()
        .args([
            "native",
            "--target",
            "riscv64-unknown-linux-gnu",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "unknown target must fail");
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(combined.contains("L0347"), "reports L0347: {combined}");
}

/// `lullaby native --debug` must emit a CodeView `.debug$S` source-line section
/// (opt-in) and print the debug notice, while the default (no `--debug`) object
/// stays byte-for-byte identical. This structural part always runs. If
/// `llvm-pdbutil` is discoverable it optionally reads back the CodeView stream.
#[test]
fn native_debug_emits_codeview_line_info() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_debug.exe");

    let output = lullaby()
        .args([
            "native",
            "--debug",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("debug info: CodeView"),
        "expected the debug notice: {}",
        stdout(&output)
    );

    // The debug object carries a `.debug$S` section (searched in the section
    // header table: NumberOfSections at header offset 2, 40-byte headers after
    // the 20-byte COFF header, 8-byte name field).
    let obj = out.with_extension("obj");
    let bytes = std::fs::read(&obj).expect("read native debug object");
    let num_sections = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    let mut debug_hdr = None;
    for i in 0..num_sections {
        let hdr = 20 + i * 40;
        if &bytes[hdr..hdr + 8] == b".debug\x24S" {
            debug_hdr = Some(hdr);
        }
    }
    let hdr = debug_hdr.expect("`.debug$S` section present with --debug");

    // Its raw data begins with the CodeView C13 signature (4), and the source
    // file name and per-function declaration line (`main` on line 15) are
    // recoverable from the stream bytes.
    let raw_ptr = u32::from_le_bytes(bytes[hdr + 20..hdr + 24].try_into().unwrap()) as usize;
    let raw_size = u32::from_le_bytes(bytes[hdr + 16..hdr + 20].try_into().unwrap()) as usize;
    let section = &bytes[raw_ptr..raw_ptr + raw_size];
    assert_eq!(
        u32::from_le_bytes(section[0..4].try_into().unwrap()),
        4,
        "CodeView C13 signature"
    );
    assert!(
        section
            .windows(b"native_scalars.lby".len())
            .any(|w| w == b"native_scalars.lby"),
        "source file name recorded in the debug section"
    );

    // Without `--debug`, the object has no `.debug$S` section and is byte-for-byte
    // the default native object.
    let plain_out = std::env::temp_dir().join("lullaby_native_debug_off.exe");
    let plain = lullaby()
        .args([
            "native",
            "-o",
            plain_out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(plain.status.success(), "{}", stderr(&plain));
    let plain_bytes =
        std::fs::read(plain_out.with_extension("obj")).expect("read plain native object");
    let plain_sections = u16::from_le_bytes([plain_bytes[2], plain_bytes[3]]) as usize;
    for i in 0..plain_sections {
        let ph = 20 + i * 40;
        assert_ne!(
            &plain_bytes[ph..ph + 8],
            b".debug\x24S",
            "default object must have no debug section"
        );
    }

    // Optional real-toolchain readback. Prefer `llvm-readobj` (bundled with the
    // rustc toolchain that already provides `rust-lld`), else any `llvm-pdbutil`
    // or `llvm-readobj` on PATH. When found, decode the CodeView stream and assert
    // it surfaces the source file plus the `main` declaration line (15). Skip
    // gracefully when no such tool is discoverable.
    let readobj = llvm_readobj_path().or_else(|| find_tool("llvm-readobj"));
    if let Some(tool) = readobj {
        let dump = Command::new(tool)
            .args(["--codeview", obj.to_str().expect("obj path")])
            .output();
        if let Ok(dump) = dump {
            if dump.status.success() {
                let text = String::from_utf8_lossy(&dump.stdout);
                assert!(
                    text.contains("native_scalars.lby"),
                    "llvm-readobj should surface the source file: {text}"
                );
                assert!(
                    text.contains("LineNumberStart: 15"),
                    "llvm-readobj should surface `main`'s declaration line 15: {text}"
                );
            } else {
                eprintln!("llvm-readobj --codeview failed; skipping readback assertion");
            }
        }
    } else {
        eprintln!("no llvm-readobj/llvm-pdbutil found; skipping CodeView readback");
    }
}

/// Locate `llvm-readobj.exe` in the rustc toolchain bin dir (alongside
/// `rust-lld`). `None` if the toolchain or tool cannot be found.
fn llvm_readobj_path() -> Option<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let tool =
        PathBuf::from(sysroot).join("lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-readobj.exe");
    tool.is_file().then_some(tool)
}

/// A file with no i64-scalar function eligible reports diagnostic `L0339`.
#[test]
fn native_reports_no_eligible_functions() {
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

/// Best-effort execution parity: link the i64-scalar fixture into a real `.exe`
/// and assert its exit code equals the interpreter's `main` result (mod 256).
/// If `rust-lld` or `kernel32.lib` is unavailable, skip with a message.
#[test]
fn native_execution_parity_when_linkable() {
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
fn native_signed_div_overflow_parity_when_linkable() {
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
fn native_aggregates_execution_parity_when_linkable() {
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

/// Best-effort execution parity for the native **stack-argument** ABI:
/// native-compile a program whose functions take more than four scalar
/// parameters (six and eight `i64`, plus a mixed int/float six-parameter
/// signature), so their 5th+ arguments are passed on the stack above the shadow
/// space. Assert every such function compiles natively (not skipped), the
/// interpreter result agrees across AST/IR/bytecode, and — when linkable — the
/// `.exe` exit code equals the interpreter's `main` result (mod 256). Sources
/// MSVC's `LIB` (via vcvars64) when unset so the link+run executes.
#[test]
fn native_many_args_execution_parity_when_linkable() {
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
fn native_aggregate_boundary_execution_parity_when_linkable() {
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
fn native_enum_match_execution_parity_when_linkable() {
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
fn native_overflow_arith_execution_parity_when_linkable() {
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
fn wasm_overflow_arith_execution_parity_with_node() {
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
fn native_string_collections_execution_parity_when_linkable() {
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
fn native_list_execution_parity_when_linkable() {
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
fn native_list_struct_execution_parity_when_linkable() {
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
fn native_map_execution_parity_when_linkable() {
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
fn native_control_flow_execution_parity_when_linkable() {
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
fn native_strings_execution_parity_when_linkable() {
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
fn native_string_ops_execution_parity_when_linkable() {
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
fn native_string_build_execution_parity_when_linkable() {
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
fn native_floats_execution_parity_when_linkable() {
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
fn native_f32_precision_execution_parity_when_linkable() {
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

/// Whether `ucrt.lib` (the C runtime import library, providing `llabs`) is
/// reachable via the `LIB` environment variable, like `kernel32_available`.
fn ucrt_available() -> bool {
    std::env::var("LIB").ok().is_some_and(|lib| {
        lib.split(';')
            .any(|dir| !dir.is_empty() && PathBuf::from(dir.trim()).join("ucrt.lib").is_file())
    })
}

/// Best-effort: if the MSVC `LIB` environment variable is not already set (so a
/// native link would skip), construct it from the installed MSVC toolset and
/// Windows SDK x64 library directories and set it in this test process's
/// environment. The child `lullaby native` invocation inherits the environment,
/// so it can then discover `kernel32.lib`/`ucrt.lib` and actually link + run. A
/// no-op when `LIB` is already set or no MSVC/SDK install is found — the link+run
/// step then skips gracefully as before. Windows-only.
///
/// The directories are located directly on the filesystem (rather than by sourcing
/// `vcvars64.bat`) so the setup is independent of any shell quoting: `LIB` is the
/// concatenation of the MSVC toolset `lib\x64`, the Windows SDK `ucrt\x64`, and the
/// Windows SDK `um\x64` directories — exactly the three vcvars adds for a link.
fn ensure_msvc_env() {
    if std::env::var_os("LIB").is_some() {
        return;
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(msvc_lib) = latest_msvc_lib_x64() {
        dirs.push(msvc_lib);
    }
    let (ucrt, um) = latest_sdk_lib_x64();
    dirs.extend(ucrt);
    dirs.extend(um);
    // Only set `LIB` if we actually found the two the linker needs (kernel32.lib
    // lives in `um\x64`, ucrt.lib in `ucrt\x64`); otherwise leave it unset so the
    // gated tests skip cleanly.
    let has_kernel32 = dirs.iter().any(|d| d.join("kernel32.lib").is_file());
    let has_ucrt = dirs.iter().any(|d| d.join("ucrt.lib").is_file());
    if !has_kernel32 || !has_ucrt {
        return;
    }
    let joined = dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(";");
    // SAFETY: called from single-threaded test setup, before spawning any child.
    unsafe { std::env::set_var("LIB", joined) };
}

/// The newest MSVC toolset `lib\x64` directory across the known VS 2022 install
/// roots (Enterprise/Professional/Community/BuildTools), or `None` if none exist.
fn latest_msvc_lib_x64() -> Option<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    for base in vs_2022_roots() {
        let tools = base.join("VC\\Tools\\MSVC");
        let Ok(entries) = std::fs::read_dir(&tools) else {
            continue;
        };
        for entry in entries.flatten() {
            let lib = entry.path().join("lib\\x64");
            if lib.join("libcmt.lib").is_file() || lib.is_dir() {
                let version = entry.file_name().to_string_lossy().into_owned();
                if best.as_ref().is_none_or(|(v, _)| version > *v) {
                    best = Some((version, lib));
                }
            }
        }
    }
    best.map(|(_, path)| path)
}

/// The newest Windows SDK `ucrt\x64` and `um\x64` library directories (each as a
/// single-element vec, or empty if absent).
fn latest_sdk_lib_x64() -> (Vec<PathBuf>, Vec<PathBuf>) {
    for program_files in [
        std::env::var_os("ProgramFiles(x86)"),
        std::env::var_os("ProgramFiles"),
    ]
    .into_iter()
    .flatten()
    {
        let lib_root = PathBuf::from(&program_files).join("Windows Kits\\10\\Lib");
        let Ok(entries) = std::fs::read_dir(&lib_root) else {
            continue;
        };
        let mut best: Option<(String, PathBuf)> = None;
        for entry in entries.flatten() {
            let version = entry.file_name().to_string_lossy().into_owned();
            if entry.path().join("um\\x64\\kernel32.lib").is_file()
                && best.as_ref().is_none_or(|(v, _)| version > *v)
            {
                best = Some((version, entry.path()));
            }
        }
        if let Some((_, sdk)) = best {
            return (vec![sdk.join("ucrt\\x64")], vec![sdk.join("um\\x64")]);
        }
    }
    (Vec::new(), Vec::new())
}

/// The known Visual Studio 2022 install roots (per edition) on this machine.
fn vs_2022_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for base in [
        "C:\\Program Files\\Microsoft Visual Studio\\2022",
        "C:\\Program Files (x86)\\Microsoft Visual Studio\\2022",
    ] {
        for edition in ["Enterprise", "Professional", "Community", "BuildTools"] {
            let root = PathBuf::from(base).join(edition);
            if root.is_dir() {
                roots.push(root);
            }
        }
    }
    roots
}

/// C-ABI FFI: a program that declares `extern fn llabs x i64 -> i64` and returns
/// `llabs(-7)`. On the interpreters the extern call is rejected with `L0423`
/// (they cannot execute C). Native-compiled and linked against the C runtime
/// (`ucrt.lib`), the `.exe` calls the real C `llabs` and exits with code 7.
/// Gated on `rust-lld` + `kernel32.lib` + `ucrt.lib`; skips gracefully otherwise.
#[test]
fn ffi_calls_c_abs_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_llabs.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423 rather than
    // panicking or silently no-op-ing.
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
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_llabs.exe");
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

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 7, "llabs(-7) via C FFI must exit 7");
}

/// C-ABI FFI (non-`i64` scalar width): a program that declares
/// `extern fn toupper c i32 -> i32` and returns `to_i64(toupper(to_i32(97)))`.
/// `toupper('a')` is `'A'` (65), so the `.exe` exits with code 65. This exercises
/// the extended scalar marshalling: an `i32` C argument passed in the low bits of
/// `rcx` and an `i32` C return re-normalized in `rax` (`movsxd rax, eax`). On the
/// interpreters the extern call is rejected with `L0423`. Native-compiled and
/// linked against `ucrt.lib` (which provides `toupper`), the `.exe` calls the real
/// C `toupper`. Gated on `rust-lld` + `kernel32.lib` + `ucrt.lib`; skips otherwise.
#[test]
fn ffi_calls_c_toupper_i32_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_toupper.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423.
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
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_toupper.exe");
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

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping i32 C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 65, "toupper('a') via C FFI must exit 65 ('A')");
}

/// C-ABI FFI (float scalar): a program that declares `extern fn sqrt x f64 -> f64`
/// and computes `sqrt(16.0)` (== 4.0), then derives a deterministic `i64` via two
/// float comparisons (`> 3.9` gives 3, `< 4.1` adds 4) so the `.exe` exits 7. This
/// exercises the Win64 float marshalling: the `f64` argument is passed in `xmm0`
/// and the `f64` return is read from `xmm0`. On the interpreters the extern call is
/// rejected with `L0423`. Native-compiled and linked against `ucrt.lib` (which
/// provides `sqrt`), the `.exe` calls the real C `sqrt`. Gated on `rust-lld` +
/// `kernel32.lib` + `ucrt.lib`; skips gracefully otherwise.
#[test]
fn ffi_calls_c_sqrt_f64_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_sqrt.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423.
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
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    // Make MSVC's `LIB` available (source vcvars64 if it is not already set) so the
    // link+run step actually executes rather than skipping.
    ensure_msvc_env();

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_sqrt.exe");
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

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping f64 C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 7, "sqrt(16.0)==4.0 via C FFI must exit 7");
}

/// C-ABI FFI (mixed float + int scalars): a program that declares
/// `extern fn ldexp x f64 e i32 -> f64` and computes `ldexp(1.5, 3)` (== 12.0),
/// then derives a deterministic `i64` via two float comparisons so the `.exe`
/// exits 12. This exercises Win64 positional register routing: the `f64` at
/// position 0 goes to `xmm0`, the `i32` at position 1 goes to integer register 1
/// (`rdx`), and the `f64` return comes back in `xmm0` — each position consuming its
/// slot in exactly one register sequence. On the interpreters the extern call is
/// rejected with `L0423`. Native-compiled and linked against `ucrt.lib` (which
/// provides `ldexp`), the `.exe` calls the real C `ldexp`. Gated on `rust-lld` +
/// `kernel32.lib` + `ucrt.lib`; skips gracefully otherwise.
#[test]
fn ffi_calls_c_ldexp_mixed_scalars_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_ldexp.lby");

    // `check` validates the extern declaration and its call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the extern call with L0423.
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
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    ensure_msvc_env();

    // Native codegen: emit + link + run.
    let out = std::env::temp_dir().join("lullaby_ffi_ldexp.exe");
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

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib (via the LIB env var) not available; \
             skipping mixed float/int C-ABI FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 12, "ldexp(1.5, 3)==12.0 via C FFI must exit 12");
}

/// Assert every interpreter backend rejects an extern-call fixture with `L0423`
/// (FFI is native-only), then native-compile it and assert `main` compiled. The
/// shared preamble for the pointer/cstr/many-arg FFI link+run tests below.
fn assert_ffi_native_only_and_compiles(fixture: &Path) {
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

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
        assert!(
            !run.status.success(),
            "extern call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }
}

/// C-ABI FFI (`cstr` string marshalling): `extern fn strlen s cstr -> usize` is
/// called with a Lullaby `string` literal `"lullaby"`. The FFI boundary
/// materializes a NUL-terminated UTF-8 copy (`__lullaby_to_cstr`) and passes its
/// `const char*` to the real C `strlen`, which returns 7 — so the `.exe` exits 7.
/// This proves a Lullaby `string` round-trips to C as a `char*`. On the
/// interpreters the extern call is `L0423`. Gated on `rust-lld` + `kernel32.lib` +
/// `ucrt.lib`; sources MSVC's `LIB` when unset so the link+run executes.
#[test]
fn ffi_cstr_marshals_string_to_c_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_cstr_strlen.lby");
    assert_ffi_native_only_and_compiles(&fixture);

    ensure_msvc_env();
    let out = std::env::temp_dir().join("lullaby_ffi_cstr_strlen.exe");
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

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib not available; skipping cstr FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 7, "strlen(\"lullaby\") via cstr FFI must exit 7");
}

/// C-ABI FFI (raw pointer arguments/returns + round-trip): a Lullaby-controlled C
/// pointer round-trips through three C functions —
/// `malloc(16) -> ptr<byte>`, `strcpy(p, "hello") -> ptr<byte>` (a `cstr` source),
/// `strlen(p) -> usize`. `strlen` reads the buffer `strcpy` filled through the
/// `malloc`'d pointer, returning 5, so the `.exe` exits 5. This proves a
/// pointer alloc'd through C passes back into C by its raw machine address across
/// several calls. On the interpreters the extern call is `L0423`. Gated on
/// `rust-lld` + `kernel32.lib` + `ucrt.lib`; sources MSVC's `LIB` when unset.
#[test]
fn ffi_pointer_round_trips_through_c_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_ptr_roundtrip.lby");
    assert_ffi_native_only_and_compiles(&fixture);

    ensure_msvc_env();
    let out = std::env::temp_dir().join("lullaby_ffi_ptr_roundtrip.exe");
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

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib not available; skipping pointer FFI link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 5,
        "malloc+strcpy(\"hello\")+strlen round-trip must exit 5"
    );
}

/// C-ABI FFI (>4 extern arguments, Win64 stack spill): a caller object declares
/// `extern fn lullaby_sum6 a..f i64 -> i64` and calls it with six arguments
/// (`1+2+4+8+16+32 = 63`); a separate library object *exports* the same six-`i64`
/// function. Linking the two objects across the C ABI resolves the extern to the
/// export, so the `.exe` exits 63. This verifies the extern caller spills its 5th
/// and 6th arguments to the stack above the shadow space exactly where the export
/// callee reads them — end to end, without a C compiler (rust-lld links the two
/// Lullaby objects). On the interpreters the extern call is `L0423`. Gated on
/// `rust-lld` + `kernel32.lib` + `ucrt.lib`; sources MSVC's `LIB` when unset.
#[test]
fn ffi_extern_call_with_stack_args_when_linkable() {
    let caller = workspace_root().join("tests/fixtures/native_only/ffi_extern_sum6.lby");
    let callee = workspace_root().join("tests/fixtures/native_only/ffi_export_sum6.lby");

    // The caller (an extern call) is native-only and rejected by every interpreter.
    assert_ffi_native_only_and_compiles(&caller);

    // The callee is a C-callable library object (export only, no `main`); `check`
    // validates its export signature.
    let callee_check = lullaby()
        .args(["check", callee.to_str().expect("callee path")])
        .output()
        .expect("run cli");
    assert!(callee_check.status.success(), "{}", stderr(&callee_check));

    ensure_msvc_env();

    // Emit both objects. The CLI derives each `.obj` from the `-o` `.exe` stem and
    // writes it unconditionally (the caller's own self-link fails on the
    // unresolved export symbol, but the object is still produced — the reliable
    // floor).
    let caller_exe = std::env::temp_dir().join("lullaby_ffi_extern_sum6.exe");
    let callee_exe = std::env::temp_dir().join("lullaby_ffi_export_sum6.exe");
    let caller_obj = caller_exe.with_extension("obj");
    let callee_obj = callee_exe.with_extension("obj");
    let _ = std::fs::remove_file(&caller_obj);
    let _ = std::fs::remove_file(&callee_obj);
    for (src, exe) in [(&caller, &caller_exe), (&callee, &callee_exe)] {
        let emit = lullaby()
            .args([
                "native",
                "--verbose",
                "-o",
                exe.to_str().expect("out path"),
                src.to_str().expect("src path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{}", stderr(&emit));
    }
    // The caller's `main` and the callee's six-parameter export both compile
    // natively (the stack-argument ABI keeps the >4-arg extern call in the subset).
    let caller_emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            caller_exe.to_str().expect("out path"),
            caller.to_str().expect("caller path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        stdout(&caller_emit).contains("compiled main"),
        "expected caller `main` compiled: {}",
        stdout(&caller_emit)
    );
    assert!(caller_obj.is_file(), "expected caller object");
    assert!(callee_obj.is_file(), "expected callee object");

    if rust_lld_path().is_none() || !kernel32_available() || !ucrt_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib/ucrt.lib not available; skipping >4-arg extern link+run"
        );
        return;
    }

    // Link the two Lullaby objects into one executable. The caller supplies the
    // entry stub (`_lullaby_start`); the extern `lullaby_sum6` resolves to the
    // library object's exported symbol. `ucrt.lib` is on the line for the caller's
    // recorded C-runtime dependency (unused for this intra-Lullaby symbol).
    let lld = rust_lld_path().expect("rust-lld present (gate checked)");
    let linked = std::env::temp_dir().join("lullaby_ffi_sum6_linked.exe");
    let _ = std::fs::remove_file(&linked);
    let mut command = Command::new(&lld);
    command.args(["-flavor", "link", "/nologo", "/subsystem:console"]);
    command.arg("/entry:_lullaby_start");
    command.arg(format!("/out:{}", linked.display()));
    for dir in lib_dirs_from_env() {
        command.arg(format!("/libpath:{}", dir.display()));
    }
    command.arg(&caller_obj);
    command.arg(&callee_obj);
    command.arg("kernel32.lib");
    command.arg("ucrt.lib");
    let link = command.output().expect("run rust-lld");
    assert!(
        link.status.success(),
        "two-object link failed: {}{}",
        String::from_utf8_lossy(&link.stdout),
        String::from_utf8_lossy(&link.stderr)
    );

    assert!(
        linked.is_file(),
        "expected linked exe at {}",
        linked.display()
    );
    let exe = Command::new(&linked).output().expect("run linked exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 63,
        "lullaby_sum6(1,2,4,8,16,32) via a >4-arg C-ABI extern call must exit 63"
    );
}

/// The MSVC library search directories named by the `LIB` environment variable
/// (set in a Developer Command Prompt or by `ensure_msvc_env`). Used to build the
/// `/libpath:` arguments for a direct two-object `rust-lld` link.
fn lib_dirs_from_env() -> Vec<PathBuf> {
    std::env::var("LIB")
        .ok()
        .into_iter()
        .flat_map(|lib| {
            lib.split(';')
                .filter(|d| !d.is_empty())
                .map(|d| PathBuf::from(d.trim()))
                .collect::<Vec<_>>()
        })
        .filter(|d| d.is_dir())
        .collect()
}

/// Inline assembly: a `main` whose `unsafe` `asm` block emits the seven bytes of
/// `mov rax, 42` (`0x48,0xC7,0xC0,0x2A,0x00,0x00,0x00`). On the interpreters the
/// `asm` is rejected with `L0425` (raw machine code needs native codegen). Native-
/// compiled and linked, the emitted `mov rax, 42` reaches the Win64 epilogue, so
/// the process exits 42. Gated on `rust-lld` + `kernel32.lib`; skips gracefully.
#[test]
fn asm_emits_raw_bytes_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/asm_mov.lby");

    // `check` validates the asm shape (byte range + enclosing `unsafe`).
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects the `asm` with L0425.
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
        assert!(
            !run.status.success(),
            "asm must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0425"),
            "expected L0425 on {backend}: {rendered}"
        );
    }

    // Native codegen: emit + (best-effort) link + run.
    let out = std::env::temp_dir().join("lullaby_asm_mov.exe");
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
             skipping inline-asm link+run"
        );
        return;
    }

    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(exit, 42, "asm `mov rax, 42` must make the process exit 42");
}

/// Whether the raw bytes of a native COFF object or linked PE image contain any
/// C-runtime dependency marker. A CRT-linked Windows image imports one of these
/// runtime DLLs (`ucrtbase`, `vcruntime*`, `msvcrt`, or an `api-ms-win-crt-*`
/// forwarder); a freestanding (kernel32-only) image imports none of them, and a
/// freestanding object carries no undefined external symbol from them either.
fn contains_crt_marker(bytes: &[u8]) -> Option<String> {
    // Case-insensitive substring scan over the ASCII import/symbol names embedded
    // in the object/image. These markers never appear in a kernel32-only build.
    const CRT_MARKERS: [&[u8]; 4] = [b"ucrt", b"vcruntime", b"msvcrt", b"api-ms-win-crt"];
    let lower: Vec<u8> = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
    for marker in CRT_MARKERS {
        if lower.windows(marker.len()).any(|w| w == marker) {
            return Some(String::from_utf8_lossy(marker).into_owned());
        }
    }
    None
}

/// Freestanding / no-std native build: `lullaby native --freestanding` must emit
/// an executable with NO C-runtime dependency — only the minimal OS import
/// (`kernel32!ExitProcess`) needed to terminate. This proves that end to end:
///
/// - The emitted object contains no CRT import/symbol marker (structural, always
///   runs). The only undefined external is `ExitProcess` (kernel32).
/// - When `rust-lld` + `kernel32.lib` are available, the linked `.exe` also
///   contains no CRT DLL import and its exit code equals the interpreter result
///   (mod 256), proving the kernel32-only image runs correctly.
///
/// Skips the link+run gracefully when the toolchain is unavailable, but always
/// runs the object-level no-CRT assertion.
#[test]
fn native_freestanding_has_no_crt_dependency_when_linkable() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_native_freestanding.exe");

    let emit = lullaby()
        .args([
            "native",
            "--freestanding",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let listing = stdout(&emit);
    assert!(
        listing.contains("freestanding (no-std)"),
        "expected the freestanding no-CRT notice: {listing}"
    );
    assert!(
        listing.contains("compiled main"),
        "expected `main` compiled: {listing}"
    );

    // Structural (always runs): the emitted object has no C-runtime marker. The
    // only undefined external symbol is `ExitProcess` (from kernel32), which is
    // not a CRT dependency.
    let obj = out.with_extension("obj");
    let obj_bytes = std::fs::read(&obj).expect("read native object");
    if let Some(marker) = contains_crt_marker(&obj_bytes) {
        panic!("freestanding object must not reference the C runtime; found `{marker}`");
    }
    // Sanity: the object references `ExitProcess` (the minimal OS import).
    assert!(
        obj_bytes.windows(11).any(|w| w == b"ExitProcess"),
        "freestanding object should import kernel32!ExitProcess for process exit"
    );

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
             skipping freestanding link+run parity (object-level no-CRT check already ran)"
        );
        return;
    }

    // The linked image must also carry no C-runtime import (kernel32-only), and
    // its exit code must match the interpreter result (mod 256).
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe_bytes = std::fs::read(&out).expect("read linked exe");
    if let Some(marker) = contains_crt_marker(&exe_bytes) {
        panic!("freestanding exe must not import the C runtime; found `{marker}`");
    }
    let exe = Command::new(&out).output().expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit,
        (interp.rem_euclid(256)) as i32,
        "freestanding native exit code must equal the interpreter result (mod 256)"
    );
}

/// A freestanding build that also declares an `extern fn` (which requires the C
/// runtime import library `ucrt.lib`) is a contradiction: `--freestanding`
/// guarantees no C runtime. The CLI rejects the combination with `L0426` rather
/// than silently linking the CRT.
#[test]
fn native_freestanding_rejects_extern_fn_with_l0426() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_llabs.lby");
    let output = lullaby()
        .args([
            "native",
            "--freestanding",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        !output.status.success(),
        "freestanding + extern fn must be rejected"
    );
    let rendered = format!("{}{}", stdout(&output), stderr(&output));
    assert!(rendered.contains("L0426"), "expected L0426: {rendered}");
    assert!(
        rendered.contains("ucrt.lib"),
        "diagnostic should name the offending C runtime import library: {rendered}"
    );
}

/// Discover a C compiler for the export-into-Lullaby execution test: prefer
/// MSVC `cl.exe` (present in a Developer Command Prompt, alongside `kernel32.lib`
/// on `LIB`), else `clang`. Returns the compiler program name when it runs.
fn find_c_compiler() -> Option<&'static str> {
    for candidate in ["cl", "clang"] {
        let ok = Command::new(candidate)
            .arg(if candidate == "cl" { "/?" } else { "--version" })
            .output()
            .map(|out| out.status.success() || candidate == "cl")
            .unwrap_or(false);
        if ok {
            return Some(candidate);
        }
    }
    None
}

/// C-calls-into-Lullaby FFI: an `export fn add_seven x i64 -> i64` is compiled to
/// a *library* COFF object (no `main`, no entry stub) whose `add_seven` symbol is
/// externally visible and defined in `.text`. A tiny C program declares
/// `extern long long add_seven(long long);`, calls it, and returns the result;
/// compiled and linked against the Lullaby object, the `.exe` exits with the
/// value `add_seven` computes. Gated on a discoverable C compiler; skips
/// gracefully otherwise (the object emission part always runs).
#[test]
fn c_calls_into_exported_lullaby_function_when_compilable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/export_add_seven.lby");

    // `check` validates the export declaration and body (i64-scalar signature).
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Native codegen: emit the library object. `add_seven` compiles; there is no
    // `main`, so the CLI reports a C-callable library object rather than an exe.
    // The CLI derives the object path from the `-o` exe path (same stem, `.obj`).
    let exe_arg = std::env::temp_dir().join("lullaby_export_add_seven.exe");
    let obj = exe_arg.with_extension("obj");
    let _ = std::fs::remove_file(&obj);
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            exe_arg.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled add_seven"),
        "expected `add_seven` compiled: {}",
        stdout(&emit)
    );
    assert!(
        stdout(&emit).contains("C-callable library object"),
        "expected a C-callable library object report: {}",
        stdout(&emit)
    );
    assert!(obj.is_file(), "expected object at {}", obj.display());

    let Some(cc) = find_c_compiler() else {
        eprintln!("no C compiler (cl/clang) found; skipping C-calls-into-Lullaby execution");
        return;
    };

    // A tiny C caller that calls the exported Lullaby function.
    let c_src = std::env::temp_dir().join("lullaby_export_caller.c");
    std::fs::write(
        &c_src,
        "extern long long add_seven(long long);\nint main(void){ return (int)add_seven(35); }\n",
    )
    .expect("write c caller");
    let out_exe = std::env::temp_dir().join("lullaby_export_caller.exe");
    let _ = std::fs::remove_file(&out_exe);

    let link = if cc == "cl" {
        // cl caller.c lullaby.obj /Fe:out.exe (MSVC driver links the CRT + obj).
        Command::new("cl")
            .args(["/nologo"])
            .arg(&c_src)
            .arg(&obj)
            .arg(format!("/Fe:{}", out_exe.display()))
            .current_dir(std::env::temp_dir())
            .output()
    } else {
        Command::new("clang")
            .arg(&c_src)
            .arg(&obj)
            .arg("-o")
            .arg(&out_exe)
            .output()
    };
    let link = match link {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "C compiler `{cc}` could not link the export object; skipping run:\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        Err(error) => {
            eprintln!("could not run C compiler `{cc}`: {error}; skipping run");
            return;
        }
    };
    let _ = link;

    assert!(
        out_exe.is_file(),
        "expected linked exe at {}",
        out_exe.display()
    );
    let run = Command::new(&out_exe).output().expect("run c caller exe");
    let exit = run.status.code().expect("c caller exit code");
    // add_seven(35) == 42; the C `main` returns it as the process exit code.
    assert_eq!(
        exit, 42,
        "C caller into Lullaby `add_seven(35)` must exit 42"
    );
}

#[test]
fn test_runner_passes_on_demo_suite() {
    // The user-facing demo test suite has four `test_*` functions that all pass
    // via `assert`, with no `main`. `lullaby test` exits 0 and reports all pass.
    let demo = workspace_root().join("examples/valid/tests_demo/tests_demo.lby");
    let output = lullaby()
        .args(["test", demo.to_str().expect("demo path")])
        .output()
        .expect("run cli");
    let out = stdout(&output);
    assert!(output.status.success(), "{output:?}\n{out}");
    assert!(out.contains("PASS test_arith"), "{out}");
    assert!(out.contains("4 passed, 0 failed"), "{out}");
}

#[test]
fn test_runner_reports_failing_assert_and_exits_nonzero() {
    // A test that `assert(false)`s must fail: `lullaby test` prints FAIL with the
    // `assertion failed` message and exits non-zero.
    let tmp = std::env::temp_dir().join("lullaby_test_failing.lby");
    std::fs::write(
        &tmp,
        "fn test_passes -> void\n    assert(true)\n\nfn test_fails -> void\n    assert(false)\n",
    )
    .expect("write temp");
    let output = lullaby()
        .args(["test", tmp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    let out = stdout(&output);
    assert!(!output.status.success(), "{output:?}\n{out}");
    assert!(out.contains("PASS test_passes"), "{out}");
    assert!(out.contains("FAIL test_fails"), "{out}");
    assert!(out.contains("assertion failed"), "{out}");
    assert!(out.contains("1 passed, 1 failed"), "{out}");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn runs_project_manifest_across_backends() {
    // `project_demo` has a `lullaby.json` naming `src/main.lby` as its entry, its
    // own `src` module (`geometry`), and a local path dependency `mathx`. The
    // build resolves `import mathx`/`import geometry` across the project's and the
    // dependency's `src` directories and must produce 45 on every backend, whether
    // the argument is the project directory or the manifest path.
    let project = workspace_root().join("examples/valid/project_demo");
    let manifest = project.join("lullaby.json");
    for target in [&project, &manifest] {
        for backend in ["ast", "ir", "bytecode"] {
            let output = lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    target.to_str().expect("project path"),
                ])
                .output()
                .expect("run cli");
            assert!(output.status.success(), "{backend} {target:?}: {output:?}");
            assert_eq!(stdout(&output).trim(), "45", "{backend} {target:?}");
        }
    }
}

#[test]
fn checks_project_manifest() {
    let project = workspace_root().join("examples/valid/project_demo");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("ok:"), "{}", stdout(&output));
}

#[test]
fn checks_library_project_without_entry() {
    // `mathx` is a library project (no `entry`): `check` validates every module.
    let project = workspace_root().join("examples/valid/mathx");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("ok:"), "{}", stdout(&output));
}

#[test]
fn rejects_malformed_manifest_with_l0343() {
    let project = workspace_root().join("tests/fixtures/invalid/project_bad_manifest");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0343 [loader error]"), "{stderr}");
    assert!(stderr.contains("parse project manifest"), "{stderr}");
}

#[test]
fn rejects_missing_dependency_with_l0343() {
    let project = workspace_root().join("tests/fixtures/invalid/project_missing_dep");
    let output = lullaby()
        .args(["run", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0343 [loader error]"), "{stderr}");
    assert!(stderr.contains("ghost"), "{stderr}");
}

#[test]
fn rejects_cross_package_private_use_with_l0392() {
    // `app` imports the `libp` dependency and calls its private `hidden_helper`,
    // which is not visible across the package boundary.
    let project = workspace_root().join("tests/fixtures/invalid/project_private_cross/app");
    let output = lullaby()
        .args(["check", project.to_str().expect("project path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("L0392 [loader error]"), "{stderr}");
    assert!(stderr.contains("hidden_helper"), "{stderr}");
}

/// A unique, empty scratch directory for a `lullaby new` test, cleaned first so
/// re-runs start fresh. Keyed by the test name to avoid collisions.
fn scratch_dir(key: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lullaby_new_test_{key}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn new_scaffolds_a_runnable_project() {
    let work = scratch_dir("ok");
    let created = lullaby()
        .current_dir(&work)
        .args(["new", "bedtime"])
        .output()
        .expect("run cli");
    assert!(created.status.success(), "{created:?}");
    assert!(
        stdout(&created).contains("created bedtime/"),
        "{}",
        stdout(&created)
    );

    let root = work.join("bedtime");
    assert!(root.join("lullaby.json").is_file());
    assert!(root.join("src/main.lby").is_file());
    assert!(root.join(".gitignore").is_file());

    // The scaffold is a valid project the toolchain runs unmodified.
    let ran = lullaby()
        .current_dir(&work)
        .args(["run", "bedtime"])
        .output()
        .expect("run cli");
    assert!(ran.status.success(), "{ran:?}");
    assert!(
        stdout(&ran).contains("hello from bedtime"),
        "{}",
        stdout(&ran)
    );

    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn new_refuses_existing_directory() {
    let work = scratch_dir("exists");
    std::fs::create_dir(work.join("taken")).expect("pre-create dir");
    let output = lullaby()
        .current_dir(&work)
        .args(["new", "taken"])
        .output()
        .expect("run cli");
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("already exists"),
        "{}",
        stderr(&output)
    );
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn new_rejects_invalid_names() {
    let work = scratch_dir("invalid");
    for bad in ["my-app", "9lives", ""] {
        let output = lullaby()
            .current_dir(&work)
            .args(["new", bad])
            .output()
            .expect("run cli");
        assert!(!output.status.success(), "name {bad:?}: {output:?}");
    }
    // A rejected name creates nothing.
    assert!(!work.join("my-app").exists());
    let _ = std::fs::remove_dir_all(&work);
}
