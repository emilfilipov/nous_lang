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

#[test]
fn checks_valid_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok:"));
}

#[test]
fn checks_valid_fixture_as_json() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.lullaby");
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
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("lby") | Some("lullaby")
            )
        })
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_streams.lullaby");
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
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("lby") | Some("lullaby")
            )
        })
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_arithmetic_fixture_with_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_inferred_let.lullaby");
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
fn compiles_fixture_to_bytecode_artifact_and_runs_it() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/valid/run_arithmetic.lullaby");
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
    let fixture = root.join("tests/fixtures/valid/run_arithmetic.lullaby");
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
    let fixture = root.join("tests/fixtures/valid/run_store.lullaby");
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
    assert!(stderr.contains("N0601 [bytecode error]"), "{stderr}");
    assert!(
        stderr.contains("unsupported bytecode artifact format"),
        "{stderr}"
    );
    let _ = std::fs::remove_file(artifact);
}

#[test]
fn rejects_planned_unsupported_syntax_with_dedicated_diagnostic() {
    for fixture_name in [
        "unsupported_import.lullaby",
        "unsupported_module.lullaby",
        "unsupported_struct.lullaby",
        "unsupported_catch.lullaby",
    ] {
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
            stderr.contains("N0211 [parser error]"),
            "{fixture_name}: {stderr}"
        );
        assert!(
            stderr.contains("planned beyond Alpha 1"),
            "{fixture_name}: {stderr}"
        );
    }
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
    assert!(stderr.contains("N0601 [bytecode error]"), "{stderr}");
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
    assert!(stderr.contains("\"code\":\"N0601\""), "{stderr}");
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
    assert!(stderr.contains("\"code\":\"N0601\""), "{stderr}");
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
    assert!(stderr.contains("\"code\":\"N0601\""), "{stderr}");
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
    let fixture = root.join("tests/fixtures/valid/run_arithmetic.lullaby");
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
    assert!(stderr.contains("\"code\":\"N0003\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"resource\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
fn runs_logic_fixture_with_optimized_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lullaby");
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
    assert!(stderr.contains("N0502 [optimizer error]"), "{stderr}");
    assert!(
        stderr.contains("--backend ir or --backend bytecode"),
        "{stderr}"
    );
}

#[test]
fn reports_optimizer_backend_mismatch_with_verbose_guidance() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lullaby");
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
    assert!(stderr.contains("N0502 [optimizer error]"), "{stderr}");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lullaby");
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
    assert!(stderr.contains("\"code\":\"N0502\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"optimizer\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
fn runs_memory_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_memory.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_memory_fixture_with_ir_backend() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_memory.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/valid/run_store.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_while_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_while.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4");
}

#[test]
fn runs_loop_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_loop.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "5");
}

#[test]
fn runs_logic_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}

#[test]
fn runs_for_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn runs_for_step_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for_step.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9");
}

#[test]
fn runs_array_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_array.lullaby");
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
    let fixture = root.join("tests/fixtures/valid/run_file_io.lullaby");
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
    let fixture = workspace_root().join("tests/fixtures/invalid/brace.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    let stderr = stderr(&output);
    assert!(stderr.contains("N0102 [lexer error]"), "{stderr}");
    assert!(
        stderr.contains("curly braces are not block delimiters"),
        "{stderr}"
    );
}

#[test]
fn reports_forbidden_braces_with_verbose_context() {
    let fixture = workspace_root().join("tests/fixtures/invalid/brace.lullaby");
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
    assert!(stderr.contains("N0102 [lexer error]"), "{stderr}");
    assert!(stderr.contains("Source:"), "{stderr}");
    assert!(stderr.contains("Problem:"), "{stderr}");
    assert!(stderr.contains("Root cause:"), "{stderr}");
    assert!(stderr.contains("Suggested fix:"), "{stderr}");
}

#[test]
fn reports_forbidden_braces_as_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/brace.lullaby");
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
    assert!(stderr.contains("\"code\":\"N0102\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"lexer\""), "{stderr}");
    assert!(
        stderr.contains("\"span\":{\"line\":2,\"column\":5}"),
        "{stderr}"
    );
    assert!(stderr.contains("\"root_cause\":"), "{stderr}");
}

#[test]
fn rejects_missing_indented_body() {
    let fixture = workspace_root().join("tests/fixtures/invalid/missing_indented_body.lullaby");
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
    assert!(stderr.contains("N0205 [parser error]"), "{stderr}");
    assert!(stderr.contains("Root cause:"), "{stderr}");
}

#[test]
fn rejects_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("N0303 [semantic error]"), "{stderr}");
    assert!(stderr.contains("N0301 [semantic error]"), "{stderr}");
}

