//! Codegen tests for the freestanding **port-mapped I/O** surface — `port_in8` /
//! `port_in16` / `port_in32` and `port_out8` / `port_out16` / `port_out32`
//! (`documents/freestanding_tier_design.md` §4).
//!
//! # Why these tests assert BYTES rather than behavior
//!
//! Every other native surface is ultimately proven by running a real `.exe` and
//! checking its exit code. **Port I/O cannot be.** `in`/`out` are privileged
//! instructions: executing one at CPL 3 raises a general-protection fault unless
//! IOPL or the TSS I/O-permission bitmap grants access, and even with access
//! there is no device behind an arbitrary port in a test harness. Running one
//! would crash the test process, not verify it.
//!
//! So the emitted bytes ARE the correctness evidence here, and they are asserted
//! exhaustively: both port forms (`imm8` and `DX`) × all three widths × read and
//! write = twelve encodings, plus the operand-size prefix and the cell
//! renormalization. `suite15.rs` separately proves a real freestanding `.exe`
//! *emits* (compile-only, never run).
//!
//! # The encodings under test
//!
//! | width | `in` imm8 | `in dx` | `out` imm8 | `out dx` |
//! | :-- | :-- | :-- | :-- | :-- |
//! |  8 | `E4 ib`      | `EC`      | `E6 ib`      | `EE`      |
//! | 16 | `66 E5 ib`   | `66 ED`   | `66 E7 ib`   | `66 EF`   |
//! | 32 | `E5 ib`      | `ED`      | `E7 ib`      | `EF`      |

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

/// Whether `haystack` contains `needle` as a contiguous byte run.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// Wrap a `no-runtime` port body in a program whose `main` calls it, so the
/// helper is reachable and compiled. `main` never runs in any test here.
fn port_program(helper: &str, call: &str) -> String {
    format!("no-runtime\n{helper}\nfn main -> i64\n{call}\n    0\n")
}

// -- The `imm8` port form (a constant port < 256) ----------------------------

/// `port_out8(0x20u16, 0x20u8)` — the PIC EOI, THE canonical low-port write.
/// A constant port below 256 takes the one-byte-shorter immediate form
/// `out imm8, al` (`E6 ib`), with no `DX` setup at all.
#[test]
fn port_out8_immediate_emits_e6_with_the_port_byte() {
    let text = text_of(&port_program(
        "fn eoi\n    unsafe\n        port_out8(0x20u16, 0x20u8)\n",
        "    eoi()",
    ));
    assert!(
        contains(&text, &[0xE6, 0x20]),
        "port_out8(0x20u16, ..) must emit `out 0x20, al` = E6 20; text={text:02X?}"
    );
    // The immediate form must NOT set up DX — there is no `mov edx, e*x`.
    assert!(
        !contains(&text, &[0x89, 0xC2]) && !contains(&text, &[0x89, 0xCA]),
        "the immediate port form must not emit a `mov edx, ..` DX setup"
    );
}

/// `port_in8(0x21u16)` — `in al, 0x21` (`E4 ib`), then the cell renormalization.
#[test]
fn port_in8_immediate_emits_e4_then_zero_extends() {
    let text = text_of(&port_program(
        "fn mask -> u8\n    unsafe\n        port_in8(0x21u16)\n",
        "    let m u8 = mask()",
    ));
    // `in al, 0x21` immediately followed by `movzx rax, al` (48 0F B6 C0).
    assert!(
        contains(&text, &[0xE4, 0x21, 0x48, 0x0F, 0xB6, 0xC0]),
        "port_in8(0x21u16) must emit `in al, 0x21` = E4 21 then movzx rax, al; text={text:02X?}"
    );
}

/// The 16-bit immediate form needs the `0x66` operand-size prefix *before* the
/// opcode: `66 E5 ib`. Without it this would decode as a 32-bit `in eax, imm8`
/// and read four bytes from the port instead of two — a silent device
/// miscompile, which is exactly why this is pinned.
#[test]
fn port_in16_immediate_emits_the_66_prefix_before_e5() {
    let text = text_of(&port_program(
        "fn kbd -> u16\n    unsafe\n        port_in16(0x60u16)\n",
        "    let w u16 = kbd()",
    ));
    assert!(
        contains(&text, &[0x66, 0xE5, 0x60, 0x48, 0x0F, 0xB7, 0xC0]),
        "port_in16(0x60u16) must emit `in ax, 0x60` = 66 E5 60 then movzx rax, ax; \
         text={text:02X?}"
    );
}

