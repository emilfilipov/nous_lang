//! CLI integration tests, part 25 — native same-name **shadowing** across lexical
//! scopes, verified END-TO-END against all three interpreters.
//!
//! # The bug these pin
//!
//! The native frame planner keyed every local by its bare source name in one flat
//! map and skipped a `let` whose name was already present. That collapsed an inner
//! shadowing binding (`let v` in a loop/`if`/`match` body) onto the SAME stack slot
//! as the outer `let v` it shadowed, so the inner write clobbered the outer value —
//! a cross-tier miscompile that shipped on `main`. With the safe-tier arena on it
//! became a use-after-free: the outer read landed on the inner slot, which the
//! loop's per-iteration bump-pointer rewind had already reclaimed.
//!
//! Measured on the unfixed backend (interpreters in parens):
//! * `while`-body shadow, outer read after → native **5** (all interpreters **17**).
//! * heap-string shadow with `char_code(v[0])` after an allocating loop → native
//!   **90** = `'Z'` (the inner `"ZZZZZZZZ"` slot) while every interpreter answered
//!   **65** = `'A'` (the outer `"A"`), the content-dependent divergence.
//!
//! The fix alpha-renames a shadowing binding apart before slot planning
//! (`native_object_rename.rs`), so each scope's binding gets its own slot — matching
//! the interpreters, whose per-block scope model gives every binding a fresh slot.
//!
//! # Teeth
//!
//! Each fixture below produced the wrong native value on the unfixed backend (see
//! the numbers above) and now agrees across all four tiers. The control
//! (`native_distinct_names_unchanged`) never shadowed and must stay correct, proving
//! the fix does not depend on the shadowing being present.

use crate::*;
use std::process::Command;

/// Run `source` on one interpreter backend and return its printed `main` result.
fn interpreter_result(source: &str, backend: &str, tag: &str) -> String {
    let dir = ScratchDir::new("shadow_interp");
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

/// Build `source` to a real `.exe` and return its exit code, or `None` off Windows.
/// Panics if the shape SKIPS — a regression that un-compiles a shadowing fixture is
/// a failure, not a silent pass.
fn native_exit_for(source: &str, tag: &str) -> Option<i32> {
    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping {tag}");
        return None;
    }
    let dir = ScratchDir::new("shadow_native");
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

/// Assert every tier agrees. Windows exit codes are full 32-bit values (not
/// truncated to 8 bits), so the native comparison is exact.
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

/// THE reported repro: an outer `let v` and a `while`-body `let v` that shadows it,
/// read the outer `v` after the loop. Unfixed native returned 5 (the inner value);
/// every interpreter returns 17.
#[test]
fn native_while_body_shadow_keeps_outer() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let v = 17\n",
            "    let i = 0\n",
            "    while i < 3\n",
            "        let v = 5\n",
            "        i = i + 1\n",
            "    v\n",
        ),
        "shadow_while_body",
        17,
    );
}

/// An `if`-branch `let v` shadows an outer `v` and mutates the inner copy; the outer
/// value must survive the branch.
#[test]
fn native_if_block_shadow_keeps_outer() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let v = 7\n",
            "    if 1 < 2\n",
            "        let v = 3\n",
            "        v = v + 100\n",
            "    v\n",
        ),
        "shadow_if_block",
        7,
    );
}

/// A `match`-arm binding shadows an outer `v`; the arm reads the inner binding, the
/// tail reads the outer. `describe(9)` = `(9 + 1)` inner `+ 50` outer = 60.
#[test]
fn native_match_arm_binding_shadow_keeps_outer() {
    assert_all_four_tiers_agree(
        concat!(
            "fn describe x i64 -> i64\n",
            "    let v = 50\n",
            "    let r = match some(x)\n",
            "        some(v) -> v + 1\n",
            "        none -> 0\n",
            "    r + v\n",
            "\n",
            "fn main -> i64\n",
            "    describe(9)\n",
        ),
        "shadow_match_arm",
        60,
    );
}

