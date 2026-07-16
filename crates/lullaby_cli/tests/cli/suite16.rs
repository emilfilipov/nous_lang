//! CLI integration tests, part 16 — VALUE-POSITION BRANCH/ARM TAIL parity: an
//! aggregate bound to a local inside an `if`/`match` branch and yielded as that
//! branch's tail expression must produce the same value natively as on the
//! interpreters.
//!
//! These are the named regression pins for a real, shipped native MISCOMPILE. The
//! routing that decides *where* a returned value must land (the hidden aggregate
//! result pointer, `xmm0`, or `rax`) was applied only to a function's own tail
//! expression and to `return`, never to an `if`-branch or `match`-arm tail. A
//! branch tail fell through to the generic statement path, which evaluates into
//! `rax` and DISCARDS, so an aggregate-returning function never wrote its hidden
//! result pointer at all — the caller read its own uninitialized scratch:
//!
//!   * `option<i64>` yielded a WRONG TAG AND PAYLOAD (native 100 vs 103, and 100
//!     vs 0 on the `none` path — the tag only looked right in the `some` case
//!     because a zeroed scratch word happens to be `some`'s discriminant);
//!   * `option<struct>` DEREFERENCED the never-written payload word as a struct
//!     pointer and crashed with `0xC0000005`.
//!
//! The class is swept far more broadly (four aggregate kinds x four tail shapes,
//! against real linked exes) by `gen_branch_tail_program` in `fuzz.rs`; these two
//! tests pin the exact reported reproductions with their exact expected values.
//! The instruction-selection proofs live in
//! `crates/lullaby_ir/src/native_object_tailvalue_tests.rs`.

use crate::*;
use std::process::Command;

/// Build `source` to a real `.exe` and return its exit code, or `None` when this
/// host cannot produce/run one. Direct-PE emission is the default for an eligible
/// build, so no external linker is required.
fn native_exit_for(source: &str, tag: &str) -> Option<i32> {
    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping {tag}");
        return None;
    }
    let dir = std::env::temp_dir();
    let src = dir.join(format!("{tag}.lby"));
    let exe = dir.join(format!("{tag}.exe"));
    std::fs::write(&src, source).expect("write source");
    let _ = std::fs::remove_file(&exe);

    let emit = lullaby()
        .args([
            "native",
            "-o",
            exe.to_str().expect("exe path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    assert!(
        emit.status.success(),
        "native emit failed for {tag}:\n{}",
        stderr(&emit)
    );
    assert!(
        exe.is_file(),
        "expected a native exe for {tag} (this shape must COMPILE, not skip):\n{}",
        stdout(&emit)
    );
    let run = Command::new(&exe).output().expect("run exe");
    Some(run.status.code().expect("exit code"))
}

/// The reported repro: `option<i64>` bound to a branch-local and yielded as the
/// branch tail. Native must agree with the interpreters on BOTH paths — the
/// `some` path (payload preserved: 103) and the `none` path (tag preserved: 0).
#[test]
fn native_branch_local_option_tail_matches_interpreters() {
    let pick = concat!(
        "fn pick n i64 -> option<i64>\n",
        "    if n > 0\n",
        "        let s option<i64> = some(n)\n",
        "        s\n",
        "    else\n",
        "        let e option<i64> = none\n",
        "        e\n",
        "\n",
    );
    // `some` path: the payload must survive (was 100 — payload silently zeroed).
    let some_src = format!(
        "{pick}fn main -> i64\n    match pick(3)\n        some(v) -> 100 + v\n        none -> 0\n"
    );
    if let Some(exit) = native_exit_for(&some_src, "lullaby_branch_tail_option_some") {
        assert_eq!(
            exit, 103,
            "a branch-local `option<i64>` tail must keep its payload (interpreters: 103)"
        );
    }
    // `none` path: the tag must survive too (was 100 — the function wrote nothing,
    // so the caller's zeroed scratch read back as `some(0)`).
    let none_src = format!(
        "{pick}fn main -> i64\n    match pick(-1)\n        some(v) -> 100 + v\n        none -> 0\n"
    );
    if let Some(exit) = native_exit_for(&none_src, "lullaby_branch_tail_option_none") {
        assert_eq!(
            exit, 0,
            "a branch-local `option<i64>` tail must keep its tag (interpreters: 0)"
        );
    }
}

/// The reported segfaulting variant: `option<struct>` — a HEAP payload — bound to
/// a branch-local and yielded as the branch tail. The never-written payload word
/// was dereferenced as a struct pointer (`0xC0000005`); it must now return 109,
/// matching the interpreters.
#[test]
fn native_branch_local_option_struct_tail_does_not_crash() {
    let source = concat!(
        "struct P\n",
        "    a i64\n",
        "    b i64\n",
        "\n",
        "fn pick n i64 -> option<P>\n",
        "    if n > 0\n",
        "        let s option<P> = some(P(n, n * 2))\n",
        "        s\n",
        "    else\n",
        "        let e option<P> = none\n",
        "        e\n",
        "\n",
        "fn main -> i64\n",
        "    match pick(3)\n",
        "        some(p) -> 100 + p.a + p.b\n",
        "        none -> 0\n",
    );
    if let Some(exit) = native_exit_for(source, "lullaby_branch_tail_option_struct") {
        assert_eq!(
            exit, 109,
            "a branch-local `option<struct>` tail must return the built struct \
             (interpreters: 109), not dereference an unwritten payload word"
        );
    }
}

/// An `asm` block as an `if`-BRANCH tail must compile AND run. This is the
/// freestanding/kernel-tier shape: `asm` is native-only (the interpreters reject it
/// with `L0425`), so refusing it does not demote the program to an interpreter — it
/// makes the program unbuildable ANYWHERE.
///
/// REGRESSION PIN: the `block_yields_value` default-deny gate was first added
/// without an `Asm` arm, which silently turned this shape into `L0339` "no functions
/// were eligible" even though the function-level tail-`asm` path had always trusted
/// an `asm` block to leave its value in `rax`. No fixture used an `asm` branch tail,
/// so `cargo test --all` stayed green while the shape broke. Both branches are
/// exercised, so a gate that refused either one fails here.
#[test]
fn native_asm_branch_tail_compiles_and_runs() {
    // `mov rax, 42` / `mov rax, 7` — the branch's value, left in `rax` per the
    // `asm` contract, returned by the convergence epilogue.
    let pick = concat!(
        "fn pick n i64 -> i64\n",
        "    if n > 0\n",
        "        unsafe\n",
        "            asm 72, 199, 192, 42, 0, 0, 0\n",
        "    else\n",
        "        unsafe\n",
        "            asm 72, 199, 192, 7, 0, 0, 0\n",
        "\n",
    );
    for (arg, want) in [("1", 42), ("-1", 7)] {
        let source = format!("{pick}fn main -> i64\n    pick({arg})\n");
        if let Some(exit) = native_exit_for(&source, &format!("lullaby_branch_tail_asm_{want}")) {
            assert_eq!(
                exit, want,
                "an `asm` branch tail must leave its value in `rax` and be returned \
                 by the convergence epilogue (pick({arg}) should exit {want})"
            );
        }
    }
}
