//! Native backend: the freestanding-tier **port-mapped I/O** surface —
//! `port_in8` / `port_in16` / `port_in32` and `port_out8` / `port_out16` /
//! `port_out32`. A sibling of `native_object_rawptr.rs` (which owns the
//! raw-pointer surface); sees the parent's items via `use super::*`. See
//! `documents/freestanding_tier_design.md` §4.
//!
//! # Why this cannot be synthesized from pointers
//!
//! MMIO composes out of the delivered surface — `int_to_ptr` + `ptr_offset` +
//! `volatile_store` already write the VGA text buffer natively, no new intrinsic
//! needed, because a device register mapped into the address space *is* memory.
//! Port I/O is different in kind: the x86 I/O port space is a **separate address
//! space** reachable only by the `in`/`out` instructions. No sequence of loads
//! and stores can reach it, so these six builtins lower to real `in`/`out`.
//!
//! # The instruction encodings
//!
//! `in`/`out` are fixed-operand instructions: the data always lives in
//! `AL`/`AX`/`EAX` (never another register), and the port is either an **8-bit
//! immediate** (ports `0..=255` only) or the **`DX` register** (the full 16-bit
//! port space). Both forms are emitted — the immediate form is one byte shorter
//! and needs no `DX` setup, and the low ports it covers are exactly the legacy
//! devices a kernel drives first (PIC at `0x20`/`0xA0`, PIT at `0x40`).
//!
//! Operand size selects the opcode, with the `0x66` prefix for the 16-bit forms
//! (16-bit operand size is *not* the default in 64-bit mode; 32-bit is):
//!
//! | width | `in` imm8 | `in dx` | `out` imm8 | `out dx` |
//! | :-- | :-- | :-- | :-- | :-- |
//! |  8 | `E4 ib`      | `EC`      | `E6 ib`      | `EE`      |
//! | 16 | `66 E5 ib`   | `66 ED`   | `66 E7 ib`   | `66 EF`   |
//! | 32 | `E5 ib`      | `ED`      | `E7 ib`      | `EF`      |
//!
//! There is no REX-prefixed 64-bit port I/O: the architecture caps port access at
//! 32 bits, which is why the surface stops at `port_in32`/`port_out32`.
//!
//! # Renormalizing an `in` result into the 8-byte cell
//!
//! Every Lullaby scalar is a normalized 8-byte cell. `in al, dx` writes **only**
//! `AL` and leaves `RAX[63:8]` holding whatever was there before — a stale cell
//! that would corrupt every downstream use. So each read is followed by
//! [`emit_normalize_rax`], the *same* helper every other narrow-width native path
//! uses, with the **unsigned** `IntKind` (`U8`/`U16`/`U32`): a port read is a raw
//! device byte/word/dword with no sign to extend, so it zero-extends. That reuse
//! is deliberate — the `in` result lands in a cell indistinguishable from one
//! produced by `to_u8`/`to_u16`/`to_u32`.
//!
//! (For the 32-bit case `emit_normalize_rax` emits `mov eax, eax`, which is
//! architecturally redundant — `in eax, dx` already zeroes `RAX[63:32]`, since any
//! 32-bit GPR write does. It is kept rather than special-cased so this path stays
//! byte-identical to every other `u32` normalization in the backend.)
//!
//! # Writing a port: the low bits are already the value
//!
//! `out` reads `AL`/`AX`/`EAX` — the low 1/2/4 bytes of the value's cell — and
//! `DX` — the low 2 bytes of the port's cell. Both are correct **regardless of
//! whether the cell above those bits is masked**, because the instruction simply
//! ignores the upper bits. So no pre-masking is emitted: `mov edx, eax` puts the
//! port's low 16 bits in `DX` by construction.
//!
//! # Other targets
//!
//! Port I/O is x86-only. The AArch64 backend and the WASM backend do not know
//! these names, so a function using one fails their `callable` lookup and **skips
//! cleanly** (`L0339` natively, an unsupported-builtin error on WASM) — neither
//! needed a change. Such a program then falls back to an interpreter, which
//! refuses it with `L0444`. That chain is honest end to end: no target ever
//! fabricates a port value.
//!
//! # Not executable at CPL 3
//!
//! These instructions **fault** (general-protection) in a user-mode process
//! unless IOPL or the TSS I/O-permission bitmap grants access, and there is no
//! device behind the port in a test harness regardless. So the emitted code is
//! verified by **asserting the emitted bytes** (`native_object_portio_tests.rs`)
//! and by compiling real executables (`suite15.rs`) — never by running them.

