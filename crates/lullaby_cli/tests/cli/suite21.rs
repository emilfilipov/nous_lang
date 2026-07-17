//! CLI integration tests, part 21 — semantics fixes that close frontend holes:
//! model-preserving `ptr_cast`, model-honest `int_to_ptr`/`arena_alloc`, the
//! `L0350` simple-alias use-after-free case, and void `export fn`.
//!
//! # The two pointer models, and the three doors that crossed them
//!
//! Lullaby has two non-convertible pointer models — the legacy `ptr_T` heap box
//! that only `alloc` produces (a heap-SLOT INDEX over a one-cell store on the
//! interpreters) and the modern `ptr<T>` address from `addr_of`/`int_to_ptr`.
//! `let`/parameter binding enforced that (`L0303`/`L0313`); three *builtins* did
//! not, because each derived its result type from the caller's annotation via
//! `expected.filter(|ty| ty.is_raw_pointer())` — a predicate that admits BOTH
//! spellings, so the annotation got to pick the model:
//!
//! * `ptr_cast` — both directions, closed first (its operand is a pointer, so it has
//!   a model to preserve).
//! * `arena_alloc` — `let fake ptr_i64 = arena_alloc(pool, 1)`. Closed here: an arena
//!   cell is unambiguously an address, so the annotation supplies the **pointee only**
//!   and a legacy annotation no longer captures it (the `let` collides at `L0303`).
//! * `int_to_ptr` — `let fake ptr_i64 = int_to_ptr(ptr_to_int(addr_of(buf[0])))`.
//!   **Deliberately still open, and irreducibly so.** Its operand is an `i64`, which
//!   carries **no provenance**, so neither model is derivable from it; on the
//!   interpreters an integer may genuinely be either. Both round trips are delivered
//!   and fixture-pinned — `run_ptr_cast.lby` rebuilds a *real box* from
//!   `ptr_to_int(box)` as `ptr_i64`, `freestanding_mmio_vga.lby` names `0xB8000` as
//!   `ptr<i64>` — so the annotation is an `unsafe` **assertion**, not an inference,
//!   and restricting it breaks the first fixture. Provenance tracking, splitting the
//!   builtin, and refusing the `addr_of`-derived shape were each designed and each
//!   failed; only removing `ptr_to_int(box)` from the language closes it.
//!
//! # The fourth door: NESTING, where no gate is handed a `ptr_T` at all
//!
//! Each fix above made a gate correct about the model it *names*. None of them looked
//! at the model nested *inside* what it names — and `addr_of` over a `ptr_i64` place
//! yields `ptr<ptr_i64>`, whose OUTER spelling reads modern. So `ptr_cast` retargeted
//! that pointee to `i64` and erased the box model with no `ptr_T` ever appearing as an
//! operand: **every gate was bypassed rather than defeated**, which is exactly why
//! three consecutive laundering fixes missed it. Measured live on `main` (65f76ea):
//! `check` clean, native exiting on a real heap address where the interpreters printed
//! `ptr(0)`, and a `ptr_write` escalation SEGFAULTING natively (`0xC0000005`) against
//! `L0409` on all three interpreters.
//!
//! The rule is therefore depth-insensitive (`mentions_box_model`): a box model is
//! opaque at ANY nesting depth and storage may be reinterpreted neither FROM nor INTO
//! one. A fourth point-fix on the shape would have lost the same way.
//!
//! **This closes reinterpretation, NOT the types.** `ptr<ptr_i64>` remains legal and
//! coherent — 7 on all four tiers, pinned by `run_addr_of_box.lby` — because
//! `ptr_read` of a box-typed cell reproduces each tier's own faithful box rather than
//! reinterpreting one. That fixture is what ruled out the alternative fix of refusing
//! `addr_of` over a box place: it would have broken a measurably correct program.
//!
//! Each closure is pinned below with a negative control proving it did not over-reach
//! (the freestanding MMIO idiom must still COMPILE), and `int_to_ptr`'s open route is
//! pinned as an honesty test. **Nothing here should be read as claiming the native
//! gate contains the model mismatch in general — it does not** (it is a prefix test on
//! the outer type name, and is deliberately NOT widened: the nesting question is
//! answered at the frontend, because natively a nested box is often perfectly
//! coherent). The gate test below pins the gate's own behaviour only.

use crate::*;

