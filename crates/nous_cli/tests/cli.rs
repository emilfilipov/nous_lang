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
