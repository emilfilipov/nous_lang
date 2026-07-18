//! CLI integration tests, part 26 — the explicit **`region` block** (arena
//! increment I1), verified END-TO-END across all four execution tiers.
//!
//! # What the region block is
//!
//! A bare `region` keyword followed by an indentation-only body (NO braces):
//! ```text
//! region
//!     scratch = build_report data
//!     print scratch
//! # scratch's arena is reclaimed here, at dedent
//! ```
//! It introduces a nested arena scope. The block's own bindings are lexically
//! scoped to the block (dead after dedent — referencing one afterward is `L0306`,
//! exactly like a loop-body `let`), which makes block-local values sound with no
//! escape analysis needed.
//!
//! # What this increment delivers, and the line drawn
//!
//! Execution is **value-neutral on every tier today**: the block runs as an
//! ordinary nested scope and NO tier reclaims, so native == the three interpreters
//! trivially. The native bulk-reclamation of the block's sub-region (rewinding the
//! bump pointer at dedent, reclaiming only when the block provably confines its
//! heap) is a follow-up increment. These fixtures pin the value-neutral contract
//! that reclamation must preserve: in particular
//! [`region_block_escaping_store_matches_all_tiers`] pins the escape channel a
//! reclaiming backend must NEVER reclaim (a heap value stored into an
//! outer-declared binding), so when reclamation lands this fixture becomes its
//! use-after-free guard.
//!
//! The oracle is native == interpreters, run through the shared four-tier harness.

use crate::*;
use std::process::Command;

/// Run `source` on one interpreter backend and return its printed `main` result.
fn interpreter_result(source: &str, backend: &str, tag: &str) -> String {
    let dir = ScratchDir::new("region_interpreter_result");
    let src = dir.join(format!("{tag}_{backend}.lby"));
    std::fs::write(&src, source).expect("write source");
    let out = lullaby()
        .args(["run", "--backend", backend, src.to_str().expect("src path")])
        .output()
        .expect("run interpreter");
    assert!(
        out.status.success(),
        "{backend} interpreter failed for {tag}:\n{source}\n{}",
        stderr(&out)
    );
    stdout(&out).trim().to_string()
}

/// Build `source` to a real `.exe` and return its exit code, or `None` when this
/// host cannot produce/run one. Panics if the shape SKIPS — a region-block fixture
/// that un-compiles here is a failure, not a silent pass (a stale binary would
/// otherwise mask an arena UAF, so the exe is deleted first and its presence
/// asserted before it is run).
fn native_exit_for(source: &str, tag: &str) -> Option<i32> {
    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping {tag}");
        return None;
    }
    let dir = ScratchDir::new("region_native_exit_for");
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
        "native emit failed for {tag}:\n{source}\n{}",
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

/// Assert every tier agrees: the three interpreters with each other, and the native
/// exe's exit code with them. Windows exit codes are full 32-bit values, so the
/// comparison is exact.
fn assert_all_four_tiers_agree(source: &str, tag: &str, expected: i64) {
    for backend in ["ast", "ir", "bytecode"] {
        assert_eq!(
            interpreter_result(source, backend, tag),
            expected.to_string(),
            "{backend} interpreter must produce {expected} for {tag}:\n{source}"
        );
    }
    if let Some(exit) = native_exit_for(source, tag) {
        assert_eq!(
            exit as i64, expected,
            "native must agree with the interpreters ({expected}) for {tag}:\n{source}"
        );
    }
}

/// The canonical region block doing scratch work: a block-local `string` is built,
/// its length folded into an outer scalar accumulator, and the block ends. Every
/// tier answers 3 (`len("7!!")`).
#[test]
fn region_block_scratch_matches_all_tiers() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let total i64 = 0\n",
            "    region\n",
            "        let s string = to_string(7) + \"!!\"\n",
            "        total = total + len(s)\n",
            "    total\n",
        ),
        "lullaby_region_scratch",
        3,
    );
}

/// Nested region blocks: an inner block inside an outer block, each with its own
/// block-local scratch string. Both folds land in the shared outer accumulator
/// (2 + 4 = 6). A mis-nested scope or an early reclamation would diverge.
#[test]
fn region_block_nested_matches_all_tiers() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let total i64 = 0\n",
            "    region\n",
            "        let a string = to_string(1) + \"a\"\n",
            "        total = total + len(a)\n",
            "        region\n",
            "            let b string = to_string(22) + \"bb\"\n",
            "            total = total + len(b)\n",
            "    total\n",
        ),
        "lullaby_region_nested",
        6,
    );
}

/// A region block INSIDE a loop INSIDE a (non-`main`) function: the block re-opens
/// each iteration, builds a per-iteration scratch string, and folds its length into
/// a loop-carried accumulator. `for i from 0 to 3` runs i = 0,1,2,3; each
/// `len(to_string(i) + "!")` is 2, so the total is 8. This exercises the region
/// block composing with the loop sub-region and the function scope simultaneously.
#[test]
fn region_block_in_loop_and_function_matches_all_tiers() {
    assert_all_four_tiers_agree(
        concat!(
            "fn work n i64 -> i64\n",
            "    let total i64 = 0\n",
            "    for i from 0 to n\n",
            "        region\n",
            "            let s string = to_string(i) + \"!\"\n",
            "            total = total + len(s)\n",
            "    total\n\n",
            "fn main -> i64\n",
            "    work(3)\n",
        ),
        "lullaby_region_in_loop",
        8,
    );
}

/// THE escape channel. A heap value (`to_string(42) + "!"`) is assigned to `keep`,
/// a binding declared OUTSIDE the block and read AFTER it — so the value genuinely
/// outlives the block. Every tier keeps it and answers 3 (`len("42!")`). Native
/// must NOT reclaim this value; when native reclamation lands, reclaiming the
/// block's sub-region here anyway would free `keep`'s record before `len(keep)`
/// reads it and native would diverge from the interpreters (the use-after-free the
/// confinement analysis exists to prevent). Today no tier reclaims, so this pins
/// the value-neutral floor the reclaiming backend must preserve.
#[test]
fn region_block_escaping_store_matches_all_tiers() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let keep string = \"\"\n",
            "    region\n",
            "        keep = to_string(42) + \"!\"\n",
            "    len(keep)\n",
        ),
        "lullaby_region_escape",
        3,
    );
}