/// The 32-bit immediate form takes NO prefix — 32 is the default operand size in
/// 64-bit mode — so it is a bare `E7 ib`.
#[test]
fn port_out32_immediate_emits_e7_with_no_prefix() {
    let text = text_of(&port_program(
        "fn w value u32\n    unsafe\n        port_out32(0x70u16, value)\n",
        "    w(0u32)",
    ));
    assert!(
        contains(&text, &[0xE7, 0x70]),
        "port_out32(0x70u16, v) must emit `out 0x70, eax` = E7 70; text={text:02X?}"
    );
    assert!(
        !contains(&text, &[0x66, 0xE7, 0x70]),
        "the 32-bit port form must NOT carry the 0x66 operand-size prefix"
    );
}

/// A constant port of exactly 255 still fits `imm8`; 256 does not. This pins the
/// boundary of the immediate form — an off-by-one here would either truncate a
/// real port number or needlessly fall back to `DX`.
#[test]
fn the_immediate_port_boundary_is_255() {
    let at_255 = text_of(&port_program(
        "fn r -> u8\n    unsafe\n        port_in8(255u16)\n",
        "    let v u8 = r()",
    ));
    assert!(
        contains(&at_255, &[0xE4, 0xFF]),
        "port 255 must still use the immediate form `in al, 0xFF`"
    );

    let at_256 = text_of(&port_program(
        "fn r -> u8\n    unsafe\n        port_in8(256u16)\n",
        "    let v u8 = r()",
    ));
    assert!(
        contains(&at_256, &[0x89, 0xC2, 0xEC]),
        "port 256 exceeds imm8 and must fall back to the DX form (mov edx, eax; in al, dx); \
         text={at_256:02X?}"
    );
    assert!(
        !contains(&at_256, &[0xE4]),
        "port 256 must not emit the immediate `in al, imm8` opcode"
    );
}

// -- The `DX` port form (a port above 0xFF, or a runtime port) ---------------

/// COM1 at `0x3F8` is above `0xFF`, so the port must be loaded into `DX`:
/// `mov edx, eax` (89 C2) then `out dx, al` (EE).
#[test]
fn port_out8_above_255_uses_the_dx_form() {
    let text = text_of(&port_program(
        "fn ser b u8\n    unsafe\n        port_out8(0x3F8u16, b)\n",
        "    ser(65u8)",
    ));
    assert!(
        contains(&text, &[0x89, 0xCA, 0xEE]),
        "port_out8(0x3F8u16, b) must emit `mov edx, ecx` then `out dx, al` = 89 CA EE; \
         text={text:02X?}"
    );
}

/// A runtime (non-constant) port always uses the `DX` form: `in al, dx` (EC).
#[test]
fn port_in8_with_a_runtime_port_uses_the_dx_form() {
    let text = text_of(&port_program(
        "fn lsr p u16 -> u8\n    unsafe\n        port_in8(p)\n",
        "    let s u8 = lsr(1016u16)",
    ));
    assert!(
        contains(&text, &[0x89, 0xC2, 0xEC, 0x48, 0x0F, 0xB6, 0xC0]),
        "a runtime port must emit `mov edx, eax; in al, dx; movzx rax, al` = 89 C2 EC 48 0F B6 C0; \
         text={text:02X?}"
    );
}

/// The 16-bit `DX` read: `66 ED`, then `movzx rax, ax`.
#[test]
fn port_in16_dx_form_emits_66_ed_then_zero_extends() {
    let text = text_of(&port_program(
        "fn r p u16 -> u16\n    unsafe\n        port_in16(p)\n",
        "    let v u16 = r(1016u16)",
    ));
    assert!(
        contains(&text, &[0x89, 0xC2, 0x66, 0xED, 0x48, 0x0F, 0xB7, 0xC0]),
        "port_in16(p) must emit `mov edx, eax; in ax, dx; movzx rax, ax`; text={text:02X?}"
    );
}

/// The 32-bit `DX` read: a bare `ED` (no prefix), then the `u32` normalization
/// (`mov eax, eax`, 89 C0).
#[test]
fn port_in32_dx_form_emits_ed_with_no_prefix() {
    let text = text_of(&port_program(
        "fn r p u16 -> u32\n    unsafe\n        port_in32(p)\n",
        "    let v u32 = r(1016u16)",
    ));
    assert!(
        contains(&text, &[0x89, 0xC2, 0xED, 0x89, 0xC0]),
        "port_in32(p) must emit `mov edx, eax; in eax, dx; mov eax, eax`; text={text:02X?}"
    );
    assert!(
        !contains(&text, &[0x66, 0xED]),
        "the 32-bit `in eax, dx` must NOT carry the 0x66 prefix"
    );
}

