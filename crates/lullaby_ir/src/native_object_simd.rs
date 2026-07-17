//! Native backend: SSE2 primitives and SSE auto-vectorization. The packed
//! load/horizontal-fold/cpuid helpers and packed op enums, plus the vectorized
//! `for`-loop array reductions and element-wise maps (`i64`/`f64` sum, min/max,
//! map). Split out of native_object_stmt.rs; shared items via `use super::super::*`.
use super::super::*;

/// `imul rax, rax, imm32`.
pub(crate) fn emit_imul_rax_imm(code: &mut Vec<u8>, imm: i64) {
    code.extend_from_slice(&[0x48, 0x69, 0xC0]);
    code.extend_from_slice(&(imm as i32).to_le_bytes());
}

/// Emit an array-index bounds check on the index already in `rax`: trap with
/// `ud2` unless `0 <= rax < len`. A single UNSIGNED comparison (`cmp`+`jb`) covers
/// both ends — a negative index is a huge unsigned value, so it is `>= len` too.
/// Matches the interpreters' `L0413` (fail, don't read out of bounds); `ud2` is
/// the same deterministic trap the string-slice helper already uses. `len` is a
/// static array length that always fits `imm32`.
pub(crate) fn emit_bounds_check_rax(code: &mut Vec<u8>, len: i64) {
    code.extend_from_slice(&[0x48, 0x3D]); // cmp rax, imm32
    code.extend_from_slice(&(len as i32).to_le_bytes());
    code.extend_from_slice(&[0x72, 0x02]); // jb +2  (in bounds -> skip the trap)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2    (out of bounds -> fault)
}

/// Like [`emit_bounds_check_rax`] but compares the index in `rax` against a
/// **runtime** length held in the frame slot `[rbp - len_slot]` (a fat-pointer
/// array descriptor's length word). One UNSIGNED `cmp`+`jb` traps a negative or
/// over-large index, mirroring the interpreters' `L0413`.
pub(crate) fn emit_bounds_check_rax_against_slot(code: &mut Vec<u8>, len_slot: i32) {
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp - len_slot]
    code.extend_from_slice(&(-len_slot).to_le_bytes());
    code.extend_from_slice(&[0x72, 0x02]); // jb +2  (in bounds -> skip the trap)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2    (out of bounds -> fault)
}

/// Emit a hoisted bounds guard for an auto-vectorized `for` loop over an array of
/// `len` elements, given the counter's start slot and inclusive-end slot. The
/// vectorized loop bodies address the array inline (bypassing the per-access
/// [`emit_bounds_check_rax`]), so this one-time guard at loop entry keeps them
/// memory-safe: if the loop is NON-EMPTY (`start <= end`) it traps (`ud2`) unless
/// `start >= 0` and `end < len`. The emptiness guard means an empty range (e.g.
/// `for i from 0 to n-1` with `n == 0`, i.e. `end == -1`) never false-traps.
pub(crate) fn emit_loop_bounds_guard(code: &mut Vec<u8>, i_slot: i32, end_slot: i32, len: i64) {
    load_local(code, i_slot); // rax = start
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg skip (start > end -> empty, no access)
    let skip_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Non-empty: start >= 0 (rax still holds start) ...
    code.extend_from_slice(&[0x48, 0x83, 0xF8, 0x00]); // cmp rax, 0
    code.extend_from_slice(&[0x0F, 0x8C]); // jl trap
    let trap_a = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // ... and end < len.
    load_local(code, end_slot); // rax = end
    code.extend_from_slice(&[0x48, 0x3D]); // cmp rax, imm32 (len)
    code.extend_from_slice(&(len as i32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x83]); // jae trap (end >= len)
    let trap_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.push(0xE9); // jmp skip (in bounds)
    let skip_b = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // trap:
    patch_rel32(code, trap_a);
    patch_rel32(code, trap_b);
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    // skip:
    patch_rel32(code, skip_a);
    patch_rel32(code, skip_b);
}

// -- SSE2 integer-SIMD encoders (auto-vectorization) -------------------------
//
// x86-64 always provides SSE2, so these need no feature check. They operate on
// `xmm0` (the packed accumulator) and `xmm1` (a loaded pair), which are free in
// the i64-scalar functions that carry vectorizable array loops.

/// `movdqu xmm1, [rcx]` — load 16 unaligned bytes (two `i64`s) into `xmm1`.
pub(crate) fn emit_movdqu_xmm1_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x09]);
}

/// `movdqu xmm0, [rcx]` — load 16 unaligned bytes (two `i64`s) into `xmm0`.
pub(crate) fn emit_movdqu_xmm0_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x01]);
}

/// `movdqu [rcx], xmm0` — store the two packed `i64` lanes of `xmm0` to `[rcx]`.
pub(crate) fn emit_movdqu_rcx_from_xmm0(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF3, 0x0F, 0x7F, 0x01]);
}

/// Horizontally fold the two lanes of `xmm0` into `rax` with `op`: `movq rax,
/// xmm0` (low lane), `psrldq xmm0, 8` (bring the high lane low), `movq rcx,
/// xmm0`, then `rax = rax <op> rcx`. Leaves the packed reduction's scalar total
/// in `rax`.
pub(crate) fn emit_hfold_xmm0_into_rax(op: ReduceOp, code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // movq rax, xmm0 (low lane)
    code.extend_from_slice(&[0x66, 0x0F, 0x73, 0xD8, 0x08]); // psrldq xmm0, 8
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC1]); // movq rcx, xmm0 (high lane)
    op.emit_rax_rcx(code); // rax = rax <op> rcx
}