/// Nested loops each re-`let` `v` (and reuse the counter name is exercised in
/// `native_for_counter_shadow_keeps_outer`); the outermost `v` must be untouched.
#[test]
fn native_nested_loop_shadow_keeps_outer() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let v = 100\n",
            "    let i = 0\n",
            "    while i < 3\n",
            "        let v = i\n",
            "        let j = 0\n",
            "        while j < 2\n",
            "            let v = 99\n",
            "            j = j + 1\n",
            "        i = i + 1\n",
            "    v\n",
        ),
        "shadow_nested_loop",
        100,
    );
}

/// The two slots are genuinely distinct: the inner `v` is read INSIDE its scope
/// (each iteration adds the inner 3) and the outer `v` is read AFTER (still 10).
/// `acc = 3 * 4 = 12`, tail `= 12 + 10 + 90 = 112`. A single shared slot cannot
/// produce this: it would read the same value in both positions.
#[test]
fn native_inner_read_and_outer_read_are_distinct() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let v = 10\n",
            "    let acc = 0\n",
            "    let i = 0\n",
            "    while i < 4\n",
            "        let v = 3\n",
            "        acc = acc + v\n",
            "        i = i + 1\n",
            "    acc + v + 90\n",
        ),
        "shadow_inner_outer_distinct",
        112,
    );
}

/// A range-`for` counter shadows an outer `i`; the outer must survive the loop.
#[test]
fn native_for_counter_shadow_keeps_outer() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let i = 200\n",
            "    for i from 0 to 4\n",
            "        let x = i\n",
            "    i\n",
        ),
        "shadow_for_counter",
        200,
    );
}

/// A `for`-body `let` shadows an outer name; the outer survives.
#[test]
fn native_for_body_shadow_keeps_outer() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let v = 111\n",
            "    for k from 0 to 3\n",
            "        let v = k\n",
            "    v\n",
        ),
        "shadow_for_body",
        111,
    );
}

/// The arena-active use-after-free variant. Outer `v = "A"`; a loop that ALLOCATES
/// (an inner heap string bound to a shadowing `let v`) so the safe-tier arena
/// reclaims the inner slot each iteration; then `char_code(v[0])` of the OUTER `v`
/// after the loop. Unfixed native read the reclaimed inner slot and returned 90
/// (`'Z'`); every interpreter returns 65 (`'A'`). With distinct slots the outer
/// string (allocated before the loop mark, never rewound) stays valid.
#[test]
fn native_arena_heap_shadow_does_not_reclaim_outer() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let v = \"A\"\n",
            "    let n = 0\n",
            "    while n < 5\n",
            "        let v = \"ZZZZZZZZ\"\n",
            "        n = n + 1\n",
            "    char_code(v[0])\n",
        ),
        "shadow_arena_heap",
        65,
    );
}

/// Control: distinct names never shadow, so the fix must leave this unchanged and
/// correct. `a = 7 + 5*3 = 22`.
#[test]
fn native_distinct_names_unchanged() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let a = 7\n",
            "    let i = 0\n",
            "    while i < 3\n",
            "        let b = 5\n",
            "        a = a + b\n",
            "        i = i + 1\n",
            "    a\n",
        ),
        "shadow_control_distinct",
        22,
    );
}

/// Same-scope re-`let` (a `let x` rebinding a parameter `x`, then again) is NOT
/// cross-scope shadowing: the older binding is dead for the rest of the scope, so it
/// keeps its slot and stays correct. `f(40)` = 40 + 1 + 1 = 42.
#[test]
fn native_same_scope_rebind_unchanged() {
    assert_all_four_tiers_agree(
        concat!(
            "fn f x i64 -> i64\n",
            "    let x = x + 1\n",
            "    let x = x + 1\n",
            "    x\n",
            "\n",
            "fn main -> i64\n",
            "    f(40)\n",
        ),
        "shadow_same_scope_rebind",
        42,
    );
}
