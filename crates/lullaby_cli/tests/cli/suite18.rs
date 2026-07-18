//! CLI integration tests, part 18 — the interim heap-box builtins (`alloc` /
//! `dealloc`) on the native backend, verified END-TO-END: each fixture is compiled
//! to a real `.exe` and RUN, and its exit code is checked against all three
//! interpreters.
//!
//! # The gap these close
//!
//! `alloc` had no native lowering at all, so a heap-box program compiled to
//! NOTHING (`skipped main: call to non-i64-scalar or unknown function 'alloc'`)
//! while the interpreters ran it fine.
//!
//! # What `alloc` actually is
//!
//! Not a byte allocator. `alloc(v)` is typed `ptr_{typeof v}` and the interpreters
//! implement it as `heap.push(Some(value))` returning the cell's INDEX — so
//! `alloc(8)` is a **box holding 8**, and `ptr_read` of it yields `8`, not
//! uninitialized storage. `native_alloc_box_is_its_argument_not_a_byte_count` pins
//! that, because the name misleads.
//!
//! # The cross-frame headline
//!
//! An `alloc` box is the ONE pointer form that crosses a frame boundary on every
//! tier — an `addr_of` pointer is refused cross-frame by the interpreters (`L0459`).
//! Both directions are pinned here: the out-parameter idiom
//! (`native_alloc_box_crosses_a_function_boundary`) and a returned allocation
//! (`native_returned_alloc_box_matches_interpreters`). Note the parameter grammar:
//! differently-typed parameters are SPACE-separated (`fn poke p ptr_i64 v i64`); a
//! comma groups same-type parameters (`fn f x, y i64`), so `fn poke p ptr_i64, v
//! i64` is an `L0204` about the comma, not about `ptr_i64`.
//!
//! # What is NOT here, and why
//!
//! * **No `ptr_offset` walk fixture.** The interpreters REFUSE `ptr_offset` over an
//!   `alloc` box at run time (`L0406`), and a box is one cell anyway, so the walk is
//!   out of bounds by construction. Native refuses it too (`L0339`) rather than
//!   define it as garbage — pinned below.
//! * **No `dealloc` execution fixture.** `dealloc` has no native lowering by design;
//!   its clean skip is pinned below. See `native_object_heapbox.rs` for the full
//!   reasoning (in short: the interpreters DETECT a later use / double free with
//!   `L0406`, and the `L0350` static check does not survive aliasing, so no native
//!   lowering can match without turning a detected error into silent corruption).
//!
//! The instruction-selection proofs live in
//! `crates/lullaby_ir/src/native_object_heapbox_tests.rs`; the broad sweep against
//! the arena escape analysis lives in `gen_alloc_arena_program` in `fuzz.rs`.

use crate::*;
use std::process::Command;

/// Run `source` on one interpreter backend and return its printed `main` result.
fn interpreter_result(source: &str, backend: &str, tag: &str) -> String {
    let dir = ScratchDir::new("interpreter_result");
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
/// host cannot produce/run one. Direct-PE emission is the default for an eligible
/// build, so no external linker is required. Panics if the shape SKIPS — a
/// regression that un-compiles a fixture here is a failure, not a silent pass.
fn native_exit_for(source: &str, tag: &str) -> Option<i32> {
    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping {tag}");
        return None;
    }
    let dir = ScratchDir::new("native_exit_for");
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
/// exe's exit code with them. Windows exit codes are full 32-bit values (not
/// truncated to 8 bits), so the comparison is exact.
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

/// Assert `source` does NOT compile natively — it must skip cleanly (`L0339`) — and
/// that the reported reason mentions `reason`.
fn assert_native_skips_because(source: &str, tag: &str, reason: &str) {
    let dir = ScratchDir::new("assert_native_skips_because");
    let src = dir.join(format!("{tag}.lby"));
    let exe = dir.join(format!("{tag}.exe"));
    std::fs::write(&src, source).expect("write source");
    let _ = std::fs::remove_file(&exe);

    let out = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            exe.to_str().expect("exe path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        combined.contains("L0339"),
        "a refused shape must skip with L0339 for {tag}:\n{source}\n{combined}"
    );
    assert!(
        combined.contains(reason),
        "the skip reason should mention `{reason}` for {tag}:\n{combined}"
    );
}

/// THE reported repro. This program compiled to nothing natively while every
/// interpreter answered 42.
#[test]
fn native_alloc_ptr_write_ptr_read_matches_interpreters() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        ptr_write(p, 42)\n",
            "        ptr_read(p)\n",
        ),
        "lullaby_alloc_repro",
        42,
    );
}

