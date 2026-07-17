//! Differential fuzzing for the **`alloc` heap box** (`native_object_heapbox.rs`).
//! A submodule of `fuzz.rs`, which it reuses via `use super::*` for the shared
//! `Rng`, `Outcome`, `run_interpreters`, and `fuzz_native_exit` harness.
//!
//! Three generators, each aimed at a distinct hazard of the heap-box surface:
//!
//! * [`gen_alloc_arena_program`] — the box against the **arena escape analysis**. An
//!   `alloc`'d cell is invisible to that analysis, so a per-iteration sub-region
//!   rewind can reclaim a cell a live pointer still names. Value oracle.
//! * [`gen_alloc_cross_frame_program`] — boxes crossing **frame boundaries**, the
//!   shape that could not be checked differentially at all before `alloc` had native
//!   codegen (an `addr_of` pointer is refused cross-frame by the interpreters with
//!   `L0459`). Value oracle.
//! * [`gen_alloc_cast_launder_program`] — the **`ptr_cast` laundering route**, which
//!   rewrites the `ptr_T` spelling the native gate keys on. **Skip oracle**: these
//!   programs have no value to agree on (the interpreters refuse them or define them
//!   differently), so the only correct native behaviour is a clean `L0339` skip.
//!
//! All three have verified teeth — see each test for the measured miscompile it
//! catches when its gate is removed.

use super::*;

