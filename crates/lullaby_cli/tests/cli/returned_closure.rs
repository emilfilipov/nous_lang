//! CLI integration tests — native **RETURNED-CLOSURE** support (safe-tier arena
//! stage-4, increment **a**: the invoke-plumbing prerequisite).
//!
//! # What this increment unblocks
//!
//! Before it, a function that RETURNS a closure and a caller that INVOKES a
//! call-returned closure both skipped native: `native_signature_eligibility` refused
//! a `fn(...)` return type, and `indirect_callable_sig` could not resolve a
//! call-returned `fn` local — so the whole function demoted to the interpreters. This
//! increment makes them compile:
//!
//! - a `fn(...)` RETURN is admitted as a single `I64` block-pointer word (`rax`),
//!   gated tightly to a **locally-created closure literal** (fresh, flat,
//!   scalar-capture) — a returned fn PARAMETER (aliases a caller's env) or a
//!   call-returned closure (increment b) stays refused;
//! - a `let g fn(...) = make_adder(5)` local becomes an **indirect callable**, so
//!   `g(3)` lowers through the existing closure indirect-call ABI (env pointer in
//!   `rcx`, `mov rax,[rcx]; call rax`).
//!
//! # Soundness (increment a does NOT touch arena reclamation)
//!
//! The returning factory stays **off the arena** (it returns a `fn`/heap value, so
//! `arena_eligible_functions` refuses it), so its return-edge never rewinds and the
//! returned `[code_ptr][captures…]` block stays live on the growing heap — the caller
//! can invoke it with no dangling. `native_returned_closure_survives_heap_churn` pins
//! that: a factory-captured value read back after the caller allocates heavily still
//! reads correctly, which it could not if the block had been reclaimed.
//!
//! Every tier agrees or refuses. The refusal-boundary tests pin that a returned fn
//! parameter, a heap-capturing returned closure, and a stored (aliased) closure each
//! skip cleanly (`L0339`) while the interpreters still compute the answer — correct-or-
//! refuse, never a miscompile.

use super::{ScratchDir, lullaby, stderr, stdout};

/// Compile `source` to a native `.exe` in `dir` and return its real (32-bit) exit
/// code. Asserts the program COMPILED natively — an emitted exe file must exist, so a
/// regression that makes a supported returned-closure shape silently SKIP is a
/// failure, not a vacuous pass. `run.status.code()` reads the true exit code (never
/// the shell's 8-bit-masked view).
fn native_exit(dir: &ScratchDir, tag: &str, source: &str) -> i32 {
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
        "no native exe produced for {tag} — a supported returned-closure shape must \
         COMPILE, not skip:\n{source}\n{}{}",
        stdout(&emit),
        stderr(&emit)
    );
    let run = std::process::Command::new(&exe)
        .output()
        .expect("run native exe");
    run.status.code().expect("native exit code")
}

/// Run `source` on the three interpreter tiers, asserting each succeeds and prints the
/// same integer; returns that integer (the ground truth the native tier must match).
fn interp_value(dir: &ScratchDir, tag: &str, source: &str) -> i64 {
    let src = dir.join(format!("{tag}.lby"));
    std::fs::write(&src, source).expect("write source");
    let mut value: Option<i64> = None;
    for backend in [None, Some("ir"), Some("bytecode")] {
        let mut args = vec!["run".to_string()];
        if let Some(b) = backend {
            args.push("--backend".to_string());
            args.push(b.to_string());
        }
        args.push(src.to_str().expect("src path").to_string());
        let out = lullaby().args(&args).output().expect("run cli");
        assert!(
            out.status.success(),
            "interpreter {backend:?} failed for {tag}:\n{source}\n{}",
            stderr(&out)
        );
        let v: i64 = stdout(&out)
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("interp {backend:?} did not print an i64 for {tag}"));
        match value {
            Some(prev) => assert_eq!(
                prev, v,
                "interpreter {backend:?} disagrees with an earlier tier for {tag}"
            ),
            None => value = Some(v),
        }
    }
    value.expect("at least one interpreter tier")
}

/// Assert all four tiers agree that `source` returns `expected`: the three
/// interpreters via stdout, native via a real `.exe` exit code.
fn assert_four_tiers(tag: &str, source: &str, expected: i64) {
    let dir = ScratchDir::new(tag);
    let interp = interp_value(&dir, tag, source);
    assert_eq!(
        interp, expected,
        "interpreters must return {expected} for {tag}"
    );
    let native = native_exit(&dir, tag, source);
    assert_eq!(
        native as i64, expected,
        "native must agree with the interpreters ({expected}) for {tag}"
    );
}