// -- SSE4.2 min/max vectorization with runtime CPUID dispatch -----------------
//
// 64-bit packed integer min/max needs a 64-bit packed compare (`pcmpgtq`), which
// is SSE4.2 — NOT in the SSE2 baseline every x86-64 CPU guarantees. So a min/max
// reduction emits BOTH a packed SSE4.2 path and a scalar fallback, and a one-time
// `cpuid` at loop entry picks between them at runtime: the produced binary uses
// the vector path on SSE4.2 hardware and the scalar path on an older CPU, staying
// correct everywhere. (`cpuid` runs once per loop entry, never per iteration, so
// its cost is amortized to nothing over the array.)

/// Emit a one-time CPUID SSE4.2 probe and a `jz` taken when SSE4.2 is ABSENT.
/// Returns the rel32 patch site of that jump for the caller to point at its scalar
/// fallback. `cpuid` clobbers eax/ebx/ecx/edx; rbx is callee-saved and may hold a
/// promoted local, so it is preserved around the probe. The SSE4.2 feature bit is
/// CPUID leaf 1, ECX bit 20; the ZF from `test` survives the `pop` (pop leaves
/// flags untouched).
pub(crate) fn emit_cpuid_sse42_probe(code: &mut Vec<u8>) -> usize {
    code.push(0x53); // push rbx (preserve a possibly-promoted local across cpuid)
    emit_mov_rax_imm(code, 1); // eax = 1 (feature leaf)
    code.extend_from_slice(&[0x0F, 0xA2]); // cpuid
    code.extend_from_slice(&[0x89, 0xC8]); // mov eax, ecx (feature bits -> scratch eax)
    code.push(0x5B); // pop rbx (restores the local; leaves ZF untouched)
    // test eax, 1<<20 (SSE4.2) ; jz fallback.
    code.extend_from_slice(&[0xA9]); // test eax, imm32
    code.extend_from_slice(&(1u32 << 20).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x84]); // jz rel32 (patched to the scalar fallback)
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    site
}

/// A vectorizable integer min/max reduction (`acc = max(acc, a[i])` /
/// `min(acc, a[i])`). Both are associative and commutative, so the two-lane packed
/// fold matches the scalar fold exactly.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum MinMaxOp {
    Min,
    Max,
}

impl MinMaxOp {
    /// The reduction identity, broadcast into both lanes of `xmm0` as the packed
    /// seed: `i64::MIN` for max, `i64::MAX` for min (so any real element wins).
    fn emit_packed_seed(self, code: &mut Vec<u8>) {
        let ident = match self {
            MinMaxOp::Max => i64::MIN,
            MinMaxOp::Min => i64::MAX,
        };
        emit_mov_rax_imm(code, ident);
        code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // movq xmm0, rax
        code.extend_from_slice(&[0x66, 0x0F, 0x6C, 0xC0]); // punpcklqdq xmm0,xmm0 (broadcast)
    }

    /// `xmm0 = minmax(xmm0, xmm1)` per lane, via the SSE4.2 `pcmpgtq` mask-blend.
    /// mask = (xmm0 > xmm1). Max keeps xmm0 where mask, xmm1 elsewhere; min is the
    /// mirror. Uses xmm2/xmm3 as scratch (free — these functions have no float
    /// locals).
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xD0]); // movdqa xmm2, xmm0
        code.extend_from_slice(&[0x66, 0x0F, 0x38, 0x37, 0xD1]); // pcmpgtq xmm2, xmm1 (mask = xmm0>xmm1)
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xDA]); // movdqa xmm3, xmm2 (copy mask)
        match self {
            MinMaxOp::Max => {
                code.extend_from_slice(&[0x66, 0x0F, 0xDB, 0xD8]); // pand  xmm3, xmm0 (mask & xmm0)
                code.extend_from_slice(&[0x66, 0x0F, 0xDF, 0xD1]); // pandn xmm2, xmm1 (~mask & xmm1)
            }
            MinMaxOp::Min => {
                code.extend_from_slice(&[0x66, 0x0F, 0xDB, 0xD9]); // pand  xmm3, xmm1 (mask & xmm1)
                code.extend_from_slice(&[0x66, 0x0F, 0xDF, 0xD0]); // pandn xmm2, xmm0 (~mask & xmm0)
            }
        }
        code.extend_from_slice(&[0x66, 0x0F, 0xEB, 0xDA]); // por xmm3, xmm2
        code.extend_from_slice(&[0x66, 0x0F, 0x6F, 0xC3]); // movdqa xmm0, xmm3 (result)
    }

    /// `rax = minmax(rax, rcx)` via `cmp`+`cmov` (branchless, exact for signed i64).
    fn emit_scalar_rax_rcx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x48, 0x39, 0xC8]); // cmp rax, rcx
        match self {
            // max: if rax < rcx, take rcx  -> cmovl rax, rcx
            MinMaxOp::Max => code.extend_from_slice(&[0x48, 0x0F, 0x4C, 0xC1]),
            // min: if rax > rcx, take rcx  -> cmovg rax, rcx
            MinMaxOp::Min => code.extend_from_slice(&[0x48, 0x0F, 0x4F, 0xC1]),
        }
    }

    /// `acc = minmax(acc, rax)`, honoring register promotion of `acc`. Preserves
    /// the loaded element (in rax) by moving it to rcx first.
    fn emit_reduce_into_acc(self, ctx: &NativeCtx, acc_slot: i32, code: &mut Vec<u8>) {
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (element)
        match ctx.promoted_reg(acc_slot) {
            Some(reg) => reg.to_rax(code),
            None => load_local(code, acc_slot),
        }
        self.emit_scalar_rax_rcx(code); // rax = minmax(acc, element)
        match ctx.promoted_reg(acc_slot) {
            Some(reg) => reg.from_rax(code),
            None => store_local(code, acc_slot),
        }
    }
}

