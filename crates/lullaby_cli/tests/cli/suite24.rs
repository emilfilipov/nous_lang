//! CLI integration tests, part 24 — **inline, by-value fixed-extent arrays as
//! struct fields** on the native backend (road_to_1_0_stable A2, increment 2),
//! verified END-TO-END: each fixture is compiled to a real `.exe` and RUN, and its
//! exit code is checked against all three interpreters.
//!
//! # The gap these close
//!
//! Increment 1 erased `array<T, N>` to a length-agnostic `array<T>` before every
//! backend, so a struct with an array FIELD had no length for the native backend to
//! infer (a field has no initializer) and the whole function SKIPPED (`L0339`). The
//! extent-survival channel (`IrStructDef::field_extents`) carries each fixed-array
//! field's un-erased type to native, which lays the field out INLINE and by value
//! inside the struct (`NativeType::Array`) — the representation a known extent
//! unlocks and a fat pointer cannot soundly have in a by-value aggregate.
//!
//! # The soundness core (why the copy tests matter most)
//!
//! A by-value struct copy (`let g = f`), a by-value parameter, and a by-value return
//! must COPY the inline array's elements — mutating the copy must never touch the
//! original. `copy_isolation_*` and `by_value_param_mutation_*` pin exactly that:
//! they mutate a copy / a callee's parameter and assert the original is unchanged on
//! every tier. A miscompile that aliased the elements (e.g. a fat pointer sneaking
//! into a by-value position) would show up as the original changing.
//!
//! # The lower-vs-skip boundary (correct-or-refuse)
//!
//! LOWERS inline: a struct field `array<T, N>` whose element is an entirely
//! stack-inline (scalar-only) layout — `i64`/fixed-width/`bool`/`char`/`byte`,
//! `f64`, a packed narrow integer (`u8`/`i32`/…), a nested fixed array, or a
//! scalar-only struct element. Construction (literal or `[v; k]` fill), the by-value
//! copy/param/return, `f.field[i]`, `len(f.field)`, and `addr_of(f.field[0])` +
//! `ptr_offset` all operate on the inline representation. **Increment 4 adds the two
//! whole-field shapes** that increment 2 deferred: `for x in f.field` (iterate the
//! inline field array directly — the desugar's hidden `let __coll = f.field`
//! collection binding now lowers, then the numeric loop reads each element) and
//! `let c = f.field` (bind the whole inline field array to a fresh local by an
//! element-wise by-value copy — mutating `c` never touches `f.field`). Both work for
//! scalar-element fields, including packed narrow (`u8`/`i32`, sign/zero-extension
//! preserved) and nested (`array<array<i32, N>, M>`) fields.
//!
//! SKIPS cleanly (`L0339`, demote to the interpreters — never a miscompile, never a
//! fat pointer in a by-value aggregate): a plain dynamic `array<T>` field (no
//! extent, no static length), a heap-word element (`array<string, N>` — copying the
//! struct would SHARE the string pointers, which is value-safe for immutable
//! strings but is deferred to keep the by-value copy uniformly element-wise), a
//! whole-field copy from a RUNTIME-indexed source (`let row = grid[i]` for a runtime
//! `i` — the source base is dynamic), and `for x in f.field` where the ELEMENT is
//! itself an aggregate (a struct or nested array), because the per-element
//! `let x = coll[idx]` is then a dynamic-index AGGREGATE copy that stays deferred.
//! Each skip is proven a sound DEMOTION by checking the interpreters still answer
//! correctly.

use crate::*;
use std::process::Command;

/// Run `source` on one interpreter backend and return its printed `main` result.
fn interpreter_result(source: &str, backend: &str, tag: &str) -> String {
    let dir = ScratchDir::new("suite24_interp");
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
/// regression that un-compiles a fixture here is a failure, not a silent pass, which
/// is the tooth for the lowering tests.
fn native_exit_for(source: &str, tag: &str) -> Option<i32> {
    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping native run for {tag}");
        return None;
    }
    let dir = ScratchDir::new("suite24_native");
    let src = dir.join(format!("{tag}.lby"));
    let exe = dir.join(format!("{tag}.exe"));
    std::fs::write(&src, source).expect("write source");

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
        "expected a native exe for {tag} (this shape must COMPILE inline, not skip):\n{}",
        stdout(&emit)
    );
    let run = Command::new(&exe).output().expect("run exe");
    Some(run.status.code().expect("exit code"))
}

