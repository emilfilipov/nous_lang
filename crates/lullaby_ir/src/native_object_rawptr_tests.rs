//! Codegen tests for the freestanding-tier raw-pointer surface
//! (`native_object_rawptr.rs`): which shapes lower, which shapes skip cleanly,
//! the emitted instruction selection, the `volatile_*` non-elision guarantee, and
//! the register-promotion / address-taken hazard.
//!
//! These inspect the emitted `.text` bytes. The end-to-end "compile a real `.exe`
//! and check its exit code" proofs — including the headline
//! `ptr_write(addr_of(x), 5)` aliasing fixture — live in
//! `crates/lullaby_cli/tests/cli/suite15.rs`, which can actually run the binary.

use super::*;
use crate::{lower, lower_to_bytecode};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;

/// Compile source through the full frontend into a `BytecodeModule`.
fn module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// The emitted `.text` bytes of a program that must compile without skipping.
fn text_of(source: &str) -> Vec<u8> {
    let program = emit_native_program(&module_for(source)).expect("emit native program");
    assert!(
        program.skipped.is_empty(),
        "no function should be skipped: {:?}",
        program.skipped
    );
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32_at(&program.bytes, sec + 20) as usize;
    let text_size = read_u32_at(&program.bytes, sec + 16) as usize;
    program.bytes[text_offset..text_offset + text_size].to_vec()
}

/// Assert `main` does NOT compile natively — it must skip cleanly (`L0339`),
/// never be miscompiled. Optionally assert the skip reason mentions `reason`.
fn assert_main_skips_because(source: &str, reason: &str) {
    match emit_native_program(&module_for(source)) {
        Err(error) => {
            assert_eq!(error.code, "L0339", "a skip must carry L0339: {source}");
            let joined = format!("{:?}", error.skipped);
            assert!(
                joined.contains(reason),
                "skip reason should mention `{reason}`: {joined}"
            );
        }
        Ok(program) => {
            assert!(
                !program.compiled.contains(&"main".to_string()),
                "main must NOT compile for this shape: {source}\ncompiled={:?}",
                program.compiled
            );
            let joined = format!("{:?}", program.skipped);
            assert!(
                joined.contains(reason),
                "skip reason should mention `{reason}`: {joined}"
            );
        }
    }
}

fn contains(text: &[u8], needle: &[u8]) -> bool {
    text.windows(needle.len()).any(|w| w == needle)
}

fn count_of(text: &[u8], needle: &[u8]) -> usize {
    text.windows(needle.len()).filter(|w| *w == needle).count()
}

// -- What lowers --------------------------------------------------------------

/// `addr_of(x)` of a scalar local is a real `lea rax, [rbp - slot]` — the
/// instruction that makes native aliasing genuine. `ptr_write` through it is a
/// full-width `mov [rcx], rax`.
#[test]
fn addr_of_local_emits_lea_and_ptr_write_emits_store() {
    let text = text_of(concat!(
        "fn main -> i64\n",
        "    let x i64 = 1\n",
        "    unsafe\n",
        "        let p ptr<i64> = addr_of(x)\n",
        "        ptr_write(p, 5)\n",
        "    x\n",
    ));
    // lea rax, [rbp + disp32] -> 48 8D 85 <disp32>
    assert!(
        contains(&text, &[0x48, 0x8D, 0x85]),
        "`addr_of` must emit a real `lea rax, [rbp - slot]`"
    );
    // mov [rcx], rax -> 48 89 01
    assert!(
        contains(&text, &[0x48, 0x89, 0x01]),
        "`ptr_write` of an i64 must emit `mov [rcx], rax`"
    );
}