/// Run `source` and return `(exit code, stdout+stderr)`.
fn run_backend(source: &str, backend: &str, tag: &str) -> (i32, String) {
    let dir = ScratchDir::new("run_backend");
    let src = dir.join(format!("{tag}_{backend}.lby"));
    std::fs::write(&src, source).expect("write source");
    let out = lullaby()
        .args(["run", "--backend", backend, src.to_str().expect("src path")])
        .output()
        .expect("run backend");
    (
        out.status.code().expect("exit code"),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

/// Assert `source` is REJECTED by every interpreter frontend with `code`. A
/// frontend diagnostic is tier-independent, so all three must agree exactly.
fn assert_all_interpreters_reject(source: &str, tag: &str, code: &str) {
    for backend in ["ast", "ir", "bytecode"] {
        let (exit, output) = run_backend(source, backend, tag);
        assert_ne!(
            exit, 0,
            "{backend} must REJECT this program for {tag}:\n{source}\n{output}"
        );
        assert!(
            output.contains(code),
            "{backend} must reject {tag} with {code}:\n{source}\n{output}"
        );
    }
}

/// Assert every interpreter accepts `source` and prints `expected`.
fn assert_all_interpreters_yield(source: &str, tag: &str, expected: i64) {
    for backend in ["ast", "ir", "bytecode"] {
        let (exit, output) = run_backend(source, backend, tag);
        assert_eq!(
            exit, 0,
            "{backend} must ACCEPT this program for {tag}:\n{source}\n{output}"
        );
        assert_eq!(
            output.trim(),
            expected.to_string(),
            "{backend} must print {expected} for {tag}:\n{source}"
        );
    }
}

/// THE laundering repro: `ptr_cast` used to rewrite an `alloc` box (`ptr_i64`) into
/// a raw address (`ptr<i64>`) purely because the annotation said so. That defeated
/// the `L0303`/`L0313` walls and let `ptr_offset` (below) type-check over a
/// one-cell box. The operand's model must win, so this is now `L0303`.
#[test]
fn ptr_cast_cannot_launder_an_alloc_box_into_a_raw_pointer() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q ptr<i64> = ptr_cast(p)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_ptr_cast_launder_box",
        "L0303",
    );
}

/// The memory-corruption shape the laundering enabled: once the box is spelled
/// `ptr<i64>`, `ptr_offset(q, 1)` type-checks. The interpreters refuse it at RUN
/// time (`L0406`) and the native gate refuses it, but the frontend accepted the
/// program — natively this strides 8 bytes off a one-cell payload into the next
/// heap block's `[size]` header, the word the allocator's free-list scan reads.
/// Now it never gets past the checker.
#[test]
fn laundered_box_pointer_arithmetic_is_rejected_at_the_frontend() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q ptr<i64> = ptr_cast(p)\n",
            "        let r = ptr_offset(q, 1)\n",
            "        ptr_to_int(r)\n",
        ),
        "lullaby_ptr_cast_launder_offset",
        "L0303",
    );
}

/// The REVERSE direction, which the original report did not cover: a legacy `ptr_U`
/// annotation used to capture an `addr_of` address, relabelling a real machine
/// address as an `alloc` box. That falsifies the invariant the native backend's
/// `is_legacy_box_pointer` spelling test rests on — that a `ptr_T`-typed expression
/// is always `alloc`-derived. The model is taken from the operand, so this is
/// `L0303` too.
#[test]
fn ptr_cast_cannot_relabel_a_raw_pointer_as_an_alloc_box() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let buf array<i64> = [10, 20, 30]\n",
            "        let base = addr_of(buf[0])\n",
            "        let fake ptr_i64 = ptr_cast(base)\n",
            "        ptr_read(fake)\n",
        ),
        "lullaby_ptr_cast_relabel_address",
        "L0303",
    );
}

/// NEGATIVE CONTROL: model-preservation must not break `ptr_cast` on legitimate
/// `ptr<T>` operands. Retargeting the pointee within the modern model — the
/// `addr_of` -> `ptr_cast<u8>` -> back -> read idiom — still works on every tier.
/// If this fails, the fix over-reached.
#[test]
fn ptr_cast_still_retargets_a_genuine_raw_pointer_pointee() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let buf array<i64> = [10, 20, 30]\n",
            "        let base = addr_of(buf[0])\n",
            "        let bp ptr<u8> = ptr_cast(base)\n",
            "        let back ptr<i64> = ptr_cast(bp)\n",
            "        ptr_read(back)\n",
        ),
        "lullaby_ptr_cast_roundtrip",
        10,
    );
}

/// An identity cast of a box stays legal and stays a box: `ptr_cast` of a `ptr_T`
/// yields exactly `ptr_T`, so inference binds it and the box still reads back. This
/// pins that the fix preserves rather than rejects — existing box-cast source that
/// did not launder keeps compiling.
#[test]
fn ptr_cast_of_an_alloc_box_is_an_identity_that_stays_a_box() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(42)\n",
            "        let q = ptr_cast(p)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_ptr_cast_box_identity",
        42,
    );
}

/// THE NESTED ROUTE — `ptr_cast(addr_of(box))`, a live arbitrary read/write that
/// passed `check` on every tier and that the three prior laundering fixes all missed.
///
/// `addr_of(a)` over a `ptr_i64` place yields `ptr<ptr_i64>`: the OUTER spelling reads
/// *modern*, so `is_legacy_box_spelling` — a prefix test on the outer name — never
/// fires, and `ptr_cast` retargeted the pointee to `i64`, erasing the box model. **No
/// gate was ever handed a `ptr_T` to be correct about**, so every gate was BYPASSED
/// rather than defeated. That is why a fourth point-fix would have lost the same way,
/// and why the rule is now depth-insensitive (`mentions_box_model` in
/// `semantics_raw_ptr.rs`).
///
/// Measured on `main` (65f76ea) before the fix, verbatim:
///
/// * `check` clean on every tier; native compiled and exited **1073758240** — a real
///   heap address — while ast/ir/bytecode all printed `ptr(0)`, the slot index.
/// * Escalated with `ptr_write(pa, 999999)`, native compiled and **SEGFAULTED**
///   (`0xC0000005`) where all three interpreters raised `L0409`.
///
/// The refusal lands at the existing `L0303` wall: the cast is now an identity on
/// anything mentioning a box, so `ptr<ptr_i64>` collides with the `ptr<i64>`
/// annotation.
#[test]
fn ptr_cast_cannot_launder_a_box_through_its_own_address() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    let a ptr_i64 = alloc(7)\n",
            "    let n i64 = 0\n",
            "    unsafe\n",
            "        let pa ptr<i64> = ptr_cast(addr_of(a))\n",
            "        n = ptr_read(pa)\n",
            "    n\n",
        ),
        "lullaby_ptr_cast_addr_of_box",
        "L0303",
    );
}