/// Assert every tier agrees: the three interpreters with each other, and the native
/// exe's exit code with them. Windows exit codes are full 32-bit values (not
/// truncated to 8 bits), so the comparison is exact for the >255 expected values.
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

/// Assert `source` does NOT compile natively — it must skip cleanly (`L0339`) with a
/// reason mentioning `reason` — AND that this is a sound DEMOTION, not a miscompile:
/// every interpreter still answers `expected`.
fn assert_native_skips_soundly(source: &str, tag: &str, reason: &str, expected: i64) {
    for backend in ["ast", "ir", "bytecode"] {
        assert_eq!(
            interpreter_result(source, backend, tag),
            expected.to_string(),
            "{backend} interpreter must still produce {expected} for the demoted {tag}:\n{source}"
        );
    }
    if !cfg!(windows) {
        return;
    }
    let dir = ScratchDir::new("suite24_skip");
    let src = dir.join(format!("{tag}.lby"));
    let exe = dir.join(format!("{tag}.exe"));
    std::fs::write(&src, source).expect("write source");
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
    assert!(
        !exe.is_file(),
        "a refused shape must NOT emit an exe for {tag}"
    );
}

// -- Construction, field read, len, pass + return by value -------------------

/// THE headline: a struct with a fixed-array field constructed, field-read, its
/// `len` folded, and passed by value into a helper. This is the shape that skipped
/// natively at baseline; it now lays the array out inline and matches every tier.
#[test]
fn struct_fixed_array_field_construct_read_len_pass_by_value() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Frame\n",
            "    tag i64\n",
            "    pixels array<i64, 4>\n",
            "    tail i64\n\n",
            "fn sum_frame f Frame -> i64\n",
            "    f.tag + f.tail + f.pixels[0] + f.pixels[1] + f.pixels[2] + f.pixels[3]\n\n",
            "fn main -> i64\n",
            "    let f = Frame(tag: 100, pixels: [1, 2, 3, 4], tail: 200)\n",
            "    sum_frame(f) + len(f.pixels)\n",
        ),
        "struct_field_construct_read_len_pass",
        // 100 + 200 + 1 + 2 + 3 + 4 + len 4 = 314
        314,
    );
}

/// A struct with an array field constructed, then RETURNED by value from a helper
/// (through the hidden result pointer, like every aggregate return), then read.
#[test]
fn struct_fixed_array_field_returned_by_value() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Frame\n",
            "    pixels array<i64, 3>\n",
            "    tag i64\n\n",
            "fn make v i64 -> Frame\n",
            "    Frame(pixels: [v, v + 1, v + 2], tag: v * 10)\n\n",
            "fn main -> i64\n",
            "    let f = make(4)\n",
            "    f.pixels[0] + f.pixels[1] + f.pixels[2] + f.tag + len(f.pixels)\n",
        ),
        "struct_field_returned_by_value",
        // 4 + 5 + 6 + 40 + len 3 = 58
        58,
    );
}

// -- The soundness core: by-value copy isolation -----------------------------

/// Mutating a by-value COPY of a struct with an inline array field must NOT touch
/// the original — the copy duplicates the elements. This reads THREE distinct
/// witnesses so it has teeth against both failure modes: (1) the ORIGINAL's fields
/// (`f.pixels[0]`, `f.pixels[1]`, `f.tag`) must be unchanged after `g` is mutated —
/// an aliasing copy that shared storage would make them observe `g`'s writes; and
/// (2) a COPIED-but-not-mutated field of `g` (`g.pixels[1]`) must equal the source
/// value — a partial/no-op copy that left `g`'s array uninitialized would read
/// garbage. Only the exact element-wise by-value copy yields the expected sum.
#[test]
fn copy_isolation_mutating_copy_leaves_original() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Frame\n",
            "    tag i64\n",
            "    pixels array<i64, 4>\n\n",
            "fn main -> i64\n",
            "    let f = Frame(tag: 5, pixels: [1, 2, 3, 4])\n",
            "    let g = f\n",
            "    g.pixels[0] = 99\n",
            "    g.tag = 77\n",
            // f fully unchanged (1 + 2 + 5); g's written fields (99 + 77); g's
            // COPIED, unmutated element (2). A shared-storage alias would corrupt
            // the f terms; a truncated copy would corrupt g.pixels[1].
            "    f.pixels[0] + f.pixels[1] + f.tag + g.pixels[0] + g.pixels[1] + g.tag\n",
        ),
        "copy_isolation",
        // (1 + 2 + 5) + (99 + 77) + 2 = 186
        186,
    );
}

