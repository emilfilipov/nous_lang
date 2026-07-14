//! End-to-end `lullaby fmt` comment-preservation tests.
//!
//! These exercise the real CLI binary (not just the library formatter) to prove
//! that `fmt`, `fmt --write`, and `fmt --check` preserve source comments through a
//! format round-trip. A formatter that silently deletes comments is a defect;
//! these tests are the regression guard for it.

use super::{lullaby, stdout};

/// The exact reported repro: a full-line comment and a trailing comment inside a
/// function body, both of which the pre-fix formatter deleted.
const REPRO: &str =
    "fn main -> i64\n    # this is a comment\n    let x i64 = 5  # trailing comment\n    x\n";

/// Write `source` to a fresh `.lby` file under the temp dir and return its path.
fn write_temp(name: &str, source: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("lullaby_fmt_comments_{name}.lby"));
    std::fs::write(&path, source).expect("write temp source");
    path
}

#[test]
fn fmt_print_preserves_comments_in_repro() {
    let path = write_temp("print_repro", REPRO);
    let output = lullaby()
        .args(["fmt", path.to_str().expect("path")])
        .output()
        .expect("run fmt");
    assert!(output.status.success(), "fmt failed: {output:?}");
    // The full-line and trailing comments both survive verbatim.
    assert_eq!(stdout(&output), REPRO);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fmt_write_then_check_is_clean_and_idempotent() {
    let path = write_temp("write_check", REPRO);

    // `--write` rewrites the file but keeps every comment.
    let write = lullaby()
        .args(["fmt", "--write", path.to_str().expect("path")])
        .output()
        .expect("run fmt --write");
    assert!(write.status.success(), "fmt --write failed: {write:?}");
    let on_disk = std::fs::read_to_string(&path).expect("read back");
    assert_eq!(on_disk, REPRO, "comments must survive --write");

    // `--check` on the already-formatted, commented file reports no diff (exit 0).
    let check = lullaby()
        .args(["fmt", "--check", path.to_str().expect("path")])
        .output()
        .expect("run fmt --check");
    assert!(
        check.status.success(),
        "fmt --check must be clean on a formatted commented file: {check:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fmt_preserves_leading_standalone_and_trailing_comments() {
    // A file with a leading file comment, a standalone comment attached to a
    // statement, and a trailing comment, across two functions. It is already
    // canonically formatted, so `fmt` is a fixed point (exact round-trip).
    let source = concat!(
        "# file header\n",
        "fn helper n i64 -> i64\n",
        "    # double it\n",
        "    n * 2  # result\n",
        "\n",
        "# entry point\n",
        "fn main -> i64\n",
        "    helper(21)\n",
    );
    let path = write_temp("leading_standalone_trailing", source);
    let output = lullaby()
        .args(["fmt", path.to_str().expect("path")])
        .output()
        .expect("run fmt");
    assert!(output.status.success(), "fmt failed: {output:?}");
    assert_eq!(stdout(&output), source);

    // And `--check` confirms there is no diff.
    let check = lullaby()
        .args(["fmt", "--check", path.to_str().expect("path")])
        .output()
        .expect("run fmt --check");
    assert!(
        check.status.success(),
        "fmt --check must be clean: {check:?}"
    );
    let _ = std::fs::remove_file(&path);
}