/// `alloc(8)` BOXES the value 8 — it does not reserve 8 bytes. Reading the box back
/// without writing yields `8` on every tier. This pins the semantic the builtin's
/// name misleads about, and would catch a native lowering that (following the name)
/// allocated `n` bytes of uninitialized storage instead.
#[test]
fn native_alloc_box_is_its_argument_not_a_byte_count() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        ptr_read(p)\n",
        ),
        "lullaby_alloc_box_value",
        8,
    );
}

/// Independent boxes are independent cells: two `alloc`s must not alias, and a
/// read-modify-write through each must compose. A lowering that reused one cell (or
/// mis-sized the allocation so the second overlapped the first) gives a wrong sum.
#[test]
fn native_multiple_alloc_boxes_are_independent() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let a = alloc(10)\n",
            "        let b = alloc(20)\n",
            "        ptr_write(a, ptr_read(a) + 5)\n",
            "        ptr_write(b, ptr_read(b) * 2)\n",
            "        ptr_read(a) + ptr_read(b)\n",
        ),
        "lullaby_alloc_independent",
        55,
    );
}

/// THE headline: an `alloc` box passed ACROSS A FUNCTION BOUNDARY to a callee that
/// writes through it, read back in the caller — the out-parameter idiom. This is the
/// shape `addr_of` cannot do on the interpreters (they refuse a cross-frame `addr_of`
/// pointer with `L0459`, since a callee cannot reach its caller's `Env`), so an
/// `alloc` box is the only pointer form that works across frames on EVERY tier. It
/// now does natively too — which is exactly what makes the `L0459` hint's "works
/// across frames on every tier" honest.
#[test]
fn native_alloc_box_crosses_a_function_boundary() {
    assert_all_four_tiers_agree(
        concat!(
            "fn poke p ptr_i64 v i64\n",
            "    unsafe\n",
            "        ptr_write(p, v)\n",
            "\n",
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p ptr_i64 = alloc(0)\n",
            "        poke(p, 99)\n",
            "        ptr_read(p)\n",
        ),
        "lullaby_alloc_cross_frame",
        99,
    );
}

/// The other direction across a boundary: a callee RETURNS a fresh box and the
/// caller reads it. Two independent allocations prove the returned cells do not
/// alias. `ptr_i64` lowers as a `NativeType::I64` register value, so this exercises
/// a box flowing out through the ordinary scalar return convention.
#[test]
fn native_returned_alloc_box_matches_interpreters() {
    assert_all_four_tiers_agree(
        concat!(
            "fn make v i64 -> ptr_i64\n",
            "    unsafe\n",
            "        alloc(v)\n",
            "\n",
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p ptr_i64 = make(77)\n",
            "        let q ptr_i64 = make(23)\n",
            "        ptr_read(p) + ptr_read(q)\n",
        ),
        "lullaby_alloc_returned",
        100,
    );
}