/// The associative-and-commutative reductions that vectorize into an SSE2 packed
/// loop. Each is exact on `i64` — `+` is associative mod 2^64, and bitwise
/// `& | ^` are associative and commutative bit-for-bit — so the packed result is
/// identical to the scalar fold regardless of pairing order. (Multiplication is
/// also associative mod 2^64, but SSE2 has no 64-bit packed multiply, so it is
/// not offered here.)
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ReduceOp {
    Add,
    And,
    Or,
    Xor,
}

impl ReduceOp {
    /// Seed the packed accumulator (`xmm0`) with this operator's identity: all
    /// ones for `AND` (`pcmpeqd`), zero for `+`/`OR`/`XOR` (`pxor`).
    fn emit_packed_identity(self, code: &mut Vec<u8>) {
        match self {
            ReduceOp::And => code.extend_from_slice(&[0x66, 0x0F, 0x76, 0xC0]), // pcmpeqd xmm0,xmm0
            _ => code.extend_from_slice(&[0x66, 0x0F, 0xEF, 0xC0]),             // pxor xmm0,xmm0
        }
    }

    /// Combine the loaded pair (`xmm1`) into the packed accumulator: `xmm0 <op>= xmm1`.
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x66, 0x0F, 0xD4, 0xC1], // paddq
            ReduceOp::And => &[0x66, 0x0F, 0xDB, 0xC1], // pand
            ReduceOp::Or => &[0x66, 0x0F, 0xEB, 0xC1],  // por
            ReduceOp::Xor => &[0x66, 0x0F, 0xEF, 0xC1], // pxor
        });
    }

    /// `rax = rax <op> rcx`.
    fn emit_rax_rcx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x48, 0x01, 0xC8], // add rax, rcx
            ReduceOp::And => &[0x48, 0x21, 0xC8], // and rax, rcx
            ReduceOp::Or => &[0x48, 0x09, 0xC8],  // or  rax, rcx
            ReduceOp::Xor => &[0x48, 0x31, 0xC8], // xor rax, rcx
        });
    }

    /// `rax = rax <op> rdx`.
    fn emit_rax_rdx(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            ReduceOp::Add => &[0x48, 0x01, 0xD0], // add rax, rdx
            ReduceOp::And => &[0x48, 0x21, 0xD0], // and rax, rdx
            ReduceOp::Or => &[0x48, 0x09, 0xD0],  // or  rax, rdx
            ReduceOp::Xor => &[0x48, 0x31, 0xD0], // xor rax, rdx
        });
    }
}

/// The element-wise map operators that vectorize into an SSE2 packed loop. `+`/`-`
/// are exact mod 2^64; `& | ^` are exact bit-for-bit. All are per-lane, so the
/// packed store is identical to the scalar loop (including under destination
/// aliasing). Multiplication is excluded (no 64-bit packed multiply in SSE2).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum MapOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
}

impl MapOp {
    /// Combine the two loaded pairs: `xmm0 <op>= xmm1` (with `xmm0` = lhs, `xmm1` = rhs).
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            MapOp::Add => &[0x66, 0x0F, 0xD4, 0xC1], // paddq
            MapOp::Sub => &[0x66, 0x0F, 0xFB, 0xC1], // psubq
            MapOp::And => &[0x66, 0x0F, 0xDB, 0xC1], // pand
            MapOp::Or => &[0x66, 0x0F, 0xEB, 0xC1],  // por
            MapOp::Xor => &[0x66, 0x0F, 0xEF, 0xC1], // pxor
        });
    }

    /// Scalar-tail combine `lhs <op> rhs` given `rcx` = lhs, `rax` = rhs, leaving
    /// the result in `rax`. The commutative ops fold in place; `-` (non-commutative)
    /// computes `rcx - rax` then moves it into `rax`.
    fn emit_scalar_tail(self, code: &mut Vec<u8>) {
        match self {
            MapOp::Add => code.extend_from_slice(&[0x48, 0x01, 0xC8]), // add rax, rcx
            MapOp::And => code.extend_from_slice(&[0x48, 0x21, 0xC8]), // and rax, rcx
            MapOp::Or => code.extend_from_slice(&[0x48, 0x09, 0xC8]),  // or  rax, rcx
            MapOp::Xor => code.extend_from_slice(&[0x48, 0x31, 0xC8]), // xor rax, rcx
            MapOp::Sub => {
                code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax (lhs - rhs)
                code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
            }
        }
    }
}

/// The element-wise map operators over `array<f64>`: `+ - *`. Each lane is an
/// independent IEEE-754 double op, so the packed store is bit-for-bit identical to
/// the scalar loop (element-wise maps do NOT reorder, so unlike an f64 *reduction*
/// they stay parity-exact and need no fast-math opt-in).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum FloatMapOp {
    Add,
    Sub,
    Mul,
}

impl FloatMapOp {
    /// Packed: `xmm0 <op>= xmm1` (two f64 lanes). addpd/subpd/mulpd.
    fn emit_packed(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            FloatMapOp::Add => &[0x66, 0x0F, 0x58, 0xC1], // addpd
            FloatMapOp::Sub => &[0x66, 0x0F, 0x5C, 0xC1], // subpd
            FloatMapOp::Mul => &[0x66, 0x0F, 0x59, 0xC1], // mulpd
        });
    }
    /// Scalar tail: `xmm0 <op>= xmm1` (single f64). addsd/subsd/mulsd.
    fn emit_scalar(self, code: &mut Vec<u8>) {
        code.extend_from_slice(match self {
            FloatMapOp::Add => &[0xF2, 0x0F, 0x58, 0xC1], // addsd
            FloatMapOp::Sub => &[0xF2, 0x0F, 0x5C, 0xC1], // subsd
            FloatMapOp::Mul => &[0xF2, 0x0F, 0x59, 0xC1], // mulsd
        });
    }
}

/// An element-wise map's element type + operator: integer (`paddq`…) or float
/// (`addpd`…). Selected by the operand type at detection; the emitter branches on
/// it for the packed op and the scalar tail.
#[derive(Clone, Copy)]
pub(crate) enum MapKind {
    Int(MapOp),
    Float(FloatMapOp),
}