use super::*;

/// The operand width of a port-I/O builtin. x86 port access is 8/16/32-bit only;
/// there is no 64-bit `in`/`out`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PortWidth {
    Bits8,
    Bits16,
    Bits32,
}

impl PortWidth {
    /// The `IntKind` an `in` result normalizes to. Always **unsigned**: a port
    /// read is a raw device value with no sign bit to extend.
    fn int_kind(self) -> IntKind {
        match self {
            Self::Bits8 => IntKind::U8,
            Self::Bits16 => IntKind::U16,
            Self::Bits32 => IntKind::U32,
        }
    }

    /// Whether this width needs the `0x66` operand-size prefix. In 64-bit mode
    /// the default operand size is 32, so only the 16-bit forms are prefixed.
    fn needs_operand_size_prefix(self) -> bool {
        self == Self::Bits16
    }
}

/// Whether `name` is a port-I/O builtin, and if so whether it *reads* a port
/// (`in`) and at what width. `None` for any other name.
pub(crate) fn port_io_builtin(name: &str) -> Option<(bool, PortWidth)> {
    Some(match name {
        "port_in8" => (true, PortWidth::Bits8),
        "port_in16" => (true, PortWidth::Bits16),
        "port_in32" => (true, PortWidth::Bits32),
        "port_out8" => (false, PortWidth::Bits8),
        "port_out16" => (false, PortWidth::Bits16),
        "port_out32" => (false, PortWidth::Bits32),
        _ => return None,
    })
}