/// Generates one program that tries to **launder an `alloc` heap box into a typed
/// `ptr<T>` via `ptr_cast`**, then do something with it that is only valid for a real
/// typed pointer.
///
/// This is the adversarial generator for the box/pointer divide. It now covers TWO
/// layers, because the defence moved:
///
/// * **Shapes (1)–(4): the historical laundering route, now closed at the FRONTEND.**
///   `refuse_legacy_box_pointer` keys on the *spelling* `ptr_i64`, and `ptr_cast`
///   used to be free to CHANGE the spelling: `check_ptr_cast` derived its result type
///   from the caller's expected ANNOTATION, defaulting to `ptr<i64>`, never from the
///   operand. So `let q ptr<i64> = ptr_cast(p)` rewrote a box into exactly the
///   spelling the gate looks for, and `ptr_offset(q, 1)` then strode 8 bytes past the
///   one-cell payload into the NEXT block's `[size]` header — the word
///   `__lullaby_alloc`'s free-list first-fit scan reads to decide reuse — so a write
///   through it corrupted allocator metadata governing later allocation sizes.
///   `check_ptr_cast` now takes the result's MODEL from the operand, so these are
///   rejected with `L0303` (or `L0301` at the laundering helper's signature) and never
///   reach the backend at all.
/// * **Shapes (5)–(6): the model-preserving identity cast, which still reaches the
///   backend.** `let q = ptr_cast(p)` keeps the `ptr_i64` model, so the operand
///   arriving at the native gate is genuinely still a box. These are what keep this
///   fuzzer honest about `refuse_legacy_box_pointer`: without them it would silently
///   have degraded into a frontend-only test, still green, while the native gate went
///   unexercised.
///
/// **The oracle is "native must SKIP", not "native must match".** Every shape here is
/// one the interpreters either refuse outright (`L0406` for `ptr_offset` over a box)
/// or define differently (`ptr_to_int` of a box is a heap-SLOT INDEX, not an
/// address), so there is no value to agree on — the only correct behaviour is no
/// runnable image, whether refused by the frontend or skipped with `L0339`. See
/// [`fuzz_alloc_cast_launder_native_always_skips`].
fn gen_alloc_cast_launder_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x1A5D_3C90_7E42_B6F8u64);
    let v = rng.range(1, 60);
    let n = rng.range(1, 4);

    // The pointee the box is laundered INTO. `ptr_cast`'s target comes from the
    // annotation, so each of these is a distinct laundering spelling.
    let (target, elem) = match rng.below(3) {
        0 => ("ptr<i64>", "i64"),
        1 => ("ptr<u8>", "u8"),
        _ => ("ptr<usize>", "usize"),
    };

    match rng.below(6) {
        // (1) Strided READ through the laundered pointer -> off the end of the cell.
        // FRONTEND-REJECTED since model preservation (`L0303`).
        0 => format!(
            "fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc({v})\n        \
             let q {target} = ptr_cast(p)\n        \
             let r {target} = ptr_offset(q, {n})\n        to_i64(ptr_read(r))\n"
        ),
        // (2) Pointer IDENTITY through the laundered pointer -> a real address where
        // the interpreters give a slot index. FRONTEND-REJECTED (`L0303`).
        1 => format!(
            "fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc({v})\n        \
             let q {target} = ptr_cast(p)\n        ptr_to_int(q)\n"
        ),
        // (3) Strided WRITE -> corrupts the next block's allocator `[size]` header.
        // FRONTEND-REJECTED (`L0303`).
        2 => format!(
            "fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc({v})\n        \
             let q {target} = ptr_cast(p)\n        \
             ptr_write(ptr_offset(q, {n}), to_{elem}(99))\n        ptr_read(p)\n"
        ),
        // (4) CROSS-FUNCTION laundering: the helper hides the cast, so `main` never
        // mentions `ptr_i64`. Now FRONTEND-REJECTED at the helper's own signature
        // (`L0301`: its `ptr_cast(p)` body yields `ptr_i64`, not the declared target).
        3 => format!(
            "fn launder p ptr_i64 -> {target}\n    unsafe\n        ptr_cast(p)\n\
             \n\
             fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc({v})\n        \
             let q {target} = launder(p)\n        \
             to_i64(ptr_read(ptr_offset(q, {n})))\n"
        ),
        // (5) MODEL-PRESERVING identity cast. The frontend ALLOWS this (the box casts
        // to itself), so it is the shape that still reaches — and must be refused by —
        // the native `refuse_legacy_box_pointer` gate. Without shapes (5)/(6) this
        // fuzzer would have quietly become a frontend test only, since (1)-(4) never
        // get near the backend any more.
        4 => format!(
            "fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc({v})\n        \
             let q = ptr_cast(p)\n        ptr_read(q)\n"
        ),
        // (6) MODEL-PRESERVING identity cast across a function: `rebox` keeps the
        // `ptr_i64` model, so its own `ptr_cast` site carries the `ptr_T` operand, the
        // helper skips, and the demotion fixpoint must skip `main` too.
        _ => format!(
            "fn rebox p ptr_i64 -> ptr_i64\n    unsafe\n        ptr_cast(p)\n\
             \n\
             fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc({v})\n        \
             let q = rebox(p)\n        ptr_read(q)\n"
        ),
    }
}

#[test]
fn fuzz_alloc_cast_launder_native_always_skips() {
    // The adversarial oracle for the `ptr_cast` laundering route. Unlike the other
    // fuzzers this does NOT compare values: every generated program is one the
    // interpreters refuse (`L0406`) or define differently (a slot index, not an
    // address), so the only correct native behaviour is a clean `L0339` skip with no
    // exe. Producing an exe at all is the failure.
    //
    // Teeth, per layer — both still real:
    //   * Remove the `ptr_cast` arm of `refuse_legacy_box_pointer` and the
    //     MODEL-PRESERVING shapes (5)/(6) compile, producing an exe for a box the
    //     interpreters define as a slot index.
    //   * Restore the old annotation-derived `check_ptr_cast` and the LAUNDERING
    //     shapes (1)-(4) compile — the strided read exits 0 where the interpreters
    //     raise `L0406`, `ptr_to_int` returns a real address where the interpreters
    //     give 0, and the strided write lands on the next block's `[size]` header.
    // Shapes (1)-(4) alone can no longer prove the native gate: the frontend now
    // refuses them first, which is why (5)/(6) exist.
    //
    // Gated on the link toolchain: on a host that cannot produce an exe anyway the
    // assertion would be vacuous.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping alloc ptr_cast laundering fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 200;
    let base_seed = 0x2E84_71BC_05A3_F9D6u64;
    let dir = ScratchDir::new("alloc_launder");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_alloc_cast_launder_program(seed);

        let src = dir.join(format!("launder_{i}.lby"));
        let exe = dir.join(format!("launder_{i}.exe"));
        std::fs::write(&src, &source).expect("write fuzz source");
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
        // The emit COMMAND may fail (L0339 when nothing is eligible) or succeed with
        // the function recorded as skipped; either is fine. What must never happen is
        // a runnable image for a laundered box.
        assert!(
            !exe.is_file(),
            "LAUNDERED BOX COMPILED on #{i} (seed {seed:#x}) — `ptr_cast` rewrote the \
             `ptr_i64` spelling the gate keys on, so a one-cell box reached typed-pointer \
             codegen:\n{source}\n{}",
            stdout(&emit)
        );
    }
}