/// Assert native SKIPS `source` cleanly (`L0339`, no exe run) while the interpreters
/// still compute `expected` — the correct-or-refuse boundary.
fn assert_native_skips(tag: &str, source: &str, expected: i64) {
    let dir = ScratchDir::new(tag);
    let interp = interp_value(&dir, tag, source);
    assert_eq!(
        interp, expected,
        "the skipped program must still run on the interpreters for {tag}"
    );
    let src = dir.join(format!("{tag}.lby"));
    let native = lullaby()
        .args(["native", "--verbose", src.to_str().expect("src path")])
        .output()
        .expect("run native");
    assert!(
        !native.status.success(),
        "native must refuse {tag} (an escaping/non-fresh returned closure)"
    );
    let rendered = format!("{}{}", stdout(&native), stderr(&native));
    assert!(
        rendered.contains("L0339"),
        "expected a clean L0339 skip for {tag}: {rendered}"
    );
}

// -- Supported shapes: four-tier parity on a real exe exit code ---------------

/// The canonical factory: `make_adder(5)` returns `fn x -> x + n`, invoked `g(3)` = 8.
/// The closure captures the factory's parameter `n`; the returned block pointer is
/// bound to `g` and called through the indirect ABI.
#[test]
fn native_returned_adder_factory() {
    let source = "\
fn make_adder n i64 -> fn(i64) -> i64
    fn x i64 -> x + n
fn main -> i64
    let g fn(i64) -> i64 = make_adder(5)
    g(3)
";
    assert_four_tiers("ret_adder", source, 8);
}

/// A NON-capturing returned closure: the factory takes no arguments and returns a
/// capture-free `fn x -> x * 2`. The block is a bare `[code_ptr]` word.
#[test]
fn native_returned_noncapturing_closure() {
    let source = "\
fn make_doubler -> fn(i64) -> i64
    fn x i64 -> x * 2
fn main -> i64
    let g fn(i64) -> i64 = make_doubler()
    g(21)
";
    assert_four_tiers("ret_noncap", source, 42);
}

/// A MULTI-capture returned closure: `make_affine(3, 4)` returns `fn x -> a*x + b`,
/// so the env block carries two captured words read in order when invoked.
#[test]
fn native_returned_multicapture_closure() {
    let source = "\
fn make_affine a i64 b i64 -> fn(i64) -> i64
    fn x i64 -> a * x + b
fn main -> i64
    let g fn(i64) -> i64 = make_affine(3, 4)
    g(10)
";
    assert_four_tiers("ret_multicap", source, 34);
}

/// Shape B — the factory binds the closure to a LOCAL literal and returns the local
/// (an implicit tail `f`). `closure_local_ok`'s return relaxation admits it.
#[test]
fn native_returned_local_literal_closure() {
    let source = "\
fn make_adder n i64 -> fn(i64) -> i64
    let f fn(i64) -> i64 = fn x i64 -> x + n
    f
fn main -> i64
    let g fn(i64) -> i64 = make_adder(100)
    g(23)
";
    assert_four_tiers("ret_local", source, 123);
}

/// FLOAT captures, parameters, and return through the positional-XMM ABI:
/// `make_lin(2.0, 3.0)` returns `fn x y -> a*x + b*y` (two float captures, two float
/// params, float return), sampled at three argument vectors and counted into an i64
/// so a wrong XMM register would change the exit code.
#[test]
fn native_returned_float_closure() {
    let source = "\
fn make_lin a f64 b f64 -> fn(f64, f64) -> f64
    fn x f64 y f64 -> a * x + b * y
fn main -> i64
    let g fn(f64, f64) -> f64 = make_lin(2.0, 3.0)
    let total i64 = 0
    if g(10.0, 5.0) > 30.0
        total = total + 1
    if g(1.0, 1.0) > 4.0
        total = total + 2
    if g(0.0, 10.0) > 25.0
        total = total + 4
    total
";
    // g(10,5)=35>30 ✓(+1); g(1,1)=5>4 ✓(+2); g(0,10)=30>25 ✓(+4) → 7.
    assert_four_tiers("ret_float", source, 7);
}

/// The returned closure INVOKED MULTIPLE TIMES from one stored local — each call
/// re-reads the same env block, so a call that clobbered the env pointer would drift.
#[test]
fn native_returned_closure_invoked_multiple_times() {
    let source = "\
fn make_adder n i64 -> fn(i64) -> i64
    fn x i64 -> x + n
fn main -> i64
    let g fn(i64) -> i64 = make_adder(1)
    let a i64 = g(10)
    let b i64 = g(20)
    let c i64 = g(30)
    a + b + c
";
    // (10+1) + (20+1) + (30+1) = 63.
    assert_four_tiers("ret_multi_invoke", source, 63);
}