/// A struct with an inline array field passed BY VALUE into a helper that mutates
/// its PARAMETER's elements directly: the caller's original array must be unchanged,
/// because the callee received an independent copy. This is the parameter-copy half
/// of the soundness core, and mutating the parameter itself (not a local copy of it)
/// gives it real aliasing teeth — if the struct were passed by a shared pointer, the
/// callee's writes would corrupt the caller's `f` and the sum would jump to 400.
#[test]
fn by_value_param_mutation_leaves_caller_unchanged() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Frame\n",
            "    pixels array<i64, 4>\n\n",
            "fn clobber f Frame -> i64\n",
            "    f.pixels[0] = 100\n",
            "    f.pixels[1] = 100\n",
            "    f.pixels[0] + f.pixels[1]\n\n",
            "fn main -> i64\n",
            "    let f = Frame(pixels: [1, 2, 3, 4])\n",
            "    let touched = clobber(f)\n",
            "    touched + f.pixels[0] + f.pixels[1]\n",
        ),
        "by_value_param_mutation",
        // clobbered copy (200) + caller's untouched f (1 + 2) = 203
        203,
    );
}

// -- Large array return (hidden-result-pointer ABI) --------------------------

/// A struct carrying a LARGE fixed array (`array<i64, 64>`) constructed with a fill
/// literal and returned by value. The 65-word aggregate exercises the hidden
/// result-pointer return path; a dropped hidden-pointer store would corrupt the
/// read-back.
#[test]
fn large_fixed_array_struct_returned_via_hidden_pointer() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Big\n",
            "    data array<i64, 64>\n",
            "    marker i64\n\n",
            "fn make v i64 -> Big\n",
            "    Big(data: [v; 64], marker: v + 1)\n\n",
            "fn main -> i64\n",
            "    let b = make(2)\n",
            "    b.data[0] + b.data[63] + b.marker + len(b.data)\n",
        ),
        "large_fixed_array_return",
        // 2 + 2 + 3 + len 64 = 71
        71,
    );
}

// -- Fill literal as a struct field ------------------------------------------

/// The `[value; count]` fill literal initializing an inline struct array field
/// lowers natively (it expands to a `count`-element literal, then writes the inline
/// elements), matching every tier.
#[test]
fn fill_literal_struct_field_lowers() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Buf\n",
            "    data array<i64, 6>\n\n",
            "fn main -> i64\n",
            "    let b = Buf(data: [7; 6])\n",
            "    b.data[0] + b.data[5] + len(b.data)\n",
        ),
        "fill_literal_struct_field",
        // 7 + 7 + len 6 = 20
        20,
    );
}

// -- Narrow (packed) and mixed narrow/wide fields ----------------------------

/// A packed narrow (`u8`) inline array field: the element stride is one byte, so the
/// inline layout packs eight bytes into one word — the same stride the interpreters
/// use. Reads through `to_i64` and folds `len`.
#[test]
fn narrow_u8_inline_field_packs_and_reads() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Buf\n",
            "    bytes array<u8, 8>\n",
            "    n i64\n\n",
            "fn main -> i64\n",
            "    let b = Buf(bytes: [to_u8(10), to_u8(20), to_u8(30), to_u8(40), to_u8(50), \
             to_u8(60), to_u8(70), to_u8(80)], n: 3)\n",
            "    to_i64(b.bytes[0]) + to_i64(b.bytes[7]) + b.n + len(b.bytes)\n",
        ),
        "narrow_u8_inline_field",
        // 10 + 80 + 3 + len 8 = 101
        101,
    );
}