/// Generates one program that exercises an **`alloc` heap box ACROSS FRAME
/// BOUNDARIES** — the shape that was impossible to verify differentially before
/// `alloc` had native codegen.
///
/// An `alloc` box is the ONE pointer form that crosses a frame boundary on every
/// tier: an `addr_of` pointer is refused cross-frame by the interpreters (`L0459`,
/// a callee cannot reach its caller's `Env`), and an `addr_of` of an array element
/// is native-only. So before this, cross-frame pointer programs were a
/// native-only/interpreter-only split that forced verification onto exit codes of
/// one tier alone. Now the same program runs on all four, and this generator sweeps
/// the class: boxes flowing OUT of a callee (a returned allocation), INTO a callee
/// (the out-parameter idiom, where the callee writes through the caller's box), and
/// THROUGH a chain of frames.
///
/// Note the parameter grammar: differently-typed parameters are SPACE-separated; a
/// comma groups same-type parameters (`fn f x, y i64`).
///
/// Every operation is native-subset and all-backend-agreeing (`alloc` boxes an
/// `i64`; `ptr_read`/`ptr_write` go through the box; no `ptr_offset`/`ptr_to_int`/
/// `dealloc`, each of which refuses an `alloc` box natively by design), so the
/// programs are divergence-free.
fn gen_alloc_cross_frame_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x0C7B_F2AE_9315_60D4u64);
    let hi = rng.range(3, 15);
    let bias = rng.range(-50, 50);
    let a0 = rng.range(1, 30);
    let b0 = rng.range(1, 30);

    match rng.below(4) {
        // (1) OUT-PARAMETER: the callee writes through the caller's box. This is the
        // idiom `addr_of` cannot express on the interpreters.
        0 => format!(
            "fn poke p ptr_i64 v i64\n    unsafe\n        ptr_write(p, v)\n\
             \n\
             fn h a i64 -> i64\n    unsafe\n        let p ptr_i64 = alloc({a0})\n        \
             poke(p, a * 2 + {b0})\n        ptr_read(p)\n\
             \n\
             fn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
             total = total + h(i)\n    total + {bias}\n"
        ),
        // (2) RETURNED ALLOCATION: a box flows out of the callee through the ordinary
        // scalar return convention (`ptr_i64` lowers as `NativeType::I64`).
        1 => format!(
            "fn make v i64 -> ptr_i64\n    unsafe\n        alloc(v * 3 + {a0})\n\
             \n\
             fn h a i64 -> i64\n    unsafe\n        let p ptr_i64 = make(a)\n        \
             let q ptr_i64 = make(a + {b0})\n        ptr_read(p) + ptr_read(q)\n\
             \n\
             fn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
             total = total + h(i)\n    total + {bias}\n"
        ),
        // (3) BOTH: a returned box is then mutated through an out-parameter callee,
        // and read back two frames up.
        2 => format!(
            "fn make v i64 -> ptr_i64\n    unsafe\n        alloc(v + {a0})\n\
             \n\
             fn bump p ptr_i64 d i64\n    unsafe\n        ptr_write(p, ptr_read(p) + d)\n\
             \n\
             fn h a i64 -> i64\n    unsafe\n        let p ptr_i64 = make(a)\n        \
             bump(p, {b0})\n        bump(p, a)\n        ptr_read(p)\n\
             \n\
             fn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
             total = total + h(i)\n    total + {bias}\n"
        ),
        // (4) THROUGH A CHAIN: the box is passed down two frames before being written,
        // so the address must survive several boundary crossings intact.
        _ => format!(
            "fn inner p ptr_i64 v i64\n    unsafe\n        ptr_write(p, v)\n\
             \n\
             fn outer p ptr_i64 v i64\n    unsafe\n        inner(p, v + {b0})\n\
             \n\
             fn h a i64 -> i64\n    unsafe\n        let p ptr_i64 = alloc({a0})\n        \
             outer(p, a * 2)\n        ptr_read(p) + {a0}\n\
             \n\
             fn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
             total = total + h(i)\n    total + {bias}\n"
        ),
    }
}