/// The same route two levels down (`ptr<ptr<ptr_i64>>`), pinning that the rule is
/// **depth-insensitive**. A depth-1 fix would close the shape above and leave this
/// one open — which is the whole failure mode this class-closure exists to avoid.
#[test]
fn ptr_cast_cannot_launder_a_box_buried_two_levels_deep() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    let a ptr_i64 = alloc(7)\n",
            "    let n i64 = 0\n",
            "    unsafe\n",
            "        let p1 ptr<ptr_i64> = addr_of(a)\n",
            "        let p2 ptr<ptr<ptr_i64>> = addr_of(p1)\n",
            "        let p3 ptr<i64> = ptr_cast(p2)\n",
            "        n = ptr_read(p3)\n",
            "    n\n",
        ),
        "lullaby_ptr_cast_box_two_levels",
        "L0303",
    );
}

/// The CONVERSE direction, which needs no `alloc` anywhere in the program: a nested
/// annotation FABRICATES a box out of ordinary array storage, which native then
/// dereferences. The operand is a clean `ptr<i64>` — the lie lives entirely in the
/// target type, one level down — so an outer-name test cannot see it either.
///
/// Measured ACCEPTED on `main` before the fix.
#[test]
fn ptr_cast_cannot_fabricate_a_box_from_ordinary_storage() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    let buf array<i64> = [7, 0]\n",
            "    let n i64 = 0\n",
            "    unsafe\n",
            "        let pb ptr<ptr_i64> = ptr_cast(addr_of(buf[0]))\n",
            "        let fake ptr_i64 = ptr_read(pb)\n",
            "        n = ptr_read(fake)\n",
            "    n\n",
        ),
        "lullaby_ptr_cast_fabricate_box",
        "L0303",
    );
}

/// `arena_alloc`'s nested door. The outer-name filter added when `arena_alloc` was
/// closed stopped `let fake ptr_i64 = arena_alloc(pool, 1)` but not the same lie one
/// level down — forging a box out of an arena cell with no `alloc` in the program.
/// Pins that `is_annotatable_address_type` rejects a box model at any depth.
#[test]
fn arena_alloc_cannot_bury_a_box_in_its_pointee() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    let buf array<i64> = [7, 0, 0, 0]\n",
            "    region pool in buf\n",
            "    let n i64 = 0\n",
            "    unsafe\n",
            "        let pb ptr<ptr_i64> = arena_alloc(pool, 1)\n",
            "        let fake ptr_i64 = ptr_read(pb)\n",
            "        n = ptr_read(fake)\n",
            "    n\n",
        ),
        "lullaby_arena_alloc_nested_box",
        "L0303",
    );
}

/// OVER-REFUSAL CONTROL — the counterweight to every test above, and the program that
/// decided how this hole had to be fixed.
///
/// Two candidate fixes were on the table: refuse `addr_of` over a `ptr_T` place
/// outright, or refuse only the model-crossing REINTERPRETATION. This program
/// distinguishes them — it is measurably correct (**7 on all four tiers**, native
/// included), so the first candidate would have broken a coherent program to close an
/// incoherent one. It was rejected on that evidence, not on taste.
///
/// `ptr<ptr_i64>` is coherent because `ptr_read` of a box-typed cell REPRODUCES each
/// tier's own faithful box rather than REINTERPRETING one. So the rule refuses
/// reinterpretation across the model boundary while leaving the types usable.
///
/// Also pinned as the four-tier fixture `tests/fixtures/valid/run_addr_of_box.lby`
/// (see `runs_addr_of_box_fixture_on_all_backends`). If a later tightening makes
/// this fail, that change over-refused.
#[test]
fn a_boxs_address_stays_readable_on_every_tier() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    let a ptr_i64 = alloc(7)\n",
            "    let n i64 = 0\n",
            "    unsafe\n",
            "        let pa ptr<ptr_i64> = addr_of(a)\n",
            "        let back ptr_i64 = ptr_read(pa)\n",
            "        n = ptr_read(back)\n",
            "    n\n",
        ),
        "lullaby_addr_of_box_coherent",
        7,
    );
}

/// OVER-REFUSAL CONTROL — nesting a *modern* pointer (`ptr<ptr<i64>>`) is ordinary
/// double indirection with no box anywhere, and must stay untouched. Pins that
/// `mentions_box_model` keys on the box MODEL and not on nesting itself.
#[test]
fn nested_modern_pointers_still_retarget_freely() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    let buf array<i64> = [5, 6]\n",
            "    let n i64 = 0\n",
            "    unsafe\n",
            "        let p ptr<i64> = addr_of(buf[0])\n",
            "        let pp ptr<ptr<i64>> = addr_of(p)\n",
            "        let q ptr<i64> = ptr_read(pp)\n",
            "        n = ptr_read(q)\n",
            "    n\n",
        ),
        "lullaby_nested_modern_ptr",
        5,
    );
}