/// `ptr_read` selects a width- and signedness-correct load per pointee, always
/// normalizing back into the 8-byte cell the rest of the backend expects.
#[test]
fn ptr_read_selects_width_correct_loads() {
    for (pointee, opcode) in [
        ("i64", &[0x48, 0x8B, 0x01][..]),       // mov rax, [rcx]
        ("i32", &[0x48, 0x63, 0x01][..]),       // movsxd rax, dword [rcx]
        ("u32", &[0x8B, 0x01][..]),             // mov eax, [rcx] (zero-extends)
        ("i16", &[0x48, 0x0F, 0xBF, 0x01][..]), // movsx rax, word [rcx]
        ("u16", &[0x48, 0x0F, 0xB7, 0x01][..]), // movzx rax, word [rcx]
        ("i8", &[0x48, 0x0F, 0xBE, 0x01][..]),  // movsx rax, byte [rcx]
        ("u8", &[0x48, 0x0F, 0xB6, 0x01][..]),  // movzx rax, byte [rcx]
    ] {
        // An `i64` read is already the return type; every other pointee is a
        // fixed-width cell that widens back to `i64`.
        let tail = if pointee == "i64" {
            "        ptr_read(p)\n".to_string()
        } else {
            "        to_i64(ptr_read(p))\n".to_string()
        };
        let src = format!(
            "fn main -> i64\n    unsafe\n        let p ptr<{pointee}> = int_to_ptr(4096)\n{tail}"
        );
        let text = text_of(&src);
        assert!(
            contains(&text, opcode),
            "`ptr_read` through `ptr<{pointee}>` should emit {opcode:02X?}"
        );
    }
}

/// `ptr_write` stores exactly the pointee's width — never a wider store that
/// would clobber neighbouring bytes of a device register or packed structure.
#[test]
fn ptr_write_selects_width_correct_stores() {
    for (pointee, opcode) in [
        ("i64", &[0x48, 0x89, 0x01][..]), // mov [rcx], rax
        ("i32", &[0x89, 0x01][..]),       // mov [rcx], eax
        ("i16", &[0x66, 0x89, 0x01][..]), // mov [rcx], ax
        ("u8", &[0x88, 0x01][..]),        // mov [rcx], al
    ] {
        // An `i64` value is written directly; a fixed-width pointee needs its
        // value narrowed to the pointee's own cell type first.
        let value = if pointee == "i64" {
            "1".to_string()
        } else {
            format!("to_{pointee}(1)")
        };
        let src = format!(
            "fn main -> i64\n    unsafe\n        let p ptr<{pointee}> = int_to_ptr(4096)\n        ptr_write(p, {value})\n    0\n"
        );
        let text = text_of(&src);
        assert!(
            contains(&text, opcode),
            "`ptr_write` through `ptr<{pointee}>` should emit {opcode:02X?}"
        );
    }
}

/// `ptr_offset(p, n)` is a single scaled `lea rax, [rcx + rax*stride]` — the
/// x86 SIB scale encodes the pointee size directly for 1/2/4/8.
#[test]
fn ptr_offset_emits_scaled_lea_per_pointee_size() {
    for (pointee, sib) in [("u8", 0x01u8), ("i16", 0x41), ("i32", 0x81), ("i64", 0xC1)] {
        let src = format!(
            "fn main -> i64\n    unsafe\n        let p ptr<{pointee}> = int_to_ptr(4096)\n        ptr_to_int(ptr_offset(p, 3))\n"
        );
        let text = text_of(&src);
        assert!(
            contains(&text, &[0x48, 0x8D, 0x04, sib]),
            "`ptr_offset` over `ptr<{pointee}>` should emit `lea rax, [rcx + rax*N]` (SIB {sib:#04X})"
        );
    }
}

