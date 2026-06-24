use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn nlang() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nous_cli"))
}

#[test]
fn checks_valid_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/add.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("ok:"));
}

#[test]
fn runs_arithmetic_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_arithmetic.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_memory_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_memory.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "42");
}

#[test]
fn runs_while_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_while.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "4");
}

#[test]
fn runs_loop_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_loop.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "5");
}

#[test]
fn runs_logic_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_logic.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "true");
}

#[test]
fn runs_for_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn runs_for_step_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_for_step.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "9");
}

#[test]
fn runs_array_fixture() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_array.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "6");
}

#[test]
fn rejects_forbidden_braces() {
    let fixture = workspace_root().join("tests/fixtures/invalid/brace.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0102"));
}

#[test]
fn rejects_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/type_mismatch.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0303"));
}

#[test]
fn rejects_assignment_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/assignment_type_mismatch.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0314"));
}

#[test]
fn rejects_break_outside_loop() {
    let fixture = workspace_root().join("tests/fixtures/invalid/break_outside_loop.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0317"));
}

#[test]
fn rejects_logical_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/logical_type_mismatch.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0320"));
}

#[test]
fn rejects_ordering_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/ordering_type_mismatch.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0327"));
}

#[test]
fn rejects_for_range_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_range_type_mismatch.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0321"));
}

#[test]
fn rejects_for_zero_step_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/for_zero_step.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0411"));
}

#[test]
fn rejects_array_literal_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_literal_type_mismatch.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0324"));
}

#[test]
fn rejects_array_index_type_mismatch() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_type_mismatch.nl");
    let output = nlang()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0326"));
}

#[test]
fn rejects_array_index_out_of_bounds_at_runtime() {
    let fixture = workspace_root().join("tests/fixtures/invalid/array_index_out_of_bounds.nl");
    let output = nlang()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");

    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("N0413"));
}
