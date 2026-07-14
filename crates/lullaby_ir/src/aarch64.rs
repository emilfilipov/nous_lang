//! AArch64 (ARM64) native code generator — the `i64`-scalar subset.
//!
//! This is Lullaby's second instruction-set backend. It consumes the *same*
//! [`BytecodeModule`] the x86-64 backend lowers (see
//! [`crate::native_object::emit_native_program_for_target`]) and emits a
//! freestanding AArch64 Linux **ELF64** relocatable object through the shared
//! [`crate::object_model::ObjectModel`] / [`crate::elf_object`] path.
//!
//! # Covered subset
//!
//! Mirroring the scalar core the x86-64 backend started from:
//!
//! * `i64` (and `bool`, represented as a `0`/`1` `i64` word) literals, locals,
//!   parameters (in `x0..x7`), and returns (`x0`);
//! * arithmetic `+ - * /` (`add`/`sub`/`mul`/`sdiv`), bitwise `& | ^ << >>`
//!   (`and`/`orr`/`eor`/`lslv`/`asrv`), comparisons (`cmp` + `cset`), and the
//!   short-circuit `and`/`or`/`not` logical forms;
//! * control flow: `if`/`elif`/`else`, `while`, `loop`, and inclusive range
//!   `for` with `break`/`continue` (`b`/`b.cond`/`cbz` with fixed-up offsets);
//! * inter-function calls under AAPCS64 (arguments in `x0..x7`, result in `x0`,
//!   `bl` with an `R_AARCH64_CALL26` relocation, 16-byte stack alignment);
//! * the `i64` scalar math builtins `abs`/`min`/`max`/`gcd`/`sign`/`clamp`,
//!   lowered inline (branchless `cmp`+`csel`, `asr`/`eor`/`sub` abs, and an
//!   unsigned `udiv`/`msub` Euclid loop for `gcd`), bit-for-bit with the
//!   interpreters (the `f64` forms skip, since the core has no float support);
//! * a freestanding `_start` that calls `main`, then invokes the Linux `exit`
//!   syscall (`x8 = 93`, `svc #0`), so the object needs no libc.
//!
//! # Skipped (recorded, never miscompiled)
//!
//! Everything outside the scalar core — heap/aggregates (`struct`/`array`/
//! `list`/`map`), `string`/`char`/`float` values, `enum`/`match`, `extern` FFI,
//! `throw`/`try`, inline `asm`, and `await`/closures — makes a function
//! ineligible; it is reported as a skipped function (exactly like the x86-64
//! backend records its unsupported cases) rather than being emitted incorrectly.
//! A call to a skipped (or otherwise non-eligible) function likewise skips the
//! caller.

use std::collections::{HashMap, HashSet};

use lullaby_parser::{AssignOp, BinaryOp, TypeRef, UnaryOp};

use crate::native_contract::NativeTarget;
use crate::native_object::{
    NATIVE_NO_ELIGIBLE_CODE, NativeProgram, NativeProgramError, NativeSkippedFunction,
};
use crate::object_model::{
    ObjectMachine, ObjectModel, ObjectRelocation, ObjectRelocationKind, ObjectSection,
    ObjectSectionKind, ObjectSymbol, ObjectSymbolKind,
};
use crate::{
    BytecodeExpr, BytecodeExprKind, BytecodeFunction, BytecodeIfBranch, BytecodeInstruction,
    BytecodeModule,
};

// -- AArch64 instruction encoders --------------------------------------------
//
// Every encoder returns a 32-bit little-endian instruction word. Register
// numbers are 0..=30 for `x0..x30`; 31 denotes `xzr`/`sp` per the field. These
// are the AAPCS64/A64 encodings; the unit tests pin the ones with a known fixed
// value (`stp`, `ldp`, `ret`, `svc`, `bl`, `movz`).

/// The zero register / stack pointer register number (31, field-dependent).
const XZR: u32 = 31;
/// The frame-pointer register (`x29`).
const X29: u32 = 29;

/// AArch64 condition codes (the low nibble of a `b.cond`/`cset` selector).
const COND_EQ: u32 = 0b0000;
const COND_NE: u32 = 0b0001;
const COND_GE: u32 = 0b1010;
const COND_LT: u32 = 0b1011;
const COND_GT: u32 = 0b1100;
const COND_LE: u32 = 0b1101;

/// `movz Xd, #imm16, lsl #(16*hw)` — load a 16-bit immediate, zeroing the rest.
fn movz(rd: u32, imm16: u32, hw: u32) -> u32 {
    0xD280_0000 | (hw << 21) | ((imm16 & 0xFFFF) << 5) | rd
}

/// `movk Xd, #imm16, lsl #(16*hw)` — insert a 16-bit immediate, keeping the rest.
fn movk(rd: u32, imm16: u32, hw: u32) -> u32 {
    0xF280_0000 | (hw << 21) | ((imm16 & 0xFFFF) << 5) | rd
}

/// `add Xd, Xn, Xm` (shifted register, shift 0).
fn add_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x8B00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `sub Xd, Xn, Xm` (shifted register, shift 0).
fn sub_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xCB00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `mul Xd, Xn, Xm` (`madd Xd, Xn, Xm, xzr`).
fn mul_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x9B00_0000 | (rm << 16) | (XZR << 10) | (rn << 5) | rd
}

/// `sdiv Xd, Xn, Xm` (signed division, truncating toward zero).
fn sdiv_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x9AC0_0C00 | (rm << 16) | (rn << 5) | rd
}

/// `msub Xd, Xn, Xm, Xa` (`Xd = Xa - Xn*Xm`). Used with `sdiv` to compute the
/// truncated remainder `a - (a/b)*b`.
fn msub_reg(rd: u32, rn: u32, rm: u32, ra: u32) -> u32 {
    0x9B00_8000 | (rm << 16) | (ra << 10) | (rn << 5) | rd
}

/// `and Xd, Xn, Xm` (bitwise, shifted register).
fn and_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x8A00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `orr Xd, Xn, Xm` (bitwise, shifted register).
fn orr_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xAA00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `eor Xd, Xn, Xm` (bitwise XOR, shifted register).
fn eor_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0xCA00_0000 | (rm << 16) | (rn << 5) | rd
}

/// `lslv Xd, Xn, Xm` (logical shift left by a register amount, low 6 bits used).
fn lslv_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x9AC0_2000 | (rm << 16) | (rn << 5) | rd
}

/// `asrv Xd, Xn, Xm` (arithmetic shift right by a register amount, low 6 bits).
fn asrv_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x9AC0_2800 | (rm << 16) | (rn << 5) | rd
}

/// `mov Xd, Xm` (`orr Xd, xzr, Xm`).
fn mov_reg(rd: u32, rm: u32) -> u32 {
    orr_reg(rd, XZR, rm)
}

/// `mvn Xd, Xm` (bitwise NOT — `orn Xd, xzr, Xm`).
fn mvn_reg(rd: u32, rm: u32) -> u32 {
    0xAA20_0000 | (rm << 16) | (XZR << 5) | rd
}

/// `cmp Xn, Xm` (`subs xzr, Xn, Xm`) — set NZCV from `Xn - Xm`.
fn cmp_reg(rn: u32, rm: u32) -> u32 {
    0xEB00_0000 | (rm << 16) | (rn << 5) | XZR
}

/// `cmp Xn, #0` (`subs xzr, Xn, #0`).
fn cmp_imm0(rn: u32) -> u32 {
    0xF100_0000 | (rn << 5) | XZR
}

/// `cset Xd, cond` — set `Xd` to 1 when `cond` holds, else 0
/// (`csinc Xd, xzr, xzr, invert(cond)`).
fn cset(rd: u32, cond: u32) -> u32 {
    0x9A80_0400 | (XZR << 16) | ((cond ^ 1) << 12) | (XZR << 5) | rd
}