/// The compile-time port number of `expr`, if it has one.
///
/// A port argument is typed `u16`, so a literal port reaches the backend as the
/// typed-literal desugaring `to_u16(<Integer>)` — or, if the constant folder got
/// there first, as a bare `Integer` already carrying the `u16` type. Both are
/// recognized. The `as u16` truncation *is* `to_u16`'s wrapping semantics, so a
/// folded and an unfolded literal agree exactly.
///
/// Anything else (a variable, arithmetic, a call) has no compile-time value and
/// yields `None`, selecting the `DX` form.
fn const_port(expr: &BytecodeExpr) -> Option<u16> {
    match &expr.kind {
        BytecodeExprKind::Integer(value) => Some(*value as u16),
        BytecodeExprKind::Call { name, args } if name == "to_u16" && args.len() == 1 => {
            match &args[0].kind {
                BytecodeExprKind::Integer(value) => Some(*value as u16),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The 8-bit immediate port for `expr`, if it is a compile-time constant that
/// fits the `imm8` form. Ports above `0xFF` must use `DX`.
fn const_imm8_port(expr: &BytecodeExpr) -> Option<u8> {
    u8::try_from(const_port(expr)?).ok()
}

/// Lower a port-I/O builtin call, leaving an `in` result (a normalized unsigned
/// cell) in `rax`. An `out` returns `void`; like `ptr_write`, it leaves a dead
/// value in `rax` that no one reads. Returns `None` when `name` is not a port
/// builtin, so the caller falls through to its other dispatch arms.
pub(crate) fn lower_port_io_call(
    ctx: &mut NativeCtx,
    name: &str,
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Option<Result<(), String>> {
    let (is_read, width) = port_io_builtin(name)?;
    Some(if is_read {
        lower_port_in(ctx, name, args, width, code)
    } else {
        lower_port_out(ctx, name, args, width, code)
    })
}

/// `port_in<N>(port u16) -> u<N>` — read a device port into a normalized cell.
fn lower_port_in(
    ctx: &mut NativeCtx,
    name: &str,
    args: &[BytecodeExpr],
    width: PortWidth,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let [port] = args else {
        return Err(format!("`{name}` takes exactly one argument"));
    };
    if let Some(imm) = const_imm8_port(port) {
        // `in al/ax/eax, imm8` — no DX setup, and the constant port expression is
        // a literal by construction, so not evaluating it elides nothing.
        emit_operand_size_prefix(code, width);
        code.push(match width {
            PortWidth::Bits8 => 0xE4,
            PortWidth::Bits16 | PortWidth::Bits32 => 0xE5,
        });
        code.push(imm);
    } else {
        lower_native_expr(ctx, port, code)?; // port -> rax
        emit_mov_edx_from(code, Reg::Rax); // DX = port (low 16 bits)
        emit_operand_size_prefix(code, width);
        code.push(match width {
            PortWidth::Bits8 => 0xEC,
            PortWidth::Bits16 | PortWidth::Bits32 => 0xED,
        });
    }
    // `in` writes only AL/AX/EAX; renormalize the full 8-byte cell (zero-extend).
    emit_normalize_rax(code, width.int_kind());
    Ok(())
}

/// `port_out<N>(port u16, value u<N>)` — write a device port.
fn lower_port_out(
    ctx: &mut NativeCtx,
    name: &str,
    args: &[BytecodeExpr],
    width: PortWidth,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let [port, value] = args else {
        return Err(format!("`{name}` takes exactly two arguments"));
    };
    if let Some(imm) = const_imm8_port(port) {
        // `out imm8, al/ax/eax` — only the value needs evaluating.
        lower_native_expr(ctx, value, code)?; // value -> rax (AL/AX/EAX)
        emit_operand_size_prefix(code, width);
        code.push(match width {
            PortWidth::Bits8 => 0xE6,
            PortWidth::Bits16 | PortWidth::Bits32 => 0xE7,
        });
        code.push(imm);
    } else {
        // Evaluate the port first and spill it, then the value — the same
        // two-operand idiom `ptr_write` uses, so a call inside either operand is
        // staged correctly and neither clobbers the other.
        lower_native_expr(ctx, port, code)?; // port -> rax
        code.push(0x50); // push rax (port)
        lower_native_expr(ctx, value, code)?; // value -> rax (AL/AX/EAX)
        code.push(0x59); // pop rcx  (port)
        emit_mov_edx_from(code, Reg::Rcx); // DX = port (low 16 bits)
        emit_operand_size_prefix(code, width);
        code.push(match width {
            PortWidth::Bits8 => 0xEE,
            PortWidth::Bits16 | PortWidth::Bits32 => 0xEF,
        });
    }
    Ok(())
}

/// The source register of an `emit_mov_edx_from` port move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reg {
    Rax,
    Rcx,
}

/// `mov edx, eax` / `mov edx, ecx` — put the port number's low 16 bits in `DX`,
/// the only register `in`/`out` accept a port in. A 32-bit move suffices: `DX` is
/// the low half of `EDX`, and the bits above the port are ignored by the
/// instruction, so no masking is needed even for an unnormalized cell.
fn emit_mov_edx_from(code: &mut Vec<u8>, source: Reg) {
    // 89 /r = MOV r/m32, r32 with ModRM = 11 (reg-direct) | reg=src | rm=edx(010).
    code.extend_from_slice(&[
        0x89,
        match source {
            Reg::Rax => 0xC2, // 11 000 010 : edx <- eax
            Reg::Rcx => 0xCA, // 11 001 010 : edx <- ecx
        },
    ]);
}

/// Emit the `0x66` operand-size prefix for the 16-bit port forms. The 8-bit forms
/// have their own opcodes and the 32-bit forms are the 64-bit-mode default, so
/// neither is prefixed.
fn emit_operand_size_prefix(code: &mut Vec<u8>, width: PortWidth) {
    if width.needs_operand_size_prefix() {
        code.push(0x66);
    }
}