/// `int_to_ptr` / `ptr_to_int` / `ptr_cast` are value-neutral: each is the
/// identity on the operand's register word, so a chain of them emits no extra
/// instruction beyond materializing the operand once.
#[test]
fn pointer_identity_ops_emit_no_extra_work() {
    // Both bodies contain a raw-pointer call (so both are equally excluded from
    // register promotion — otherwise the comparison would measure promotion, not
    // the identity ops). The only difference is two extra `ptr_cast` hops, which
    // must cost exactly zero bytes.
    let one_hop = text_of(concat!(
        "fn main -> i64\n",
        "    let n i64 = 4096\n",
        "    unsafe\n",
        "        ptr_to_int(int_to_ptr(n))\n",
    ));
    let three_hops = text_of(concat!(
        "fn main -> i64\n",
        "    let n i64 = 4096\n",
        "    unsafe\n",
        "        ptr_to_int(ptr_cast(ptr_cast(int_to_ptr(n))))\n",
    ));
    assert_eq!(
        one_hop.len(),
        three_hops.len(),
        "int_to_ptr/ptr_to_int/ptr_cast must be pure no-ops at machine level"
    );
}

/// A `ptr<T>` crosses a call boundary as an ordinary pointer-sized GPR scalar,
/// as a parameter and as a return value.
#[test]
fn pointer_values_cross_the_call_abi() {
    let program = emit_native_program(&module_for(concat!(
        "fn advance p ptr<i64> n i64 -> ptr<i64>\n",
        "    unsafe\n",
        "        ptr_offset(p, n)\n",
        "fn main -> i64\n",
        "    let x i64 = 42\n",
        "    unsafe\n",
        "        let p ptr<i64> = advance(addr_of(x), 0)\n",
        "        ptr_read(p)\n",
    )))
    .expect("emit native program");
    assert!(
        program.skipped.is_empty(),
        "a ptr<T> param/return must be native-eligible: {:?}",
        program.skipped
    );
    assert!(program.compiled.contains(&"advance".to_string()));
    assert!(program.compiled.contains(&"main".to_string()));
}

// -- volatile non-elision -----------------------------------------------------

/// Three `volatile_load`s of the same pointer emit three real loads. Nothing in
/// the pipeline (CSE / LICM / copy-prop / DCE / the inliner / this module) may
/// collapse them — that is the MMIO correctness requirement.
#[test]
fn repeated_volatile_load_emits_one_load_each() {
    let text = text_of(concat!(
        "fn main -> i64\n",
        "    let cell i64 = 1\n",
        "    unsafe\n",
        "        let p ptr<i64> = addr_of(cell)\n",
        "        volatile_load(p) + volatile_load(p) + volatile_load(p)\n",
    ));
    // Each volatile_load lowers to `mov rcx, rax` (48 89 C1) + `mov rax, [rcx]`
    // (48 8B 01). The frame word is also read by other paths, so assert on the
    // load-through-rcx pair count, which only this module emits for a read.
    assert_eq!(
        count_of(&text, &[0x48, 0x89, 0xC1, 0x48, 0x8B, 0x01]),
        3,
        "each `volatile_load` must emit its own load; none may be CSE'd away"
    );
}

/// A `volatile_load` inside a loop is re-loaded every iteration — never hoisted
/// out by LICM. The load must remain *inside* the backward-jump region.
#[test]
fn volatile_load_in_loop_is_not_hoisted() {
    let text = text_of(concat!(
        "fn main -> i64\n",
        "    let cell i64 = 1\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    unsafe\n",
        "        let p ptr<i64> = addr_of(cell)\n",
        "        while i < 4\n",
        "            acc = acc + volatile_load(p)\n",
        "            volatile_store(p, volatile_load(p) + 10)\n",
        "            i = i + 1\n",
        "    acc\n",
    ));
    // Two volatile reads per iteration, emitted once each in the loop body.
    assert_eq!(
        count_of(&text, &[0x48, 0x89, 0xC1, 0x48, 0x8B, 0x01]),
        2,
        "the loop's volatile reads must be emitted in the body, not hoisted/duplicated"
    );
    // And the loop is still a real loop (a backward jmp survives) — the volatile
    // accesses must not have let a reduction recognizer close-form it away.
    assert!(
        text.windows(5)
            .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0),
        "the loop must remain a real loop around the volatile accesses"
    );
}