/// A struct mixing a packed narrow field (`array<u8, 4>`), a wide field
/// (`array<i64, 3>`), and a narrow scalar (`i32`): each field's inline stride is
/// independent and the field offsets stay word-aligned.
#[test]
fn mixed_narrow_and_wide_inline_fields() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Mix\n",
            "    small array<u8, 4>\n",
            "    big array<i64, 3>\n",
            "    tag i32\n\n",
            "fn main -> i64\n",
            "    let m = Mix(small: [to_u8(1), to_u8(2), to_u8(3), to_u8(4)], \
             big: [100, 200, 300], tag: to_i32(9))\n",
            "    to_i64(m.small[0]) + to_i64(m.small[3]) + m.big[0] + m.big[2] + to_i64(m.tag) \
             + len(m.small) + len(m.big)\n",
        ),
        "mixed_narrow_wide_fields",
        // 1 + 4 + 100 + 300 + 9 + len 4 + len 3 = 421
        421,
    );
}

// -- Interop: addr_of + ptr_offset walk over the inline field ----------------

/// `addr_of(f.field[0])` names the inline field's first element, and a
/// `ptr_offset`/`ptr_read` walk traverses the packed-word storage in place —
/// matching the interpreters' `addr_of`/`ptr_offset` size law over the same array.
#[test]
fn addr_of_and_ptr_offset_walk_over_inline_field() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Buf\n",
            "    data array<i64, 4>\n\n",
            "fn main -> i64\n",
            "    let b = Buf(data: [5, 6, 7, 8])\n",
            "    let out i64 = 0\n",
            "    unsafe\n",
            "        let p ptr<i64> = addr_of(b.data[0])\n",
            "        let i i64 = 0\n",
            "        while i < 4\n",
            "            out = out + ptr_read(ptr_offset(p, i))\n",
            "            i = i + 1\n",
            "    out\n",
        ),
        "addr_of_walk_inline_field",
        // 5 + 6 + 7 + 8 = 26
        26,
    );
}

// -- Nested: a fixed array of scalar structs ---------------------------------

/// A struct holding a fixed array of scalar STRUCT elements (`array<P, 3>`): the
/// element is itself an inline aggregate, so `g.pts[i].x` indexes the inline array
/// then hops the element's field — the place machinery recurses through both.
#[test]
fn fixed_array_of_scalar_structs_inline() {
    assert_all_four_tiers_agree(
        concat!(
            "struct P\n",
            "    x i64\n",
            "    y i64\n\n",
            "struct Grid\n",
            "    pts array<P, 3>\n\n",
            "fn main -> i64\n",
            "    let g = Grid(pts: [P(x: 1, y: 2), P(x: 3, y: 4), P(x: 5, y: 6)])\n",
            "    g.pts[0].x + g.pts[1].y + g.pts[2].x + len(g.pts)\n",
        ),
        "fixed_array_of_structs",
        // 1 + 4 + 5 + len 3 = 13
        13,
    );
}

// -- Increment 4: whole-field ops (`for x in f.field`, `let c = f.field`) -----

/// `for x in f.field` iterates the inline field array directly. The desugar binds
/// the collection to a hidden `let __coll = f.pixels` (the whole-field by-value copy
/// this increment adds), then the numeric loop reads each element. Summed against a
/// scalar field so a dropped/duplicated element or a wrong stride diverges.
#[test]
fn for_x_in_inline_i64_field_reduces() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Frame\n",
            "    tag i64\n",
            "    pixels array<i64, 4>\n\n",
            "fn main -> i64\n",
            "    let f = Frame(tag: 10, pixels: [3, 5, 7, 9])\n",
            "    let total i64 = 0\n",
            "    for x in f.pixels\n",
            "        total = total + x\n",
            "    total + f.tag\n",
        ),
        "for_x_in_i64_field",
        // (3 + 5 + 7 + 9) + 10 = 34
        34,
    );
}

/// `for x in f.field` over a packed narrow (`u8`) field: each element is read at its
/// one-byte stride and ZERO-extended into an i64 cell. The `200` element is the
/// tooth — a signed byte read would yield `-56` and the sum would be wrong, so this
/// pins the unsigned narrow stride + zero-extension end to end.
#[test]
fn for_x_in_inline_narrow_u8_field_zero_extends() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Buf\n",
            "    bytes array<u8, 4>\n",
            "    n i64\n\n",
            "fn main -> i64\n",
            "    let b = Buf(bytes: [to_u8(200), to_u8(50), to_u8(7), to_u8(3)], n: 5)\n",
            "    let s i64 = 0\n",
            "    for x in b.bytes\n",
            "        s = s + to_i64(x)\n",
            "    s + b.n\n",
        ),
        "for_x_in_narrow_u8_field",
        // (200 + 50 + 7 + 3) + 5 = 265  (200 zero-extends; a signed read would give -56)
        265,
    );
}