/// `movsd xmm1, [rcx]` — load a single f64 into xmm1 (scalar-tail rhs).
pub(crate) fn emit_movsd_xmm1_from_rcx(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x09]);
}

/// `sub rcx, imm32` (imm may be any i32; encodes the 32-bit immediate form).
pub(crate) fn emit_sub_rcx_imm(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x81, 0xE9]);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// A recognized `for i from S to E: acc <op>= a[i]` reduction over an `array<i64>`,
/// ready to vectorize. Element `k` of the array sits at
/// `[rbp - array_base_static + 8*k]` (elements ASCEND), matching the scalar
/// index addressing.
pub(crate) struct Reduction {
    acc_slot: i32,
    array_base_static: i32,
    array_len: i64,
    op: ReduceOp,
}

/// Recognize a `for counter from S to E: acc <op>= array[counter]` reduction,
/// where `acc` is an `i64` local, `array` is an `array<i64>`, and `<op>` is one of
/// the vectorizable reductions: `+` (spelled `acc += array[i]`) or bitwise
/// `& | ^` (spelled `acc = acc <op> array[i]`, either operand order — they are
/// commutative). Returns `None` for anything else so the caller falls back to the
/// scalar loop. Bounds `S`/`E` are lowered by the vectorizer itself.
pub(crate) fn detect_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<Reduction> {
    // Default ascending step of 1 only.
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    // Exactly one body statement, assigning a plain local (no field/index path).
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: assign_op,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() || acc == counter {
        return None;
    }
    // Determine the reduction operator and the `array[counter]` element read.
    // `+` uses the compound `acc += array[i]` (value is the bare element read);
    // bitwise ops use `acc = acc <op> array[i]` (value is a binary with `acc` as
    // one operand and the element read as the other, in either order).
    let (op, element) = match assign_op {
        AssignOp::Add => (ReduceOp::Add, value),
        AssignOp::Replace => {
            let BytecodeExprKind::Binary { left, op, right } = &value.kind else {
                return None;
            };
            let reduce_op = match op {
                BinaryOp::BitAnd => ReduceOp::And,
                BinaryOp::BitOr => ReduceOp::Or,
                BinaryOp::BitXor => ReduceOp::Xor,
                _ => return None,
            };
            // One operand must be exactly the accumulator variable; the other is
            // the element read. Bitwise ops are commutative, so accept both orders.
            let is_acc =
                |e: &BytecodeExpr| matches!(&e.kind, BytecodeExprKind::Variable(v) if v == acc);
            let element = if is_acc(left) {
                right
            } else if is_acc(right) {
                left
            } else {
                return None;
            };
            (reduce_op, element.as_ref())
        }
        _ => return None,
    };
    // The element must be `array[counter]`: resolve it as a dynamic i64 element
    // read (reusing the scalar addressing) whose index is exactly the counter.
    let BytecodeExprKind::Index { index, .. } = &element.kind else {
        return None;
    };
    let BytecodeExprKind::Variable(idx) = &index.kind else {
        return None;
    };
    if idx != counter {
        return None;
    }
    let place = resolve_read_place(ctx, element).ok()?;
    let ScalarPlace::Dynamic {
        base_slot,
        const_bytes,
        elem_bytes,
        index_len,
        ..
    } = place
    else {
        return None;
    };
    // An 8-byte element stride. A packed narrow element (stride 1/2/4) can never
    // reach here — `resolve_read_place` is the STRICT i64 resolver and refuses a
    // `NativeType::Narrow` element outright — but this keeps the 8-byte lane
    // assumption explicit at the point the vectorizer depends on it.
    if elem_bytes != 8 {
        return None; // only a contiguous 8-byte-element array is 16-byte packable
    }
    // The accumulator must be a plain `i64` local, distinct from the array root.
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    Some(Reduction {
        acc_slot: acc_local.slot,
        array_base_static: base_slot - const_bytes as i32,
        array_len: index_len,
        op,
    })
}

/// A recognized `for i from S to E: acc = max(acc, a[i])` / `min(acc, a[i])`
/// reduction over a contiguous `array<i64>`. Vectorized via SSE4.2 with a runtime
/// CPUID gate and scalar fallback (see [`lower_native_minmax_reduction`]).
pub(crate) struct MinMaxReduction {
    acc_slot: i32,
    array_base_static: i32,
    array_len: i64,
    op: MinMaxOp,
}

/// Recognize `for counter from S to E: acc = max(acc, array[counter])` (or `min`),
/// where `acc` is an `i64` local and `array` is a contiguous `array<i64>`. `max`/
/// `min` are commutative, so the accumulator may be either argument. Returns `None`
/// for anything else so the caller falls back to the scalar loop.
pub(crate) fn detect_minmax_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<MinMaxReduction> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: AssignOp::Replace,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() || acc == counter {
        return None;
    }
    // value must be `max(_, _)` / `min(_, _)` with exactly two args.
    let BytecodeExprKind::Call { name, args } = &value.kind else {
        return None;
    };
    let op = match name.as_str() {
        "max" => MinMaxOp::Max,
        "min" => MinMaxOp::Min,
        _ => return None,
    };
    let [a0, a1] = args.as_slice() else {
        return None;
    };
    // One argument is exactly the accumulator; the other is `array[counter]`.
    let is_acc = |e: &BytecodeExpr| matches!(&e.kind, BytecodeExprKind::Variable(v) if v == acc);
    let element = if is_acc(a0) {
        a1
    } else if is_acc(a1) {
        a0
    } else {
        return None;
    };
    let BytecodeExprKind::Index { index, .. } = &element.kind else {
        return None;
    };
    let BytecodeExprKind::Variable(idx) = &index.kind else {
        return None;
    };
    if idx != counter {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_bytes,
        elem_bytes,
        index_len,
        ..
    } = resolve_read_place(ctx, element).ok()?
    else {
        return None;
    };
    // See above: an 8-byte lane is required, and a narrow element cannot reach
    // this detector through the strict resolver.
    if elem_bytes != 8 {
        return None;
    }
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::I64) {
        return None;
    }
    Some(MinMaxReduction {
        acc_slot: acc_local.slot,
        array_base_static: base_slot - const_bytes as i32,
        array_len: index_len,
        op,
    })
}