/// A volatile access must not be folded into a reduction closed form. `acc +=
/// volatile_load(p)` looks superficially like the counting-sum/affine reduction
/// shape the backend closed-forms, but the reduction detectors only match
/// `Integer`/`Variable`/`Binary`/`Index` operands — never a `Call` — so the loop
/// survives. Pinned so a future reduction-matcher widening cannot silently eat a
/// volatile read.
#[test]
fn volatile_reduction_is_not_closed_formed() {
    let text = text_of(concat!(
        "fn main -> i64\n",
        "    let cell i64 = 2\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    unsafe\n",
        "        let p ptr<i64> = addr_of(cell)\n",
        "        while i < 1000\n",
        "            acc = acc + volatile_load(p)\n",
        "            i = i + 1\n",
        "    acc\n",
    ));
    assert!(
        text.windows(5)
            .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0),
        "a loop containing a volatile read must NOT be closed-formed away"
    );
}

// -- The register-promotion / address-taken hazard ----------------------------

/// THE hazard. `acc`/`i` are exactly the hot, purely-`i64` loop locals the
/// register-promotion pass targets — it would keep them in the callee-saved
/// `rbx`/`rsi`, where they have NO address. Taking `addr_of(acc)` and writing
/// through it would then `lea` a dead frame slot and the store would silently
/// vanish. The address-taken gate in `plan_register_promotion` must keep every
/// local in its frame slot for such a function.
///
/// Proven structurally here (no `rbx`/`rsi` seating) and behaviourally by
/// `native_addr_of_defeats_register_promotion` in `suite15.rs`, which runs the
/// binary and checks the store actually landed.
#[test]
fn addr_of_defeats_register_promotion() {
    let promotable = concat!(
        "fn main -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    while i < 10\n",
        "        acc = acc + i * i\n",
        "        i = i + 1\n",
        "    acc\n",
    );
    let address_taken = concat!(
        "fn main -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    while i < 10\n",
        "        acc = acc + i * i\n",
        "        i = i + 1\n",
        "    unsafe\n",
        "        let p ptr<i64> = addr_of(acc)\n",
        "        ptr_write(p, ptr_read(p) + 100)\n",
        "    acc\n",
    );
    // `mov rbx, rax` (48 89 C3) / `mov rsi, rax` (48 89 C6) seat a promoted local.
    // The control body promotes; the address-taken body must not.
    let control = text_of(promotable);
    assert!(
        contains(&control, &[0x48, 0x89, 0xC3]) || contains(&control, &[0x48, 0x89, 0xC6]),
        "control: this shape is expected to register-promote (otherwise the test proves nothing)"
    );
    let taken = text_of(address_taken);
    assert!(
        !contains(&taken, &[0x48, 0x89, 0xC3]) && !contains(&taken, &[0x48, 0x89, 0xC6]),
        "a function that takes an address must NOT register-promote: an address-taken local \
         must live in its frame slot"
    );
    assert!(
        contains(&taken, &[0x48, 0x8D, 0x85]),
        "the address-taken function must still emit its `lea`"
    );
}

/// The address-taken scan reaches an `addr_of` nested anywhere in the body, not
/// just at statement top level.
#[test]
fn address_taken_scan_sees_nested_addr_of() {
    let nested = concat!(
        "fn main -> i64\n",
        "    let x i64 = 1\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    while i < 10\n",
        "        if i > 5\n",
        "            unsafe\n",
        "                acc = acc + ptr_to_int(addr_of(x)) - ptr_to_int(addr_of(x))\n",
        "        i = i + 1\n",
        "    acc\n",
    );
    let text = text_of(nested);
    assert!(
        !contains(&text, &[0x48, 0x89, 0xC3]) && !contains(&text, &[0x48, 0x89, 0xC6]),
        "an `addr_of` nested inside a loop/if must still defeat register promotion"
    );
}

// -- Default-deny: what must skip cleanly -------------------------------------