/// The two pointer models do NOT interoperate at a boundary: an `alloc`-produced
/// `ptr_i64` cannot be passed to a `ptr<i64>` parameter (`L0313`), and cannot be
/// bound to a `ptr<i64>` annotation (`L0303`). Pinned because the `L0459` hint calls
/// an `alloc` result a "`ptr<T>`", which a reader could reasonably act on — and it
/// would not compile. Recorded for the owner decision on unifying the two families.
#[test]
fn an_alloc_box_is_not_a_typed_ptr_at_a_boundary() {
    let dir = ScratchDir::new("an_alloc_box_is_not_a_typed_ptr_at_a_bou");
    let src = dir.join("lullaby_alloc_model_mismatch.lby");
    std::fs::write(
        &src,
        concat!(
            "fn takes p ptr<i64>\n",
            "    unsafe\n",
            "        ptr_write(p, 1)\n",
            "\n",
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p ptr_i64 = alloc(5)\n",
            "        takes(p)\n",
            "        ptr_read(p)\n",
        ),
    )
    .expect("write source");
    let out = lullaby()
        .args(["check", src.to_str().expect("src path")])
        .output()
        .expect("run check");
    let combined = format!("{}{}", stdout(&out), stderr(&out));
    assert!(
        combined.contains("L0313"),
        "passing a `ptr_i64` to a `ptr<i64>` parameter must be rejected:\n{combined}"
    );
}

/// `alloc` in a loop: 20 boxes, each read-modify-written and accumulated. This is
/// the heap-growth shape — nothing reclaims an `alloc`'d box natively (no drop glue,
/// and the arena is denied for `alloc`-using functions), exactly like the
/// interpreters, whose `heap: Vec<Option<Value>>` also only grows. The 1 MiB region
/// bounds it; exhaustion is the allocator's defined `ud2` trap, never a silent
/// overrun.
#[test]
fn native_alloc_in_a_loop_matches_interpreters() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let total i64 = 0\n",
            "    for i from 1 to 20\n",
            "        unsafe\n",
            "            let p = alloc(i)\n",
            "            ptr_write(p, ptr_read(p) * 2)\n",
            "            total = total + ptr_read(p)\n",
            "    total\n",
        ),
        "lullaby_alloc_loop",
        420,
    );
}

/// The ARENA hazard, end-to-end. An `alloc`'d cell is manually managed and invisible
/// to the arena escape analysis (`ptr_i64` is not a heap type), so `h` looks
/// arena-eligible and its loop looks heap-touching (the `string`) AND confined (the
/// only store is a `ptr_i64`) — earning a per-iteration sub-region whose bump rewind
/// reclaims the cell `q` still names, which the post-loop string then overwrites.
///
/// With the `alloc_defeats_arena` gate REMOVED this program is a real miscompile:
/// native answers **92** where every interpreter answers **2116** (measured). The
/// gate keeps `h` on the RC / free-list path, where nothing reclaims its boxes.
#[test]
fn native_alloc_is_not_reclaimed_by_an_arena_rewind() {
    assert_all_four_tiers_agree(
        concat!(
            "fn h a i64 -> i64\n",
            "    unsafe\n",
            "        let q = alloc(0)\n",
            "        for j from 0 to 5\n",
            "            q = alloc(j * 100 + 7)\n",
            "            let s string = to_string(a + j)\n",
            "        let z string = to_string(a) + \"clobberclobberclobber\"\n",
            "        ptr_read(q) + len(z)\n",
            "\n",
            "fn main -> i64\n",
            "    let total i64 = 0\n",
            "    for i from 0 to 3\n",
            "        total = total + h(i)\n",
            "    total\n",
        ),
        "lullaby_alloc_arena_uaf",
        2116,
    );
}

/// `dealloc` must skip cleanly rather than be lowered. Every available lowering
/// diverges from the interpreters, which invalidate the cell and DETECT a later use
/// or a double free (`L0406`): `rc_free` would make a use-after-free read free-list
/// memory silently and a double free alias two live allocations; a no-op would make
/// a use-after-free succeed. The `L0350` static check does not close the gap: it now
/// rejects a DIRECT copy (`let q = p  dealloc(p)  ptr_read(q)`, suite21), but it is
/// copy tracking rather than alias analysis, so an alias through a call or an
/// aggregate still compiles and reaches the backend — and one untracked alias is all
/// `rc_free` needs to turn a detected `L0406` into silent corruption.
#[test]
fn native_dealloc_skips_gracefully() {
    assert_native_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(5)\n",
            "        dealloc(p)\n",
            "        7\n",
        ),
        "lullaby_alloc_dealloc_skip",
        "`dealloc` is not lowered natively",
    );
}