/// `for x in f.field` over a packed narrow SIGNED (`i32`) field: each element is read
/// at its four-byte stride and SIGN-extended. The negative elements are the tooth — a
/// zero-extended read would turn `-5` into `4294967291` and the sum would explode.
#[test]
fn for_x_in_inline_narrow_i32_field_sign_extends() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Sig\n",
            "    vals array<i32, 4>\n\n",
            "fn main -> i64\n",
            "    let s = Sig(vals: [to_i32(0 - 5), to_i32(0 - 10), to_i32(100), to_i32(0 - 1)])\n",
            "    let acc i64 = 0\n",
            "    for x in s.vals\n",
            "        acc = acc + to_i64(x)\n",
            "    acc + 100\n",
        ),
        "for_x_in_narrow_i32_field",
        // (-5 - 10 + 100 - 1) + 100 = 184
        184,
    );
}

/// `let c = f.field` binds the whole inline field array to a fresh local, then reads
/// it every way (`for x in c`, `c[i]`, `len(c)`). A truncated/no-op copy would leave
/// `c` uninitialized and the reads would be garbage; only the exact element-wise copy
/// yields the expected total.
#[test]
fn let_binds_whole_inline_field_then_reads() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Frame\n",
            "    pixels array<i64, 4>\n",
            "    tag i64\n\n",
            "fn main -> i64\n",
            "    let f = Frame(pixels: [1, 2, 3, 4], tag: 100)\n",
            "    let c = f.pixels\n",
            "    let s i64 = 0\n",
            "    for x in c\n",
            "        s = s + x\n",
            "    s + c[0] + c[3] + len(c) + f.tag\n",
        ),
        "let_binds_whole_inline_field",
        // (1 + 2 + 3 + 4) + 1 + 4 + 4 + 100 = 119
        119,
    );
}

/// THE aliasing tooth for the whole-field copy: mutating the local copy `c` must NOT
/// touch the source `f.field`. It reads two witnesses — the ORIGINAL's untouched
/// elements (`f.pixels[0]`, `f.pixels[1]`) and the mutated copy's (`c[0]`, `c[1]`) —
/// so an aliasing copy that shared storage would make the original observe `c`'s
/// writes and the sum would jump from 293 to 477. Pinned hardest per the spec.
#[test]
fn mutating_whole_field_copy_leaves_source_unchanged() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Frame\n",
            "    pixels array<i64, 4>\n",
            "    tag i64\n\n",
            "fn main -> i64\n",
            "    let f = Frame(pixels: [1, 2, 3, 4], tag: 100)\n",
            "    let c = f.pixels\n",
            "    c[0] = 99\n",
            "    c[1] = 88\n",
            "    f.pixels[0] + f.pixels[1] + c[0] + c[1] + c[2] + f.tag\n",
        ),
        "mutating_whole_field_copy",
        // (1 + 2) + (99 + 88) + 3 + 100 = 293  (an aliasing copy would give 477)
        293,
    );
}

/// The whole-field copy of a packed NARROW field: `let c = b.bytes` duplicates the
/// eight packed bytes, then mutating `c[0]` must leave `b.bytes[0]` at its source
/// value. Combines the narrow packing/zero-extension tooth (the `200` element) with
/// the copy-isolation tooth (an aliasing copy would corrupt `b.bytes[0]` to 200 and
/// jump the total from 568 to 758).
#[test]
fn whole_field_copy_of_narrow_packed_field_is_isolated() {
    assert_all_four_tiers_agree(
        concat!(
            "struct Buf\n",
            "    bytes array<u8, 8>\n\n",
            "fn main -> i64\n",
            "    let b = Buf(bytes: [to_u8(10), to_u8(20), to_u8(30), to_u8(40), \
             to_u8(50), to_u8(60), to_u8(70), to_u8(80)])\n",
            "    let c = b.bytes\n",
            "    c[0] = to_u8(200)\n",
            "    let s i64 = 0\n",
            "    for x in c\n",
            "        s = s + to_i64(x)\n",
            "    s + to_i64(b.bytes[0]) + len(c)\n",
        ),
        "whole_field_copy_narrow",
        // (200 + 20 + 30 + 40 + 50 + 60 + 70 + 80) + 10 + 8 = 568  (alias -> 758)
        568,
    );
}