#[test]
fn fuzz_alloc_cross_frame_interpreters_agree() {
    // Cross-check the three engines on cross-frame `alloc`-box programs. Always runs
    // (no toolchain needed); a divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x3F51_A0C4_88E2_71D9u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_alloc_cross_frame_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "cross-frame alloc divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "cross-frame generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_alloc_cross_frame_native_matches_interpreter_when_linkable() {
    // The cross-frame oracle. Before `alloc` had native codegen these programs could
    // not be checked differentially at all: an `alloc` program did not compile
    // natively, and the `addr_of` alternative is refused cross-frame by the
    // interpreters (`L0459`) — so cross-frame pointer programs were a native-only /
    // interpreter-only split. Now the same program runs on all four tiers and the
    // exit codes must agree exactly.
    //
    // Gated on the link toolchain; skips cleanly when absent.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native cross-frame alloc fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0xB6D3_2E77_10C9_4A85u64;
    let dir = ScratchDir::new("alloc_cross");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_alloc_cross_frame_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-cross-frame-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => {
                panic!("cross-frame generator produced {other:?} on seed {seed:#x}:\n{source}")
            }
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_alloc_cross_{i}")) else {
            return;
        };
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (cross-frame alloc box) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}

/// Generates one program that exercises the **`alloc` heap box against the arena
/// escape analysis** — the highest-risk interaction in the heap-box surface.
///
/// An `alloc`'d cell is manually managed and INVISIBLE to that analysis:
/// `type_is_directly_heap` does not include `ptr_*`, and `expr_touches_heap` on an
/// `alloc` call only inspects its arguments. So a leaf helper that mixes `alloc`
/// with a `string` looks arena-eligible, and a loop that stores a box into an
/// iteration-outliving local looks heap-touching (the string) AND confined (the
/// only store is a `ptr_i64`) — earning a per-iteration sub-region whose bump
/// rewind reclaims a cell a live pointer still names. A later allocation then
/// reuses those bytes and the read returns garbage.
///
/// `alloc_defeats_arena` (in `native_object_heapbox.rs`) is what prevents that, and
/// this generator is its oracle: every shape below deliberately mixes an `alloc`
/// with heap-touching `string` work, in and around loops, with an allocation AFTER
/// the loop that would reuse any wrongly-rewound bytes.
///
/// **This generator has teeth** — with the gate disabled this class miscompiles for
/// real (the pinned `suite17` shape answers native `92` vs the interpreters' `2116`).
///
/// Every operation is native-subset and all-backend-agreeing: `alloc` boxes an
/// `i64`, `ptr_read`/`ptr_write` go through the box, the strings are ASCII so `len`
/// agrees, and no `ptr_offset`/`ptr_to_int`/`dealloc` appears (each refuses an
/// `alloc` box natively by design), so the programs are divergence-free.
fn gen_alloc_arena_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x0A11_0C7B_0C57_3E11u64);
    let hi = rng.range(3, 12);
    let bias = rng.range(-50, 50);
    let main = format!(
        "\n\nfn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
         total = total + h(i)\n    total + {bias}\n"
    );

    // The per-iteration string scratch: heap work that makes the loop look
    // "heap-touching" to the escape analysis, so the sub-region logic engages.
    let scratch = match rng.below(4) {
        0 => "to_string(a + j)".to_string(),
        1 => "trim(to_string(a * 2 + j))".to_string(),
        2 => "upper(to_string(a + j * 3))".to_string(),
        _ => "repeat(\"ab\", 2) + to_string(j)".to_string(),
    };
    // A post-loop allocation that REUSES any wrongly-rewound bytes, so a bad rewind
    // becomes an observable wrong answer rather than a latent stale-but-correct read.
    let clobber = match rng.below(3) {
        0 => "to_string(a) + \"clobberclobberclobber\"".to_string(),
        1 => "repeat(\"zy\", 8) + to_string(a)".to_string(),
        _ => "upper(to_string(a * 3)) + \"padpadpadpad\"".to_string(),
    };
    let k = rng.range(2, 8);
    let seed_v = rng.range(1, 40);

    let h = match rng.below(3) {
        // (1) The box ESCAPES the iteration: `q` is rebound inside the loop, so its
        // cell is allocated within any sub-region and must survive the rewind.
        0 => format!(
            "fn h a i64 -> i64\n    unsafe\n        let q = alloc({seed_v})\n        \
             for j from 0 to {k}\n            q = alloc(j * 10 + a)\n            \
             let s string = {scratch}\n            ptr_write(q, ptr_read(q) + len(s))\n        \
             let z string = {clobber}\n        ptr_read(q) + len(z)\n"
        ),
        // (2) The box is loop-INVARIANT (allocated before the loop) and accumulated
        // through across iterations, with string scratch inside: the function-scoped
        // arena must not rewind it either.
        1 => format!(
            "fn h a i64 -> i64\n    unsafe\n        let q = alloc({seed_v})\n        \
             for j from 0 to {k}\n            let s string = {scratch}\n            \
             ptr_write(q, ptr_read(q) + len(s) + j)\n        \
             let z string = {clobber}\n        ptr_read(q) + len(z)\n"
        ),
        // (3) TWO boxes, one rebound inside the loop and one outside, so a rewind
        // that reclaims either is observable, plus the post-loop clobber.
        _ => format!(
            "fn h a i64 -> i64\n    unsafe\n        let p = alloc({seed_v})\n        \
             let q = alloc(0)\n        for j from 0 to {k}\n            \
             q = alloc(j + a)\n            let s string = {scratch}\n            \
             ptr_write(p, ptr_read(p) + len(s))\n        \
             let z string = {clobber}\n        ptr_read(p) + ptr_read(q) + len(z)\n"
        ),
    };
    format!("{h}{main}")
}