/// The over-refusal control as a committed FOUR-tier fixture. The interpreter half is
/// `a_boxs_address_stays_readable_on_every_tier`; the half that matters here is
/// **native**, because the hole was a native/interpreter DIVERGENCE and a control that
/// only ran the interpreters could not see native over-refusing or diverging.
///
/// `compiled main` is asserted, not merely a clean exit: an `L0339` skip would make
/// the run a silent re-test of the interpreters, and the fixture would go on claiming
/// four-tier agreement while proving three. The fixture carries no `dealloc` for that
/// exact reason — `dealloc` is not lowered natively, so it would force the skip.
#[test]
fn runs_addr_of_box_fixture_on_all_backends() {
    let fixture = workspace_root().join("tests/fixtures/valid/run_addr_of_box.lby");
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
        assert_eq!(stdout(&output).trim(), "7", "{backend} result");
    }

    let dir = ScratchDir::new("addr_of_box");
    let out = dir.join("run_addr_of_box.exe");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run native");
    let listing = format!("{}{}", stdout(&emit), stderr(&emit));
    assert!(
        emit.status.success(),
        "a box's address is coherent natively and must COMPILE; refusing it here would \
         be the over-refusal this fixture exists to catch:\n{listing}"
    );
    assert!(
        listing.contains("compiled main"),
        "`main` must be natively LOWERED, not `L0339`-skipped — a skip would silently \
         reduce this to a three-tier test:\n{listing}"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld/kernel32 unavailable; skipping run of run_addr_of_box.exe \
             (native compile + `compiled main` assertions DID run)"
        );
        return;
    }
    let exe = std::process::Command::new(&out).output().expect("run exe");
    assert_eq!(
        exe.status.code(),
        Some(7),
        "native must agree with all three interpreters on 7: {exe:?}"
    );
}

/// HONESTY PIN — this documents a route that is deliberately OPEN, not a fix.
///
/// `int_to_ptr` carries the same `expected.filter(|ty| ty.is_raw_pointer())` shape
/// `ptr_cast` was fixed for, so a `ptr_i64` annotation captures a pointer built from a
/// real machine address — asserting `alloc` provenance it does not have. **This still
/// compiles, on purpose.**
///
/// Restricting it to `ptr<T>` was tried and is wrong. `int_to_ptr`'s operand is an
/// `i64`, and **an integer carries no provenance** — so neither model is derivable
/// from it, by construction. On the interpreters an integer genuinely may be either,
/// and both round trips are delivered and fixture-pinned: `run_ptr_cast.lby`
/// reconstructs a *real box* from `ptr_to_int(box)` as `ptr_i64` (restricting the
/// builtin breaks that fixture), and `freestanding_mmio_vga.lby` names `0xB8000` as
/// `ptr<i64>`. Three closure designs were attacked and all failed — tracking
/// provenance into the `i64` (defeated by arithmetic, arrays, function boundaries),
/// splitting the builtin (`int_to_ptr(753664)`, a pure constant, already yields a
/// `ptr_i64`, so `int_to_box` would launder identically), and refusing the
/// `addr_of`-derived shape (`run_ptr_cast.lby` launders through a temp var,
/// indistinguishable). Only removing `ptr_to_int(box)` from the language closes it.
///
/// So the annotation is an `unsafe` **assertion** that may be false. This test pins
/// only that the false assertion compiles and that *this* program's value reads back
/// correctly — it asserts nothing about the mismatch being contained in general.
///
/// If a later change closes this, the test will start failing — that is the intent: it
/// must be rewritten, not deleted, so the frontier stays documented.
#[test]
fn int_to_ptr_may_still_assert_the_box_spelling_over_an_address() {
    let source = concat!(
        "fn main -> i64\n",
        "    unsafe\n",
        "        let buf array<i64> = [10, 20, 30]\n",
        "        let fake ptr_i64 = int_to_ptr(ptr_to_int(addr_of(buf[0])))\n",
        "        ptr_read(fake)\n",
    );
    let (exit, output) = check_source(source, "lullaby_int_to_ptr_box_assertion");
    assert_eq!(
        exit, 0,
        "`int_to_ptr`'s annotation is an unsafe ASSERTION over both models; if this now \
         fails, the builtin was restricted and both this pin and `run_ptr_cast.lby` \
         need revisiting:\n{output}"
    );
    // The value is a real address, so it reads back correctly on every tier: the
    // spelling is a lie, but nothing consumes the lie.
    assert_all_interpreters_yield(source, "lullaby_int_to_ptr_box_assertion_run", 10);
}