/// The 16-bit `DX` write: `66 EF`.
#[test]
fn port_out16_dx_form_emits_66_ef() {
    let text = text_of(&port_program(
        "fn w p u16 v u16\n    unsafe\n        port_out16(p, v)\n",
        "    w(1016u16, 7u16)",
    ));
    assert!(
        contains(&text, &[0x89, 0xCA, 0x66, 0xEF]),
        "port_out16(p, v) must emit `mov edx, ecx; out dx, ax` = 89 CA 66 EF; text={text:02X?}"
    );
}

/// The 32-bit `DX` write: a bare `EF`.
#[test]
fn port_out32_dx_form_emits_ef_with_no_prefix() {
    let text = text_of(&port_program(
        "fn w p u16 v u32\n    unsafe\n        port_out32(p, v)\n",
        "    w(1016u16, 7u32)",
    ));
    assert!(
        contains(&text, &[0x89, 0xCA, 0xEF]),
        "port_out32(p, v) must emit `mov edx, ecx; out dx, eax` = 89 CA EF; text={text:02X?}"
    );
    assert!(
        !contains(&text, &[0x66, 0xEF]),
        "the 32-bit `out dx, eax` must NOT carry the 0x66 prefix"
    );
}

// -- Operand staging ---------------------------------------------------------

/// The `DX` write form must stage its two operands like `ptr_write` does: the
/// port is evaluated FIRST and spilled (`push rax` = 50), then the value is
/// evaluated (landing in `rax`/`AL`), then the port is popped into `rcx` (59) and
/// moved to `DX`.
///
/// This ordering is what makes a CALL inside either operand safe. Had the port
/// been left in a register across the value's evaluation, any call in the value
/// expression would clobber it and the write would go to the wrong port — a
/// silent device miscompile.
#[test]
fn the_dx_write_form_spills_the_port_across_the_value_evaluation() {
    let text = text_of(&port_program(
        concat!(
            "fn pick -> u8\n    42u8\n",
            "fn w p u16\n    unsafe\n        port_out8(p, pick())\n",
        ),
        "    w(1016u16)",
    ));
    // push rax (50) ... pop rcx (59), mov edx, ecx (89 CA), out dx, al (EE).
    assert!(
        contains(&text, &[0x59, 0x89, 0xCA, 0xEE]),
        "the port must be popped into rcx and moved to edx right before `out dx, al`; \
         text={text:02X?}"
    );
    assert!(
        contains(&text, &[0x50]),
        "the port must be spilled with `push rax` across the value evaluation"
    );
}

/// A port read must renormalize into the 8-byte cell EVERY time — `in` writes
/// only `AL`/`AX`/`EAX` and leaves the rest of `RAX` stale. Two reads in one
/// body must produce two normalizations, never one hoisted or shared.
#[test]
fn every_port_read_renormalizes_its_own_cell() {
    let text = text_of(&port_program(
        "fn two -> u8\n    unsafe\n        let a u8 = port_in8(0x21u16)\n        \
         let b u8 = port_in8(0x21u16)\n        a\n",
        "    let v u8 = two()",
    ));
    assert_eq!(
        count_occurrences(&text, &[0xE4, 0x21, 0x48, 0x0F, 0xB6, 0xC0]),
        2,
        "each of the two `port_in8` reads must emit its own `in`+`movzx`; text={text:02X?}"
    );
}

// -- Composition -------------------------------------------------------------

/// A port builtin must be dispatched as a BUILTIN, never mistaken for a user
/// function — and a void `port_out*` composes with the void-function surface, so
/// a `fn ... ` wrapper with no return type still compiles.
#[test]
fn a_void_port_out_wrapper_compiles() {
    let program = emit_native_program(&module_for(&port_program(
        "fn eoi\n    unsafe\n        port_out8(0x20u16, 0x20u8)\n",
        "    eoi()",
    )))
    .expect("emit");
    assert!(
        program.compiled.contains(&"eoi".to_string()),
        "a void port_out wrapper must compile: {:?}",
        program.skipped
    );
    assert!(
        program.compiled.contains(&"main".to_string()),
        "its caller must not be demoted: {:?}",
        program.skipped
    );
}

/// The whole §4 serial-driver idiom — poll the line-status register, then write
/// the data register — compiles as one function.
#[test]
fn the_serial_driver_idiom_compiles() {
    let text = text_of(&port_program(
        concat!(
            "fn serial_put b u8\n",
            "    unsafe\n",
            "        let status u8 = port_in8(0x3FDu16)\n",
            "        if (to_i64(status) & 0x20) != 0\n",
            "            port_out8(0x3F8u16, b)\n",
        ),
        "    serial_put(65u8)",
    ));
    // The read is a DX-form `in al, dx`; the write is a DX-form `out dx, al`.
    assert!(
        contains(&text, &[0xEC]) && contains(&text, &[0xEE]),
        "the serial idiom must emit both `in al, dx` (EC) and `out dx, al` (EE); text={text:02X?}"
    );
}