/// A recognized `for i from S to E: c[i] = a[i] <op> b[i]` element-wise map over
/// contiguous `array<i64>`s (`op` is `+ - & | ^`). Element `k` of each array sits
/// at `[rbp - base + 8*k]` (ASCENDING), matching the scalar index addressing.
pub(crate) struct ElementwiseMap {
    dest_base: i32,
    lhs_base: i32,
    rhs_base: i32,
    /// The smallest of the three arrays' lengths — the loop must stay within all
    /// of dest/lhs/rhs, so the hoisted bounds guard checks against the minimum.
    min_len: i64,
    kind: MapKind,
}

/// True when `expr` is exactly the loop counter `counter`.
pub(crate) fn index_is_counter(expr: &BytecodeExpr, counter: &str) -> bool {
    matches!(&expr.kind, BytecodeExprKind::Variable(v) if v == counter)
}

/// If `expr` is `array[counter]` over a contiguous `i64` array, return the array's
/// static element-0 base (`base_slot - const_bytes`) and its element count.
pub(crate) fn indexed_i64_base(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
    counter: &str,
) -> Option<(i32, i64)> {
    let BytecodeExprKind::Index { index, .. } = &expr.kind else {
        return None;
    };
    if !index_is_counter(index, counter) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_bytes,
        elem_bytes,
        index_len,
        ..
    } = resolve_read_place(ctx, expr).ok()?
    else {
        return None;
    };
    // See above: an 8-byte lane is required, and a narrow element cannot reach
    // this detector through the strict resolver.
    if elem_bytes != 8 {
        return None;
    }
    Some((base_slot - const_bytes as i32, index_len))
}

/// Like [`indexed_i64_base`] but for a contiguous `array<f64>` element read.
pub(crate) fn indexed_f64_base(
    ctx: &NativeCtx,
    expr: &BytecodeExpr,
    counter: &str,
) -> Option<(i32, i64)> {
    let BytecodeExprKind::Index { index, .. } = &expr.kind else {
        return None;
    };
    if !index_is_counter(index, counter) {
        return None;
    }
    let (place, elem_ty) = resolve_read_place_typed(ctx, expr).ok()?;
    if !matches!(elem_ty, NativeType::F64) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_bytes,
        elem_bytes,
        index_len,
        ..
    } = place
    else {
        return None;
    };
    // See above: an 8-byte lane is required, and a narrow element cannot reach
    // this detector through the strict resolver.
    if elem_bytes != 8 {
        return None;
    }
    Some((base_slot - const_bytes as i32, index_len))
}

/// Recognize `for counter from S to E: dest[counter] = lhs[counter] (+|-)
/// rhs[counter]` over contiguous `array<i64>`s (default step 1). Returns `None`
/// for anything else so the caller falls back to the scalar loop.
pub(crate) fn detect_elementwise_map(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<ElementwiseMap> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: dest,
            path,
            op: AssignOp::Replace,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    // The destination is `dest[counter]`.
    let [BytecodePlace::Index(dest_index)] = path.as_slice() else {
        return None;
    };
    if !index_is_counter(dest_index, counter) {
        return None;
    }
    // The value is `lhs[counter] <op> rhs[counter]` for a vectorizable `op`. Try the
    // i64 forms first (`+ - & | ^`); if the operands are `array<f64>` instead, try
    // the float forms (`+ - *` via addpd/subpd/mulpd — bit-exact, per-lane).
    let BytecodeExprKind::Binary { left, op, right } = &value.kind else {
        return None;
    };
    let (lhs_base, lhs_len, rhs_base, rhs_len, kind, dest_float) = if let Some(map_op) = match op {
        BinaryOp::Add => Some(MapOp::Add),
        BinaryOp::Subtract => Some(MapOp::Sub),
        BinaryOp::BitAnd => Some(MapOp::And),
        BinaryOp::BitOr => Some(MapOp::Or),
        BinaryOp::BitXor => Some(MapOp::Xor),
        _ => None,
    }
    .filter(|_| indexed_i64_base(ctx, left, counter).is_some())
    {
        let (lb, ll) = indexed_i64_base(ctx, left, counter)?;
        let (rb, rl) = indexed_i64_base(ctx, right, counter)?;
        (lb, ll, rb, rl, MapKind::Int(map_op), false)
    } else {
        let fop = match op {
            BinaryOp::Add => FloatMapOp::Add,
            BinaryOp::Subtract => FloatMapOp::Sub,
            BinaryOp::Multiply => FloatMapOp::Mul,
            _ => return None,
        };
        let (lb, ll) = indexed_f64_base(ctx, left, counter)?;
        let (rb, rl) = indexed_f64_base(ctx, right, counter)?;
        (lb, ll, rb, rl, MapKind::Float(fop), true)
    };
    let (dest_place, dest_ty) = resolve_scalar_place_typed(ctx, dest, path).ok()?;
    // The destination element type must match the operands (i64 or f64).
    if dest_float != matches!(dest_ty, NativeType::F64) {
        return None;
    }
    let ScalarPlace::Dynamic {
        base_slot,
        const_bytes,
        elem_bytes,
        index_len: dest_len,
        ..
    } = dest_place
    else {
        return None;
    };
    // See above: an 8-byte lane is required, and a narrow element cannot reach
    // this detector through the strict resolver.
    if elem_bytes != 8 {
        return None;
    }
    Some(ElementwiseMap {
        dest_base: base_slot - const_bytes as i32,
        lhs_base,
        rhs_base,
        min_len: dest_len.min(lhs_len).min(rhs_len),
        kind,
    })
}