/// The interpreters model an `alloc` box as a heap-SLOT INDEX, not an address, so
/// `ptr_to_int` of one is a slot number — a DEFINED program a real machine address
/// would answer differently. Native must skip, not diverge. (On the interpreters
/// this program answers `0`; natively it would be a heap address.)
#[test]
fn native_ptr_to_int_of_an_alloc_box_skips_gracefully() {
    assert_native_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(7)\n",
            "        ptr_to_int(p)\n",
        ),
        "lullaby_alloc_ptr_to_int_skip",
        "`ptr_to_int` over the `ptr_i64` produced by `alloc`",
    );
}

/// An `alloc` box is ONE cell and the interpreters refuse to stride over it at all
/// (`L0406`: "ptr_offset requires a pointer produced by addr_of"); natively a stride
/// would walk into the allocator's own RC header. Native must skip.
#[test]
fn native_ptr_offset_over_an_alloc_box_skips_gracefully() {
    assert_native_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(7)\n",
            "        ptr_read(ptr_offset(p, 1))\n",
        ),
        "lullaby_alloc_ptr_offset_skip",
        "`ptr_offset` over the `ptr_i64` produced by `alloc`",
    );
}

/// `ptr_cast` over an `alloc` box must not be lowered natively — defense in depth
/// behind the frontend's model-preservation rule.
///
/// **History, and why this shape changed.** `check_ptr_cast` used to derive its
/// result type from the caller's ANNOTATION (defaulting to `ptr<i64>`), never from
/// the operand, so `let q ptr<i64> = ptr_cast(p)` rewrote `ptr_i64` into the very
/// spelling this gate keys on — after which `ptr_offset(q, 1)` strode 8 bytes past
/// the one-cell payload into the NEXT block's `[size]` header, the word
/// `__lullaby_alloc`'s free-list scan reads to decide reuse. A write there corrupted
/// allocator metadata. Measured then, against interpreters that raise `L0406` /
/// answer `0`: the strided read compiled and exited **0**; `ptr_to_int` gave a real
/// address (**1073758240**) where the interpreters give the slot index **0**; the
/// strided write compiled and executed.
///
/// `check_ptr_cast` now takes the result's pointer model from the OPERAND, so those
/// annotations are rejected outright with `L0303` and the laundered program can no
/// longer be written — `suite21.rs` pins that, in both directions. What this test
/// keeps pinned is the gate itself, through the IDENTITY cast the frontend still
/// allows: `let q = ptr_cast(p)` preserves the `ptr_i64` model, so the operand that
/// reaches the backend is genuinely still a box, and the gate must refuse it.
#[test]
fn native_ptr_cast_over_an_alloc_box_is_not_lowered() {
    for (tag, tail) in [("read", "ptr_read(q)"), ("identity", "ptr_to_int(q)")] {
        assert_native_skips_because(
            &format!(
                "fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc(7)\n        \
                 let q = ptr_cast(p)\n        {tail}\n"
            ),
            &format!("lullaby_alloc_cast_box_{tag}"),
            "`ptr_cast` over the `ptr_i64` produced by `alloc`",
        );
    }
}