/// The native gate's own behaviour: a `ptr_T`-spelled operand is refused, so the
/// function skips to the interpreters rather than computing an answer natively.
/// Asserting the SKIP — not just the answer — is the point, since the skip is the
/// gate's entire observable contract.
///
/// **Scope, deliberately narrow:** this pins the gate on the shape it names. It is
/// *not* evidence that the gate contains the pointer-model mismatch in general, and
/// must not be cited as such — `is_legacy_box_pointer` is a prefix test on the OUTER
/// type name, so a box model nested in a pointee (`ptr<ptr_i64>`, which `addr_of` over
/// a box place yields) never reaches it.
#[test]
fn a_falsely_boxed_address_is_refused_by_the_native_gate() {
    let dir = ScratchDir::new("int_to_ptr_gate_skip");
    let src = dir.join("lullaby_int_to_ptr_gate_skip.lby");
    let obj = dir.join("lullaby_int_to_ptr_gate_skip.obj");
    std::fs::write(
        &src,
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let buf array<i64> = [10, 20, 30]\n",
            "        let fake ptr_i64 = int_to_ptr(ptr_to_int(addr_of(buf[0])))\n",
            "        ptr_to_int(fake)\n",
        ),
    )
    .expect("write source");
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            obj.to_str().expect("obj path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    let listing = format!("{}{}", stdout(&emit), stderr(&emit));
    assert!(
        listing.contains("skipped main"),
        "the native gate must REFUSE a `ptr_T`-spelled operand — that skip IS the \
         containment for `int_to_ptr`'s unsafe assertion:\n{listing}"
    );
    assert!(
        !listing.contains("compiled main"),
        "`main` must not compile natively:\n{listing}"
    );
}

/// THE third door, which neither the original `ptr_cast` report nor the `int_to_ptr`
/// follow-up named — and unlike `int_to_ptr`, this one closes cleanly. An arena cell is
/// a real address bumped out of a caller-owned `array<i64>`; the host allocator is
/// never involved and no integer is in play, so `arena_alloc`'s model is *known* and a
/// `ptr_i64` spelling over it is a plain falsehood with no legitimate reading.
#[test]
fn arena_alloc_cannot_relabel_an_arena_cell_as_an_alloc_box() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    let backing array<i64> = [0, 0, 0, 0]\n",
            "    region pool in backing\n",
            "    unsafe\n",
            "        let fake ptr_i64 = arena_alloc(pool, 1)\n",
            "        ptr_write(fake, 77)\n",
            "        ptr_read(fake)\n",
        ),
        "lullaby_arena_alloc_relabel_cell",
        "L0303",
    );
}

/// A falsely-`ptr_i64` address handed to `dealloc`, which exists only for real boxes.
/// It is **detected, not silent** — the interpreters raise `L0406` ("invalid pointer")
/// because the value is a byte address above `RAW_POINTER_BASE` rather than a heap-slot
/// handle, and native does not lower `dealloc` at all.
///
/// **Scope:** this pins the outcome of *this* shape only. It is not a general claim
/// that a false `int_to_ptr` assertion is always detected — do not generalize it into
/// one.
#[test]
fn deallocing_a_falsely_boxed_address_is_detected_at_runtime() {
    let source = concat!(
        "fn main -> i64\n",
        "    unsafe\n",
        "        let buf array<i64> = [10, 20, 30]\n",
        "        let fake ptr_i64 = int_to_ptr(ptr_to_int(addr_of(buf[0])))\n",
        "        dealloc(fake)\n",
        "        0\n",
    );
    for backend in ["ast", "ir", "bytecode"] {
        let (exit, output) = run_backend(source, backend, "lullaby_int_to_ptr_false_dealloc");
        assert_ne!(
            exit, 0,
            "{backend} must DETECT a `dealloc` of a non-box address:\n{output}"
        );
        assert!(
            output.contains("L0406"),
            "{backend} must report L0406 (invalid pointer), not free arbitrary \
             memory:\n{output}"
        );
    }
}

/// NEGATIVE CONTROL: a legitimate `int_to_ptr` -> `ptr<T>` -> `ptr_offset` walk must
/// still work on every tier. The round trip through an integer is the delivered
/// pointer-identity idiom; only the *legacy annotation* was ever wrong. If this
/// fails, the fix over-reached and broke `int_to_ptr` generally.
#[test]
fn int_to_ptr_still_round_trips_a_genuine_address_for_a_walk() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let buf array<i64> = [10, 20, 30]\n",
            "        let base = addr_of(buf[0])\n",
            "        let back ptr<i64> = int_to_ptr(ptr_to_int(base))\n",
            "        ptr_read(ptr_offset(back, 2))\n",
        ),
        "lullaby_int_to_ptr_roundtrip_walk",
        30,
    );
}

/// NEGATIVE CONTROL: `int_to_ptr` with NO annotation must still default to
/// `ptr<i64>` and stay walkable. The fix changed which annotations capture the
/// result; it must not have disturbed the default.
#[test]
fn int_to_ptr_without_an_annotation_still_defaults_to_a_walkable_pointer() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let buf array<i64> = [10, 20, 30]\n",
            "        let back = int_to_ptr(ptr_to_int(addr_of(buf[0])))\n",
            "        ptr_read(ptr_offset(back, 1))\n",
        ),
        "lullaby_int_to_ptr_default_pointee",
        20,
    );
}

/// NEGATIVE CONTROL: `arena_alloc` under a legitimate modern annotation still bumps,
/// still hands out a walkable pointer, and still aliases its backing buffer.
#[test]
fn arena_alloc_still_hands_out_a_walkable_arena_pointer() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    let backing array<i64> = [0, 0, 0, 0]\n",
            "    region pool in backing\n",
            "    unsafe\n",
            "        let cells ptr<i64> = arena_alloc(pool, 2)\n",
            "        ptr_write(cells, 5)\n",
            "        ptr_write(ptr_offset(cells, 1), 6)\n",
            "        ptr_read(cells) + ptr_read(ptr_offset(cells, 1))\n",
        ),
        "lullaby_arena_alloc_walkable",
        11,
    );
}