/// Vectorize an element-wise map `dest[i] = lhs[i] (+|-) rhs[i]` into an SSE2
/// packed loop (two `i64` lanes per iteration) with a scalar tail for the odd
/// element. Lane order is preserved because all three arrays share the same
/// ascending `[rbp - base + 8*k]` addressing, so this is bit-for-bit identical to
/// the scalar loop (and correct under `dest` aliasing `lhs`/`rhs`).
pub(crate) fn lower_native_vectorized_map(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    map: &ElementwiseMap,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;

    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard against the smallest of dest/lhs/rhs (all three are
    // indexed inline below without a per-access check).
    emit_loop_bounds_guard(code, i_slot, end_slot, map.min_len);

    // `rcx = rbp - base + rdx - bias`. Elements ASCEND from element 0 (at
    // `[rbp - base]`), so the address ADDS the scaled index. `bias` folds a
    // constant element offset into the SAME `sub rcx, imm32` the base already
    // needs, costing no extra instruction: the main loop holds `rdx = 8*(i+1)` but
    // wants `&a[i]` (bias 8), while the scalar remainder holds `rdx = 8*i` and
    // wants `&a[i]` (bias 0).
    let block_addr = |code: &mut Vec<u8>, base: i32, bias: i32| {
        code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
        code.extend_from_slice(&[0x48, 0x01, 0xD1]); // add rcx, rdx
        emit_sub_rcx_imm(code, base + bias);
    };

    // --- main SIMD loop: while i + 1 <= end, map the pair (i, i+1) ---
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // The 16-byte block covering (i, i+1) starts at element `i` — the LOWER
    // address under the ascending layout — so `block_addr` biases by 8.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (block offset)
    block_addr(code, map.lhs_base, 8);
    emit_movdqu_xmm0_from_rcx(code);
    block_addr(code, map.rhs_base, 8);
    emit_movdqu_xmm1_from_rcx(code);
    match map.kind {
        MapKind::Int(op) => op.emit_packed(code),
        MapKind::Float(op) => op.emit_packed(code),
    }
    block_addr(code, map.dest_base, 8);
    emit_movdqu_rcx_from_xmm0(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // --- scalar remainder: while i <= end, dest[i] = lhs[i] <op> rhs[i] ---
    patch_rel32(code, after_main_site);
    let rem_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rdx = 8*i (element offset; the scalar addressing uses &array[i]).
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
    match map.kind {
        MapKind::Int(op) => {
            // rax = lhs[i]
            block_addr(code, map.lhs_base, 0);
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            code.push(0x50); // push rax (lhs)
            // rax = rhs[i]
            block_addr(code, map.rhs_base, 0);
            code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
            code.push(0x59); // pop rcx (rcx = lhs)
            op.emit_scalar_tail(code); // rax = lhs <op> rhs
            // dest[i] = rax
            code.push(0x50); // push rax (result)
            block_addr(code, map.dest_base, 0);
            code.push(0x58); // pop rax (result)
            code.extend_from_slice(&[0x48, 0x89, 0x01]); // mov [rcx], rax
        }
        MapKind::Float(op) => {
            // xmm0 = lhs[i] ; xmm1 = rhs[i] ; xmm0 <op>= xmm1 ; dest[i] = xmm0.
            block_addr(code, map.lhs_base, 0);
            load_float_from_rcx(code, FloatWidth::F64); // movsd xmm0, [rcx]
            block_addr(code, map.rhs_base, 0);
            emit_movsd_xmm1_from_rcx(code); // movsd xmm1, [rcx]
            op.emit_scalar(code); // addsd/subsd/mulsd xmm0, xmm1
            block_addr(code, map.dest_base, 0);
            store_float_from_rcx(code, FloatWidth::F64); // movsd [rcx], xmm0
        }
    }
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done_site);
    Ok(())
}

/// `acc = acc <op> rax`, honoring register promotion of the accumulator.
/// Preserves the operand (the loaded element or packed total) in `rdx` while
/// loading/combining/storing `acc`.
pub(crate) fn emit_reduce_into_acc(
    ctx: &NativeCtx,
    acc_slot: i32,
    op: ReduceOp,
    code: &mut Vec<u8>,
) {
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (save operand)
    match ctx.promoted_reg(acc_slot) {
        Some(reg) => reg.to_rax(code), // rax = acc
        None => load_local(code, acc_slot),
    }
    op.emit_rax_rdx(code); // rax = acc <op> rdx
    match ctx.promoted_reg(acc_slot) {
        Some(reg) => reg.from_rax(code), // acc = rax
        None => store_local(code, acc_slot),
    }
}

/// Emit the vectorized reduction `acc <op>= a[S..=E]`. Combines the array two
/// `i64`s at a time with the packed op (`paddq`/`pand`/`por`/`pxor`), horizontally
/// folds the packed accumulator into `acc`, then a scalar tail loop handles a
/// final odd element. The counter and bound live on the stack for this loop (the
/// counter is dead after it). Every offered op is associative (and, for bitwise,
/// commutative) and exact on `i64`, so the total matches the scalar fold
/// bit-for-bit regardless of the pairing order.
pub(crate) fn lower_native_vectorized_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    reduction: &Reduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    let base = reduction.array_base_static;
    let op = reduction.op;

    // i = start ; end_local = end
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard: the inline-addressed loop below bypasses the per-access
    // check, so trap here if the (non-empty) index range escapes the array.
    emit_loop_bounds_guard(code, i_slot, end_slot, reduction.array_len);
    op.emit_packed_identity(code); // packed accumulator = identity

    // --- main SIMD loop: while i + 1 <= end, combine the pair (a[i], a[i+1]) ---
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1  -> rax = i+1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main (i+1 > end)
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // The 16-byte block covering (a[i], a[i+1]) starts at element `i` (the lower
    // address under the ascending layout): addr = rbp - base + 8*i. rax holds i+1,
    // so the +8 it carries is folded into the base displacement (`base + 8`) — no
    // extra instruction versus the previous descending form.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
    emit_sub_rcx_imm(code, base + 8); // rcx = &a[i] (covers a[i],a[i+1])
    emit_movdqu_xmm1_from_rcx(code);
    op.emit_packed(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // after_main: fold the two packed lanes into acc.
    patch_rel32(code, after_main_site);
    emit_hfold_xmm0_into_rax(op, code); // rax = lane0 <op> lane1
    emit_reduce_into_acc(ctx, reduction.acc_slot, op, code);

    // --- scalar remainder: while i <= end, combine a[i] ---
    let rem_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done (i > end)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // load a[i]: addr = rbp - base + 8*i
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
    emit_sub_rcx_imm(code, base);
    code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
    emit_reduce_into_acc(ctx, reduction.acc_slot, op, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done_site);
    Ok(())
}

/// Emit a min/max reduction `acc = max(acc, a[S..=E])` (or `min`) with runtime
/// CPUID dispatch: an SSE4.2 packed path (`pcmpgtq` mask-blend, two `i64` lanes per
/// iteration + scalar tail) when the CPU has SSE4.2, else a plain scalar fold. Both
/// paths fold the array into `acc`, so the result is identical (min/max is
/// associative and commutative). The `cpuid` probe runs once at loop entry.
pub(crate) fn lower_native_minmax_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    red: &MinMaxReduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    let base = red.array_base_static;
    let op = red.op;

    // i = start ; end_local = end  (set before the probe so the scalar fallback,
    // which the probe jumps to BEFORE any SIMD code runs, starts from `start`).
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    // Hoisted bounds guard (both the SSE4.2 and scalar paths below index inline).
    emit_loop_bounds_guard(code, i_slot, end_slot, red.array_len);

    // Runtime CPUID gate: jump to the scalar fallback when SSE4.2 is absent.
    let fallback_site = emit_cpuid_sse42_probe(code);

    // --- SSE4.2 packed path ---
    op.emit_packed_seed(code); // xmm0 = identity broadcast to both lanes
    let main_top = code.len();
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1  (i+1)
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
    // rax carries 8*(i+1); fold the +8 into the base so `rcx = &a[i]` (block
    // start, covers a[i],a[i+1]) with no extra instruction.
    emit_sub_rcx_imm(code, base + 8);
    emit_movdqu_xmm1_from_rcx(code);
    op.emit_packed(code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // Fold the two packed lanes, then into acc.
    patch_rel32(code, after_main_site);
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]); // movq rax, xmm0 (lane0)
    code.extend_from_slice(&[0x66, 0x0F, 0x73, 0xD8, 0x08]); // psrldq xmm0, 8
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC1]); // movq rcx, xmm0 (lane1)
    op.emit_scalar_rax_rcx(code); // rax = minmax(lane0, lane1)
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);

    // Scalar tail for the odd final element (SSE4.2 path).
    let tail_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg simd_done
    let simd_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_load_array_elem(code, i_slot, base); // rax = a[i]
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, tail_top);
    patch_rel32(code, simd_done_site);
    // Skip the scalar fallback.
    code.push(0xE9);
    let done_jmp_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // --- scalar fallback (no SSE4.2): fold every element into acc ---
    patch_rel32(code, fallback_site);
    let scalar_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]); // cmp rax, [rbp-end_slot]
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let scalar_done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    emit_load_array_elem(code, i_slot, base); // rax = a[i]
    op.emit_reduce_into_acc(ctx, red.acc_slot, code);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, scalar_top);

    patch_rel32(code, scalar_done_site);
    patch_rel32(code, done_jmp_site); // both fallthroughs land here
    Ok(())
}

