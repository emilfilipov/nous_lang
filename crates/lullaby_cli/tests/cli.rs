use std::path::PathBuf;
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
fn compiles_fixture_to_bytecode_artifact_and_runs_it() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/valid/run_arithmetic.lby");
    let artifact = root.join("target/run_arithmetic.lbc");
    let _ = std::fs::remove_file(&artifact);

    let compile = lullaby()
        .args([
            "compile",
            "--optimize",
            "alpha",
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
            "alpha",
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
            stderr.contains("planned beyond Alpha 1"),
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
        "{\"format\":\"lullaby-bytecode\",\"version\":4,\"metadata\":{\"producer\":\"test\",\"target\":\"alpha1\",\"payload\":\"instruction-bytecode\"},\"entry\":\"main\",\"function_table\":[],\"module\":{\"functions\":[{\"name\":\"main\",\"params\":[],\"return_type\":{\"name\":\"i64\"},\"span\":{\"line\":1,\"column\":1}}]}}",
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
        "{\"format\":\"lullaby-bytecode\",\"version\":5,\"metadata\":{\"producer\":\"test\",\"target\":\"alpha1\",\"payload\":\"instruction-bytecode\"},\"entry\":\"main\",\"function_table\":[{\"name\":\"main\",\"params\":[],\"return_type\":{\"name\":\"i64\"}}],\"module\":{\"functions\":[{\"name\":\"main\",\"params\":[],\"return_type\":{\"name\":\"i64\"},\"instructions\":[{\"Break\":{\"line\":1,\"column\":1}}],\"span\":{\"line\":1,\"column\":1}}]}}",
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
fn runs_arithmetic_fixture_with_alpha_optimized_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "ir",
            "--optimize",
            "alpha",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_arithmetic_fixture_with_alpha_optimized_bytecode_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lby");
    let output = lullaby()
        .args([
            "run",
            "--backend",
            "bytecode",
            "--optimize",
            "alpha",
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
    // (strings/structs/arrays are now supported, `option`/enums are not): nothing
    // is eligible, so the WASM backend reports L0338.
    // `wasm` reuses the executable pipeline, which requires `main`; make `main`
    // itself return `option<i64>` so nothing is eligible and the emitter reports
    // L0338 rather than compiling anything.
    let source = "fn main -> option<i64>\n    some(1)\n";
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
    // export, and additionally reads the interned `[len i32][utf8]` string layout
    // straight out of `memory` at the reserved base (offset 16).
    let runner = std::env::temp_dir().join("lullaby_wasm_heap_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const slen=dv.getInt32(16,true);\
           const sbytes=new Uint8Array(e.memory.buffer).slice(20,20+slen);\
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
fn wasm_fixed_width_integers_execution_parity_with_node() {
    // The fixed-width integer step: three fixtures whose `main` returns `i64` but
    // whose bodies exercise the width-normalized operations (wrapping arithmetic,
    // signedness-correct comparison/division, bitwise/shift, `~`, and the
    // `to_<T>`/`to_i64` conversions). Each compiles to WASM now, and each exported
    // `main` must equal the interpreter's ground truth bit-for-bit.
    let cases: [(&str, &str); 3] = [
        ("run_int_widths", "2147483649"),
        ("run_int_widths_wide", "7"),
        ("run_bitwise_widths", "410"),
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

    // The harness decodes each string from the `[len i32][utf8]` layout at
    // `ptr`, captures console/dom calls, and prints the whole-program `main`.
    let runner = std::env::temp_dir().join("lullaby_wasm_interop_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const logs=[];const doms=[];let mem;\
         const dec=(ptr,len)=>Buffer.from(new Uint8Array(mem.buffer).slice(ptr+4,ptr+4+len)).toString();\
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
         const dec=(ptr,len)=>Buffer.from(new Uint8Array(mem.buffer).slice(ptr+4,ptr+4+len)).toString();\
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
    let source = "fn main -> string\n    \"hi\"\n";
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