#[test]
fn reports_type_mismatch_as_ordered_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/type_mismatch.lullaby");
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
    let n0303 = stderr.find("\"code\":\"N0303\"").expect("N0303");
    let n0301 = stderr.find("\"code\":\"N0301\"").expect("N0301");
    assert!(n0303 < n0301, "{stderr}");
    assert!(stderr.contains("\"function\":\"main\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
fn check_allows_library_style_source_without_main() {
    let fixture = workspace_root().join("tests/fixtures/invalid/missing_main.lullaby");
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
    let fixture = root.join("tests/fixtures/invalid/missing_main.lullaby");
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
    assert!(stderr.contains("N0329 [semantic error]"), "{stderr}");
    assert!(stderr.contains("zero-argument `main`"), "{stderr}");
    assert!(!artifact.exists(), "{artifact:?}");
}

#[test]
fn run_rejects_main_with_parameters_as_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/main_with_parameter.lullaby");
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
    assert!(stderr.contains("\"code\":\"N0329\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"semantic\""), "{stderr}");
    assert!(stderr.contains("\"function\":\"main\""), "{stderr}");
    assert!(stderr.contains("\"suggested_fix\":"), "{stderr}");
}

#[test]
fn rejects_assignment_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/assignment_type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0314"));
}

#[test]
fn rejects_break_outside_loop() {
    let fixture = workspace_root().join("tests/fixtures/invalid/break_outside_loop.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0317"));
}

#[test]
fn rejects_logical_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/logical_type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0320"));
}

#[test]
fn rejects_ordering_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/ordering_type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0327"));
}

#[test]
fn rejects_for_range_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_range_type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0321"));
}

#[test]
fn rejects_for_zero_step_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_zero_step.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0411"));
}

#[test]
fn rejects_array_literal_type_mismatch() {
    let fixture =
        workspace_root().join("tests/fixtures/invalid/array_literal_type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0324"));
}

#[test]
fn rejects_array_index_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0326"));
}

#[test]
fn rejects_array_index_out_of_bounds_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0413"));
}

#[test]
fn reports_runtime_error_with_verbose_traceback() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.lullaby");
    let output = lullaby()
        .args(["run", "--verbose", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("N0413 [runtime error]"), "{stderr}");
    assert!(stderr.contains("Traceback:"), "{stderr}");
    assert!(stderr.contains("in `main`"), "{stderr}");
    assert!(stderr.contains("Suggested fix:"), "{stderr}");
}

#[test]
fn reports_runtime_error_as_json() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.lullaby");
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
    assert!(stderr.contains("\"code\":\"N0413\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"runtime\""), "{stderr}");
    assert!(
        stderr.contains("\"traceback\":[{\"function\":\"main\""),
        "{stderr}"
    );
}

#[test]
fn rejects_store_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/store_type_mismatch.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0328"));
}

#[test]
fn rejects_use_after_free_at_compile_time() {
    let fixture = workspace_root().join("tests/fixtures/invalid/use_after_free.lby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(stderr(&output).contains("N0350"), "{}", stderr(&output));
}

#[test]
fn rejects_store_after_dealloc_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/store_after_dealloc.lullaby");
    let output = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0406"));
}

#[test]
fn rejects_missing_file_with_structured_resource_error() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/invalid/read_missing_file.lullaby");
    let _ = std::fs::remove_file(root.join("target/lullaby_missing_file.txt"));

    let output = lullaby()
        .current_dir(root)
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("N0414 [resource error]"), "{stderr}");
}

#[test]
fn reports_missing_file_resource_error_as_json() {
    let root = workspace_root();
    let fixture = root.join("tests/fixtures/invalid/read_missing_file.lullaby");
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
    assert!(stderr.contains("\"code\":\"N0414\""), "{stderr}");
    assert!(stderr.contains("\"phase\":\"resource\""), "{stderr}");
    assert!(stderr.contains("\"root_cause\":"), "{stderr}");
}

#[test]
fn rejects_extra_positionals() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.lullaby");
    let output = lullaby()
        .args([
            "check",
            fixture.to_str().expect("fixture path"),
            "extra.lullaby",
        ])
        .output()
        .expect("run cli");

    let stderr = stderr(&output);
    assert!(!output.status.success(), "{output:?}");
    assert!(stderr.contains("usage: lullaby check"), "{stderr}");
}

#[test]
fn rejects_file_builtin_argument_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/read_file_path_type.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0313"));
}

#[test]
fn rejects_write_file_content_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/write_file_content_type.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0313"));
}

#[test]
fn rejects_system_builtin_argument_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/sys_args_type.lullaby");
    let output = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0313"));
}