/// `csel Xd, Xn, Xm, cond` — `Xd = cond ? Xn : Xm` (conditional select).
fn csel(rd: u32, rn: u32, rm: u32, cond: u32) -> u32 {
    0x9A80_0000 | (rm << 16) | (cond << 12) | (rn << 5) | rd
}

/// `udiv Xd, Xn, Xm` (unsigned division, truncating toward zero).
fn udiv_reg(rd: u32, rn: u32, rm: u32) -> u32 {
    0x9AC0_0800 | (rm << 16) | (rn << 5) | rd
}

/// `asr Xd, Xn, #shift` — arithmetic shift right by an immediate
/// (`sbfm Xd, Xn, #shift, #63`). Used for the two's-complement sign mask.
fn asr_imm(rd: u32, rn: u32, shift: u32) -> u32 {
    0x9340_0000 | ((shift & 0x3F) << 16) | (0x3F << 10) | (rn << 5) | rd
}

/// `str Xt, [Xn, #(8*slot)]` (unsigned scaled offset).
fn str_local(rt: u32, rn: u32, slot: u32) -> u32 {
    0xF900_0000 | ((slot & 0xFFF) << 10) | (rn << 5) | rt
}

/// `ldr Xt, [Xn, #(8*slot)]` (unsigned scaled offset).
fn ldr_local(rt: u32, rn: u32, slot: u32) -> u32 {
    0xF940_0000 | ((slot & 0xFFF) << 10) | (rn << 5) | rt
}

/// `str Xt, [sp, #-16]!` (pre-indexed push, keeps 16-byte alignment).
fn push_reg(rt: u32) -> u32 {
    let imm9 = (-16i32 as u32) & 0x1FF;
    0xF800_0C00 | (imm9 << 12) | (XZR << 5) | rt
}

/// `ldr Xt, [sp], #16` (post-indexed pop).
fn pop_reg(rt: u32) -> u32 {
    let imm9 = 16u32 & 0x1FF;
    0xF840_0400 | (imm9 << 12) | (XZR << 5) | rt
}

/// `sub sp, sp, #imm12`.
fn sub_sp_imm(imm12: u32) -> u32 {
    0xD100_0000 | ((imm12 & 0xFFF) << 10) | (XZR << 5) | XZR
}

/// `add sp, sp, #imm12`.
fn add_sp_imm(imm12: u32) -> u32 {
    0x9100_0000 | ((imm12 & 0xFFF) << 10) | (XZR << 5) | XZR
}

/// `mov x29, sp` (`add x29, sp, #0`).
fn mov_x29_sp() -> u32 {
    0x9100_0000 | (XZR << 5) | X29
}

/// `mov sp, x29` (`add sp, x29, #0`).
fn mov_sp_x29() -> u32 {
    0x9100_0000 | (X29 << 5) | XZR
}

/// `stp x29, x30, [sp, #-16]!` — save the frame pointer and link register.
const STP_FP_LR_PRE: u32 = 0xA9BF_7BFD;
/// `ldp x29, x30, [sp], #16` — restore the frame pointer and link register.
const LDP_FP_LR_POST: u32 = 0xA8C1_7BFD;
/// `ret` (return to `x30`).
const RET: u32 = 0xD65F_03C0;
/// `nop` (used to pad functions to a 16-byte boundary).
const NOP: u32 = 0xD503_201F;
/// `svc #0` — supervisor call (Linux syscall).
const SVC0: u32 = 0xD400_0001;
/// `brk #0` — software breakpoint (unreachable trap after `exit`).
const BRK0: u32 = 0xD420_0000;
/// The base `bl #0` word (imm26 = 0); the `R_AARCH64_CALL26` relocation fills it.
const BL_BASE: u32 = 0x9400_0000;
/// The Linux AArch64 `exit` syscall number.
const SYS_EXIT: u32 = 93;

// -- Code buffer with labels and relocations ---------------------------------

/// A branch that must be patched once its target label's offset is known.
struct Fixup {
    /// Byte offset of the branch instruction word within the function's code.
    at: usize,
    /// The label the branch targets.
    target: usize,
    /// `true` for a 26-bit immediate (`b`), `false` for a 19-bit immediate
    /// (`b.cond`/`cbz`).
    wide: bool,
}

/// A `bl` call site: patch the branch immediate at `offset` (relative to the
/// function's own code start) to reference `symbol`.
struct CallSite {
    offset: u32,
    symbol: String,
}

/// A loop's `break`/`continue` label targets, tracked so nested `break`/
/// `continue` resolve to the innermost loop.
struct LoopFrame {
    break_label: usize,
    continue_label: usize,
}

/// A fully lowered AArch64 function: its machine code and `bl` call sites.
struct LoweredFn {
    name: String,
    code: Vec<u8>,
    calls: Vec<CallSite>,
}

/// Per-function AArch64 codegen state: a stack-machine over `x0` (accumulator)
/// with `x1` as the binary right-hand temp and the machine stack for spills.
struct FnGen<'a> {
    code: Vec<u8>,
    fixups: Vec<Fixup>,
    labels: Vec<Option<usize>>,
    calls: Vec<CallSite>,
    loops: Vec<LoopFrame>,
    /// Local name → slot index; word `k` lives at `[x29, #8*k]`.
    locals: HashMap<String, u32>,
    /// The 16-byte-aligned locals region size in bytes.
    locals_size: u32,
    /// The set of names that may be `bl`-called (all compiled functions).
    callable: &'a HashSet<String>,
}

impl<'a> FnGen<'a> {
    fn emit(&mut self, word: u32) {
        self.code.extend_from_slice(&word.to_le_bytes());
    }