/// NEGATIVE CONTROL — THE POINT OF THE FREESTANDING TIER. The MMIO idiom
/// (`int_to_ptr(0xB8000)` + `ptr_offset` + `volatile_store`) in a `no-runtime`
/// module must keep compiling NATIVELY. This mirrors
/// `tests/fixtures/valid/no_runtime/freestanding_mmio_vga.lby`, which is the whole
/// reason `int_to_ptr` is annotation-governed in the first place: a driver names a
/// fixed physical address and says what lives there.
///
/// Asserting `compiled`, not merely `check`-clean: a skip would silently gut the
/// tier while staying green.
#[test]
fn the_freestanding_mmio_idiom_still_compiles_natively() {
    let dir = ScratchDir::new("mmio_after_int_to_ptr_fix");
    let src = dir.join("lullaby_mmio_after_int_to_ptr_fix.lby");
    let obj = dir.join("lullaby_mmio_after_int_to_ptr_fix.obj");
    let source = concat!(
        "no-runtime\n",
        "\n",
        "fn vga_put off i64 ch i64\n",
        "    unsafe\n",
        "        let base ptr<i64> = int_to_ptr(753664)\n",
        "        volatile_store(ptr_offset(base, off), ch)\n",
        "\n",
        "fn vga_get off i64 -> i64\n",
        "    unsafe\n",
        "        let base ptr<i64> = int_to_ptr(753664)\n",
        "        volatile_load(ptr_offset(base, off))\n",
        "\n",
        "fn main -> i64\n",
        "    vga_put(0, 65)\n",
        "    vga_get(0)\n",
    );
    std::fs::write(&src, source).expect("write source");

    let emit = lullaby()
        .args([
            "native",
            "--freestanding",
            "--verbose",
            "-o",
            obj.to_str().expect("obj path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    let listing = format!("{}{}", stdout(&emit), stderr(&emit));
    assert!(
        emit.status.success(),
        "the freestanding MMIO idiom must still emit natively:\n{listing}"
    );
    for name in ["vga_put", "vga_get"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "`{name}` must COMPILE, not skip — a skip would gut the freestanding \
             tier while staying green:\n{listing}"
        );
    }
}

/// Run `lullaby check` on `source` and return `(exit code, stdout+stderr)`. The
/// `L0350` lifetime check is a frontend check, so `check` is the whole surface.
fn check_source(source: &str, tag: &str) -> (i32, String) {
    let dir = ScratchDir::new("check_source");
    let src = dir.join(format!("{tag}.lby"));
    std::fs::write(&src, source).expect("write source");
    let out = lullaby()
        .args(["check", src.to_str().expect("src path")])
        .output()
        .expect("run check");
    (
        out.status.code().expect("exit code"),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

fn assert_check_rejects(source: &str, tag: &str, code: &str) {
    let (exit, output) = check_source(source, tag);
    assert_ne!(exit, 0, "must be REJECTED for {tag}:\n{source}\n{output}");
    assert!(
        output.contains(code),
        "must be rejected with {code} for {tag}:\n{source}\n{output}"
    );
}

fn assert_check_accepts(source: &str, tag: &str) {
    let (exit, output) = check_source(source, tag);
    assert_eq!(exit, 0, "must be ACCEPTED for {tag}:\n{source}\n{output}");
}

/// THE `L0350` alias repro: a copy of a box escaped the freed-name tracking
/// entirely, so this type-checked and reached the backend, failing only at RUN time
/// (`L0406`). That hole is why native `dealloc` skips instead of lowering to
/// `rc_free` — under `rc_free` this would silently read free-list memory.
#[test]
fn use_after_free_through_a_direct_alias_is_rejected() {
    assert_check_rejects(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        dealloc(p)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_l0350_alias_uaf",
        "L0350",
    );
}

/// Aliasing is transitive AND symmetric over copies: `p`/`q`/`r` denote one box, so
/// freeing `r` — the last copy — kills the ORIGINAL `p`. This is the direction a
/// naive "dest aliases source" rule gets wrong.
#[test]
fn use_after_free_through_a_transitive_alias_is_rejected() {
    assert_check_rejects(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        let r = q\n",
            "        dealloc(r)\n",
            "        ptr_read(p)\n",
        ),
        "lullaby_l0350_alias_transitive",
        "L0350",
    );
}

/// A double free through an alias. Under a native `rc_free` this would push one
/// block onto the free list twice, making it cyclic.
#[test]
fn double_free_through_a_direct_alias_is_rejected() {
    assert_check_rejects(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        dealloc(p)\n",
            "        dealloc(q)\n",
            "        0\n",
        ),
        "lullaby_l0350_alias_double_free",
        "L0350",
    );
}

/// FALSE-POSITIVE CONTROL: re-binding an alias detaches it from the group and
/// revives it. `q` gets a fresh box after `p` is freed, so reading it is fine. If
/// this fails, the alias tracking is too eager and breaks working programs.
#[test]
fn rebinding_an_alias_after_a_free_is_accepted() {
    assert_check_accepts(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        dealloc(p)\n",
            "        q = alloc(5)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_l0350_alias_rebound",
    );
}

/// FALSE-POSITIVE CONTROL: two independent boxes are not aliases. Freeing one must
/// not implicate the other — a whole-type-based or too-coarse rule would fail here.
#[test]
fn freeing_one_box_does_not_implicate_an_independent_box() {
    assert_check_accepts(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let a = alloc(10)\n",
            "        let b = alloc(20)\n",
            "        dealloc(a)\n",
            "        ptr_read(b)\n",
        ),
        "lullaby_l0350_independent_boxes",
    );
}

/// FALSE-POSITIVE CONTROL: using an alias BEFORE the free is legal, and must stay
/// legal — the check is straight-line and order-sensitive, not name-based.
#[test]
fn using_an_alias_before_the_free_is_accepted() {
    assert_check_accepts(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        let v = ptr_read(q)\n",
            "        dealloc(p)\n",
            "        v\n",
        ),
        "lullaby_l0350_alias_use_before_free",
    );
}

