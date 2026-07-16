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
//!
//! This suite also covers VOID-RETURNING functions, which live here because their
//! correctness question is the mirror image of the above: a void function has NO
//! value position, so the routing this suite pins must NOT be applied to it. Its
//! tail `if`/`match` is a statement, and the `block_yields_value` default-deny
//! gate — which a void body could never pass, having no value to yield — must
//! never be consulted for it. The codegen-level proofs live in
//! `crates/lullaby_ir/src/native_object_void_tests.rs`.

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

// -- VOID-returning functions -------------------------------------------------
//
// A function declaring no return type was NOT native-eligible: the eligibility
// gate ran its `void` return through the shared signature resolver, which
// answered — right for a parameter, wrong for a return — "type `void` is not in
// the native stack subset". Every void function skipped with `L0339`, and the
// demotion fixpoint cascaded that skip into every caller, so the natural driver
// spelling `fn poke p ptr<T> v T` could not be compiled at all and had to be
// worked around with a dummy `i64` return.

/// The natural DRIVER spelling: `fn poke p ptr<i64> v i64` writing through a
/// caller-supplied out-parameter. This is the shape every MMIO/kernel routine
/// wants, and the reason void eligibility matters.
///
/// This test stays native-scoped because what it pins is the **void `export fn`
/// eligibility and lowering** — a `void` driver compiling at all, and its tail shapes —
/// not the aliasing. The exe's exit code IS that verification: `addr_of` is a real
/// machine address natively, so `poke` genuinely mutates `main`'s local.
///
/// (This program used to be native-only *by design*: the interpreters refused a
/// cross-frame `addr_of` store with `L0459`, keeping each frame's locals in its own
/// environment. The **env shelf** retired that divergence — the interpreters now
/// resolve a cross-frame `addr_of` for real, and the four-tier agreement on exactly
/// this shape is pinned by `native_cross_frame_addr_of_matches_the_interpreters` and
/// `cross_frame_addr_of_reaches_the_callers_place` in `suite15.rs`.)
#[test]
fn native_void_driver_spelling_compiles_and_runs() {
    let source = concat!(
        "fn poke p ptr<i64> v i64\n",
        "    unsafe\n",
        "        ptr_write(p, v)\n",
        "\n",
        "fn main -> i64\n",
        "    let cell i64 = 0\n",
        "    unsafe\n",
        "        poke(addr_of(cell), 99)\n",
        "    return cell\n",
    );
    if let Some(exit) = native_exit_for(source, "lullaby_void_driver_poke") {
        assert_eq!(
            exit, 99,
            "`fn poke p ptr<i64> v i64` must compile natively and genuinely write \
             through the caller's out-parameter"
        );
    }
}

/// The void TAIL shapes, each called for effect through an out-parameter, all in
/// one program: a body ending in a non-exhaustive `if`, a body ending in a
/// `match`, and a bare `return` on both its taken and untaken paths.
///
/// The non-exhaustive `if` is the load-bearing case: `block_yields_value` refuses
/// exactly that shape in a VALUE position, so if a void tail were ever routed
/// through the value path this function would skip (`L0339`) and `native_exit_for`
/// would fail its "must COMPILE, not skip" assertion.
#[test]
fn native_void_tail_shapes_compile_and_run() {
    let source = concat!(
        // Ends in a NON-EXHAUSTIVE `if` — statement position.
        "fn poke_if p ptr<i64> n i64\n",
        "    unsafe\n",
        "        if n > 10\n",
        "            ptr_write(p, 1)\n",
        "        elif n > 5\n",
        "            ptr_write(p, 2)\n",
        "\n",
        // Ends in a `match` — statement position.
        "fn poke_match p ptr<i64> o option<i64>\n",
        "    unsafe\n",
        "        match o\n",
        "            some(v) -> ptr_write(p, v)\n",
        "            none -> ptr_write(p, 0)\n",
        "\n",
        // Bare `return` — no value to route.
        "fn poke_ret p ptr<i64> n i64\n",
        "    if n < 0\n",
        "        return\n",
        "    unsafe\n",
        "        ptr_write(p, n)\n",
        "\n",
        "fn main -> i64\n",
        "    let a i64 = 0\n",
        "    let b i64 = 0\n",
        "    let c i64 = 0\n",
        "    let d i64 = 7\n",
        "    unsafe\n",
        "        poke_if(addr_of(a), 20)\n",
        "        poke_match(addr_of(b), some(5))\n",
        "        poke_ret(addr_of(c), 3)\n",
        "        poke_ret(addr_of(d), -1)\n",
        "    return a * 1000 + b * 100 + c * 10 + d\n",
    );
    // a=1 (n>10 branch), b=5 (some payload), c=3 (written), d=7 (untouched: the
    // bare `return` took the early path) -> 1537.
    if let Some(exit) = native_exit_for(source, "lullaby_void_tail_shapes") {
        assert_eq!(
            exit, 1537,
            "a void body ending in a non-exhaustive `if`/`match`, and a bare \
             `return`, must each compile and take effect"
        );
    }
}

/// A void function called in a LOOP: the call is a statement whose (undefined)
/// `rax` is discarded on every iteration, and the accumulating out-parameter
/// write must land each time.
#[test]
fn native_void_call_in_a_loop_runs() {
    let source = concat!(
        "fn add_to p ptr<i64> v i64\n",
        "    unsafe\n",
        "        ptr_write(p, ptr_read(p) + v)\n",
        "\n",
        "fn main -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 1\n",
        "    while i <= 4\n",
        "        unsafe\n",
        "            add_to(addr_of(acc), i)\n",
        "        i = i + 1\n",
        "    return acc\n",
    );
    // 1+2+3+4 = 10.
    if let Some(exit) = native_exit_for(source, "lullaby_void_loop_call") {
        assert_eq!(
            exit, 10,
            "a void function called in a loop must apply its effect every iteration"
        );
    }
}