/// `addr_of` of an ARRAY ELEMENT skips. The native frame lays an aggregate's
/// words out at DESCENDING addresses (word `k` at `[rbp - (slot + 8k)]`), so a
/// pointer into an array would walk BACKWARDS under `ptr_offset` — disagreeing
/// with C, with `size_of`/`offset_of`, and with the interpreters' ascending
/// snapshot model, on a program the interpreters define. Refused, not guessed.
#[test]
fn addr_of_array_element_skips_cleanly() {
    assert_main_skips_because(
        concat!(
            "fn main -> i64\n",
            "    let a array<i64> = [10, 20, 30]\n",
            "    unsafe\n",
            "        let p ptr<i64> = addr_of(a[0])\n",
            "        ptr_read(ptr_offset(p, 1))\n",
        ),
        "DESCENDING",
    );
}

/// A whole-array `addr_of` (which decays to `ptr<element>`) skips for the same
/// descending-layout reason: the decayed pointer would not be walkable.
#[test]
fn addr_of_whole_array_skips_cleanly() {
    assert_main_skips_because(
        concat!(
            "fn main -> i64\n",
            "    let a array<i64> = [10, 20, 30]\n",
            "    unsafe\n",
            "        let p ptr<i64> = addr_of(a)\n",
            "        ptr_read(p)\n",
        ),
        "addr_of",
    );
}

/// `addr_of` of a NARROW scalar skips: an `i32` local is stored as a normalized
/// 8-byte cell, so a width-correct 4-byte store through its address would leave
/// the cell's upper half stale and corrupt every later read of it.
#[test]
fn addr_of_narrow_scalar_skips_cleanly() {
    assert_main_skips_because(
        concat!(
            "fn main -> i64\n",
            "    let x i32 = to_i32(5)\n",
            "    unsafe\n",
            "        let p ptr<i32> = addr_of(x)\n",
            "        to_i64(ptr_read(p))\n",
        ),
        "8-byte scalar",
    );
}

/// A float pointee skips: `ptr_read` would have to deliver its result in an XMM
/// register, which this integer-`rax` path cannot do. A clean skip, not an
/// approximation.
#[test]
fn float_pointee_skips_cleanly() {
    assert_main_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p ptr<f64> = int_to_ptr(4096)\n",
            "        let v f64 = ptr_read(p)\n",
            "        if v > 0.0\n",
            "            1\n",
            "        else\n",
            "            0\n",
        ),
        "float call",
    );
}

/// `ptr_offset` over a pointee whose C size this backend does not know exactly
/// (here a struct) skips rather than guessing a stride.
#[test]
fn ptr_offset_over_unsupported_pointee_skips_cleanly() {
    assert_main_skips_because(
        concat!(
            "struct Pair\n",
            "    lo i64\n",
            "    hi i64\n",
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p ptr<Pair> = int_to_ptr(4096)\n",
            "        ptr_to_int(ptr_offset(p, 1))\n",
        ),
        "not lowered natively",
    );
}

// -- Struct-field addressing --------------------------------------------------

/// `addr_of(s.f)` of an 8-byte field IS lowered: the address is genuine and a
/// read/write through it aliases the field exactly. (Only a *walk* across fields
/// would hit the descending-layout mismatch, and that is undefined on the
/// interpreters too — see the module docs.)
#[test]
fn addr_of_struct_field_lowers() {
    let text = text_of(concat!(
        "struct Pair\n",
        "    lo i64\n",
        "    hi i64\n",
        "fn main -> i64\n",
        "    let pair Pair = Pair(7, 9)\n",
        "    unsafe\n",
        "        let hp ptr<i64> = addr_of(pair.hi)\n",
        "        ptr_write(hp, 33)\n",
        "    pair.hi\n",
    ));
    assert!(
        contains(&text, &[0x48, 0x8D, 0x85]),
        "`addr_of(s.f)` must emit a real `lea` of the field's frame word"
    );
    assert!(
        contains(&text, &[0x48, 0x89, 0x01]),
        "the store through the field pointer must be a real `mov [rcx], rax`"
    );
}