/// A factory with MULTIPLE RETURN EDGES returning DIFFERENT closures: each edge is its
/// own fresh literal (a distinct synthesized body), and the caller invokes whichever
/// the factory produced.
#[test]
fn native_returned_multiple_return_edges() {
    let source = "\
fn pick c bool -> fn(i64) -> i64
    if c
        return fn x i64 -> x + 1
    fn x i64 -> x * 100
fn main -> i64
    let g fn(i64) -> i64 = pick(false)
    let h fn(i64) -> i64 = pick(true)
    g(5) + h(5)
";
    // pick(false) → x*100 → 500; pick(true) → x+1 → 6; 500 + 6 = 506.
    assert_four_tiers("ret_multi_edge", source, 506);
}

/// **Soundness pin — the factory stays off the arena, so the returned block is never
/// reclaimed.** The factory captures `222`; after obtaining the closure the caller
/// runs a heap-churning helper that would reuse a freed region if the factory had
/// rewound its heap, then invokes the closure. Reading back exactly `222` proves the
/// block survived — a dangling/reclaimed block would read garbage.
#[test]
fn native_returned_closure_survives_heap_churn() {
    let source = "\
fn make_const n i64 -> fn(i64) -> i64
    fn x i64 -> x + n
fn churn -> i64
    let total i64 = 0
    let i i64 = 0
    while i < 50
        let s string = \"reuse-the-heap-region-aggressively\"
        total = total + len(s)
        i = i + 1
    total
fn main -> i64
    let g fn(i64) -> i64 = make_const(222)
    let junk i64 = churn()
    g(junk - junk)
";
    assert_four_tiers("ret_no_dangle", source, 222);
}

// -- Refusal boundary: native skips cleanly, interpreters still answer --------

/// A returned fn PARAMETER aliases a caller's env — never this increment's fresh-block
/// case — so `returns_only_local_closure_literals` refuses `identity_fn` while the
/// interpreters run it. This is the alias hazard the admit-fn-return gate must reject.
/// `base` is a direct literal (not a factory result), so the ONLY reason native
/// declines is the returned parameter — isolating that guard, whose teeth are proven by
/// injection in the closure fuzzer / this module's history.
#[test]
fn native_returned_fn_parameter_skips() {
    let source = "\
fn identity_fn f fn(i64) -> i64 -> fn(i64) -> i64
    return f
fn main -> i64
    let base fn(i64) -> i64 = fn x i64 -> x + 3
    let g fn(i64) -> i64 = identity_fn(base)
    g(7)
";
    assert_native_skips("ret_param_skip", source, 10);
}