/// REGRESSION PIN: a void `main` must exit **0**, matching the interpreters —
/// not leak whatever its body left in `rax`.
///
/// `main` is the one void function whose "no value" is externally observable: the
/// entry stub is a CALLER of it, and it read `eax` unconditionally as the process
/// exit code. So a void `main` exited with the body's last computed value —
/// `77` for the shape below, and `210` (= `1234 & 0xFF`) for the 1234 variant —
/// where all three interpreters exit `0`.
///
/// The body is what makes this a real pin: a preceding `i64` call makes a NONZERO
/// `rax` overwhelmingly likely at the epilogue. An empty-bodied void `main` (as in
/// `tests/fixtures/valid/main.lby`) passes even on the broken backend, because the
/// fallthrough path's `xor rax, rax` zeroes `rax` by coincidence — which is
/// exactly why no existing test caught this.
///
/// Both nonzero values are checked, and both a value that survives `& 0xFF` (77)
/// and one that does not (1234 -> 210), so a fix that merely truncated would fail.
#[test]
fn native_void_main_exits_zero_like_the_interpreters() {
    for (leak, tag) in [(77, "77"), (1234, "1234")] {
        let source = format!(
            "fn f -> i64\n    {leak}\n\nfn main -> void\n    let x i64 = f()\n    return\n"
        );
        // The interpreters are the oracle: a void `main` produces no exit code, so
        // the process exits 0.
        for backend in ["ast", "ir", "bytecode"] {
            let path = std::env::temp_dir().join(format!("lullaby_void_main_{tag}_{backend}.lby"));
            std::fs::write(&path, &source).expect("write source");
            let output = lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    path.to_str().expect("src path"),
                ])
                .output()
                .expect("run cli");
            assert_eq!(
                output.status.code().expect("exit code"),
                0,
                "[{backend}] a void `main` must exit 0"
            );
        }
        if let Some(exit) = native_exit_for(&source, &format!("lullaby_void_main_{tag}")) {
            assert_eq!(
                exit, 0,
                "a void `main` must exit 0 like the interpreters, not leak `rax` \
                 (this returned {leak} from a call; the broken stub exited with it)"
            );
        }
    }
}

/// The void `main` fix must hold on EVERY return path the entry stub has to cover
/// — fallthrough (no `return` at all), an explicit tail `return`, a `return`
/// inside a branch, and a `return` inside a loop — since fixing at the stub rather
/// than at each epilogue is what makes that structural.
#[test]
fn native_void_main_exits_zero_on_every_return_path() {
    let cases: &[(&str, &str)] = &[
        ("fallthrough", "fn main -> void\n    let x i64 = f()\n"),
        (
            "tail_return",
            "fn main -> void\n    let x i64 = f()\n    return\n",
        ),
        (
            "return_in_branch",
            "fn main -> void\n    let x i64 = f()\n    if x > 0\n        return\n    let y i64 = 5\n",
        ),
        (
            "return_in_loop",
            "fn main -> void\n    let i i64 = 0\n    while i < 4\n        let x i64 = f()\n        if x > 0\n            return\n        i = i + 1\n",
        ),
    ];
    for (label, main_src) in cases {
        let source = format!("fn f -> i64\n    77\n\n{main_src}");
        if let Some(exit) = native_exit_for(&source, &format!("lullaby_void_main_path_{label}")) {
            assert_eq!(
                exit, 0,
                "a void `main` must exit 0 on the `{label}` path (77 = the leaked `rax`)"
            );
        }
    }
}

/// Cross-tier PARITY for void functions, over the interpreter-defined subset.
///
/// `native_void_effects.lby` keeps every void helper's effect inside its own
/// frame precisely so all three interpreters model it, giving a real four-way
/// comparison (the out-parameter programs above cannot have one — the
/// interpreters refuse a cross-frame `addr_of` store by design). It covers a void
/// body that loops, one ending in a non-exhaustive `if`, one ending in a `match`,
/// a bare `return`, and a void function calling another void function.
#[test]
fn native_void_effects_fixture_matches_the_interpreters() {
    let path = workspace_root().join("tests/fixtures/valid/native_void_effects.lby");
    let fixture = path.to_str().expect("fixture path");

    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args(["run", "--backend", backend, fixture])
            .output()
            .expect("run cli");
        assert!(
            output.status.success(),
            "[{backend}] the void-effects fixture must run. stderr: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "131",
            "[{backend}] void helpers must run for effect and leave the caller intact"
        );
    }

    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping the native leg");
        return;
    }
    let exe = std::env::temp_dir().join("lullaby_void_effects_fixture.exe");
    let _ = std::fs::remove_file(&exe);
    // `--verbose` is what prints the per-function `compiled`/`skipped` notes.
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            exe.to_str().expect("exe path"),
            fixture,
        ])
        .output()
        .expect("run native");
    assert!(
        emit.status.success(),
        "native emit failed for the void-effects fixture:\n{}",
        stderr(&emit)
    );
    // Every helper is void; if void eligibility regressed, all of them would skip
    // and take `main` with them through the demotion fixpoint.
    let notes = stdout(&emit);
    for name in [
        "poke_local",
        "spin",
        "classify_effect",
        "from_opt_effect",
        "early",
        "outer",
        "main",
    ] {
        assert!(
            notes.contains(&format!("compiled {name}")),
            "`{name}` must compile natively (void functions must not skip):\n{notes}"
        );
    }
    let run = Command::new(&exe).output().expect("run exe");
    assert_eq!(
        run.status.code().expect("exit code"),
        131,
        "native must agree with all three interpreters (131) on the void-effects fixture"
    );
}