/// Locate `llvm-nm.exe` in the rustc toolchain bin dir, mirroring
/// `llvm_readobj_path`. `None` when the toolchain or tool cannot be found.
fn llvm_nm_path() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let tool = std::path::PathBuf::from(sysroot)
        .join("lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-nm.exe");
    tool.is_file().then_some(tool)
}

/// A void `export fn` is the natural C-ABI shape for a driver/callback entry point
/// (`void NAME(...)`), but `is_exportable_scalar` admitted only `i64`/`f64`/`f32`
/// for BOTH parameters and the return, so it was rejected with `L0424` — even
/// though void functions compile natively. It must now check, compile, and emit a
/// real external symbol.
///
/// The symbol assertion is unconditional (the name is in the COFF symbol table
/// bytes); the `llvm-nm` decode below is the stronger, gated check.
#[test]
fn void_export_fn_compiles_and_emits_a_c_callable_symbol() {
    let dir = ScratchDir::new("void_export");
    let src = dir.join("lullaby_void_export.lby");
    let obj = dir.join("lullaby_void_export.obj");
    // No `main`: an export-only program is a C-callable LIBRARY object, which is
    // exactly the driver/callback shape a void export exists for.
    let source = concat!(
        "export fn tick x i64 -> void\n",
        "    let y = x + 1\n",
        "\n",
        "export fn compute a i64 -> i64\n",
        "    a * 2\n",
    );
    std::fs::write(&src, source).expect("write source");

    let check = lullaby()
        .args(["check", src.to_str().expect("src path")])
        .output()
        .expect("run check");
    assert!(
        check.status.success(),
        "a void `export fn` must type-check:\n{}",
        stderr(&check)
    );

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            obj.to_str().expect("obj path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    assert!(
        emit.status.success(),
        "a void `export fn` must emit natively:\n{}",
        stderr(&emit)
    );
    let listing = format!("{}{}", stdout(&emit), stderr(&emit));
    assert!(
        listing.contains("compiled tick"),
        "the void export must COMPILE, not skip:\n{listing}"
    );
    assert!(obj.is_file(), "expected a native object:\n{listing}");

    // Unconditional: the exported name must be in the object's symbol table.
    let bytes = std::fs::read(&obj).expect("read object");
    assert!(
        contains_subslice(&bytes, b"tick"),
        "the exported symbol `tick` must appear in the object's symbol table"
    );

    // Stronger, toolchain-gated: decode the symbol table and assert `tick` is an
    // external DEFINED text symbol (`T`) — i.e. genuinely C-callable, not a local.
    match llvm_nm_path() {
        Some(tool) => {
            let dump = std::process::Command::new(tool)
                .arg(obj.to_str().expect("obj path"))
                .output()
                .expect("run llvm-nm");
            let symbols = String::from_utf8_lossy(&dump.stdout).to_string();
            assert!(
                symbols.lines().any(|line| line.ends_with(" T tick")),
                "`tick` must be an external defined text symbol (T):\n{symbols}"
            );
            assert!(
                symbols.lines().any(|line| line.ends_with(" T compute")),
                "the i64 export must still be external too:\n{symbols}"
            );
            eprintln!("llvm-nm decode ran: verified `T tick` and `T compute`");
        }
        None => eprintln!(
            "llvm-nm not found; ran the unconditional symbol-table byte check only \
             (the `T tick` linkage decode was NOT executed)"
        ),
    }
}