/// `rax = a[i]` for a contiguous i64 array whose element 0 sits at `rbp - base`:
/// addr = rbp - base + 8*i (elements ASCEND from element 0).
pub(crate) fn emit_load_array_elem(code: &mut Vec<u8>, i_slot: i32, base: i32) {
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
    code.extend_from_slice(&[0x48, 0x01, 0xC1]); // add rcx, rax
    emit_sub_rcx_imm(code, base);
    code.extend_from_slice(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
}

/// `movsd xmm1, [rbp - slot]` — load an f64 local into xmm1.
pub(crate) fn emit_movsd_xmm1_from_local(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x8D]); // movsd xmm1, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// A recognized f64 reduction: `for i: acc += a[i]` (sum) or `acc += a[i]*b[i]`
/// (dot). Only vectorized under `--fast-math` (the 2-lane packed fold reorders the
/// additions). `rhs_base` is `Some` for a dot product, `None` for a plain sum.
pub(crate) struct F64Reduction {
    acc_slot: i32,
    lhs_base: i32,
    rhs_base: Option<i32>,
    array_len: i64,
}

/// Recognize `for counter from S to E: acc += a[counter]` or
/// `acc += a[counter] * b[counter]` where `acc` is an `f64` local and the arrays are
/// `array<f64>`. Returns `None` for anything else (scalar fallback).
pub(crate) fn detect_f64_reduction(
    ctx: &NativeCtx,
    counter: &str,
    step: Option<&BytecodeExpr>,
    body: &[BytecodeInstruction],
) -> Option<F64Reduction> {
    match step {
        None => {}
        Some(expr) => match expr.kind {
            BytecodeExprKind::Integer(1) => {}
            _ => return None,
        },
    }
    let [
        BytecodeInstruction::Assign {
            name: acc,
            path,
            op: AssignOp::Add,
            value,
            ..
        },
    ] = body
    else {
        return None;
    };
    if !path.is_empty() {
        return None;
    }
    let acc_local = ctx.locals.get(acc)?;
    if !matches!(acc_local.ty, NativeType::F64) {
        return None;
    }
    match &value.kind {
        // sum: acc += a[i]
        BytecodeExprKind::Index { .. } => {
            let (lhs_base, len) = indexed_f64_base(ctx, value, counter)?;
            Some(F64Reduction {
                acc_slot: acc_local.slot,
                lhs_base,
                rhs_base: None,
                array_len: len,
            })
        }
        // dot: acc += a[i] * b[i]
        BytecodeExprKind::Binary {
            left,
            op: BinaryOp::Multiply,
            right,
        } => {
            let (lhs_base, ll) = indexed_f64_base(ctx, left, counter)?;
            let (rhs_base, rl) = indexed_f64_base(ctx, right, counter)?;
            Some(F64Reduction {
                acc_slot: acc_local.slot,
                lhs_base,
                rhs_base: Some(rhs_base),
                array_len: ll.min(rl),
            })
        }
        _ => None,
    }
}