#[test]
fn fuzz_alloc_arena_interpreters_agree() {
    // Cross-check the three engines on `alloc`-box + arena programs. Always runs (no
    // toolchain needed); a divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x0A11_0C21_5EED_9E37u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_alloc_arena_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "alloc-box backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "alloc generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_alloc_arena_native_matches_interpreter_when_linkable() {
    // THE oracle for the `alloc` heap box against the arena escape analysis. An
    // `alloc`'d cell is invisible to that analysis, so without `alloc_defeats_arena`
    // a per-iteration sub-region rewind reclaims a cell a live pointer still names
    // and a later allocation overwrites it. The generated programs deliberately mix
    // `alloc` with heap-touching `string` work in and around loops, and clobber after
    // the loop, so any wrongly-reclaimed box becomes a WRONG EXIT CODE rather than a
    // latent stale-but-correct read.
    //
    // Teeth verified: with the gate disabled this class miscompiles for real (the
    // pinned suite17 shape gives native 92 vs the interpreters' 2116).
    //
    // Gated on the link toolchain; skips cleanly when absent.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native alloc-box fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x7C0F_1A93_44D2_6B8Eu64;
    let dir = ScratchDir::new("alloc_arena");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_alloc_arena_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-alloc-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("alloc generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_alloc_arena_{i}")) else {
            return;
        };
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (alloc box / arena) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}