/// The C ABI of a void export: it must take its argument in the Win64 integer
/// register and return WITHOUT publishing a return value — no C caller of a `void`
/// function may read `rax`. Pinned by disassembling the emitted symbol.
///
/// This is the mirror of the entry-stub defect where a void `main` leaked `rax` as
/// the process exit code: a void function has no value position, so nothing may
/// treat its `rax` as meaningful.
#[test]
fn void_export_fn_uses_the_c_abi_and_publishes_no_return_value() {
    let Some(nm) = llvm_nm_path() else {
        eprintln!("llvm-nm not found; the void-export ABI disassembly did NOT run");
        return;
    };
    let objdump = nm.with_file_name("llvm-objdump.exe");
    if !objdump.is_file() {
        eprintln!("llvm-objdump not found; the void-export ABI disassembly did NOT run");
        return;
    }
    let dir = ScratchDir::new("void_export_abi");
    let src = dir.join("lullaby_void_export_abi.lby");
    let obj = dir.join("lullaby_void_export_abi.obj");
    std::fs::write(&src, "export fn tick x i64 -> void\n    let y = x + 1\n")
        .expect("write source");

    let emit = lullaby()
        .args([
            "native",
            "-o",
            obj.to_str().expect("obj path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    assert!(
        emit.status.success(),
        "void export must emit:\n{}",
        stderr(&emit)
    );

    let dump = std::process::Command::new(&objdump)
        .args(["-d", obj.to_str().expect("obj path")])
        .output()
        .expect("run llvm-objdump");
    let text = String::from_utf8_lossy(&dump.stdout).to_string();
    let body: String = text
        .lines()
        .skip_while(|line| !line.contains("<tick>:"))
        .take_while(|line| !line.contains("<compute>:"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!body.is_empty(), "could not find `tick` in:\n{text}");
    // Win64: the first integer argument arrives in `rcx`.
    assert!(
        body.contains("%rcx"),
        "a void export must read its i64 argument from the Win64 register `rcx`:\n{body}"
    );
    // It must return normally...
    assert!(
        body.contains("retq"),
        "a void export must return to its C caller:\n{body}"
    );
    // ...and must not publish a computed value: the void path zeroes `rax` rather
    // than leaving the body's last value in it, so nothing can be mistaken for a
    // return value (and nothing leaks).
    assert!(
        body.contains("xorq\t%rax, %rax") || body.contains("xorq %rax, %rax"),
        "a void export must not publish a return value in `rax`:\n{body}"
    );
    eprintln!("llvm-objdump decode ran: verified rcx arg, retq, and a zeroed rax");
}

/// A void export must still be CALLABLE and return control correctly on every tier.
/// The exported symbol is ordinary code, so calling it from Lullaby's own `main`
/// exercises the same lowering a C caller would reach — and `main`'s exit code
/// proves the void call returned cleanly without disturbing the caller's value.
///
/// Note what this does NOT do: it cannot observe a side effect, because an export's
/// parameters are limited to the `i64`/`f64`/`f32` scalar set — a `ptr<i64>`
/// out-parameter is itself still `L0424` (a separate, pre-existing limit that this
/// change deliberately does not touch). So a void export currently has no way to
/// communicate anything back to a caller. That makes the feature real but narrow:
/// useful for a callback invoked purely for its effect on external state, not yet
/// for the `poke(addr_of(cell), v)` driver spelling. Widening export parameters to
/// pointers is the follow-up that makes void exports genuinely useful.
#[test]
fn void_export_fn_is_callable_and_returns_cleanly_on_every_tier() {
    assert_all_interpreters_yield(
        concat!(
            "export fn tick x i64 -> void\n",
            "    let y = x + 1\n",
            "\n",
            "fn main -> i64\n",
            "    let v i64 = 42\n",
            "    tick(7)\n",
            "    return v\n",
        ),
        "lullaby_void_export_call",
        42,
    );
}

/// NEGATIVE CONTROL: admitting `void` must not open the export gate generally. A
/// genuinely non-exportable return (`string`) is still `L0424`.
#[test]
fn a_non_exportable_return_type_is_still_rejected() {
    assert_check_rejects(
        concat!(
            "export fn name -> string\n",
            "    \"hi\"\n",
            "\n",
            "fn main -> i64\n",
            "    0\n",
        ),
        "lullaby_export_string_return",
        "L0424",
    );
}

/// NEGATIVE CONTROL: `void` is a RETURN-only concession. There is no `void` value
/// to pass, so a `void` PARAMETER stays rejected — the asymmetry is deliberate.
#[test]
fn a_void_parameter_is_still_rejected() {
    assert_check_rejects(
        concat!(
            "export fn sink x void -> void\n",
            "    let y = 1\n",
            "\n",
            "fn main -> i64\n",
            "    0\n",
        ),
        "lullaby_export_void_param",
        "L0424",
    );
}

/// HONESTY PIN — this documents a hole that is still OPEN, not a fix.
///
/// An alias laundered through a **call** is not tracked: `identity(p)` returns the
/// same box, but the checker sees an opaque call, so this compiles and dies at RUN
/// time with `L0406`. Closing it needs interprocedural alias analysis, which is out
/// of scope. If a later change closes it, this test will start failing — that is the
/// intent: it must be rewritten, not deleted, so the frontier stays documented.
#[test]
fn alias_through_a_call_is_not_tracked_and_still_fails_only_at_runtime() {
    let source = concat!(
        "fn identity p ptr_i64 -> ptr_i64\n",
        "    p\n",
        "\n",
        "fn main -> i64\n",
        "    unsafe\n",
        "        let p = alloc(8)\n",
        "        let q = identity(p)\n",
        "        dealloc(p)\n",
        "        ptr_read(q)\n",
    );
    let (exit, output) = check_source(source, "lullaby_l0350_alias_via_call");
    assert_eq!(
        exit, 0,
        "an alias through a call is NOT statically tracked today; if this now fails, \
         interprocedural aliasing was closed and this pin needs rewriting:\n{output}"
    );
    // It is still caught, but only at run time, by the interpreters.
    let (run_exit, run_output) = run_backend(source, "ast", "lullaby_l0350_alias_via_call_run");
    assert_ne!(
        run_exit, 0,
        "the runtime must still catch it:\n{run_output}"
    );
    assert!(
        run_output.contains("L0406"),
        "the runtime diagnostic should be L0406:\n{run_output}"
    );
}