/// Emit an f64 sum/dot reduction with a 2-lane packed accumulator: `pxor` seeds
/// `xmm0` to `0.0`; the main loop `movdqu`-loads a pair (and `mulpd`s the b-pair
/// for a dot), `addpd`s into `xmm0`; then the two lanes fold (`unpckhpd`+`addsd`,
/// SSE2) into the `acc` local, and a scalar tail (`addsd`/`mulsd`) handles the odd
/// element. `--fast-math` only (the packed pairing reorders the additions).
pub(crate) fn lower_native_f64_reduction(
    ctx: &mut NativeCtx,
    counter: &str,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    red: &F64Reduction,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let i_slot = ctx.local_slot(counter)?;
    let end_slot = ctx.local_slot(&format!("{counter}__end"))?;
    lower_native_expr(ctx, start, code)?;
    store_local(code, i_slot);
    lower_native_expr(ctx, end, code)?;
    store_local(code, end_slot);
    emit_loop_bounds_guard(code, i_slot, end_slot, red.array_len);
    code.extend_from_slice(&[0x66, 0x0F, 0xEF, 0xC0]); // pxor xmm0, xmm0 (packed acc = 0)

    // `rcx = rbp - base + rdx - bias` (ASCENDING). As in the map lowering, `bias`
    // folds a constant element offset into the base's own `sub rcx, imm32`: the
    // main loop holds `rdx = 8*(i+1)` and wants `&a[i]` (bias 8); the scalar tail
    // holds `rdx = 8*i` and wants `&a[i]` (bias 0).
    let block_addr = |code: &mut Vec<u8>, base: i32, bias: i32| {
        code.extend_from_slice(&[0x48, 0x89, 0xE9]); // mov rcx, rbp
        code.extend_from_slice(&[0x48, 0x01, 0xD1]); // add rcx, rdx
        emit_sub_rcx_imm(code, base + bias);
    };

    // --- main SIMD loop: while i + 1 <= end, accumulate the pair ---
    let main_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg after_main
    let after_main = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // The pair block starts at element `i` (the lower address), so bias by 8.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3  -> 8*(i+1)
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (block offset)
    block_addr(code, red.lhs_base, 8);
    emit_movdqu_xmm1_from_rcx(code); // xmm1 = a pair
    if let Some(rhs_base) = red.rhs_base {
        block_addr(code, rhs_base, 8);
        code.extend_from_slice(&[0xF3, 0x0F, 0x6F, 0x11]); // movdqu xmm2, [rcx] (b pair)
        code.extend_from_slice(&[0x66, 0x0F, 0x59, 0xCA]); // mulpd xmm1, xmm2
    }
    code.extend_from_slice(&[0x66, 0x0F, 0x58, 0xC1]); // addpd xmm0, xmm1
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x02]); // add rax, 2
    store_local(code, i_slot);
    emit_jmp_to(code, main_top);

    // after_main: fold the two lanes, then add into acc.
    patch_rel32(code, after_main);
    code.extend_from_slice(&[0x66, 0x0F, 0x28, 0xC8]); // movapd xmm1, xmm0
    code.extend_from_slice(&[0x66, 0x0F, 0x15, 0xC9]); // unpckhpd xmm1, xmm1 (high lane -> low)
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1 (lane0 = lane0+lane1)
    emit_movsd_xmm1_from_local(code, red.acc_slot); // xmm1 = acc
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1 (acc + packed sum)
    store_float_local(code, red.acc_slot, FloatWidth::F64);

    // --- scalar tail: while i <= end, acc += a[i] (* b[i]) ---
    let rem_top = code.len();
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x3B, 0x85]);
    code.extend_from_slice(&(-end_slot).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x8F]); // jg done
    let done = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
    block_addr(code, red.lhs_base, 0);
    load_float_from_rcx(code, FloatWidth::F64); // xmm0 = a[i]
    if let Some(rhs_base) = red.rhs_base {
        block_addr(code, rhs_base, 0);
        emit_movsd_xmm1_from_rcx(code); // xmm1 = b[i]
        code.extend_from_slice(&[0xF2, 0x0F, 0x59, 0xC1]); // mulsd xmm0, xmm1
    }
    emit_movsd_xmm1_from_local(code, red.acc_slot); // xmm1 = acc
    code.extend_from_slice(&[0xF2, 0x0F, 0x58, 0xC1]); // addsd xmm0, xmm1
    store_float_local(code, red.acc_slot, FloatWidth::F64);
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x01]); // add rax, 1
    store_local(code, i_slot);
    emit_jmp_to(code, rem_top);

    patch_rel32(code, done);
    Ok(())
}