/// A whole-field copy of a NESTED inline field array (`array<array<i32, 4>, 4>`): the
/// element is itself an inline array, so the copy moves all sixteen packed i32 cells.
/// Mutating `c[0][0]` must leave `m.grid[0][0]` unchanged — an aliasing copy would
/// corrupt it and jump the total from 120 to 218.
#[test]
fn whole_field_copy_of_nested_array_field_is_isolated() {
    assert_all_four_tiers_agree(
        concat!(
            "struct M\n",
            "    grid array<array<i32, 4>, 4>\n\n",
            "fn main -> i64\n",
            "    let m = M(grid: [[to_i32(1), to_i32(2), to_i32(3), to_i32(4)], \
             [to_i32(5), to_i32(6), to_i32(7), to_i32(8)], \
             [to_i32(9), to_i32(10), to_i32(11), to_i32(12)], \
             [to_i32(13), to_i32(14), to_i32(15), to_i32(16)]])\n",
            "    let c = m.grid\n",
            "    c[0][0] = to_i32(99)\n",
            "    to_i64(c[0][0]) + to_i64(m.grid[0][0]) + to_i64(c[3][3]) + len(c)\n",
        ),
        "whole_field_copy_nested",
        // 99 + 1 + 16 + 4 = 120  (an aliasing copy would give 99 + 99 + 16 + 4 = 218)
        120,
    );
}

// -- The lower-vs-skip boundary (each skip proven a sound demotion) ----------

/// `for x in f.field` where the ELEMENT is itself an aggregate (a struct) still skips
/// cleanly: the per-element `let p = coll[idx]` is a dynamic-index AGGREGATE copy,
/// which the whole-aggregate copy path refuses (a runtime base is deferred). The
/// interpreters still answer correctly — a sound demotion, not a miscompile.
#[test]
fn for_x_in_field_of_struct_elements_skips_natively() {
    assert_native_skips_soundly(
        concat!(
            "struct P\n",
            "    x i64\n",
            "    y i64\n\n",
            "struct Grid\n",
            "    pts array<P, 3>\n\n",
            "fn main -> i64\n",
            "    let g = Grid(pts: [P(x: 1, y: 2), P(x: 3, y: 4), P(x: 5, y: 6)])\n",
            "    let total i64 = 0\n",
            "    for p in g.pts\n",
            "        total = total + p.x + p.y\n",
            "    total\n",
        ),
        "for_x_in_struct_element_field_skips",
        "initializer is not a native aggregate constructor",
        // 1 + 2 + 3 + 4 + 5 + 6 = 21
        21,
    );
}

/// A plain dynamic `array<T>` field (no extent) has no static length, so it lays out
/// no inline representation and the function DEMOTES cleanly to the interpreters. The
/// interpreters still answer correctly — a sound skip, not a miscompile.
#[test]
fn dynamic_array_field_skips_natively_but_runs_on_interpreters() {
    assert_native_skips_soundly(
        concat!(
            "struct Bag\n",
            "    items array<i64>\n\n",
            "fn main -> i64\n",
            "    let b = Bag(items: [1, 2, 3])\n",
            "    b.items[0] + len(b.items)\n",
        ),
        "dynamic_array_field_skips",
        "array length for `array<i64>` is unknown from its type",
        // 1 + len 3 = 4
        4,
    );
}

/// A fixed array of a HEAP-WORD element (`array<string, 2>`) is refused inline (a
/// by-value struct copy would share the string pointers rather than copy uniformly),
/// so the function demotes cleanly. The interpreters still answer correctly.
#[test]
fn string_element_fixed_array_field_skips_natively() {
    assert_native_skips_soundly(
        concat!(
            "struct Names\n",
            "    labels array<string, 2>\n\n",
            "fn main -> i64\n",
            "    let n = Names(labels: [\"hi\", \"yo\"])\n",
            "    len(n.labels[0]) + len(n.labels)\n",
        ),
        "string_element_fixed_array_skips",
        "heap-word element is deferred",
        // len("hi") 2 + len 2 = 4
        4,
    );
}