    fn new_label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }

    fn bind(&mut self, label: usize) {
        self.labels[label] = Some(self.code.len());
    }

    /// `b <label>` — unconditional branch (26-bit immediate).
    fn emit_b(&mut self, label: usize) {
        let at = self.code.len();
        self.emit(0x1400_0000);
        self.fixups.push(Fixup {
            at,
            target: label,
            wide: true,
        });
    }

    /// `b.<cond> <label>` — conditional branch (19-bit immediate).
    fn emit_bcond(&mut self, cond: u32, label: usize) {
        let at = self.code.len();
        self.emit(0x5400_0000 | cond);
        self.fixups.push(Fixup {
            at,
            target: label,
            wide: false,
        });
    }

    /// `cbz Xt, <label>` — branch when `Xt == 0` (19-bit immediate).
    fn emit_cbz(&mut self, rt: u32, label: usize) {
        let at = self.code.len();
        self.emit(0xB400_0000 | rt);
        self.fixups.push(Fixup {
            at,
            target: label,
            wide: false,
        });
    }

    /// `cbnz Xt, <label>` — branch when `Xt != 0` (19-bit immediate).
    fn emit_cbnz(&mut self, rt: u32, label: usize) {
        let at = self.code.len();
        self.emit(0xB500_0000 | rt);
        self.fixups.push(Fixup {
            at,
            target: label,
            wide: false,
        });
    }

    /// Load an arbitrary `i64` immediate into `Xd` with `movz` + up to three
    /// `movk`s (covering the full 64-bit value regardless of sign).
    fn load_imm(&mut self, rd: u32, value: i64) {
        let u = value as u64;
        self.emit(movz(rd, (u & 0xFFFF) as u32, 0));
        for hw in 1..4u32 {
            let chunk = ((u >> (16 * hw)) & 0xFFFF) as u32;
            if chunk != 0 {
                self.emit(movk(rd, chunk, hw));
            }
        }
    }

    fn slot_of(&self, name: &str) -> Result<u32, String> {
        self.locals
            .get(name)
            .copied()
            .ok_or_else(|| format!("reference to unknown local `{name}`"))
    }

    /// Resolve every pending branch fixup into its final PC-relative immediate.
    fn resolve_fixups(&mut self) {
        for fixup in &self.fixups {
            let target = self.labels[fixup.target].expect("every branch label is bound");
            let delta = (target as i64 - fixup.at as i64) >> 2;
            let mut word = u32::from_le_bytes(
                self.code[fixup.at..fixup.at + 4]
                    .try_into()
                    .expect("4-byte instruction word"),
            );
            if fixup.wide {
                word = (word & !0x03FF_FFFF) | (delta as u32 & 0x03FF_FFFF);
            } else {
                word = (word & !(0x7FFFF << 5)) | ((delta as u32 & 0x7FFFF) << 5);
            }
            self.code[fixup.at..fixup.at + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
}

// -- Type gating (the i64/bool scalar surface) -------------------------------

/// Whether a type is a native scalar the AArch64 core handles: `i64` or `bool`
/// (both a single `x`-register word). Everything else is out of subset.
fn is_scalar(ty: &TypeRef) -> bool {
    matches!(ty.name.as_str(), "i64" | "bool")
}

/// Signature eligibility for the AArch64 scalar core: every parameter and the
/// return type must be `i64`/`bool`, at most eight parameters (`x0..x7`), and a
/// `main` entry must take no parameters (the freestanding `_start` calls it with
/// no arguments).
fn signature_eligible(function: &BytecodeFunction) -> Result<(), String> {
    if function.params.len() > 8 {
        return Err(format!(
            "{} parameters exceeds the eight AArch64 argument registers (x0..x7)",
            function.params.len()
        ));
    }
    for param in &function.params {
        if !is_scalar(&param.ty) {
            return Err(format!(
                "parameter `{}` type `{}` is not in the AArch64 i64-scalar subset",
                param.name, param.ty.name
            ));
        }
    }
    if !is_scalar(&function.return_type) {
        return Err(format!(
            "return type `{}` is not in the AArch64 i64-scalar subset",
            function.return_type.name
        ));
    }
    if function.name == "main" && !function.params.is_empty() {
        return Err("entry `main` must take no parameters for the freestanding core".to_string());
    }
    Ok(())
}

// -- Local collection --------------------------------------------------------

/// Collect every local name a function declares, in first-seen order: parameters
/// first, then `let` bindings and `for` counters (each `for` also reserves the
/// hidden `{name}__end` and `{name}__step` slots) from a recursive walk.
fn collect_locals(function: &BytecodeFunction) -> Vec<String> {
    /// Append `name` to `names` if not already present (slots are unique, so a
    /// name reused across disjoint scopes shares one over-allocated slot).
    fn add(names: &mut Vec<String>, name: &str) {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    fn walk(body: &[BytecodeInstruction], names: &mut Vec<String>) {
        for instr in body {
            match instr {
                BytecodeInstruction::Let { name, .. } => add(names, name),
                BytecodeInstruction::For { name, body, .. } => {
                    add(names, name);
                    add(names, &format!("{name}__end"));
                    add(names, &format!("{name}__step"));
                    walk(body, names);
                }
                BytecodeInstruction::If {
                    branches,
                    else_body,
                    ..
                } => {
                    for branch in branches {
                        walk(&branch.body, names);
                    }
                    walk(else_body, names);
                }
                BytecodeInstruction::While { body, .. }
                | BytecodeInstruction::Loop { body, .. } => {
                    walk(body, names);
                }
                _ => {}
            }
        }
    }
    let mut names: Vec<String> = Vec::new();
    for param in &function.params {
        add(&mut names, &param.name);
    }
    walk(&function.instructions, &mut names);
    names
}

// -- Function lowering -------------------------------------------------------

/// Lower one eligible function to AArch64 machine code, or return a skip reason.
fn lower_function(
    function: &BytecodeFunction,
    callable: &HashSet<String>,
) -> Result<LoweredFn, String> {
    let names = collect_locals(function);
    let num_locals = names.len() as u32;
    // Round the locals region up to a 16-byte boundary (stack stays aligned).
    let locals_size = (num_locals * 8).div_ceil(16) * 16;
    if locals_size > 0xFFF {
        return Err(format!(
            "frame of {locals_size} bytes exceeds the AArch64 core's immediate-offset limit"
        ));
    }
    let mut locals = HashMap::new();
    for (index, name) in names.iter().enumerate() {
        locals.insert(name.clone(), index as u32);
    }

    let mut fg = FnGen {
        code: Vec::new(),
        fixups: Vec::new(),
        labels: Vec::new(),
        calls: Vec::new(),
        loops: Vec::new(),
        locals,
        locals_size,
        callable,
    };

    emit_prologue(&mut fg, function);
    lower_stmts(&mut fg, &function.instructions)?;
    // Fall-through / trailing-expression return: whatever is in `x0` is the
    // result, mirroring the x86-64 backend's tail-expression convention.
    emit_epilogue(&mut fg);
    fg.resolve_fixups();

    Ok(LoweredFn {
        name: function.name.clone(),
        code: fg.code,
        calls: fg.calls,
    })
}

/// Emit the AArch64 prologue: save `x29`/`x30`, reserve the locals region, point
/// `x29` at its base, and spill the incoming parameter registers to their slots.
fn emit_prologue(fg: &mut FnGen, function: &BytecodeFunction) {
    fg.emit(STP_FP_LR_PRE);
    if fg.locals_size > 0 {
        fg.emit(sub_sp_imm(fg.locals_size));
    }
    fg.emit(mov_x29_sp());
    for (index, param) in function.params.iter().enumerate() {
        let slot = fg.slot_of(&param.name).expect("param is a local");
        fg.emit(str_local(index as u32, X29, slot));
    }
}

/// Emit the AArch64 epilogue: discard the frame, restore `x29`/`x30`, and `ret`.
/// The result is already in `x0`.
fn emit_epilogue(fg: &mut FnGen) {
    fg.emit(mov_sp_x29());
    if fg.locals_size > 0 {
        fg.emit(add_sp_imm(fg.locals_size));
    }
    fg.emit(LDP_FP_LR_POST);
    fg.emit(RET);
}

fn lower_stmts(fg: &mut FnGen, body: &[BytecodeInstruction]) -> Result<(), String> {
    for stmt in body {
        lower_stmt(fg, stmt)?;
    }
    Ok(())
}

fn lower_stmt(fg: &mut FnGen, stmt: &BytecodeInstruction) -> Result<(), String> {
    match stmt {
        BytecodeInstruction::Let {
            name, ty, value, ..
        } => {
            if !is_scalar(ty) {
                return Err(format!(
                    "`let {name}` type `{}` is not in the AArch64 i64-scalar subset",
                    ty.name
                ));
            }
            lower_expr(fg, value)?;
            let slot = fg.slot_of(name)?;
            fg.emit(str_local(0, X29, slot));
            Ok(())
        }
        BytecodeInstruction::Assign {
            name,
            path,
            op,
            value,
            ..
        } => {
            if !path.is_empty() {
                return Err(
                    "field/index assignment is not in the AArch64 i64-scalar subset".to_string(),
                );
            }
            let slot = fg.slot_of(name)?;
            match op {
                AssignOp::Replace => {
                    lower_expr(fg, value)?;
                    fg.emit(str_local(0, X29, slot));
                }
                AssignOp::Add
                | AssignOp::Subtract
                | AssignOp::Multiply
                | AssignOp::Divide
                | AssignOp::Remainder => {
                    let bin = match op {
                        AssignOp::Add => BinaryOp::Add,
                        AssignOp::Subtract => BinaryOp::Subtract,
                        AssignOp::Multiply => BinaryOp::Multiply,
                        AssignOp::Divide => BinaryOp::Divide,
                        AssignOp::Remainder => BinaryOp::Remainder,
                        AssignOp::Replace => unreachable!("handled above"),
                    };
                    fg.emit(ldr_local(0, X29, slot)); // x0 = current (left)
                    fg.emit(push_reg(0));
                    lower_expr(fg, value)?; // x0 = right
                    fg.emit(mov_reg(1, 0)); // x1 = right
                    fg.emit(pop_reg(0)); // x0 = left
                    emit_binary_op(fg, bin); // handles Divide (sdiv) and Remainder (sdiv+msub)
                    fg.emit(str_local(0, X29, slot));
                }
            }
            Ok(())
        }
        BytecodeInstruction::Return(Some(expr)) => {
            lower_expr(fg, expr)?;
            emit_epilogue(fg);
            Ok(())
        }
        BytecodeInstruction::Return(None) => {
            Err("AArch64 subset functions must return an i64/bool value".to_string())
        }
        BytecodeInstruction::Expr(expr) => {
            // A tail expression is the function result; a non-tail expression's
            // value is simply discarded. Either way it is evaluated into `x0`.
            lower_expr(fg, expr)?;
            Ok(())
        }
        BytecodeInstruction::Break(_) => {
            let target = fg.loops.last().ok_or("`break` outside a loop")?.break_label;
            fg.emit_b(target);
            Ok(())
        }
        BytecodeInstruction::Continue(_) => {
            let target = fg
                .loops
                .last()
                .ok_or("`continue` outside a loop")?
                .continue_label;
            fg.emit_b(target);
            Ok(())
        }
        BytecodeInstruction::If {
            branches,
            else_body,
            ..
        } => lower_if(fg, branches, else_body),
        BytecodeInstruction::While {
            condition, body, ..
        } => lower_while(fg, condition, body),
        BytecodeInstruction::Loop { body, .. } => lower_loop(fg, body),
        BytecodeInstruction::For {
            name,
            start,
            end,
            step,
            body,
            ..
        } => lower_for(fg, name, start, end, step.as_ref(), body),
        BytecodeInstruction::Asm { .. } => {
            Err("inline `asm` is x86-64 only; not in the AArch64 subset".to_string())
        }
        BytecodeInstruction::Throw { .. } | BytecodeInstruction::Try { .. } => {
            Err("throw/try is not in the AArch64 i64-scalar subset".to_string())
        }
        BytecodeInstruction::Match { .. } => {
            Err("match is not in the AArch64 i64-scalar subset".to_string())
        }
    }
}

/// Lower `if`/`elif`/`else` into a chain of `cbz`-skips with a shared exit label.
fn lower_if(
    fg: &mut FnGen,
    branches: &[BytecodeIfBranch],
    else_body: &[BytecodeInstruction],
) -> Result<(), String> {
    let end = fg.new_label();
    for branch in branches {
        lower_expr(fg, &branch.condition)?;
        let next = fg.new_label();
        fg.emit_cbz(0, next); // condition false → skip this branch
        lower_stmts(fg, &branch.body)?;
        fg.emit_b(end);
        fg.bind(next);
    }
    lower_stmts(fg, else_body)?;
    fg.bind(end);
    Ok(())
}

/// Lower `while cond: body` — test at the top, exit when the condition is 0.
fn lower_while(
    fg: &mut FnGen,
    condition: &BytecodeExpr,
    body: &[BytecodeInstruction],
) -> Result<(), String> {
    let top = fg.new_label();
    let end = fg.new_label();
    fg.bind(top);
    lower_expr(fg, condition)?;
    fg.emit_cbz(0, end);
    fg.loops.push(LoopFrame {
        break_label: end,
        continue_label: top,
    });
    lower_stmts(fg, body)?;
    fg.loops.pop();
    fg.emit_b(top);
    fg.bind(end);
    Ok(())
}

/// Lower `loop: body` — an unconditional infinite loop exited only by `break`.
fn lower_loop(fg: &mut FnGen, body: &[BytecodeInstruction]) -> Result<(), String> {
    let top = fg.new_label();
    let end = fg.new_label();
    fg.bind(top);
    fg.loops.push(LoopFrame {
        break_label: end,
        continue_label: top,
    });
    lower_stmts(fg, body)?;
    fg.loops.pop();
    fg.emit_b(top);
    fg.bind(end);
    Ok(())
}

/// Lower an inclusive range `for i = start..=end step s`, mirroring the
/// interpreter and x86-64 backend: ascending stops when `i > end`, descending
/// when `i < end`; `continue` jumps to the step, `break` exits.
fn lower_for(
    fg: &mut FnGen,
    name: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Result<(), String> {
    let i_slot = fg.slot_of(name)?;
    let end_slot = fg.slot_of(&format!("{name}__end"))?;
    let step_slot = fg.slot_of(&format!("{name}__step"))?;

    // i = start; end_local = end; step_local = step (default 1).
    lower_expr(fg, start)?;
    fg.emit(str_local(0, X29, i_slot));
    lower_expr(fg, end)?;
    fg.emit(str_local(0, X29, end_slot));
    match step {
        Some(step_expr) => lower_expr(fg, step_expr)?,
        None => fg.load_imm(0, 1),
    }
    fg.emit(str_local(0, X29, step_slot));

    let top = fg.new_label();
    let desc = fg.new_label();
    let body_label = fg.new_label();
    let cont = fg.new_label();
    let exit = fg.new_label();

    fg.bind(top);
    // Choose the guard direction from the step's sign.
    fg.emit(ldr_local(0, X29, step_slot));
    fg.emit(cmp_imm0(0));
    fg.emit_bcond(COND_LT, desc);
    // Ascending: exit when i > end.
    fg.emit(ldr_local(0, X29, i_slot));
    fg.emit(ldr_local(1, X29, end_slot));
    fg.emit(cmp_reg(0, 1));
    fg.emit_bcond(COND_GT, exit);
    fg.emit_b(body_label);
    // Descending: exit when i < end.
    fg.bind(desc);
    fg.emit(ldr_local(0, X29, i_slot));
    fg.emit(ldr_local(1, X29, end_slot));
    fg.emit(cmp_reg(0, 1));
    fg.emit_bcond(COND_LT, exit);

    fg.bind(body_label);
    fg.loops.push(LoopFrame {
        break_label: exit,
        continue_label: cont,
    });
    lower_stmts(fg, body)?;
    fg.loops.pop();

    // Step block (target of `continue`): i += step.
    fg.bind(cont);
    fg.emit(ldr_local(0, X29, i_slot));
    fg.emit(ldr_local(1, X29, step_slot));
    fg.emit(add_reg(0, 0, 1));
    fg.emit(str_local(0, X29, i_slot));
    fg.emit_b(top);

    fg.bind(exit);
    Ok(())
}

/// Lower an expression, leaving its value in `x0`.
fn lower_expr(fg: &mut FnGen, expr: &BytecodeExpr) -> Result<(), String> {
    match &expr.kind {
        BytecodeExprKind::Integer(value) => {
            fg.load_imm(0, *value);
            Ok(())
        }
        BytecodeExprKind::Bool(value) => {
            fg.load_imm(0, i64::from(*value));
            Ok(())
        }
        BytecodeExprKind::Variable(name) => {
            let slot = fg.slot_of(name)?;
            fg.emit(ldr_local(0, X29, slot));
            Ok(())
        }
        BytecodeExprKind::Unary { op, expr } => {
            lower_expr(fg, expr)?;
            match op {
                UnaryOp::Not => {
                    // Logical not: x0 = (x0 == 0) ? 1 : 0.
                    fg.emit(cmp_imm0(0));
                    fg.emit(cset(0, COND_EQ));
                }
                UnaryOp::BitNot => {
                    fg.emit(mvn_reg(0, 0));
                }
                UnaryOp::Negate => {
                    // Integer negation: `neg x0, x0` == `sub x0, xzr, x0` (wrapping,
                    // so `-i64::MIN == i64::MIN`). Floats are deferred on AArch64.
                    fg.emit(sub_reg(0, XZR, 0));
                }
            }
            Ok(())
        }
        BytecodeExprKind::Binary { left, op, right } => lower_binary(fg, left, *op, right),
        BytecodeExprKind::Call { name, args } => lower_call(fg, name, args),
        BytecodeExprKind::Float(_)
        | BytecodeExprKind::String(_)
        | BytecodeExprKind::Char(_)
        | BytecodeExprKind::Array(_)
        | BytecodeExprKind::Index { .. }
        | BytecodeExprKind::Field { .. }
        | BytecodeExprKind::Await { .. }
        | BytecodeExprKind::Closure { .. } => Err(format!(
            "expression `{}` is not in the AArch64 i64-scalar subset",
            expr.ty.name
        )),
    }
}

/// Lower a binary expression. `and`/`or` short-circuit; the rest evaluate both
/// operands (left spilled to the stack) and apply the arithmetic/compare op.
fn lower_binary(
    fg: &mut FnGen,
    left: &BytecodeExpr,
    op: BinaryOp,
    right: &BytecodeExpr,
) -> Result<(), String> {
    match op {
        BinaryOp::And => {
            // left && right: if left is 0, result 0; else result = (right != 0).
            let false_label = fg.new_label();
            let end = fg.new_label();
            lower_expr(fg, left)?;
            fg.emit_cbz(0, false_label);
            lower_expr(fg, right)?;
            fg.emit(cmp_imm0(0));
            fg.emit(cset(0, COND_NE));
            fg.emit_b(end);
            fg.bind(false_label);
            fg.load_imm(0, 0);
            fg.bind(end);
            Ok(())
        }
        BinaryOp::Or => {
            // left || right: if left is non-zero, result 1; else result = (right != 0).
            let true_label = fg.new_label();
            let end = fg.new_label();
            lower_expr(fg, left)?;
            fg.emit_cbnz(0, true_label);
            lower_expr(fg, right)?;
            fg.emit(cmp_imm0(0));
            fg.emit(cset(0, COND_NE));
            fg.emit_b(end);
            fg.bind(true_label);
            fg.load_imm(0, 1);
            fg.bind(end);
            Ok(())
        }
        _ => {
            // Strict binary op: evaluate left, spill, evaluate right, combine.
            lower_expr(fg, left)?;
            fg.emit(push_reg(0));
            lower_expr(fg, right)?;
            fg.emit(mov_reg(1, 0)); // x1 = right
            fg.emit(pop_reg(0)); // x0 = left
            emit_binary_op(fg, op);
            Ok(())
        }
    }
}

/// Emit the register-to-register form of an arithmetic/bitwise/comparison op
/// with the left operand in `x0`, the right in `x1`, and the result in `x0`.
fn emit_binary_op(fg: &mut FnGen, op: BinaryOp) {
    match op {
        BinaryOp::Add
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::BitAnd
        | BinaryOp::BitOr
        | BinaryOp::BitXor
        | BinaryOp::Shl
        | BinaryOp::Shr => emit_arith(fg, op),
        // Remainder has no single AArch64 instruction: `x0 = x0 - (x0/x1)*x1`
        // via `sdiv` into a scratch register, then `msub`. This also yields the
        // correct `0` for `i64::MIN % -1` (sdiv wraps to i64::MIN, and
        // `i64::MIN - i64::MIN*(-1)` wraps back to 0), matching the interpreters.
        BinaryOp::Remainder => {
            fg.emit(sdiv_reg(2, 0, 1)); // x2 = x0 / x1
            fg.emit(msub_reg(0, 2, 1, 0)); // x0 = x0 - x2*x1
        }
        BinaryOp::Equal => emit_compare(fg, COND_EQ),
        BinaryOp::NotEqual => emit_compare(fg, COND_NE),
        BinaryOp::Less => emit_compare(fg, COND_LT),
        BinaryOp::LessEqual => emit_compare(fg, COND_LE),
        BinaryOp::Greater => emit_compare(fg, COND_GT),
        BinaryOp::GreaterEqual => emit_compare(fg, COND_GE),
        BinaryOp::And | BinaryOp::Or => unreachable!("short-circuit ops handled in lower_binary"),
    }
}

/// Emit an arithmetic/bitwise op: `x0 = x0 <op> x1`.
fn emit_arith(fg: &mut FnGen, op: BinaryOp) {
    let word = match op {
        BinaryOp::Add => add_reg(0, 0, 1),
        BinaryOp::Subtract => sub_reg(0, 0, 1),
        BinaryOp::Multiply => mul_reg(0, 0, 1),
        BinaryOp::Divide => sdiv_reg(0, 0, 1),
        BinaryOp::BitAnd => and_reg(0, 0, 1),
        BinaryOp::BitOr => orr_reg(0, 0, 1),
        BinaryOp::BitXor => eor_reg(0, 0, 1),
        BinaryOp::Shl => lslv_reg(0, 0, 1),
        BinaryOp::Shr => asrv_reg(0, 0, 1),
        _ => unreachable!("emit_arith only handles arithmetic/bitwise ops"),
    };
    fg.emit(word);
}

/// Emit a signed comparison: `cmp x0, x1; cset x0, <cond>` (result 0/1 in `x0`).
fn emit_compare(fg: &mut FnGen, cond: u32) {
    fg.emit(cmp_reg(0, 1));
    fg.emit(cset(0, cond));
}

/// Emit the branchless two's-complement `abs` of `reg` in place, using `tmp` as
/// scratch: `tmp = reg >> 63` (sign mask), `reg ^= tmp`, `reg -= tmp`. Matches
/// release `i64::abs`, so `abs(i64::MIN)` wraps back to `i64::MIN`. The unsigned
/// magnitude it produces (`|x|` reinterpreted as a `u64`) is exactly what
/// `gcd`'s Euclid loop consumes.
fn emit_abs_in_place(fg: &mut FnGen, reg: u32, tmp: u32) {
    fg.emit(asr_imm(tmp, reg, 63)); // tmp = sign mask (all 1s if reg < 0)
    fg.emit(eor_reg(reg, reg, tmp)); // reg ^= mask
    fg.emit(sub_reg(reg, reg, tmp)); // reg -= mask  -> |reg|
}

/// Lower an `i64` scalar math builtin (`abs`/`min`/`max`/`gcd`/`sign`/`clamp`)
/// inline, bit-for-bit with the interpreters. Returns `Ok(true)` when the call
/// matched a builtin (result left in `x0`), `Ok(false)` when it did not (so the
/// caller falls through to the ordinary `bl` path). Only the `i64` argument
/// shapes are recognized; an `f64` operand (which the AArch64 core has no float
/// support for) leaves the whole function to skip gracefully to the interpreters,
/// mirroring the x86-64 backend's builtin recognition.
fn lower_builtin_call(fg: &mut FnGen, name: &str, args: &[BytecodeExpr]) -> Result<bool, String> {
    // `abs(x)` on i64: the branchless two's-complement idiom, matching release
    // `i64::abs` (`abs(i64::MIN)` wraps to `i64::MIN`).
    if name == "abs" && args.len() == 1 && args[0].ty.name == "i64" {
        lower_expr(fg, &args[0])?; // x in x0
        emit_abs_in_place(fg, 0, 1);
        return Ok(true);
    }
    // `min(a, b)` / `max(a, b)` on i64: `cmp` + signed `csel`, matching
    // `i64::min`/`i64::max`. Evaluate left (spilled), right, then pop left.
    if (name == "min" || name == "max")
        && args.len() == 2
        && args[0].ty.name == "i64"
        && args[1].ty.name == "i64"
    {
        lower_expr(fg, &args[0])?; // left
        fg.emit(push_reg(0));
        lower_expr(fg, &args[1])?; // right in x0
        fg.emit(pop_reg(1)); // x1 = left, x0 = right
        fg.emit(cmp_reg(1, 0)); // compare left, right
        // min: take left when left < right; max: take left when left > right.
        let cond = if name == "min" { COND_LT } else { COND_GT };
        fg.emit(csel(0, 1, 0, cond)); // x0 = cond ? left : right
        return Ok(true);
    }
    // `gcd(a, b)` on i64: unsigned-magnitude Euclid with `udiv` + `msub`,
    // matching `gcd_i64`. Every magnitude is bounded by 2^63, which fits a u64,
    // so the u64 loop is bit-identical — including `gcd(i64::MIN, 0)`, where
    // `|i64::MIN|` = 2^63 reinterprets back to `i64::MIN`.
    if name == "gcd" && args.len() == 2 && args[0].ty.name == "i64" {
        lower_expr(fg, &args[0])?; // a
        fg.emit(push_reg(0));
        lower_expr(fg, &args[1])?; // b in x0
        fg.emit(pop_reg(1)); // x1 = a, x0 = b
        emit_abs_in_place(fg, 1, 2); // x1 = |a| = x (dividend)
        emit_abs_in_place(fg, 0, 2); // x0 = |b| = y (divisor)
        // while y(x0) != 0 { r = x % y; x = y; y = r }; result = x(x1).
        let top = fg.new_label();
        let done = fg.new_label();
        fg.bind(top);
        fg.emit_cbz(0, done); // y == 0 -> done
        fg.emit(udiv_reg(2, 1, 0)); // x2 = x / y  (unsigned quotient)
        fg.emit(msub_reg(3, 2, 0, 1)); // x3 = x - q*y = x % y
        fg.emit(mov_reg(1, 0)); // x = y
        fg.emit(mov_reg(0, 3)); // y = r
        fg.emit_b(top);
        fg.bind(done);
        fg.emit(mov_reg(0, 1)); // result = x
        return Ok(true);
    }
    // `sign(x)` on i64 -> `-1`/`0`/`1`: `(x > 0) - (x < 0)` via two `cset`s,
    // matching `i64::signum`.
    if name == "sign" && args.len() == 1 && args[0].ty.name == "i64" {
        lower_expr(fg, &args[0])?; // x in x0
        fg.emit(cmp_imm0(0));
        fg.emit(cset(1, COND_GT)); // x1 = (x > 0) ? 1 : 0
        fg.emit(cset(2, COND_LT)); // x2 = (x < 0) ? 1 : 0
        fg.emit(sub_reg(0, 1, 2)); // x0 = (x>0) - (x<0)
        return Ok(true);
    }
    // `clamp(x, lo, hi)` on i64: apply the upper clamp then the lower clamp (the
    // lower wins, applied last), each comparing the *original* x — matching the
    // interpreters' `if x < lo { lo } else if x > hi { hi } else { x }` for every
    // ordering of `lo`/`hi` (including `lo > hi`).
    if name == "clamp" && args.len() == 3 && args[0].ty.name == "i64" {
        lower_expr(fg, &args[0])?; // x
        fg.emit(push_reg(0));
        lower_expr(fg, &args[1])?; // lo
        fg.emit(push_reg(0));
        lower_expr(fg, &args[2])?; // hi in x0
        fg.emit(pop_reg(1)); // x1 = lo
        fg.emit(pop_reg(2)); // x2 = x (original, preserved)
        fg.emit(mov_reg(3, 2)); // x3 = result, seeded with x
        fg.emit(cmp_reg(2, 0)); // x vs hi
        fg.emit(csel(3, 0, 3, COND_GT)); // x > hi -> take hi
        fg.emit(cmp_reg(2, 1)); // x vs lo
        fg.emit(csel(3, 1, 3, COND_LT)); // x < lo -> take lo (wins)
        fg.emit(mov_reg(0, 3));
        return Ok(true);
    }
    Ok(false)
}

/// Lower a direct call: evaluate each argument, marshal into `x0..x7`, then `bl`.
fn lower_call(fg: &mut FnGen, name: &str, args: &[BytecodeExpr]) -> Result<(), String> {
    if lower_builtin_call(fg, name, args)? {
        return Ok(());
    }
    if !fg.callable.contains(name) {
        return Err(format!(
            "call to `{name}` is not an AArch64-eligible compiled function"
        ));
    }
    if args.len() > 8 {
        return Err(format!(
            "call to `{name}` has {} arguments, exceeding the eight AArch64 argument registers",
            args.len()
        ));
    }
    // Evaluate each argument in turn, spilling to the stack so a later argument's
    // evaluation (which may itself call) cannot clobber an earlier one.
    for arg in args {
        lower_expr(fg, arg)?;
        fg.emit(push_reg(0));
    }
    // Pop into x0..x(n-1); the stack top is the last argument, so unwind in
    // reverse to land each value in its register.
    for index in (0..args.len()).rev() {
        fg.emit(pop_reg(index as u32));
    }
    let offset = fg.code.len() as u32;
    fg.emit(BL_BASE);
    fg.calls.push(CallSite {
        offset,
        symbol: name.to_string(),
    });
    Ok(())
}

// -- Program assembly (ObjectModel construction) -----------------------------

/// The freestanding AArch64 Linux entry-point symbol (`ld`'s default entry).
const ELF_ENTRY_SYMBOL: &str = "_start";

/// Emit the AArch64 native program for `module` targeting `target` (an
/// `aarch64-unknown-linux-gnu` ELF object). Consumes the same [`BytecodeModule`]
/// as the x86-64 backend; see the module docs for the covered/skipped subset.
pub fn emit_aarch64_program(
    module: &BytecodeModule,
    target: &NativeTarget,
) -> Result<NativeProgram, NativeProgramError> {
    let mut skipped: Vec<NativeSkippedFunction> = Vec::new();
    let mut eligible: Vec<String> = Vec::new();
    for function in &module.functions {
        match signature_eligible(function) {
            Ok(()) => eligible.push(function.name.clone()),
            Err(reason) => skipped.push(NativeSkippedFunction {
                name: function.name.clone(),
                reason,
            }),
        }
    }

    // Fixpoint: lower every candidate; a body that calls a non-eligible function
    // (or uses an out-of-subset construct) is demoted and the loop retries, so a
    // call only ever targets a function that also compiles.
    let lowered = loop {
        let callable: HashSet<String> = eligible.iter().cloned().collect();
        let mut lowered: Vec<LoweredFn> = Vec::new();
        let mut demoted: Option<NativeSkippedFunction> = None;
        for name in &eligible {
            let function = module
                .functions
                .iter()
                .find(|f| &f.name == name)
                .expect("eligible name exists");
            match lower_function(function, &callable) {
                Ok(l) => lowered.push(l),
                Err(reason) => {
                    demoted = Some(NativeSkippedFunction {
                        name: name.clone(),
                        reason,
                    });
                    break;
                }
            }
        }
        match demoted {
            Some(skip) => {
                eligible.retain(|n| n != &skip.name);
                if !skipped.iter().any(|s| s.name == skip.name) {
                    skipped.push(skip);
                }
            }
            None => break lowered,
        }
    };

    let has_main = lowered.iter().any(|f| f.name == "main");
    let has_export = lowered
        .iter()
        .any(|f| module.export_functions.contains(&f.name));
    if lowered.is_empty() || (!has_main && !has_export) {
        let message = if lowered.is_empty() {
            "no functions were eligible for the AArch64 i64-scalar subset".to_string()
        } else {
            "neither `main` nor an `export fn` is eligible for the AArch64 i64-scalar subset"
                .to_string()
        };
        return Err(NativeProgramError {
            code: NATIVE_NO_ELIGIBLE_CODE,
            message,
            skipped,
        });
    }

    let compiled: Vec<String> = lowered.iter().map(|f| f.name.clone()).collect();
    let model = build_aarch64_object(&lowered, has_main);
    let entry_symbol = model.entry_symbol.clone().unwrap_or_default();

    Ok(NativeProgram {
        target: target.clone(),
        bytes: crate::elf_object::write_elf64(&model),
        entry_symbol,
        compiled,
        skipped,
        // The scalar core links no C runtime (freestanding `exit` syscall).
        import_libs: Vec::new(),
        // Direct PE emission is a Windows/COFF-only path; the AArch64 ELF keeps
        // the object-file + cross-linker workflow.
        pe_image: None,
    })
}

/// Assemble the lowered functions (and, when `emit_stub`, the freestanding
/// `_start`) into a single `.text` [`ObjectModel`] with `R_AARCH64_CALL26`
/// relocations for every `bl` call site.
fn build_aarch64_object(functions: &[LoweredFn], emit_stub: bool) -> ObjectModel {
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<(u64, String)> = Vec::new();

    if emit_stub {
        // _start: bl main; mov x8, #93 (exit); svc #0; brk (unreachable).
        // main's return value is already in x0 (the exit status).
        relocations.push((text.len() as u64, "main".to_string()));
        text.extend_from_slice(&BL_BASE.to_le_bytes());
        text.extend_from_slice(&movz(8, SYS_EXIT, 0).to_le_bytes());
        text.extend_from_slice(&SVC0.to_le_bytes());
        text.extend_from_slice(&BRK0.to_le_bytes());
    }

    let mut func_offsets: HashMap<String, u64> = HashMap::new();
    for function in functions {
        while !text.len().is_multiple_of(16) {
            text.extend_from_slice(&NOP.to_le_bytes());
        }
        let start = text.len() as u64;
        func_offsets.insert(function.name.clone(), start);
        for call in &function.calls {
            relocations.push((start + u64::from(call.offset), call.symbol.clone()));
        }
        text.extend_from_slice(&function.code);
    }

    let mut symbols: Vec<ObjectSymbol> = Vec::new();
    if emit_stub {
        symbols.push(ObjectSymbol {
            name: ELF_ENTRY_SYMBOL.to_string(),
            section: Some(0),
            value: 0,
            kind: ObjectSymbolKind::Function,
        });
    }
    for function in functions {
        symbols.push(ObjectSymbol {
            name: function.name.clone(),
            section: Some(0),
            value: func_offsets[&function.name],
            kind: ObjectSymbolKind::Function,
        });
    }

    let index_of = |name: &str| -> usize {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("every AArch64 call target is a compiled function")
    };
    let text_relocs: Vec<ObjectRelocation> = relocations
        .iter()
        .map(|(offset, symbol)| ObjectRelocation {
            offset: *offset,
            symbol: index_of(symbol),
            kind: ObjectRelocationKind::Aarch64Call26,
        })
        .collect();

    let text_len = text.len() as u64;
    let sections = vec![ObjectSection {
        kind: ObjectSectionKind::Text,
        data: text,
        size: text_len,
        relocations: text_relocs,
    }];

    ObjectModel {
        sections,
        symbols,
        entry_symbol: emit_stub.then(|| ELF_ENTRY_SYMBOL.to_string()),
        machine: ObjectMachine::Aarch64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_contract::native_target_for_triple;
    use lullaby_diagnostics::Span;

    /// Decode the little-endian 32-bit instruction word at byte `offset`.
    fn word_at(code: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(code[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn fixed_instruction_encodings_are_correct() {
        // Pin the encodings with a well-known fixed value against a reference
        // assembler (verified against `llvm-mc`/ARM ARM).
        assert_eq!(STP_FP_LR_PRE, 0xA9BF_7BFD, "stp x29, x30, [sp, #-16]!");
        assert_eq!(LDP_FP_LR_POST, 0xA8C1_7BFD, "ldp x29, x30, [sp], #16");
        assert_eq!(RET, 0xD65F_03C0, "ret");
        assert_eq!(SVC0, 0xD400_0001, "svc #0");
        assert_eq!(BL_BASE, 0x9400_0000, "bl #0");
        assert_eq!(movz(8, 93, 0), 0xD280_0BA8, "movz x8, #93");
        assert_eq!(add_reg(0, 0, 1), 0x8B01_0000, "add x0, x0, x1");
        assert_eq!(sub_reg(0, 0, 1), 0xCB01_0000, "sub x0, x0, x1");
        assert_eq!(mul_reg(0, 0, 1), 0x9B01_7C00, "mul x0, x0, x1");
        assert_eq!(sdiv_reg(0, 0, 1), 0x9AC1_0C00, "sdiv x0, x0, x1");
        assert_eq!(cmp_reg(0, 1), 0xEB01_001F, "cmp x0, x1");
        assert_eq!(mov_reg(1, 0), 0xAA00_03E1, "mov x1, x0");
        // The scalar-math-builtin encoders, pinned against known-good hex.
        assert_eq!(csel(0, 1, 2, COND_EQ), 0x9A82_0020, "csel x0, x1, x2, eq");
        assert_eq!(udiv_reg(0, 1, 2), 0x9AC2_0820, "udiv x0, x1, x2");
        assert_eq!(asr_imm(1, 0, 63), 0x937F_FC01, "asr x1, x0, #63");
        assert_eq!(msub_reg(3, 2, 0, 1), 0x9B00_8443, "msub x3, x2, x0, x1");
    }

    fn i64(kind: BytecodeExprKind) -> BytecodeExpr {
        BytecodeExpr {
            kind,
            ty: TypeRef::new("i64"),
            span: Span::new(1, 1),
        }
    }

    /// `fn main -> i64: return 6 * 7`
    fn mul_module() -> BytecodeModule {
        let expr = i64(BytecodeExprKind::Binary {
            left: Box::new(i64(BytecodeExprKind::Integer(6))),
            op: BinaryOp::Multiply,
            right: Box::new(i64(BytecodeExprKind::Integer(7))),
        });
        BytecodeModule {
            functions: vec![BytecodeFunction {
                name: "main".to_string(),
                params: Vec::new(),
                return_type: TypeRef::new("i64"),
                instructions: vec![BytecodeInstruction::Return(Some(expr))],
                span: Span::new(1, 1),
            }],
            structs: Vec::new(),
            enums: Vec::new(),
            impls: Vec::new(),
            trait_methods: Vec::new(),
            async_functions: Vec::new(),
            extern_functions: Vec::new(),
            extern_signatures: Vec::new(),
            export_functions: Vec::new(),
            closures: Vec::new(),
        }
    }

    #[test]
    fn emits_aarch64_elf_with_entry_and_exit() {
        let target = native_target_for_triple("aarch64-unknown-linux-gnu").unwrap();
        let program = emit_aarch64_program(&mul_module(), &target).expect("main compiles");
        let bytes = &program.bytes;
        // Valid AArch64 ELF64.
        assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
        assert_eq!(bytes[4], 2, "ELFCLASS64");
        assert_eq!(
            u16::from_le_bytes([bytes[18], bytes[19]]),
            183,
            "EM_AARCH64"
        );
        assert_eq!(program.entry_symbol, "_start");
        assert_eq!(program.compiled, vec!["main".to_string()]);
    }

    #[test]
    fn entry_stub_calls_main_and_exits() {
        let program = build_aarch64_object(
            &[LoweredFn {
                name: "main".to_string(),
                code: vec![],
                calls: vec![],
            }],
            true,
        );
        let text = &program.sections[0].data;
        // _start: bl #0 (reloc), movz x8,#93, svc #0, brk #0.
        assert_eq!(word_at(text, 0), BL_BASE, "bl main");
        assert_eq!(word_at(text, 4), movz(8, 93, 0), "movz x8, #93");
        assert_eq!(word_at(text, 8), SVC0, "svc #0");
        assert_eq!(word_at(text, 12), BRK0, "brk #0");
        // One CALL26 relocation targeting `main` at the bl site.
        let relocs = &program.sections[0].relocations;
        assert_eq!(relocs.len(), 1);
        assert_eq!(relocs[0].offset, 0);
        assert_eq!(relocs[0].kind, ObjectRelocationKind::Aarch64Call26);
        assert_eq!(program.symbols[relocs[0].symbol].name, "main");
    }

    #[test]
    fn function_body_has_prologue_and_ret() {
        let lowered = lower_function(
            &mul_module().functions[0],
            &HashSet::from(["main".to_string()]),
        )
        .expect("main lowers");
        // Prologue leads with the frame save; the body ends with `ret`.
        assert_eq!(word_at(&lowered.code, 0), STP_FP_LR_PRE, "prologue stp");
        let last = lowered.code.len() - 4;
        assert_eq!(word_at(&lowered.code, last), RET, "trailing ret");
    }

    #[test]
    fn non_scalar_function_is_skipped_not_miscompiled() {
        // A `main` returning a string is out of subset; it must be reported as
        // skipped, and with no other eligible function the program has none.
        let module = BytecodeModule {
            functions: vec![BytecodeFunction {
                name: "main".to_string(),
                params: Vec::new(),
                return_type: TypeRef::new("string"),
                instructions: vec![BytecodeInstruction::Return(Some(BytecodeExpr {
                    kind: BytecodeExprKind::String("hi".to_string()),
                    ty: TypeRef::new("string"),
                    span: Span::new(1, 1),
                }))],
                span: Span::new(1, 1),
            }],
            structs: Vec::new(),
            enums: Vec::new(),
            impls: Vec::new(),
            trait_methods: Vec::new(),
            async_functions: Vec::new(),
            extern_functions: Vec::new(),
            extern_signatures: Vec::new(),
            export_functions: Vec::new(),
            closures: Vec::new(),
        };
        let target = native_target_for_triple("aarch64-unknown-linux-gnu").unwrap();
        let error = emit_aarch64_program(&module, &target).expect_err("nothing eligible");
        assert_eq!(error.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(error.skipped.iter().any(|s| s.name == "main"));
    }

    // -- i64 scalar-math builtin lowering ------------------------------------

    /// Lower a single `i64` builtin call with integer-literal arguments, in
    /// isolation, and return the emitted (fixup-resolved) machine-code words.
    fn builtin_words(name: &str, args: &[i64]) -> Vec<u32> {
        let callable: HashSet<String> = HashSet::new();
        let mut fg = FnGen {
            code: Vec::new(),
            fixups: Vec::new(),
            labels: Vec::new(),
            calls: Vec::new(),
            loops: Vec::new(),
            locals: HashMap::new(),
            locals_size: 0,
            callable: &callable,
        };
        let arg_exprs: Vec<BytecodeExpr> = args
            .iter()
            .map(|v| i64(BytecodeExprKind::Integer(*v)))
            .collect();
        let handled =
            lower_builtin_call(&mut fg, name, &arg_exprs).expect("builtin lowers without error");
        assert!(handled, "`{name}` must be recognized as an i64 builtin");
        fg.resolve_fixups();
        fg.code
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    /// Whether `needle` appears as a contiguous subsequence of `haystack`.
    fn contains_seq(haystack: &[u32], needle: &[u32]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn abs_i64_lowers_to_sign_mask_idiom() {
        // asr x1, x0, #63 ; eor x0, x0, x1 ; sub x0, x0, x1
        let words = builtin_words("abs", &[-5]);
        assert!(
            contains_seq(
                &words,
                &[asr_imm(1, 0, 63), eor_reg(0, 0, 1), sub_reg(0, 0, 1)],
            ),
            "abs must emit the two's-complement sign-mask idiom: {words:08X?}"
        );
    }

    #[test]
    fn min_max_i64_lower_to_cmp_csel() {
        // min: cmp x1, x0 ; csel x0, x1, x0, lt  (take left when left < right)
        let min_words = builtin_words("min", &[7, 3]);
        assert!(
            contains_seq(&min_words, &[cmp_reg(1, 0), csel(0, 1, 0, COND_LT)]),
            "min must be a signed cmp + csel(lt): {min_words:08X?}"
        );
        // max: cmp x1, x0 ; csel x0, x1, x0, gt
        let max_words = builtin_words("max", &[7, 3]);
        assert!(
            contains_seq(&max_words, &[cmp_reg(1, 0), csel(0, 1, 0, COND_GT)]),
            "max must be a signed cmp + csel(gt): {max_words:08X?}"
        );
    }

    #[test]
    fn gcd_i64_lowers_to_unsigned_euclid() {
        // Two abs (magnitudes) + a udiv/msub remainder Euclid loop.
        let words = builtin_words("gcd", &[48, 36]);
        assert!(
            contains_seq(&words, &[udiv_reg(2, 1, 0), msub_reg(3, 2, 0, 1)]),
            "gcd must use an unsigned udiv + msub remainder: {words:08X?}"
        );
        // The abs of the dividend (|a| in x1) precedes the loop.
        assert!(
            contains_seq(
                &words,
                &[asr_imm(2, 1, 63), eor_reg(1, 1, 2), sub_reg(1, 1, 2)],
            ),
            "gcd must take the unsigned magnitude of its first operand: {words:08X?}"
        );
    }

    #[test]
    fn sign_i64_lowers_to_cmp_two_csets() {
        // cmp x0, #0 ; cset x1, gt ; cset x2, lt ; sub x0, x1, x2
        let words = builtin_words("sign", &[-7]);
        assert!(
            contains_seq(
                &words,
                &[
                    cmp_imm0(0),
                    cset(1, COND_GT),
                    cset(2, COND_LT),
                    sub_reg(0, 1, 2),
                ],
            ),
            "sign must be (x>0)-(x<0): {words:08X?}"
        );
    }

    #[test]
    fn clamp_i64_lowers_to_upper_then_lower_csel() {
        // upper: cmp x2, x0 ; csel x3, x0, x3, gt
        // lower: cmp x2, x1 ; csel x3, x1, x3, lt   (lower wins, applied last)
        let words = builtin_words("clamp", &[150, 0, 100]);
        assert!(
            contains_seq(
                &words,
                &[
                    cmp_reg(2, 0),
                    csel(3, 0, 3, COND_GT),
                    cmp_reg(2, 1),
                    csel(3, 1, 3, COND_LT),
                ],
            ),
            "clamp must apply the upper then the lower clamp: {words:08X?}"
        );
    }

    #[test]
    fn f64_builtin_operand_is_not_recognized() {
        // An f64-typed operand must NOT match the i64 builtin path (the whole
        // function then skips gracefully — there is no AArch64 float support).
        let callable: HashSet<String> = HashSet::new();
        let mut fg = FnGen {
            code: Vec::new(),
            fixups: Vec::new(),
            labels: Vec::new(),
            calls: Vec::new(),
            loops: Vec::new(),
            locals: HashMap::new(),
            locals_size: 0,
            callable: &callable,
        };
        let f64_arg = BytecodeExpr {
            kind: BytecodeExprKind::Float(1.5),
            ty: TypeRef::new("f64"),
            span: Span::new(1, 1),
        };
        assert!(
            !lower_builtin_call(&mut fg, "abs", std::slice::from_ref(&f64_arg)).unwrap(),
            "abs(f64) must not be recognized by the i64 builtin path"
        );
        assert!(
            !lower_builtin_call(&mut fg, "sign", std::slice::from_ref(&f64_arg)).unwrap(),
            "sign(f64) must not be recognized by the i64 builtin path"
        );
    }
}