/// A HEAP-capturing returned closure (`fn s -> p + s`, capturing a `string`) is
/// outside the scalar-only native closure subset, so native skips.
#[test]
fn native_returned_heap_capture_skips() {
    let source = "\
fn make_prefixer p string -> fn(string) -> string
    fn s string -> p + s
fn main -> i64
    let g fn(string) -> string = make_prefixer(\"hi-\")
    len(g(\"there\"))
";
    // "hi-" + "there" = "hi-there" → len 8.
    assert_native_skips("ret_heap_skip", source, 8);
}

/// A closure STORED (aliased) into another local rather than returned/called is an
/// escaping value read, so native skips — a different escape from a returned closure
/// and not unblocked by this increment.
#[test]
fn native_stored_closure_skips() {
    let source = "\
fn main -> i64
    let c fn(i64) -> i64 = fn x i64 -> x + 1
    let d fn(i64) -> i64 = c
    d(41)
";
    assert_native_skips("ret_stored_skip", source, 42);
}

// -- Arena stage-4b: MARK-ADVANCE PROMOTION of a returned closure --------------
//
// The factory is now ARENA-eligible (criterion 1b admits a promotable closure
// factory): its return-edge reset PROMOTES the survivor — relocates the returned
// `[code_ptr][captures…]` block DOWN to the region mark and advances `heap_next`
// past it (`heap_next = markF + size`, NOT `markF`) — so the factory reclaims its
// per-call scratch while the survivor lands in the caller's region and stays live
// until the caller's own rewind. These pins are four-tier (interpreters via stdout,
// native via a real `.exe` exit code); the teeth (a plain reset, a wrong size, an
// admitted non-fresh survivor) are proven by injection in this suite's history and
// the closure fuzzer.

/// Four-tier agreement WITHOUT a hardcoded expected: the interpreters establish the
/// ground truth and native must reproduce it. Used for loop-sum shapes whose exact
/// value is tedious to hand-compute but must be identical across tiers.
fn assert_native_matches_interp(tag: &str, source: &str) -> i64 {
    let dir = ScratchDir::new(tag);
    let interp = interp_value(&dir, tag, source);
    let native = native_exit(&dir, tag, source);
    assert_eq!(
        native as i64, interp,
        "native must agree with the interpreters ({interp}) for {tag}"
    );
    interp
}

/// **Bounded-heap proof that promotion + reclamation FIRES.** A hot loop calls a
/// factory 20 000 times; the factory allocates a ~130-byte scratch string per call and
/// returns a closure capturing an `i64` derived from it. WITH the promoting reset the
/// factory reclaims that scratch each call (only the 16-byte survivor accumulates:
/// 20 000 × 16 B ≈ 320 KB, under the 1 MB native heap), so native runs to completion
/// and agrees with the interpreters. WITHOUT reclamation the scratch would leak
/// (20 000 × ~150 B ≈ 3 MB) and the native allocator's exhaustion guard would `ud2`-trap
/// — a divergent (crashing) exit. Native matching the interpreters is therefore only
/// possible because the promoting reset reclaims the per-call scratch.
#[test]
fn native_promoting_factory_reclaims_scratch_bounded_heap() {
    let source = "\
fn make_val n i64 -> fn(i64) -> i64
    let pad string = \"PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING-PADDING\"
    let m i64 = n + len(pad)
    fn x i64 -> x + m
fn hot -> i64
    let acc i64 = 0
    let i i64 = 0
    while i < 20000
        let g fn(i64) -> i64 = make_val(i)
        acc = acc + g(0)
        i = i + 1
    acc
fn main -> i64
    hot()
";
    assert_native_matches_interp("promote_bounded", source);
}

/// The promoted survivor stays live in the caller's region until the caller's own
/// rewind — even after the caller does MORE heap allocation. The factory is now
/// arena-eligible (promoting); after obtaining the closure `main` runs a heap-churning
/// helper (`churn`, itself an arena function whose confined loop reclaims per
/// iteration) and only THEN invokes the closure. Reading back exactly `222` proves the
/// survivor was promoted into `main`'s region and never reclaimed by the factory's or
/// churn's rewinds.
#[test]
fn native_promoting_survivor_survives_more_allocation() {
    let source = "\
fn make_const n i64 -> fn(i64) -> i64
    let pad string = \"factory-scratch-reclaimed-by-the-promoting-reset\"
    let m i64 = n + len(pad) - len(pad)
    fn x i64 -> x + m
fn churn -> i64
    let total i64 = 0
    let i i64 = 0
    while i < 50
        let s string = \"reuse-the-heap-region-aggressively\"
        total = total + len(s)
        i = i + 1
    total
fn main -> i64
    let g fn(i64) -> i64 = make_const(222)
    let junk i64 = churn()
    g(junk - junk)
";
    assert_four_tiers("promote_survive_alloc", source, 222);
}

/// MULTIPLE return edges returning DIFFERENT-arity closures, so the promoting reset
/// sizes the survivor PER RETURN SITE: the `return` edge yields a 1-capture (2-word)
/// closure and the tail edge a 2-capture (3-word) closure. A single function-wide size
/// would mis-relocate one of them.
#[test]
fn native_promoting_multiple_arity_edges() {
    let source = "\
fn pick c bool a i64 b i64 -> fn(i64) -> i64
    if c
        return fn x i64 -> x + a
    fn x i64 -> x + a + b
fn main -> i64
    let g fn(i64) -> i64 = pick(false, 10, 20)
    let h fn(i64) -> i64 = pick(true, 10, 20)
    g(1) + h(1)
";
    // g = x+10+20 → g(1)=31; h = x+10 → h(1)=11; 31 + 11 = 42.
    assert_four_tiers("promote_multi_arity", source, 42);
}

/// FLOAT captures survive the relocation (the promoting reset copies raw words, so an
/// `f64` capture's bits are preserved), while the factory's per-call scratch is
/// reclaimed. A hot loop of 8 000 iterations over a float factory with scratch stays
/// heap-bounded (proving reclamation) AND computes the same float-threshold count on
/// every tier (proving the relocated float captures are read from the right words).
#[test]
fn native_promoting_float_captures_relocated() {
    let source = "\
fn make_lin a f64 b f64 -> fn(f64, f64) -> f64
    let pad string = \"float-factory-scratch-string-reclaimed-by-the-promoting-reset-each-call\"
    let n i64 = len(pad)
    fn x f64 y f64 -> a * x + b * y
fn run -> i64
    let total i64 = 0
    let i i64 = 0
    while i < 8000
        let g fn(f64, f64) -> f64 = make_lin(2.0, 3.0)
        if g(10.0, 5.0) > 30.0
            total = total + 1
        i = i + 1
    total
fn main -> i64
    run()
";
    // g(10,5) = 2*10 + 3*5 = 35 > 30 every iteration → total = 8000.
    assert_four_tiers("promote_float", source, 8000);
}