/// The CROSS-FUNCTION route. A helper that laundered the model outright
/// (`fn launder p ptr_i64 -> ptr<i64>` returning `ptr_cast(p)`) no longer type-checks:
/// its body now yields `ptr_i64`, not the declared `ptr<i64>` (`L0301`), and handing a
/// box to a `ptr<i64>` parameter is `L0313`. So this pins what is still expressible —
/// a model-preserving `-> ptr_i64` helper — and the property that matters: the
/// helper's own `ptr_cast` site carries the `ptr_T` operand, so it refuses there, the
/// helper skips, and the demotion fixpoint skips `main`.
#[test]
fn native_ptr_cast_over_an_alloc_box_is_not_lowered_across_a_function() {
    assert_native_skips_because(
        concat!(
            "fn rebox p ptr_i64 -> ptr_i64\n",
            "    unsafe\n",
            "        ptr_cast(p)\n",
            "\n",
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p ptr_i64 = alloc(7)\n",
            "        let q = rebox(p)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_alloc_cast_rebox_fn",
        "`ptr_cast` over the `ptr_i64` produced by `alloc`",
    );
}

/// The negative control for the `ptr_cast` gate, end-to-end: a genuine `ptr<T>` cast
/// chain — `addr_of` -> `ptr_cast` to `ptr<u8>` -> `ptr_cast` back -> `ptr_offset` ->
/// `ptr_to_int` — must still COMPILE and agree on all four tiers. The gate keys on the
/// legacy `ptr_T` spelling only, so the documented `let bp ptr<byte> = ptr_cast(base)`
/// idiom is untouched.
#[test]
fn native_ptr_cast_over_a_typed_pointer_still_works() {
    assert_all_four_tiers_agree(
        concat!(
            "fn main -> i64\n",
            "    let buf array<i64> = [1, 2, 3]\n",
            "    unsafe\n",
            "        let p ptr<i64> = addr_of(buf[0])\n",
            "        let bp ptr<u8> = ptr_cast(p)\n",
            "        let q ptr<i64> = ptr_cast(bp)\n",
            "        let r ptr<i64> = ptr_offset(q, 1)\n",
            "        ptr_read(r) + ptr_to_int(r) - ptr_to_int(q)\n",
        ),
        "lullaby_ptr_cast_typed_control",
        10,
    );
}

/// A box whose cell is not an 8-byte scalar has no width-exact native
/// representation on the raw-pointer read path, so it skips cleanly rather than
/// guessing a layout.
#[test]
fn native_out_of_subset_alloc_boxes_skip_gracefully() {
    assert_native_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(\"hi\")\n",
            "        len(ptr_read(p))\n",
        ),
        "lullaby_alloc_string_skip",
        "`alloc` of a `string` value is not lowered natively",
    );
}

/// Arena stage-2 target-aware confinement (I4), the POSITIVE end-to-end: rebinding a
/// loop-LOCAL `string` with a fresh allocation each iteration is newly admitted, so
/// the loop gets a per-iteration sub-region. The rewind reclaims exactly the dead
/// per-iteration scratch, so native must still agree with every interpreter — a
/// mis-scoped rewind (freeing `s` before `len(s)` reads it) would diverge here.
#[test]
fn native_i4_rebound_loop_local_matches_interpreters() {
    assert_all_four_tiers_agree(
        concat!(
            "fn sum_lens n i64 -> i64\n",
            "    let total i64 = 0\n",
            "    for i from 0 to n\n",
            "        let s string = to_string(i)\n",
            "        s = s + \"!\"\n",
            "        total = total + len(s)\n",
            "    total\n\n",
            "fn main -> i64\n",
            "    sum_lens(10)\n",
        ),
        "lullaby_i4_rebind_local",
        23,
    );
}

/// Arena stage-2 I4 guard, the escaping counterpart of the rebind above, end-to-end.
/// The store target `keep` is declared OUTSIDE the loop, so the heap value it holds
/// after the loop is genuinely live; the target-aware rule must still DENY
/// confinement (no per-iteration rewind), so the post-loop `clobber` allocation does
/// not overwrite `keep`. Native must equal every interpreter (**96**). This is the
/// string analogue of the `alloc` `92-vs-2116` pin: DROPPING the iteration-local
/// guard (admitting the outer `keep` as if it were local) makes native reclaim the
/// live `keep` and answer **48** while the interpreters answer **96** — the measured
/// use-after-free the guard prevents.
#[test]
fn native_i4_store_into_outer_string_stays_denied() {
    assert_all_four_tiers_agree(
        concat!(
            "fn h a i64 -> i64\n",
            "    let keep string = \"\"\n",
            "    for j from 0 to 5\n",
            "        keep = to_string(a + j) + \"!\"\n",
            "    let z string = to_string(a) + \"clobberclobberclobber\"\n",
            "    len(keep) + len(z)\n\n",
            "fn main -> i64\n",
            "    let total i64 = 0\n",
            "    for i from 0 to 3\n",
            "        total = total + h(i)\n",
            "    total\n",
        ),
        "lullaby_i4_store_outer_string",
        96,
    );
}
