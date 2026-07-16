//! Operation lowering for the native backend: floating-point (SSE scalar),
//! overflow-aware arithmetic, and the string/list/map builtin op lowering. Split
//! out of native_object.rs; these emit machine code into the current function's
//! buffer and recurse back into the parent's expression lowering via `use super::*`.

use super::*;

// -- Floating-point lowering (SSE scalar, XMM0/XMM1) -------------------------
//
// Float values live in XMM registers: `f64` as a `double`, `f32` as a `single`
// kept rounded to single precision after every operation (matching the
// interpreter's real `f32` storage). The float lowerer is a small stack machine
// over `xmm0`, spilling the left operand of a binary op to the machine stack
// (`sub rsp,16; movsd [rsp],xmm0`) so `xmm0` is free to evaluate the right.
//
// Float literals are materialized without any `.rdata` constant: the IEEE-754
// bit pattern is loaded into a GPR (`mov rax, imm64` for f64, `mov eax, imm32`
// for f32) and moved into an XMM register (`movq`/`movd`). This keeps every
// float function self-contained (no new relocations or data symbols).

/// Lower a float-valued expression, leaving its value in `xmm0` and returning
/// the [`FloatWidth`] of the result (`f64` as a double, `f32` rounded to single).
/// Handles float literals, float locals, the `to_f32`/`to_f64` conversions, and
/// `f64`/`f32` arithmetic (`+ - * /`). Anything else (e.g. a float-returning
/// call, a math builtin) is rejected so the enclosing function skips gracefully.
pub(crate) fn lower_native_float_expr(
    ctx: &mut NativeCtx,
    expr: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<FloatWidth, String> {
    match &expr.kind {
        BytecodeExprKind::Float(value) => {
            // A bare float literal's static type says whether it is f64 or f32.
            // The type checker pins every float literal to a concrete type.
            let width = FloatWidth::from_type_name(expr.ty.name.as_str())
                .ok_or_else(|| format!("float literal has non-float type `{}`", expr.ty.name))?;
            emit_float_immediate(code, *value, width);
            Ok(width)
        }
        BytecodeExprKind::Variable(name) => {
            let local = ctx.local(name)?;
            let width = match local.ty {
                NativeType::F64 => FloatWidth::F64,
                NativeType::F32 => FloatWidth::F32,
                _ => return Err(format!("`{name}` is not a float local")),
            };
            let slot = local.slot;
            load_float_local(code, slot, width);
            Ok(width)
        }
        BytecodeExprKind::Call { name, args } => {
            // `to_f32(x f64) -> f32`: evaluate the f64 argument, then round it to
            // single precision with `cvtsd2ss`.
            if name == "to_f32" {
                if args.len() != 1 {
                    return Err("`to_f32` takes exactly one argument".to_string());
                }
                let arg_width = lower_native_float_expr(ctx, &args[0], code)?;
                if arg_width != FloatWidth::F64 {
                    return Err("`to_f32` expects an f64 argument".to_string());
                }
                // cvtsd2ss xmm0, xmm0  (F2 0F 5A C0)
                code.extend_from_slice(&[0xF2, 0x0F, 0x5A, 0xC0]);
                return Ok(FloatWidth::F32);
            }
            // `to_f64(x f32) -> f64`: evaluate the f32 argument, then widen it with
            // `cvtss2sd` (exact).
            if name == "to_f64" {
                if args.len() != 1 {
                    return Err("`to_f64` takes exactly one argument".to_string());
                }
                let arg_width = lower_native_float_expr(ctx, &args[0], code)?;
                if arg_width != FloatWidth::F32 {
                    return Err("`to_f64` expects an f32 argument".to_string());
                }
                // cvtss2sd xmm0, xmm0  (F3 0F 5A C0)
                code.extend_from_slice(&[0xF3, 0x0F, 0x5A, 0xC0]);
                return Ok(FloatWidth::F64);
            }
            // `sqrt(x f64) -> f64`: a single SSE2 `sqrtsd` (baseline, no CPUID),
            // bit-for-bit `f64::sqrt` — matches the interpreters' `n.sqrt()` (a
            // negative operand yields NaN, like IEEE-754). f64-only, like the
            // interpreter builtin. The other transcendental/rounding math builtins
            // (`sin`/`floor`/…) stay deferred (they need a library or SSE4.1).
            if name == "sqrt" && args.len() == 1 {
                let arg_width = lower_native_float_expr(ctx, &args[0], code)?;
                if arg_width != FloatWidth::F64 {
                    return Err("native `sqrt` expects an f64 argument".to_string());
                }
                // sqrtsd xmm0, xmm0  (F2 0F 51 C0)
                code.extend_from_slice(&[0xF2, 0x0F, 0x51, 0xC0]);
                return Ok(FloatWidth::F64);
            }
            // `abs(x f64) -> f64`: clear the IEEE-754 sign bit with SSE2 only. The
            // 0x7FFF_FFFF_FFFF_FFFF mask is built in-register (no `.rdata`
            // constant, no relocation): all-ones via `pcmpeqd`, then a 1-bit
            // logical right shift per 64-bit lane, then `andpd`. Bit-for-bit
            // `f64::abs` (|-0.0| = +0.0; a NaN keeps its payload with the sign
            // cleared) — matching the interpreters' `n.abs()`. This fires only for
            // an f64 argument; `abs(i64)` and `abs(f32)` stay on the interpreters.
            if name == "abs"
                && args.len() == 1
                && float_width_of_expr(ctx, &args[0]) == Some(FloatWidth::F64)
            {
                let arg_width = lower_native_float_expr(ctx, &args[0], code)?;
                if arg_width != FloatWidth::F64 {
                    return Err("native `abs` expects an f64 argument".to_string());
                }
                code.extend_from_slice(&[
                    0x66, 0x0F, 0x76, 0xC9, // pcmpeqd xmm1, xmm1   (xmm1 = all ones)
                    0x66, 0x0F, 0x73, 0xD1, 0x01, // psrlq xmm1, 1  (0x7FFF..FF per lane)
                    0x66, 0x0F, 0x54, 0xC1, // andpd xmm0, xmm1     (clear sign bit)
                ]);
                return Ok(FloatWidth::F64);
            }
            // `get(l, i)` on a float-element list: load the raw 8-byte element
            // word into `rax`, then move its bits into `xmm0` at the element's
            // width (the low four bytes of the word for f32).
            if name == LIST_GET_BUILTIN
                && args.len() == 2
                && let Some(elem) = supported_list_element(&args[0].ty)
                && let Some(width) = FloatWidth::from_type_name(&elem.name)
            {
                lower_list_get(ctx, &args[0], &args[1], code)?; // element word -> rax
                emit_movq_xmm0_from_rax(code, width);
                return Ok(width);
            }
            // A float-returning `extern fn` C call: marshal the arguments across
            // the Win64 C ABI (integer/pointer → GPRs, float → `xmm0..3`, §4.1)
            // and read the `f64`/`f32` return from `xmm0`.
            if let Some(sig) = ctx.extern_sigs.get(name.as_str()) {
                let sig = *sig;
                return match emit_extern_call(ctx, name, sig, args, code)? {
                    Some(FfiScalarClass::Float(width)) => Ok(width),
                    Some(FfiScalarClass::Int(_)) | None => Err(format!(
                        "extern `{name}` does not return a float, so it \
                         cannot be used in a float context"
                    )),
                };
            }
            Err(format!(
                "float call `{name}` is not in the native subset (non-extern float-returning functions and math builtins are deferred)"
            ))
        }
        BytecodeExprKind::Binary { left, op, right } => {
            // Derive the width from the operands' structure (an arithmetic float
            // node is annotated `i64` in the IR, so its own `ty` is unreliable).
            let width = float_width_of_expr(ctx, left)
                .or_else(|| float_width_of_expr(ctx, right))
                .ok_or_else(|| "float binary op on non-float operands".to_string())?;
            match op {
                BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide => {
                    // Evaluate left into xmm0 and spill it; evaluate right into
                    // xmm0; restore left into xmm1; apply left <op> right.
                    let left_width = lower_native_float_expr(ctx, left, code)?;
                    push_xmm0(code); // save left
                    let right_width = lower_native_float_expr(ctx, right, code)?;
                    debug_assert_eq!(left_width, width);
                    debug_assert_eq!(right_width, width);
                    // xmm1 = right, xmm0 = left.
                    move_xmm0_to_xmm1(code); // xmm1 = right
                    pop_xmm0(code); // xmm0 = left
                    emit_float_arith(code, *op, width);
                    Ok(width)
                }
                _ => Err(
                    "float comparison does not produce a float value (handled on the i64 path)"
                        .to_string(),
                ),
            }
        }
        // Float arithmetic negation (`-x`): IEEE-754 sign-bit flip, matching the
        // interpreters' `-f`. Move the value through a GPR, XOR the sign bit, move
        // it back to xmm0. (`0 - x` would mishandle `-0.0`/NaN signs.)
        BytecodeExprKind::Unary {
            op: lullaby_parser::UnaryOp::Negate,
            expr: inner,
        } => {
            let width = lower_native_float_expr(ctx, inner, code)?;
            emit_movq_rax_from_xmm0(code, width); // rax = bits
            match width {
                FloatWidth::F64 => {
                    // mov rcx, 0x8000000000000000 ; xor rax, rcx
                    code.extend_from_slice(&[0x48, 0xB9]);
                    code.extend_from_slice(&0x8000_0000_0000_0000u64.to_le_bytes());
                    code.extend_from_slice(&[0x48, 0x31, 0xC8]);
                }
                FloatWidth::F32 => {
                    // xor eax, 0x80000000
                    code.push(0x35);
                    code.extend_from_slice(&0x8000_0000u32.to_le_bytes());
                }
            }
            emit_movq_xmm0_from_rax(code, width); // xmm0 = negated bits
            Ok(width)
        }
        // A float array element / float struct field read: `a[i]` / `s.f` where the
        // element is f64/f32. Resolve the place (constant or bounds-checked dynamic
        // address) and load it into xmm0 with movsd/movss.
        BytecodeExprKind::Index { .. } | BytecodeExprKind::Field { .. } => {
            let (place, elem_ty) = resolve_read_place_typed(ctx, expr)?;
            let width = match elem_ty {
                NativeType::F64 => FloatWidth::F64,
                NativeType::F32 => FloatWidth::F32,
                _ => return Err("float access resolved to a non-float element".to_string()),
            };
            match place {
                ScalarPlace::Const { slot } => load_float_local(code, slot, width),
                // A dynamic stack element or a fat-pointer array element: both
                // resolve their (bounds-checked) address into rcx.
                ScalarPlace::Dynamic { .. } | ScalarPlace::FatIndex { .. } => {
                    emit_dynamic_addr_into_rcx(ctx, &place, code)?; // rcx = &elem (bounds-checked)
                    load_float_from_rcx(code, width);
                }
            }
            Ok(width)
        }
        _ => Err("expression is not in the native float subset".to_string()),
    }
}

/// Emit `left <op> right` where `left` is in `xmm0` and `right` is in `xmm1`,
/// leaving the result in `xmm0`. `op` is one of `+ - * /`. Uses the double
/// (`*sd`) or single (`*ss`) opcode family per `width`; an `*ss` op inherently
/// rounds its result to single precision, matching the interpreter's f32 store.
pub(crate) fn emit_float_arith(code: &mut Vec<u8>, op: BinaryOp, width: FloatWidth) {
    // Opcode second byte selects add/mul/sub/div: 58/59/5C/5E.
    let arith = match op {
        BinaryOp::Add => 0x58,
        BinaryOp::Subtract => 0x5C,
        BinaryOp::Multiply => 0x59,
        BinaryOp::Divide => 0x5E,
        _ => unreachable!("emit_float_arith only handles + - * /"),
    };
    // Prefix: F2 for scalar-double (*sd), F3 for scalar-single (*ss).
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    // <op>s{d,s} xmm0, xmm1  ->  prefix 0F <arith> C1
    code.extend_from_slice(&[prefix, 0x0F, arith, 0xC1]);
}

/// Lower a float comparison (`< <= > >= == !=`) whose operands are `f64`/`f32`,
/// leaving a canonical `0`/`1` in `rax`. Uses ordered SSE compares (`ucomisd`/
/// `ucomiss`) with the unordered-aware condition codes so a NaN operand yields
/// exactly the interpreter's result: every relational compare is false on NaN,
/// `==` is false on NaN, and `!=` is true on NaN (Rust/IEEE-754 semantics).
pub(crate) fn lower_native_float_compare(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    op: BinaryOp,
    right: &BytecodeExpr,
    width: FloatWidth,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate left into xmm0, spill it; evaluate right into xmm0, move to xmm1;
    // restore left into xmm0. Result: xmm0 = left, xmm1 = right.
    lower_native_float_expr(ctx, left, code)?;
    push_xmm0(code);
    lower_native_float_expr(ctx, right, code)?;
    move_xmm0_to_xmm1(code); // xmm1 = right
    pop_xmm0(code); // xmm0 = left

    // `ucomis{d,s}` sets CF/ZF/PF as an unsigned-style compare of xmm0 vs xmm1:
    //   xmm0 <  xmm1 -> CF=1, ZF=0
    //   xmm0 == xmm1 -> CF=0, ZF=1
    //   xmm0 >  xmm1 -> CF=0, ZF=0
    //   unordered    -> CF=1, ZF=1, PF=1
    // `seta` (CF=0 & ZF=0) is strict-greater-ordered; `setae` (CF=0) is
    // greater-or-equal-ordered — both false when unordered. So we realize `<`
    // and `<=` by swapping the compare operands (compare right vs left).
    let cmp_prefixed = |code: &mut Vec<u8>, swap: bool| {
        // ucomis{d,s} first, second.  prefix(0x66 for sd, none for ss) 0F 2E /r
        // For F64 use `ucomisd` (66 0F 2E), for F32 `ucomiss` (0F 2E).
        let (a, b) = if swap { (1u8, 0u8) } else { (0u8, 1u8) }; // xmm regs
        let modrm = 0xC0 | (a << 3) | b; // ucomis <xmm a>, <xmm b>
        match width {
            FloatWidth::F64 => code.extend_from_slice(&[0x66, 0x0F, 0x2E, modrm]),
            FloatWidth::F32 => code.extend_from_slice(&[0x0F, 0x2E, modrm]),
        }
    };

    match op {
        BinaryOp::Greater => {
            cmp_prefixed(code, false); // ucomis xmm0, xmm1
            code.extend_from_slice(&[0x0F, 0x97, 0xC0]); // seta al
            movzx_al_to_rax(code);
        }
        BinaryOp::GreaterEqual => {
            cmp_prefixed(code, false);
            code.extend_from_slice(&[0x0F, 0x93, 0xC0]); // setae al
            movzx_al_to_rax(code);
        }
        BinaryOp::Less => {
            // left < right  <=>  right > left. Compare xmm1 vs xmm0.
            cmp_prefixed(code, true); // ucomis xmm1, xmm0
            code.extend_from_slice(&[0x0F, 0x97, 0xC0]); // seta al
            movzx_al_to_rax(code);
        }
        BinaryOp::LessEqual => {
            cmp_prefixed(code, true); // ucomis xmm1, xmm0
            code.extend_from_slice(&[0x0F, 0x93, 0xC0]); // setae al
            movzx_al_to_rax(code);
        }
        BinaryOp::Equal => {
            // Ordered equality: ZF=1 (equal) AND not unordered (PF=0).
            cmp_prefixed(code, false); // ucomis xmm0, xmm1
            code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
            code.extend_from_slice(&[0x0F, 0x9B, 0xC1]); // setnp cl
            code.extend_from_slice(&[0x20, 0xC8]); // and al, cl
            movzx_al_to_rax(code);
        }
        BinaryOp::NotEqual => {
            // Inequality including unordered: ZF=0 (not equal) OR unordered (PF=1).
            cmp_prefixed(code, false); // ucomis xmm0, xmm1
            code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
            code.extend_from_slice(&[0x0F, 0x9A, 0xC1]); // setp cl
            code.extend_from_slice(&[0x08, 0xC8]); // or al, cl
            movzx_al_to_rax(code);
        }
        _ => return Err("unsupported float comparison operator".to_string()),
    }
    Ok(())
}

/// Determine the [`FloatWidth`] of `expr` if it is a float value, using leaf
/// types that the IR annotates correctly (float literals, float locals, and the
/// `to_f32`/`to_f64` conversions) and recursing through float arithmetic. Returns
/// `None` for a non-float expression. This is more reliable than reading a
/// `Binary` node's own `ty`, which the IR annotates `i64` for float arithmetic.
pub(crate) fn float_width_of_expr(ctx: &NativeCtx, expr: &BytecodeExpr) -> Option<FloatWidth> {
    match &expr.kind {
        BytecodeExprKind::Float(_) => FloatWidth::from_type_name(expr.ty.name.as_str()),
        BytecodeExprKind::Variable(name) => match ctx.locals.get(name)?.ty {
            NativeType::F64 => Some(FloatWidth::F64),
            NativeType::F32 => Some(FloatWidth::F32),
            _ => None,
        },
        BytecodeExprKind::Call { name, args } => match name.as_str() {
            "to_f32" => Some(FloatWidth::F32),
            "to_f64" | "sqrt" => Some(FloatWidth::F64),
            // `abs` follows its argument's width, but only the f64 case lowers
            // natively; an f32/i64 `abs` reports `None` so it skips gracefully.
            "abs" if args.len() == 1 => match float_width_of_expr(ctx, &args[0]) {
                Some(FloatWidth::F64) => Some(FloatWidth::F64),
                _ => None,
            },
            _ => None,
        },
        // Float arithmetic propagates its operands' width; a comparison yields a
        // bool (not a float), so those and all other ops report `None`.
        BytecodeExprKind::Binary {
            left,
            op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide,
            right,
        } => float_width_of_expr(ctx, left).or_else(|| float_width_of_expr(ctx, right)),
        // Unary negation of a float operand is a float of the same width, so it
        // must route to the float path (a sign-bit flip), not the integer `neg`.
        BytecodeExprKind::Unary {
            op: lullaby_parser::UnaryOp::Negate,
            expr: inner,
        } => float_width_of_expr(ctx, inner),
        // A float array element / float struct field read (`a[i]`, `s.f`): resolve
        // the place (read-only) and report the element width, so `a[i] + x` and
        // `a[i] < x` route to the float lowerer / float comparator.
        BytecodeExprKind::Index { .. } | BytecodeExprKind::Field { .. } => {
            match resolve_read_place_typed(ctx, expr) {
                Ok((_, NativeType::F64)) => Some(FloatWidth::F64),
                Ok((_, NativeType::F32)) => Some(FloatWidth::F32),
                _ => None,
            }
        }
        _ => None,
    }
}

/// `movzx rax, al` — zero-extend the boolean in `al` into the full `rax`.
pub(crate) fn movzx_al_to_rax(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);
}

/// Materialize a float immediate into `xmm0`: the IEEE-754 bit pattern is loaded
/// into a GPR and moved into `xmm0` (`movq` for f64, `movd` for f32). The f32
/// path rounds `value` to `f32` first so the stored bits match the interpreter.
pub(crate) fn emit_float_immediate(code: &mut Vec<u8>, value: f64, width: FloatWidth) {
    match width {
        FloatWidth::F64 => {
            emit_mov_rax_imm(code, value.to_bits() as i64);
            // movq xmm0, rax  (66 48 0F 6E C0)
            code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]);
        }
        FloatWidth::F32 => {
            let bits = (value as f32).to_bits();
            // mov eax, imm32  (B8 imm32) — zero-extends into rax.
            code.push(0xB8);
            code.extend_from_slice(&bits.to_le_bytes());
            // movd xmm0, eax  (66 0F 6E C0)
            code.extend_from_slice(&[0x66, 0x0F, 0x6E, 0xC0]);
        }
    }
}

/// `movs{d,s} xmm0, [rbp - slot]` — load a float local into `xmm0`.
pub(crate) fn load_float_local(code: &mut Vec<u8>, slot: i32, width: FloatWidth) {
    // movsd: F2 0F 10 /r ; movss: F3 0F 10 /r. ModRM 0x85 = [rbp + disp32], reg 0.
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x10, 0x85]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `movsd xmm0, [rcx]` (F64) / `movss xmm0, [rcx]` (F32) — load a float array
/// element from its computed address in `rcx`. ModRM 0x01 = `[rcx]`, reg 0.
pub(crate) fn load_float_from_rcx(code: &mut Vec<u8>, width: FloatWidth) {
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x10, 0x01]);
}

/// `movsd [rcx], xmm0` (F64) / `movss [rcx], xmm0` (F32) — store xmm0 to a float
/// array element at its computed address in `rcx`. ModRM 0x01 = `[rcx]`, reg 0.
pub(crate) fn store_float_from_rcx(code: &mut Vec<u8>, width: FloatWidth) {
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x11, 0x01]);
}

/// `movs{d,s} [rbp - slot], xmm0` — store `xmm0` into a float local.
pub(crate) fn store_float_local(code: &mut Vec<u8>, slot: i32, width: FloatWidth) {
    // movsd: F2 0F 11 /r ; movss: F3 0F 11 /r.
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    code.extend_from_slice(&[prefix, 0x0F, 0x11, 0x85]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// Spill `xmm0` onto the machine stack (16 bytes, keeping 16-byte rsp alignment).
/// Paired with [`pop_xmm0`]/[`pop_xmm1`]. The full 8-byte `movsd` store preserves
/// an f32's low bits too, so one spill primitive serves both widths.
pub(crate) fn push_xmm0(code: &mut Vec<u8>) {
    // sub rsp, 16
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]);
    // movsd [rsp], xmm0  (F2 0F 11 04 24)
    code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x04, 0x24]);
}

/// Restore a spilled float from the machine stack into `xmm0`.
pub(crate) fn pop_xmm0(code: &mut Vec<u8>) {
    // movsd xmm0, [rsp]  (F2 0F 10 04 24)
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x04, 0x24]);
    // add rsp, 16
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x10]);
}

/// Restore a spilled float from the machine stack into `xmm1`.
pub(crate) fn pop_xmm1(code: &mut Vec<u8>) {
    // movsd xmm1, [rsp]  (F2 0F 10 0C 24)
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0x0C, 0x24]);
    // add rsp, 16
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x10]);
}

/// `movsd xmm1, xmm0` — copy `xmm0` into `xmm1` (full 8-byte move; preserves an
/// f32's low bits as well).
pub(crate) fn move_xmm0_to_xmm1(code: &mut Vec<u8>) {
    // movsd xmm1, xmm0  (F2 0F 10 C8)
    code.extend_from_slice(&[0xF2, 0x0F, 0x10, 0xC8]);
}

/// `movs{d,s} [rbp - slot], xmm{n}` — store one of the low four XMM registers
/// (`xmm0..3`, the Win64 SSE argument registers) into a frame slot. Used by the
/// prologue to spill a float parameter that arrived in its positional XMM
/// register. `movsd` (`F2`) stores an f64 word; `movss` (`F3`) an f32 word.
pub(crate) fn emit_store_xmm_to_slot(code: &mut Vec<u8>, xmm: u8, slot: i32, width: FloatWidth) {
    debug_assert!(xmm < 4, "only xmm0..3 are Win64 argument registers");
    let prefix = match width {
        FloatWidth::F64 => 0xF2,
        FloatWidth::F32 => 0xF3,
    };
    // ModRM 0x85 = [rbp + disp32], reg field selects the XMM source.
    let modrm = 0x85 | (xmm << 3);
    code.extend_from_slice(&[prefix, 0x0F, 0x11, modrm]);
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `movsd xmm{n}, [rsp + disp]` — load a raw 8-byte word from a stack offset into
/// one of the low four XMM registers (`xmm0..3`). Used to move a staged float
/// argument into its positional SSE argument register before an extern call. A
/// full 8-byte `movsd` load preserves an f32's low four bytes too, so one loader
/// serves both widths.
pub(crate) fn emit_load_xmm_from_rsp_disp(code: &mut Vec<u8>, xmm: u8, disp: i32) {
    debug_assert!(xmm < 4, "only xmm0..3 are Win64 argument registers");
    // movsd xmm{n}, [rsp + disp8/disp32]. Base rsp needs a SIB byte (0x24).
    // ModRM.reg = xmm; ModRM.rm = 100b (SIB). disp8 when it fits, else disp32.
    code.extend_from_slice(&[0xF2, 0x0F, 0x10]);
    emit_rsp_mem_operand(code, xmm, disp);
}

/// `mov reg64, [rsp + disp]` — load a raw 8-byte word from a stack offset into one
/// of the first four Win64 integer argument registers (`rcx`/`rdx`/`r8`/`r9`, by
/// index). Used to move a staged integer argument into its positional GPR before
/// an extern call.
pub(crate) fn emit_load_gpr_from_rsp_disp(code: &mut Vec<u8>, index: u8, disp: i32) {
    // rcx/rdx are in the base encoding (REX.W); r8/r9 need REX.B. Reg field:
    // rcx=1, rdx=2, r8/r9=0/1 with REX.B.
    let (rex, reg): (u8, u8) = match index {
        0 => (0x48, 1), // rcx
        1 => (0x48, 2), // rdx
        2 => (0x4C, 0), // r8
        3 => (0x4C, 1), // r9
        _ => unreachable!("only four Win64 integer argument registers"),
    };
    code.push(rex);
    code.push(0x8B); // mov r64, r/m64
    emit_rsp_mem_operand(code, reg, disp);
}

/// Emit the ModRM+SIB(+disp) bytes for an `[rsp + disp]` memory operand with the
/// given ModRM.reg field. `rsp` as a base always requires the SIB byte `0x24`
/// (base=rsp, index=none). A zero displacement still needs an explicit `disp8`
/// because `[rsp]` with mod=00 is the SIB form without a displacement — encode
/// `disp8` for values in `i8` range, otherwise `disp32`.
pub(crate) fn emit_rsp_mem_operand(code: &mut Vec<u8>, reg: u8, disp: i32) {
    if let Ok(d) = i8::try_from(disp) {
        // mod=01 (disp8), rm=100 (SIB), SIB=0x24 (base=rsp, no index).
        code.push(0x40 | (reg << 3) | 0x04);
        code.push(0x24);
        code.push(d as u8);
    } else {
        // mod=10 (disp32), rm=100 (SIB), SIB=0x24.
        code.push(0x80 | (reg << 3) | 0x04);
        code.push(0x24);
        code.extend_from_slice(&disp.to_le_bytes());
    }
}

/// Combine a fixed-width binary op whose left operand is on the stack and whose
/// right operand is in `rax`, leaving the result (a normalized cell for
/// arithmetic/bitwise/shift, a canonical `0`/`1` for comparisons) in `rax`. This
/// mirrors the interpreter free functions exactly: arithmetic wraps then
/// re-normalizes (`Value::int`), division and comparison are signedness-aware
/// (`int_div`/`int_cmp`), and shifts mask the count to the width and honor
/// signedness (`int_shl`/`int_shr`).
pub(crate) fn emit_fixed_binop_from_stack(
    code: &mut Vec<u8>,
    op: BinaryOp,
    kind: IntKind,
) -> Result<(), String> {
    match op {
        BinaryOp::Add => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Subtract => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Multiply => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]); // imul rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Divide => {
            // left / right, where left is the dividend. Divide on the full 64-bit
            // cell (signedness-correct because signed cells are sign-extended and
            // unsigned cells zero-extended), then re-normalize the quotient.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            if kind.is_unsigned() {
                code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
                code.extend_from_slice(&[0x49, 0xF7, 0xF0]); // div r8
            } else {
                emit_signed_idiv_r8(code); // guarded against i64::MIN / -1 overflow
            }
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Remainder => {
            // left % right: the same div/idiv leaves the remainder in rdx (rather
            // than the quotient in rax). Move it into rax and re-normalize.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            if kind.is_unsigned() {
                code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
                code.extend_from_slice(&[0x49, 0xF7, 0xF0]); // div r8
            } else {
                emit_signed_irem_r8(code); // guarded so `x % -1 == 0` (rdx = 0)
            }
            code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx (remainder)
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Equal | BinaryOp::NotEqual => {
            // Equality is width-agnostic on the normalized cells.
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
            let set_opcode = if matches!(op, BinaryOp::Equal) {
                0x94 // sete
            } else {
                0x95 // setne
            };
            code.extend_from_slice(&[0x0F, set_opcode, 0xC0]); // set<cc> al
            code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
        }
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            // Ordering uses unsigned condition codes for unsigned kinds and
            // signed condition codes for signed kinds, on the normalized cells.
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
            let set_opcode = if kind.is_unsigned() {
                match op {
                    BinaryOp::Less => 0x92,         // setb
                    BinaryOp::LessEqual => 0x96,    // setbe
                    BinaryOp::Greater => 0x97,      // seta
                    BinaryOp::GreaterEqual => 0x93, // setae
                    _ => unreachable!(),
                }
            } else {
                match op {
                    BinaryOp::Less => 0x9C,         // setl
                    BinaryOp::LessEqual => 0x9E,    // setle
                    BinaryOp::Greater => 0x9F,      // setg
                    BinaryOp::GreaterEqual => 0x9D, // setge
                    _ => unreachable!(),
                }
            };
            code.extend_from_slice(&[0x0F, set_opcode, 0xC0]); // set<cc> al
            code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
        }
        BinaryOp::BitAnd => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x21, 0xC8]); // and rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::BitOr => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x09, 0xC8]); // or rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::BitXor => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x31, 0xC8]); // xor rax, rcx
            emit_normalize_rax(code, kind);
        }
        BinaryOp::Shl | BinaryOp::Shr => {
            // Mask the shift count to `width-1` (matching `int_shl`/`int_shr`),
            // move it into cl, then shift the left operand and re-normalize. `<<`
            // is `shl`; `>>` is `sar` (arithmetic) for signed kinds and `shr`
            // (logical) for unsigned kinds.
            //
            // Stack holds the left operand; rax holds the right (count).
            let mask = (kind.width_bits() - 1) as u8; // 7/15/31/63, fits imm8
            code.extend_from_slice(&[0x48, 0x83, 0xE0, mask]); // and rax, mask
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (count in cl)
            code.push(0x58); // pop rax (left = value to shift)
            let shift_op: &[u8] = match (op, kind.is_unsigned()) {
                (BinaryOp::Shl, _) => &[0x48, 0xD3, 0xE0], // shl rax, cl
                (BinaryOp::Shr, true) => &[0x48, 0xD3, 0xE8], // shr rax, cl (logical)
                (BinaryOp::Shr, false) => &[0x48, 0xD3, 0xF8], // sar rax, cl (arithmetic)
                _ => unreachable!(),
            };
            code.extend_from_slice(shift_op);
            emit_normalize_rax(code, kind);
        }
        BinaryOp::And | BinaryOp::Or => {
            return Err("logical and/or must be short-circuited".to_string());
        }
    }
    Ok(())
}

// -- Overflow-aware arithmetic (checked/saturating/wrapping) -----------------
//
// The overflow-aware builtins operate on two operands of the same fixed-width
// kind `T` (`i8`…`u64`/`isize`/`usize`; `i64` is excluded by the type checker).
// `wrapping_*` reuses the default fixed-width `+`/`-`/`*` (wrap then normalize).
// `saturating_*` and `checked_*` share [`emit_overflow_core`], which computes the
// wrapped result plus an overflow flag and a saturation target using hardware
// carry/overflow flags for the 64-bit kinds and exact-then-range-check for the
// narrow kinds — producing results bit-identical to the interpreters'
// `overflow_arith` for every width and sign. No division is used, so no case can
// trap.

/// x86-64 GPR indices for the raw encoders below.
const REG_RAX: u8 = 0;
const REG_RCX: u8 = 1;
const REG_RDX: u8 = 2;
const REG_R8: u8 = 8;
const REG_R9: u8 = 9;
const REG_R10: u8 = 10;

/// `(rex_extension_bit, low_three_bits)` for a GPR index (`0`=rax … `15`=r15).
pub(crate) fn gpr_bits(reg: u8) -> (u8, u8) {
    (u8::from(reg >= 8), reg & 0x7)
}

/// `mov <reg>, imm64` (the full 10-byte form; REX.W, plus REX.B for r8..r15).
pub(crate) fn emit_mov_reg_imm64(code: &mut Vec<u8>, reg: u8, imm: i64) {
    let (ext, low) = gpr_bits(reg);
    code.push(0x48 | ext);
    code.push(0xB8 | low);
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `mov <dest>, <src>` (register to register, REX.W).
pub(crate) fn emit_mov_reg_reg(code: &mut Vec<u8>, dest: u8, src: u8) {
    let (dext, dlow) = gpr_bits(dest);
    let (sext, slow) = gpr_bits(src);
    // 89 /r: r/m <- reg, so reg field = src, r/m field = dest.
    code.push(0x48 | (sext << 2) | dext);
    code.push(0x89);
    code.push(0xC0 | (slow << 3) | dlow);
}

/// A register-to-register ALU op (`opcode` is the `r/m, r` form: `01`=add,
/// `29`=sub, `31`=xor), computing `dest <op>= src` (REX.W).
pub(crate) fn emit_alu_reg_reg(code: &mut Vec<u8>, opcode: u8, dest: u8, src: u8) {
    let (dext, dlow) = gpr_bits(dest);
    let (sext, slow) = gpr_bits(src);
    code.push(0x48 | (sext << 2) | dext);
    code.push(opcode);
    code.push(0xC0 | (slow << 3) | dlow);
}

/// `test <reg>, <reg>` (REX.W), setting SF/ZF for a following conditional jump.
pub(crate) fn emit_test_reg(code: &mut Vec<u8>, reg: u8) {
    let (ext, low) = gpr_bits(reg);
    code.push(0x48 | (ext << 2) | ext);
    code.push(0x85);
    code.push(0xC0 | (low << 3) | low);
}

/// `mov r8b, 1` — set the low byte of the (already-zeroed) overflow register.
pub(crate) fn emit_set_r8b_one(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x41, 0xB0, 0x01]);
}

/// Emit a `jcc rel32` with a placeholder displacement, returning the patch site.
/// `cc` is the low opcode byte (`0x82`=jb/jc, `0x83`=jae/jnc, `0x84`=je/jz,
/// `0x80`=jo, `0x81`=jno, `0x86`=jbe, `0x88`=js, `0x89`=jns, `0x8C`=jl, `0x8F`=jg).
pub(crate) fn emit_jcc(code: &mut Vec<u8>, cc: u8) -> usize {
    code.extend_from_slice(&[0x0F, cc]);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    site
}

/// Emit a `jmp rel32` placeholder, returning the patch site.
pub(crate) fn emit_jmp(code: &mut Vec<u8>) -> usize {
    code.push(0xE9);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    site
}

/// Compute the overflow-aware result of `a <op> b` for fixed-width `kind`.
///
/// Precondition: `a` (left) in `rax`, `b` (right) in `rcx`.
/// Postcondition: `rax` = the wrapped result normalized to `kind` (identical to
/// `wrapping_<op>`); `r8` = the overflow flag (`0`/`1`, full register);
/// `r9` = the saturation target (`T`'s `MAX`/`MIN`/`0`, valid iff `r8 == 1`).
///
/// The 64-bit kinds use the hardware `CF`/`OF` flags after `add`/`sub` and the
/// widening `mul`/`imul` (high half in `rdx`, or `OF` for signed) to detect
/// overflow exactly. The narrow kinds compute the exact 64-bit result (which
/// cannot overflow a 64-bit register) and range-check it against `[min, max]`.
pub(crate) fn emit_overflow_core(code: &mut Vec<u8>, op: OverflowOp, kind: IntKind) {
    let (min_i128, max_i128) = kind.range_i128();
    let min = min_i128 as i64;
    let max = max_i128 as i64;
    let w64 = matches!(kind, IntKind::U64 | IntKind::Usize | IntKind::Isize);
    let unsigned = kind.is_unsigned();

    if w64 {
        // Clear the overflow flag before the arithmetic (xor also clears CF/OF).
        emit_alu_reg_reg(code, 0x31, REG_R8, REG_R8); // xor r8, r8
        match op {
            OverflowOp::Add => emit_alu_reg_reg(code, 0x01, REG_RAX, REG_RCX), // add rax, rcx
            OverflowOp::Sub => emit_alu_reg_reg(code, 0x29, REG_RAX, REG_RCX), // sub rax, rcx
            OverflowOp::Mul => {
                if unsigned {
                    // mul rcx: rdx:rax = rax * rcx (unsigned). Overflow iff rdx != 0.
                    code.extend_from_slice(&[0x48, 0xF7, 0xE1]);
                } else {
                    // Signed: product sign = sign(a) ^ sign(b); capture it in r10
                    // before `imul` overwrites rax.
                    emit_mov_reg_reg(code, REG_R10, REG_RAX); // r10 = a
                    emit_alu_reg_reg(code, 0x31, REG_R10, REG_RCX); // r10 ^= b
                    // imul rcx (one-operand): rdx:rax = rax * rcx (signed); OF set
                    // when the full product does not fit 64-bit signed.
                    code.extend_from_slice(&[0x48, 0xF7, 0xE9]);
                }
            }
        }
        // Branch to `done` when there is no overflow, else set r8 = 1 and the
        // saturation target r9.
        let no_ovf = match op {
            OverflowOp::Mul if unsigned => {
                emit_test_reg(code, REG_RDX); // test rdx, rdx
                emit_jcc(code, 0x84) // jz -> no overflow
            }
            _ if unsigned => emit_jcc(code, 0x83), // jnc -> no overflow (add/sub carry)
            _ => emit_jcc(code, 0x81),             // jno -> no overflow (signed OF)
        };
        emit_set_r8b_one(code);
        match (op, unsigned) {
            // Unsigned add/mul saturate up to MAX; unsigned sub saturates to 0.
            (OverflowOp::Sub, true) => emit_alu_reg_reg(code, 0x31, REG_R9, REG_R9), // r9 = 0
            (_, true) => emit_mov_reg_imm64(code, REG_R9, max),
            // Signed mul: target sign from r10 (product sign). Add/sub: from the
            // wrapped result's sign (a signed overflow flips the true sign).
            (OverflowOp::Mul, false) => {
                emit_mov_reg_imm64(code, REG_R9, max);
                emit_test_reg(code, REG_R10);
                let keep = emit_jcc(code, 0x89); // jns -> product >= 0, keep MAX
                emit_mov_reg_imm64(code, REG_R9, min);
                patch_rel32(code, keep);
            }
            (_, false) => {
                emit_mov_reg_imm64(code, REG_R9, max);
                emit_test_reg(code, REG_RAX);
                let keep = emit_jcc(code, 0x88); // js -> wrapped < 0, keep MAX
                emit_mov_reg_imm64(code, REG_R9, min);
                patch_rel32(code, keep);
            }
        }
        patch_rel32(code, no_ovf);
    } else {
        // Narrow kinds: the exact 64-bit result cannot overflow the register.
        match op {
            OverflowOp::Add => emit_alu_reg_reg(code, 0x01, REG_RAX, REG_RCX), // add rax, rcx
            OverflowOp::Sub => emit_alu_reg_reg(code, 0x29, REG_RAX, REG_RCX), // sub rax, rcx
            OverflowOp::Mul => code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]), // imul rax, rcx
        }
        emit_alu_reg_reg(code, 0x31, REG_R8, REG_R8); // xor r8, r8 (default no overflow)
        if unsigned {
            match op {
                // Unsigned subtraction underflows iff the exact result is negative;
                // it saturates to 0.
                OverflowOp::Sub => {
                    emit_test_reg(code, REG_RAX);
                    let no_ovf = emit_jcc(code, 0x89); // jns -> >= 0, no underflow
                    emit_set_r8b_one(code);
                    emit_alu_reg_reg(code, 0x31, REG_R9, REG_R9); // r9 = 0
                    patch_rel32(code, no_ovf);
                }
                // Unsigned add/mul overflow iff the exact result exceeds MAX; they
                // saturate up to MAX.
                _ => {
                    emit_cmp_rax_imm(code, max);
                    let no_ovf = emit_jcc(code, 0x86); // jbe -> <= max, no overflow
                    emit_set_r8b_one(code);
                    emit_mov_reg_imm64(code, REG_R9, max);
                    patch_rel32(code, no_ovf);
                }
            }
        } else {
            // Signed: overflow iff the exact result is outside [min, max]; the
            // saturation target is the bound it crossed.
            emit_cmp_rax_imm(code, max);
            let pos = emit_jcc(code, 0x8F); // jg -> above max
            emit_cmp_rax_imm(code, min);
            let neg = emit_jcc(code, 0x8C); // jl -> below min
            let done_ok = emit_jmp(code);
            patch_rel32(code, pos);
            emit_set_r8b_one(code);
            emit_mov_reg_imm64(code, REG_R9, max);
            let done_pos = emit_jmp(code);
            patch_rel32(code, neg);
            emit_set_r8b_one(code);
            emit_mov_reg_imm64(code, REG_R9, min);
            patch_rel32(code, done_ok);
            patch_rel32(code, done_pos);
        }
        // Normalize the wrapped result to the kind's width (identity when the
        // result is in range, which is the only case saturating/checked read it).
        emit_normalize_rax(code, kind);
    }
}

/// Lower `saturating_<op>(a, b) -> T`: compute the clamped result into `rax`.
pub(crate) fn lower_native_saturating(
    ctx: &mut NativeCtx,
    op: OverflowOp,
    kind: IntKind,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (a)
    lower_native_expr(ctx, right, code)?; // rax = b
    emit_mov_reg_reg(code, REG_RCX, REG_RAX); // rcx = b
    code.push(0x58); // pop rax (a)
    emit_overflow_core(code, op, kind);
    // result = overflow ? target : wrapped.
    emit_test_reg(code, REG_R8);
    let keep = emit_jcc(code, 0x84); // jz -> no overflow, keep wrapped
    emit_mov_reg_reg(code, REG_RAX, REG_R9); // rax = saturation target
    patch_rel32(code, keep);
    Ok(())
}

/// Lower `checked_<op>(a, b) -> option<T>` into the enum record at `base_slot`:
/// tag word = `some`/`none` per overflow, payload word = the wrapped result
/// (read only in the `some` case). Mirrors [`lower_map_get_into`]'s option build.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_native_checked_into(
    ctx: &mut NativeCtx,
    base_slot: i32,
    result_ty: &TypeRef,
    op: OverflowOp,
    kind: IntKind,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let layout = resolve_native_type(result_ty, ctx.structs, ctx.enums)?;
    let NativeType::Enum { variants, .. } = &layout else {
        return Err(format!(
            "checked_* result type `{}` is not a supported option enum",
            result_ty.name
        ));
    };
    let some_tag = variants
        .iter()
        .find(|v| v.name == "some")
        .map(|v| v.tag)
        .ok_or_else(|| "checked_* result option layout missing `some` variant".to_string())?;
    let none_tag = variants
        .iter()
        .find(|v| v.name == "none")
        .map(|v| v.tag)
        .ok_or_else(|| "checked_* result option layout missing `none` variant".to_string())?;
    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (a)
    lower_native_expr(ctx, right, code)?; // rax = b
    emit_mov_reg_reg(code, REG_RCX, REG_RAX); // rcx = b
    code.push(0x58); // pop rax (a)
    emit_overflow_core(code, op, kind);
    // Payload word = the wrapped result (rax), 8 bytes ABOVE the tag in the
    // ascending layout, i.e. at displacement `base_slot - 8`.
    store_local(code, base_slot - 8);
    // Tag word = overflow ? none : some, at base_slot.
    emit_mov_rax_imm(code, some_tag);
    emit_test_reg(code, REG_R8);
    let store = emit_jcc(code, 0x84); // jz -> no overflow, keep `some`
    emit_mov_rax_imm(code, none_tag);
    patch_rel32(code, store);
    store_local(code, base_slot);
    Ok(())
}

/// Normalize rax to a canonical boolean (1 if non-zero, else 0).
pub(crate) fn normalize_bool(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
}

/// Combine a binary op whose left operand is on the stack (pushed) and whose
/// right operand is in rax. Result left in rax.
pub(crate) fn emit_i64_binop_from_stack(code: &mut Vec<u8>, op: BinaryOp) -> Result<(), String> {
    // pop rcx (left); result = rcx <op> rax for arithmetic that isn't commutative
    // is handled by moving operands into the right registers below.
    match op {
        BinaryOp::Add => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx
        }
        BinaryOp::Subtract => {
            // want left - right = rcx - rax
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
        }
        BinaryOp::Multiply => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]); // imul rax, rcx
        }
        BinaryOp::Divide => {
            // left / right = rcx / rax ; idiv divides rdx:rax by operand.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            emit_signed_idiv_r8(code); // guarded against i64::MIN / -1 overflow
        }
        BinaryOp::Remainder => {
            // left % right: the same idiv leaves the remainder in rdx; move it
            // into rax. `x % -1 == 0` is handled inside emit_signed_irem_r8.
            code.push(0x59); // pop rcx (left = dividend)
            code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (divisor)
            code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (dividend)
            emit_signed_irem_r8(code);
            code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx (remainder)
        }
        BinaryOp::Equal
        | BinaryOp::NotEqual
        | BinaryOp::Less
        | BinaryOp::LessEqual
        | BinaryOp::Greater
        | BinaryOp::GreaterEqual => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
            let set_opcode = match op {
                BinaryOp::Equal => 0x94,        // sete
                BinaryOp::NotEqual => 0x95,     // setne
                BinaryOp::Less => 0x9C,         // setl
                BinaryOp::LessEqual => 0x9E,    // setle
                BinaryOp::Greater => 0x9F,      // setg
                BinaryOp::GreaterEqual => 0x9D, // setge
                _ => unreachable!(),
            };
            code.extend_from_slice(&[0x0F, set_opcode, 0xC0]); // set<cc> al
            code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
        }
        BinaryOp::And | BinaryOp::Or => {
            return Err("logical and/or must be short-circuited".to_string());
        }
        BinaryOp::BitAnd => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x21, 0xC8]); // and rax, rcx
        }
        BinaryOp::BitOr => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x09, 0xC8]); // or rax, rcx
        }
        BinaryOp::BitXor => {
            code.push(0x59); // pop rcx (left)
            code.extend_from_slice(&[0x48, 0x31, 0xC8]); // xor rax, rcx
        }
        BinaryOp::Shl | BinaryOp::Shr => {
            // `i64` is signed, so `>>` is an arithmetic shift (`sar`). The count is
            // masked to 63 (matching `int_shl`/`int_shr`'s `& (width-1)`). Stack
            // holds the left value; rax holds the right (count).
            code.extend_from_slice(&[0x48, 0x83, 0xE0, 0x3F]); // and rax, 63
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (count in cl)
            code.push(0x58); // pop rax (left = value to shift)
            match op {
                BinaryOp::Shl => code.extend_from_slice(&[0x48, 0xD3, 0xE0]), // shl rax, cl
                BinaryOp::Shr => code.extend_from_slice(&[0x48, 0xD3, 0xF8]), // sar rax, cl
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

/// Emit a signed 64-bit division of `rax` (dividend) by `r8` (divisor), leaving
/// the quotient in `rax`. The plain `idiv` instruction raises a hardware #DE on
/// the single overflow case `i64::MIN / -1`, whereas the interpreters use
/// `wrapping_div`, which yields `i64::MIN` for that input (see `int_div` in
/// `lullaby_runtime`). To match the interpreters bit-for-bit and avoid the trap,
/// special-case a divisor of `-1`: for any `x`, `x / -1 == -x` under wrapping
/// (including `i64::MIN / -1 == i64::MIN`, since `neg` of `i64::MIN` wraps to
/// itself). The caller must guarantee a non-zero divisor (division by zero is
/// rejected earlier as `L0404`).
pub(crate) fn emit_signed_idiv_r8(code: &mut Vec<u8>) {
    // cmp r8, -1
    code.extend_from_slice(&[0x49, 0x83, 0xF8, 0xFF]);
    // jne +5  (skip the neg/jmp pair, fall through to cqo/idiv)
    code.extend_from_slice(&[0x75, 0x05]);
    // neg rax  (rax = -rax, wrapping; this is x / -1 for the whole i64 range)
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);
    // jmp +5  (skip cqo/idiv)
    code.extend_from_slice(&[0xEB, 0x05]);
    // cqo  (sign-extend rax into rdx:rax)
    code.extend_from_slice(&[0x48, 0x99]);
    // idiv r8
    code.extend_from_slice(&[0x49, 0xF7, 0xF8]);
}

/// Emit a signed 64-bit remainder of `rax` (dividend) by `r8` (divisor), leaving
/// the remainder in `rdx` (the caller moves it where it needs it). Like
/// [`emit_signed_idiv_r8`], the plain `idiv` raises #DE on `i64::MIN / -1`, but
/// the true remainder there is `0` (`i64::MIN % -1 == 0`, matching `wrapping_rem`
/// in the interpreters). Special-case a divisor of `-1` by setting the remainder
/// to `0` directly and skipping the trapping `idiv`. The caller must guarantee a
/// non-zero divisor.
pub(crate) fn emit_signed_irem_r8(code: &mut Vec<u8>) {
    // cmp r8, -1
    code.extend_from_slice(&[0x49, 0x83, 0xF8, 0xFF]);
    // jne +5  (skip the xor/jmp pair, fall through to cqo/idiv)
    code.extend_from_slice(&[0x75, 0x05]);
    // xor rdx, rdx  (remainder of x % -1 is 0 for the whole i64 range)
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);
    // jmp +5  (skip cqo/idiv)
    code.extend_from_slice(&[0xEB, 0x05]);
    // cqo  (sign-extend rax into rdx:rax)
    code.extend_from_slice(&[0x48, 0x99]);
    // idiv r8  (quotient -> rax, remainder -> rdx)
    code.extend_from_slice(&[0x49, 0xF7, 0xF8]);
}

// -- String op lowering (native) ---------------------------------------------
//
// A `string` value is a heap pointer to `[char_len i64][byte_len i64][utf8]`. The
// heavy lifting (allocate, header math, byte copies, itoa) lives in `.text`
// helpers so each call site stays small; the inline codegen below stages
// operands and calls them. Every helper call is a `call`, so the frame reserves
// shadow space and stays 16-byte aligned (see `expr_has_call`, which reports a
// string literal and a string `+` as calls so the planner reserves it).

/// Lower `a + b` string concatenation: evaluate both operands to record pointers
/// (`a` in `rcx`, `b` in `rdx`), then call `__lullaby_str_concat`, which
/// allocates a fresh record, sums the headers, and byte-copies both UTF-8 ranges.
/// The concatenated record pointer is left in `rax`.
pub(crate) fn lower_string_concat(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate the left operand and spill its pointer, then the right (its
    // evaluation may itself be a call that clobbers registers), then load both
    // into the helper's argument registers.
    lower_native_expr(ctx, left, code)?;
    code.push(0x50); // push rax (left pointer)
    lower_native_expr(ctx, right, code)?; // right pointer -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (right)
    code.push(0x59); // pop rcx (left)
    // If an operand is a uniquely-owned fresh string temporary (a literal,
    // `to_string`/`substring`/`trim`/`repeat`, or a nested concat — never a borrowed
    // variable/container read), it is dead after the concat; use the ownership-aware
    // helper to `rc_dec` it, reclaiming the intermediate. When neither operand is a
    // fresh temp (the common `var + var`), the bare concat keeps zero overhead.
    let mask =
        (is_owning_string_alloc(left) as i32) | ((is_owning_string_alloc(right) as i32) << 1);
    if mask == 0 {
        emit_call_symbol(ctx, STR_CONCAT_SYMBOL, code);
    } else {
        // mov r8d, mask ; call __lullaby_str_concat_own
        code.extend_from_slice(&[0x41, 0xB8]);
        code.extend_from_slice(&mask.to_le_bytes());
        emit_call_symbol(ctx, STR_CONCAT_OWN_SYMBOL, code);
    }
    Ok(())
}

/// Lower `substring(s, start, end) -> string`: evaluate the source record pointer
/// into `rcx`, the `start`/`end` char indices (i64) into `rdx`/`r8`, then call
/// `__lullaby_str_substring`, which bounds-checks (trapping on `L0413`), maps the
/// char indices to byte offsets by walking the UTF-8, allocates a fresh record,
/// and byte-copies the slice. The slice record pointer is left in `rax`. Operands
/// are evaluated left-to-right and spilled, because each evaluation may itself be
/// a call that clobbers the argument registers.
/// Emit the call for a string-read op (`substring`/`char_at`/`repeat`/`trim`) whose
/// source string is already in `rcx` (and scalar args in `rdx`/`r8`). If the source
/// `s` is a uniquely-owned fresh temporary it is dead after the op reads it, so go
/// through `__lullaby_str_read_own` (which `rc_dec`s it); otherwise the bare op.
fn emit_str_read_op(ctx: &mut NativeCtx, s: &BytecodeExpr, op_symbol: &str, code: &mut Vec<u8>) {
    if is_owning_string_alloc(s) {
        // lea r9, [rip + <op symbol>] ; call __lullaby_str_read_own
        code.extend_from_slice(&[0x4C, 0x8D, 0x0D]);
        let site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        ctx.relocations.push(CodeRelocation {
            offset: site as u32,
            symbol: op_symbol.to_string(),
        });
        emit_call_symbol(ctx, STR_READ_OWN_SYMBOL, code);
    } else {
        emit_call_symbol(ctx, op_symbol, code);
    }
}

pub(crate) fn lower_string_substring(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    start: &BytecodeExpr,
    end: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.push(0x50); // push rax (string record)
    lower_native_expr(ctx, start, code)?; // start i64 -> rax
    code.push(0x50); // push rax (start)
    lower_native_expr(ctx, end, code)?; // end i64 -> rax
    code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (end)
    code.push(0x5A); // pop rdx (start)
    code.push(0x59); // pop rcx (string record)
    emit_str_read_op(ctx, s, STR_SUBSTRING_SYMBOL, code);
    Ok(())
}

/// Lower `s[i]`: stage the string record pointer into `rcx` and the char index
/// into `rdx`, then call the char-at helper, which leaves the `i`-th code point
/// (an `i64` `char` cell) in `rax`. Operands spill because evaluating the index
/// may clobber the string's register.
pub(crate) fn lower_string_char_at(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    index: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.push(0x50); // push rax (string record)
    lower_native_expr(ctx, index, code)?; // index i64 -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (index)
    code.push(0x59); // pop rcx (string record)
    emit_str_read_op(ctx, s, STR_CHAR_AT_SYMBOL, code);
    Ok(())
}

/// Lower `repeat(s, count)`: stage the source record into `rcx` and the count
/// into `rdx`, then call the repeat helper (result record in `rax`).
pub(crate) fn lower_string_repeat(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    count: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.push(0x50); // push rax (record)
    lower_native_expr(ctx, count, code)?; // count -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (count)
    code.push(0x59); // pop rcx (record)
    emit_str_read_op(ctx, s, STR_REPEAT_SYMBOL, code);
    Ok(())
}

/// Lower `trim(s)`: stage the source record into `rcx` and call the trim helper
/// (result record in `rax`).
pub(crate) fn lower_string_trim(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_str_read_op(ctx, s, STR_TRIM_SYMBOL, code);
    Ok(())
}

/// Lower `upper(s)`/`lower(s)`: stage the source record into `rcx` and call the
/// ASCII case-fold helper (fresh record in `rax`; a fresh-temp source is reclaimed
/// through `__lullaby_str_read_own`, like `trim`).
pub(crate) fn lower_string_case(
    ctx: &mut NativeCtx,
    s: &BytecodeExpr,
    symbol: &str,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, s, code)?; // record pointer -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_str_read_op(ctx, s, symbol, code);
    Ok(())
}

/// Lower a two-string operation (`find`/`contains`/`starts_with`/`ends_with`):
/// evaluate the first string record pointer into `rcx` and the second into `rdx`,
/// then call the named `.text` helper, which leaves its result (an `i64` char
/// index for `find`, a `0`/`1` bool for the predicates) in `rax`. The operands are
/// spilled because the right operand's evaluation may clobber the left's register.
pub(crate) fn lower_string_binary_op(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    right: &BytecodeExpr,
    symbol: &str,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, left, code)?; // left record pointer -> rax
    code.push(0x50); // push rax (left)
    lower_native_expr(ctx, right, code)?; // right record pointer -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax (right)
    code.push(0x59); // pop rcx (left)
    // If an operand is a uniquely-owned fresh string temporary it is dead after the
    // op reads it, so reclaim it via the ownership-aware indirect-call helper;
    // otherwise the bare op keeps zero overhead. (`is_owning_string_alloc` requires
    // a `string`-typed fresh alloc, so an `array<string>` operand of `join` is never
    // marked — it is not a plain string and must not be flat-`rc_dec`d.)
    let mask =
        (is_owning_string_alloc(left) as i32) | ((is_owning_string_alloc(right) as i32) << 1);
    if mask == 0 {
        emit_call_symbol(ctx, symbol, code);
    } else {
        code.extend_from_slice(&[0x41, 0xB8]); // mov r8d, mask
        code.extend_from_slice(&mask.to_le_bytes());
        // lea r9, [rip + <op symbol>] (REL32 relocation, like a call target).
        code.extend_from_slice(&[0x4C, 0x8D, 0x0D]);
        let site = code.len();
        code.extend_from_slice(&[0, 0, 0, 0]);
        ctx.relocations.push(CodeRelocation {
            offset: site as u32,
            symbol: symbol.to_string(),
        });
        emit_call_symbol(ctx, STR_BINOP_OWN_SYMBOL, code);
    }
    Ok(())
}

/// Lower `to_string(x)` to a fresh heap string record pointer in `rax`, matching
/// the interpreters' `Display`/`builtin_to_string`. An `f64`/`f32` argument
/// (dtoa) is deferred and rejected so the enclosing function skips gracefully.
pub(crate) fn lower_to_string(
    ctx: &mut NativeCtx,
    arg: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let type_name = arg.ty.name.as_str();
    match type_name {
        // Identity: a string is already a heap record pointer.
        "string" => lower_native_expr(ctx, arg, code),
        // `bool` -> "true"/"false".
        "bool" => {
            lower_native_expr(ctx, arg, code)?; // 0/1 -> rax
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, STR_FROM_BOOL_SYMBOL, code);
            Ok(())
        }
        // `char` -> the code point's UTF-8 encoding (a one-char string).
        "char" => {
            lower_native_expr(ctx, arg, code)?; // code point -> rax
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, STR_FROM_CHAR_SYMBOL, code);
            Ok(())
        }
        // `byte` (0..=255) -> unsigned decimal.
        "byte" => lower_int_to_string(ctx, arg, false, code),
        // `i64` and the fixed-width integers -> decimal, signed or unsigned by kind.
        "i64" => lower_int_to_string(ctx, arg, true, code),
        name => match fixed_int_kind(name) {
            Some(kind) => lower_int_to_string(ctx, arg, !kind.is_unsigned(), code),
            None => Err(format!(
                "to_string of `{name}` is not in the native subset (float to_string is deferred)"
            )),
        },
    }
}

/// Lower an integer `to_string(x)`: evaluate `x` into `rcx`, set `rdx` to the
/// signedness flag (nonzero = signed `i64` formatting, zero = unsigned `u64`),
/// then call `__lullaby_str_from_int`.
pub(crate) fn lower_int_to_string(
    ctx: &mut NativeCtx,
    arg: &BytecodeExpr,
    signed: bool,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, arg, code)?; // normalized cell -> rax
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (value)
    // mov edx, signed_flag  (0 or 1; zero-extends into rdx)
    code.push(0xBA);
    code.extend_from_slice(&(i32::from(signed)).to_le_bytes());
    emit_call_symbol(ctx, STR_FROM_INT_SYMBOL, code);
    Ok(())
}

// -- Growable list op lowering (native) --------------------------------------
//
// A `list<T>` value is a heap pointer to `[len i64][cap i64][slots]`. The heavy
// lifting (allocate, deep-copy, grow) lives in three `.text` helpers
// (`__lullaby_list_new`/`__lullaby_list_copy`/`__lullaby_list_grow`) so each call
// site stays small; the inline codegen below stages operands and calls them. The
// helper calls (and any list op) are `Call` IR nodes, so the frame reserves
// shadow space and stays 16-byte aligned at each `call` exactly like other calls.

/// `list_new()` -> a fresh `[len=0][cap=LIST_INITIAL_CAP][slots]` heap block
/// pointer in `rax`. Just calls the runtime helper.
pub(crate) fn lower_list_new(ctx: &mut NativeCtx, code: &mut Vec<u8>) {
    emit_call_symbol(ctx, LIST_NEW_SYMBOL, code);
}

/// Emit a relocated `call rel32` against a `.text` symbol, leaving the callee's
/// `rax` result in place. Used for the list runtime helpers.
pub(crate) fn emit_call_symbol(ctx: &mut NativeCtx, symbol: &str, code: &mut Vec<u8>) {
    code.push(0xE8);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: symbol.to_string(),
    });
}

/// Deep-copy the collection-slot value whose pointer is in `rax`, leaving a fresh
/// independent value's pointer in `rax`. This is the native realization of the
/// interpreters' recursive `Value::clone` on a MUTABLE-heap collection element /
/// map value / enum payload, mirroring the WASM backend's `emit_deep_copy`:
///
/// - a **scalar or `string`** slot needs no copy — the word in `rax` is already the
///   value (a `string`'s shared pointer IS its value-semantic clone since strings
///   are immutable), so this is the identity;
/// - a **`HeapStruct`** slot calls `__lullaby_struct_copy` (a fresh independent
///   field block, deep at the one-level bound);
/// - a nested **`List`** slot calls `__lullaby_list_copy` (its own elements are
///   scalars/strings at this bound, so a flat copy is an exact deep copy);
/// - a nested **`Map`** slot calls `__lullaby_map_copy`.
///
/// The call is emitted inline within a list/map op (a `Call` IR node), so the frame
/// already reserves shadow space and keeps `rsp` 16-byte aligned at the `call`.
pub(crate) fn emit_heap_slot_deep_copy(
    ctx: &mut NativeCtx,
    slot_ty: &NativeType,
    code: &mut Vec<u8>,
) {
    match slot_ty {
        NativeType::HeapStruct { .. } => {
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, STRUCT_COPY_SYMBOL, code);
        }
        NativeType::List { .. } => {
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, LIST_COPY_SYMBOL, code);
        }
        NativeType::Map { .. } => {
            code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
            emit_call_symbol(ctx, MAP_COPY_SYMBOL, code);
        }
        // Scalar or immutable string: the value in rax is already its own copy.
        _ => {}
    }
}

/// After a list has been flat-copied (via `__lullaby_list_copy`/`_grow`), walk its
/// `len` live element slots and replace each with an INDEPENDENT deep copy of
/// itself, so a mutable-aggregate element (`HeapStruct`/nested `List`/`Map`) is not
/// shared between the source list and the copy. `list_slot`/`elem_ty`: the list
/// pointer lives in a caller frame slot (so it survives the internal helper calls),
/// and `elem_ty` is the element's resolved `NativeType`. A scalar/string element
/// needs no fixup and this is a no-op. The fixup runs entirely on volatile registers
/// plus the frame slot; each per-element deep copy keeps `rsp` aligned at its call.
pub(crate) fn emit_list_deep_fixup(
    ctx: &mut NativeCtx,
    list_slot: i32,
    elem_ty: &NativeType,
    code: &mut Vec<u8>,
) {
    if !native_slot_needs_deep_copy(elem_ty) {
        return;
    }
    // A per-element counter local and a saved list-pointer local keep state across
    // the deep-copy calls (which clobber volatiles). Use two scratch frame slots.
    let saved_scratch = ctx.scratch_next;
    let i_slot = ctx.alloc_scratch(1);
    // i = 0
    emit_mov_rax_imm(code, 0);
    store_local(code, i_slot);
    let loop_top = code.len();
    // if i >= len -> done. rcx = list; r8 = len = [rcx + LIST_LEN_OFF].
    load_local(code, list_slot); // rax = list ptr
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_mov_r8_from_rcx_disp(code, LIST_LEN_OFF); // r8 = len
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    code.extend_from_slice(&[0x0F, 0x8D]); // jge done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // rax = [rcx + rax*8 + LIST_DATA_OFF]  (load element i pointer)
    code.extend_from_slice(&[0x48, 0x8B, 0x84, 0xC1]); // mov rax, [rcx + rax*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // rax = deep copy of the element.
    emit_heap_slot_deep_copy(ctx, elem_ty, code);
    // Store the fresh pointer back: [list + i*8 + LIST_DATA_OFF] = rax.
    // rcx = list; rdx = i.
    code.push(0x50); // push rax (fresh copy)
    load_local(code, list_slot); // rax = list ptr
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax  (i)
    code.push(0x58); // pop rax (fresh copy)
    // lea r8, [rcx + rdx*8 + LIST_DATA_OFF] ; mov [r8], rax
    code.extend_from_slice(&[0x4C, 0x8D, 0x84, 0xD1]); // lea r8, [rcx + rdx*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    code.extend_from_slice(&[0x49, 0x89, 0x00]); // mov [r8], rax
    // i += 1
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    store_local(code, i_slot);
    emit_jmp_to(code, loop_top);
    patch_rel32(code, done_site);
    ctx.scratch_next = saved_scratch;
}

/// After a map has been flat-copied (via `__lullaby_map_copy`), walk its `len` live
/// entries and replace each entry's VALUE word with an independent deep copy, so a
/// mutable-aggregate value (`map<K, struct>`) is not shared between the source and
/// the copy. Keys stay flat (they are integer-cell scalars). `map_slot`/`value_ty`
/// mirror [`emit_list_deep_fixup`]. A scalar/string value needs no fixup (no-op).
pub(crate) fn emit_map_deep_fixup(
    ctx: &mut NativeCtx,
    map_slot: i32,
    value_ty: &NativeType,
    code: &mut Vec<u8>,
) {
    if !native_slot_needs_deep_copy(value_ty) {
        return;
    }
    let saved_scratch = ctx.scratch_next;
    let i_slot = ctx.alloc_scratch(1);
    emit_mov_rax_imm(code, 0);
    store_local(code, i_slot);
    let loop_top = code.len();
    load_local(code, map_slot); // rax = map ptr
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_mov_r8_from_rcx_disp(code, MAP_LEN_OFF); // r8 = len
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x4C, 0x39, 0xC0]); // cmp rax, r8
    code.extend_from_slice(&[0x0F, 0x8D]); // jge done
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // Entry value address = rcx + MAP_DATA_OFF + i*MAP_ENTRY_SIZE + MAP_VALUE_OFF.
    // i*16 = i<<4. rax = i ; shl rax, 4 ; lea rdx, [rcx + rax + MAP_DATA_OFF+VALUE].
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    code.extend_from_slice(&[0x48, 0x8D, 0x94, 0x01]); // lea rdx, [rcx + rax + disp32]
    code.extend_from_slice(&(MAP_DATA_OFF + MAP_VALUE_OFF).to_le_bytes());
    // rax = [rdx]  (the entry value pointer)
    code.extend_from_slice(&[0x48, 0x8B, 0x02]); // mov rax, [rdx]
    emit_heap_slot_deep_copy(ctx, value_ty, code); // rax = fresh copy
    // Recompute the value slot address (rcx/rdx clobbered by the copy) and store.
    code.push(0x50); // push rax (fresh copy)
    load_local(code, map_slot);
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    load_local(code, i_slot); // rax = i
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    code.extend_from_slice(&[0x48, 0x8D, 0x94, 0x01]); // lea rdx, [rcx + rax + disp32]
    code.extend_from_slice(&(MAP_DATA_OFF + MAP_VALUE_OFF).to_le_bytes());
    code.push(0x58); // pop rax (fresh copy)
    code.extend_from_slice(&[0x48, 0x89, 0x02]); // mov [rdx], rax
    load_local(code, i_slot);
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    store_local(code, i_slot);
    emit_jmp_to(code, loop_top);
    patch_rel32(code, done_site);
    ctx.scratch_next = saved_scratch;
}

/// Construct a heap struct from a `Name(field…)` constructor call, leaving the
/// fresh field-0 pointer in `rax`. Allocates `STRUCT_HEADER_SIZE + nwords * 8`,
/// writes the `[nwords]` header, and materializes each field word (a scalar/string
/// through `rax`; a nested MUTABLE-aggregate field is out of the one-level bound and
/// never reaches here). The block is freshly allocated, so the returned value is
/// already an independent snapshot (no extra deep copy needed).
pub(crate) fn lower_heap_struct_construct(
    ctx: &mut NativeCtx,
    fields: &[(String, NativeType)],
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if args.len() != fields.len() {
        return Err("heap struct constructor field-count mismatch".to_string());
    }
    let nwords = fields.len() as i32;
    // rcx = STRUCT_HEADER_SIZE + nwords * 8 ; call __lullaby_alloc -> rax = base.
    emit_mov_rcx_imm(code, (STRUCT_HEADER_SIZE + nwords * 8) as i64);
    emit_call_symbol(ctx, HEAP_ALLOC_SYMBOL, code);
    // Stash the base pointer in a scratch slot across the field evaluations.
    let saved_scratch = ctx.scratch_next;
    let base_slot = ctx.alloc_scratch(1);
    store_local(code, base_slot);
    // [base] = nwords (header word).
    load_local(code, base_slot);
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_mov_rax_imm(code, nwords as i64);
    // mov [rcx], rax
    code.extend_from_slice(&[0x48, 0x89, 0x01]);
    // Each field word at [base + STRUCT_HEADER_SIZE + k*8].
    for (k, (arg, (_, field_ty))) in args.iter().zip(fields.iter()).enumerate() {
        // Evaluate the field value into rax. A field is a scalar or a `string`
        // (immutable pointer); a float field is out of scope (structs reject floats).
        if matches!(field_ty, NativeType::F64 | NativeType::F32) {
            return Err("float heap-struct fields are not in the native subset".to_string());
        }
        lower_native_expr(ctx, arg, code)?;
        code.push(0x50); // push rax (field value)
        load_local(code, base_slot);
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (base)
        code.push(0x58); // pop rax (field value)
        // mov [rcx + STRUCT_HEADER_SIZE + k*8], rax
        code.extend_from_slice(&[0x48, 0x89, 0x81]);
        code.extend_from_slice(&(STRUCT_HEADER_SIZE + k as i32 * 8).to_le_bytes());
    }
    // rax = field-0 pointer = base + STRUCT_HEADER_SIZE.
    load_local(code, base_slot);
    emit_add_rax_imm32(code, STRUCT_HEADER_SIZE);
    ctx.scratch_next = saved_scratch;
    Ok(())
}

/// Produce a fresh, INDEPENDENT collection-slot value pointer in `rax` for a
/// MUTABLE-aggregate element/value being stored (by `push`/`set`/`map_set`).
///
/// - A **struct constructor** (`Point(1, 2)`) is built directly on the heap
///   ([`lower_heap_struct_construct`]) — already independent.
/// - A **nested-list literal or any other expression** yielding a `List`/`Map`/
///   `HeapStruct` pointer is evaluated and then DEEP-COPIED, so a later mutation of
///   the source binding never leaks into the collection (the interpreters clone the
///   argument `Value` before storing it).
pub(crate) fn lower_heap_slot_value(
    ctx: &mut NativeCtx,
    slot_ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if let NativeType::HeapStruct { name, fields } = slot_ty {
        if let BytecodeExprKind::Call { name: cname, args } = &value.kind
            && cname == name
        {
            // A direct constructor: build fresh on the heap (already independent).
            return lower_heap_struct_construct(ctx, fields, args, code);
        }
        // Any other `HeapStruct` source is a stack-flattened struct value (a struct
        // variable, parameter, or call/`get` result), NOT a `[nwords][field words]`
        // heap block. `emit_heap_slot_deep_copy` would hand its stack pointer to
        // `__lullaby_struct_copy`, which reads the word count at `[ptr - 8]` and walks
        // off into an adjacent frame word (a corrupt scalar + bad string pointer →
        // SIGSEGV). There is no stack->heap bridge for STORING a struct value into a
        // heap slot (only the reverse `get`-into-local bridge exists), so demote the
        // enclosing function to the interpreters (a clean skip via the second-pass
        // fixpoint) — exactly as the backend did before a struct became a collection
        // element. Default-deny: never miscompiled, and the inline-constructor path
        // above (`push(l, Rec("x", 1))`, `map_set(m, k, Rec(…))`, `some(Rec(…))`)
        // stays intact.
        return Err(format!(
            "storing a non-constructor `{name}` struct value into a collection element / \
             map value / enum payload is not in the native subset: a struct variable, \
             parameter, or call/`get` result is stack-flattened, not a heap block, so it \
             cannot be deep-copied into a heap slot (only an inline struct constructor is \
             supported)"
        ));
    }
    // A `List`/`Map` slot value is a genuine heap pointer even as a local, so it can be
    // evaluated and deep-copied in place.
    lower_native_expr(ctx, value, code)?;
    emit_heap_slot_deep_copy(ctx, slot_ty, code);
    Ok(())
}

/// Lower `push(l, x) -> list<T>` (value-semantic append): deep-copy `l`, grow the
/// copy if it is full, store `x` into slot `len`, bump `len`, and leave the fresh
/// list pointer in `rax`. Because `push` always returns a NEW list,
/// `l = push(l, x)` matches the interpreters' `Value::clone`-then-append.
pub(crate) fn lower_list_push(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "push expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    let deep_elem = native_slot_needs_deep_copy(&elem_ty);
    // rax = deep copy of the source list.
    lower_native_expr(ctx, list, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (copy source)
    emit_call_symbol(ctx, LIST_COPY_SYMBOL, code); // rax = fresh copy
    // Ensure room for one more element: rax = grow(copy) (a no-op when cap > len).
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, LIST_GROW_SYMBOL, code); // rax = grown copy
    // For a MUTABLE-aggregate element, deep-copy the copied list's existing elements
    // so they are independent of the source list (the flat helper copy shared them).
    let saved_scratch = ctx.scratch_next;
    let list_slot = ctx.alloc_scratch(1);
    store_local(code, list_slot); // stash the (grown) list pointer
    if deep_elem {
        emit_list_deep_fixup(ctx, list_slot, &elem_ty, code);
    }
    load_local(code, list_slot); // rax = list ptr
    code.push(0x50); // push rax (save the list pointer across value evaluation)
    // Evaluate the value to append into rax (a scalar, a float bit pattern, or a
    // MUTABLE-aggregate pointer that is deep-copied so a later mutation of the
    // source value never leaks into the list — matching the interpreters).
    if let Some(width) = FloatWidth::from_type_name(&elem.name) {
        lower_native_float_expr(ctx, value, code)?;
        emit_movq_rax_from_xmm0(code, width);
    } else if deep_elem {
        lower_heap_slot_value(ctx, &elem_ty, value, code)?;
    } else {
        lower_native_expr(ctx, value, code)?;
    }
    ctx.scratch_next = saved_scratch;
    // rcx = list pointer (restored); the element value stays in rax.
    code.push(0x59); // pop rcx
    // r8 = len = [rcx + LIST_LEN_OFF]
    emit_mov_r8_from_rcx_disp(code, LIST_LEN_OFF);
    // Element slot address: rdx = rcx + LIST_DATA_OFF + r8*8.
    // lea rdx, [rcx + r8*8 + LIST_DATA_OFF]
    code.extend_from_slice(&[0x4A, 0x8D, 0x94, 0xC1]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // mov [rdx], rax  (store the element word)
    code.extend_from_slice(&[0x48, 0x89, 0x02]);
    // len += 1: r8 += 1; mov [rcx + LIST_LEN_OFF], r8
    code.extend_from_slice(&[0x49, 0xFF, 0xC0]); // inc r8
    emit_mov_rcx_disp_from_r8(code, LIST_LEN_OFF);
    // Result: the list pointer.
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    Ok(())
}

/// Lower `set(l, i, x) -> list<T>` (value-semantic replace): deep-copy `l`, store
/// `x` into element slot `i` of the copy, leave the fresh list pointer in `rax`.
/// The index is bounds-checked against the copy's `len` header: an out-of-range
/// (or negative) index traps with `ud2` — the safe-tier guarantee, matching the
/// interpreters' `L0413` — instead of writing past the live elements.
pub(crate) fn lower_list_set(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    index: &BytecodeExpr,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "set expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    let deep_elem = native_slot_needs_deep_copy(&elem_ty);
    // rax = deep copy of the source list.
    lower_native_expr(ctx, list, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, LIST_COPY_SYMBOL, code); // rax = fresh copy
    // Deep-copy the copied list's existing elements so the returned list is fully
    // independent of the source (the flat helper copy shared their pointers).
    let saved_scratch = ctx.scratch_next;
    let list_slot = ctx.alloc_scratch(1);
    if deep_elem {
        store_local(code, list_slot);
        emit_list_deep_fixup(ctx, list_slot, &elem_ty, code);
        load_local(code, list_slot);
    }
    code.push(0x50); // push rax (the copy pointer)
    // rax = index (i64).
    lower_native_expr(ctx, index, code)?;
    code.push(0x50); // push rax (index)
    // Evaluate the replacement value into rax (float via xmm0 -> rax; a MUTABLE
    // aggregate is built/deep-copied fresh so it is independent of its source).
    if let Some(width) = FloatWidth::from_type_name(&elem.name) {
        lower_native_float_expr(ctx, value, code)?;
        emit_movq_rax_from_xmm0(code, width);
    } else if deep_elem {
        lower_heap_slot_value(ctx, &elem_ty, value, code)?;
    } else {
        lower_native_expr(ctx, value, code)?;
    }
    ctx.scratch_next = saved_scratch;
    code.push(0x59); // pop rcx (index)
    code.push(0x5A); // pop rdx (list pointer)
    // Bounds check (safe-tier guarantee, matching the interpreters' L0413): trap
    // with `ud2` unless `0 <= index < len`. One UNSIGNED compare of the index in
    // `rcx` against the copy's `len` header word catches both a negative index and
    // `index >= len`, so an out-of-range `set` faults deterministically instead of
    // writing past the live elements. `r10` is scratch here (rax = value to store,
    // rcx = index, rdx = list ptr — all preserved).
    code.extend_from_slice(&[0x4C, 0x8B, 0x92]); // mov r10, [rdx + disp32]
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xD1]); // cmp rcx, r10
    code.extend_from_slice(&[0x72, 0x02]); // jb +2 (in bounds -> skip the trap)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2   (out of bounds -> fault)
    // Element slot address: rdx = rdx + LIST_DATA_OFF + rcx*8.
    // lea rdx, [rdx + rcx*8 + LIST_DATA_OFF]
    code.extend_from_slice(&[0x48, 0x8D, 0x94, 0xCA]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // mov [rdx], rax  (store the element)
    code.extend_from_slice(&[0x48, 0x89, 0x02]);
    // Result: the (copied) list pointer. Recompute it: rax = rdx - (index*8 + DATA).
    // Simpler: the copy pointer was pushed first; recover by reloading is complex,
    // so instead keep it: we overwrote rdx with the slot address. Recompute the
    // base by subtracting the same offset.
    // rax = rdx ; rax -= rcx*8 ; rax -= LIST_DATA_OFF
    code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx
    code.extend_from_slice(&[0x48, 0xC1, 0xE1, 0x03]); // shl rcx, 3
    code.extend_from_slice(&[0x48, 0x29, 0xC8]); // sub rax, rcx
    emit_sub_rax_imm32(code, LIST_DATA_OFF);
    Ok(())
}

/// Lower `pop(l) -> list<T>` (value-semantic remove-last): deep-copy `l`,
/// decrement the copy's `len` (the slot stays allocated, like `Vec::pop`), leave
/// the fresh list pointer in `rax`. Popping an empty list is a contract violation:
/// the native path traps with `ud2` when `len <= 0` (before the decrement), the
/// safe-tier guarantee matching the interpreters' `L0413`, instead of underflowing
/// `len` toward `-1`.
pub(crate) fn lower_list_pop(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "pop expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    lower_native_expr(ctx, list, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, LIST_COPY_SYMBOL, code); // rax = fresh copy
    // Deep-copy the copied list's remaining elements so the returned list is
    // independent of the source (the flat helper copy shared their pointers).
    if native_slot_needs_deep_copy(&elem_ty) {
        let saved_scratch = ctx.scratch_next;
        let list_slot = ctx.alloc_scratch(1);
        store_local(code, list_slot);
        emit_list_deep_fixup(ctx, list_slot, &elem_ty, code);
        load_local(code, list_slot);
        ctx.scratch_next = saved_scratch;
    }
    // len -= 1: r8 = [rax + LIST_LEN_OFF]; r8 -= 1; [rax + LIST_LEN_OFF] = r8.
    // mov r8, [rax + LIST_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x80]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // Non-empty check (safe-tier guarantee, matching the interpreters' L0413): a
    // SIGNED test of `len` traps with `ud2` when `len <= 0`, so popping an empty
    // list faults deterministically instead of underflowing `len` toward `-1`.
    code.extend_from_slice(&[0x4D, 0x85, 0xC0]); // test r8, r8
    code.extend_from_slice(&[0x7F, 0x02]); // jg +2 (len > 0 -> skip the trap)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2  (empty -> fault)
    code.extend_from_slice(&[0x49, 0xFF, 0xC8]); // dec r8
    // mov [rax + LIST_LEN_OFF], r8
    code.extend_from_slice(&[0x4C, 0x89, 0x80]);
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // Result: the (copied) list pointer already in rax.
    Ok(())
}

/// Lower `get(l, i) -> T`: load element `i` from `l + LIST_DATA_OFF + i*8`. A
/// float element is loaded back into `xmm0` by the float-expr path; this integer
/// path loads the raw word into `rax` (a float `get` result is handled by
/// `lower_native_float_expr`'s list-get case).
///
/// For a MUTABLE-aggregate element (`HeapStruct`/nested `List`/`Map`) the loaded
/// element pointer is DEEP-COPIED (the interpreters' `values[i].clone()`), so the
/// returned value is independent of the list: mutating the retrieved copy never
/// touches the list's element. The result is a heap pointer word; a consumer that
/// wants the stack-flattened struct (a `Struct`-typed local or call argument)
/// bridges it via [`lower_aggregate_init`]'s heap-source path.
/// `a[i]` on a heap `array<string>` (a `list<string>`-layout block): load the
/// `i`-th slot's shared string pointer, bounds-checked against the `len` header.
/// An out-of-range index (including a negative one, caught by the unsigned compare)
/// traps with `ud2`, mirroring the interpreters' `L0413`.
pub(crate) fn lower_array_string_index(
    ctx: &mut NativeCtx,
    target: &BytecodeExpr,
    index: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, index, code)?; // rax = index
    code.push(0x50); // push rax (index)
    lower_native_expr(ctx, target, code)?; // rax = block pointer
    code.push(0x59); // pop rcx (index)
    // r10 = [rax + LIST_LEN_OFF] (element count).
    code.extend_from_slice(&[0x4C, 0x8B, 0x90]); // mov r10, [rax + disp32]
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    // if (unsigned) index >= len -> trap. cmp rcx, r10 ; jb ok ; ud2 ; ok:
    code.extend_from_slice(&[0x4C, 0x39, 0xD1]); // cmp rcx, r10
    code.extend_from_slice(&[0x72, 0x02]); // jb +2 (skip the ud2)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2
    // rax = [rax + rcx*8 + LIST_DATA_OFF] (the shared string pointer word).
    code.extend_from_slice(&[0x48, 0x8B, 0x84, 0xC8]);
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    Ok(())
}

pub(crate) fn lower_list_get(
    ctx: &mut NativeCtx,
    list: &BytecodeExpr,
    index: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let elem = supported_list_element(&list.ty).ok_or_else(|| {
        format!(
            "get expects a supported-element list but got `{}`",
            list.ty.name
        )
    })?;
    let elem_ty = native_collection_slot(&elem, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("list element `{}` is not layable-out", elem.name))?;
    // rax = index; push it; rax = list pointer; pop rcx = index.
    lower_native_expr(ctx, index, code)?;
    code.push(0x50); // push rax (index)
    lower_native_expr(ctx, list, code)?; // rax = list pointer
    code.push(0x59); // pop rcx (index)
    // Bounds check (safe-tier guarantee, matching the interpreters' L0413): trap
    // with `ud2` unless `0 <= index < len`. One UNSIGNED compare of the index in
    // `rcx` against the list's `len` header word catches both a negative index (a
    // huge unsigned value) and `index >= len`, so an out-of-range `get` faults
    // deterministically instead of reading past the live elements. `r10` is a
    // scratch register free here (rax = list ptr, rcx = index).
    code.extend_from_slice(&[0x4C, 0x8B, 0x90]); // mov r10, [rax + disp32]
    code.extend_from_slice(&LIST_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x39, 0xD1]); // cmp rcx, r10
    code.extend_from_slice(&[0x72, 0x02]); // jb +2 (in bounds -> skip the trap)
    code.extend_from_slice(&[0x0F, 0x0B]); // ud2   (out of bounds -> fault)
    // rax = [rax + rcx*8 + LIST_DATA_OFF]
    code.extend_from_slice(&[0x48, 0x8B, 0x84, 0xC8]); // mov rax, [rax + rcx*8 + disp32]
    code.extend_from_slice(&LIST_DATA_OFF.to_le_bytes());
    // A mutable-aggregate element is returned as an independent deep copy.
    if native_slot_needs_deep_copy(&elem_ty) {
        emit_heap_slot_deep_copy(ctx, &elem_ty, code);
    }
    Ok(())
}

// -- Growable map op lowering (native) ---------------------------------------
//
// A `map<K, V>` value is a heap pointer to `[len i64][cap i64][entries]` (each
// entry a `(key, value)` word pair). The heavy lifting (allocate, deep-copy,
// grow, linear-scan) lives in four `.text` helpers (`__lullaby_map_new`/`_copy`/
// `_grow`/`_find`), so each call site stays small; the inline codegen below
// stages operands, calls them, and stores results. Every map op is a `Call` IR
// node, so the frame reserves shadow space and stays 16-byte aligned at each
// `call` exactly like other calls and the list ops.

/// `map_new()` -> a fresh `[len=0][cap=MAP_INITIAL_CAP][entries]` heap block
/// pointer in `rax`. Just calls the runtime helper.
pub(crate) fn lower_map_new(ctx: &mut NativeCtx, code: &mut Vec<u8>) {
    emit_call_symbol(ctx, MAP_NEW_SYMBOL, code);
}

/// Evaluate a map key expression into `rax`. Keys are integer-cell scalars
/// (`supported_map_kv` rejects float keys), so the ordinary integer expression
/// path yields the normalized key word directly.
pub(crate) fn lower_map_key(
    ctx: &mut NativeCtx,
    key: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    lower_native_expr(ctx, key, code)
}

/// Evaluate a map value expression into `rax` as a flat 8-byte word (a float value
/// is moved bit-for-bit from `xmm0` through `rax`, mirroring the list element
/// path), so it can be stored into an entry's value slot.
pub(crate) fn lower_map_value_word(
    ctx: &mut NativeCtx,
    value_ty: &TypeRef,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if let Some(width) = FloatWidth::from_type_name(&value_ty.name) {
        lower_native_float_expr(ctx, value, code)?;
        emit_movq_rax_from_xmm0(code, width);
    } else {
        lower_native_expr(ctx, value, code)?;
    }
    Ok(())
}

/// Lower `map_set(m, k, v) -> map<K, V>` (value-semantic insert/update): deep-copy
/// `m`, scan the copy for `k`; if found, overwrite that entry's value slot in
/// place (preserving order); otherwise grow when full (capacity doubling) and
/// append a new `(k, v)` entry, bumping `len`. Leaves the fresh map pointer in
/// `rax`. Because `map_set` always returns a NEW map, `m = map_set(m, k, v)`
/// matches the interpreters' clone-then-mutate on the insertion-ordered list.
pub(crate) fn lower_map_set(
    ctx: &mut NativeCtx,
    map: &BytecodeExpr,
    key: &BytecodeExpr,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let (_key_ty, value_ty) = supported_map_kv(&map.ty).ok_or_else(|| {
        format!(
            "map_set expects a scalar-key, scalar-value map but got `{}`",
            map.ty.name
        )
    })?;
    let value_native = native_collection_slot(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value `{}` is not layable-out", value_ty.name))?;
    let deep_value = native_slot_needs_deep_copy(&value_native);
    // rax = deep copy of the source map (value semantics).
    lower_native_expr(ctx, map, code)?;
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
    emit_call_symbol(ctx, MAP_COPY_SYMBOL, code); // rax = fresh copy
    // For a MUTABLE-aggregate value, deep-copy the copied map's existing entry
    // values so the returned map is fully independent of the source (the flat helper
    // copy shared their pointers).
    let saved_scratch = ctx.scratch_next;
    if deep_value {
        let map_slot = ctx.alloc_scratch(1);
        store_local(code, map_slot);
        emit_map_deep_fixup(ctx, map_slot, &value_native, code);
        load_local(code, map_slot);
    }
    code.push(0x50); // push rax (the copy pointer)
    // Evaluate the key into a saved word (any nested call balances its own stack).
    lower_map_key(ctx, key, code)?;
    code.push(0x50); // push rax (key)
    // Evaluate the value word (float via xmm0 -> rax) and save it. A MUTABLE
    // aggregate value is built/deep-copied fresh so it is independent of its source.
    if deep_value {
        lower_heap_slot_value(ctx, &value_native, value, code)?;
    } else {
        lower_map_value_word(ctx, &value_ty, value, code)?;
    }
    code.push(0x50); // push rax (value)
    // Restore into stable non-argument-clobbered registers: r8 = value, rdx = key,
    // rcx = map copy. All three pops complete before the find call, so rsp is back
    // to the frame-aligned base at that `call`. `__lullaby_map_find` reads rcx/rdx
    // as its args and clobbers only rax/r10/r11, so rcx/rdx/r8 survive the call and
    // need no save/restore around it.
    code.push(0x41);
    code.push(0x58); // pop r8 (value)
    code.push(0x5A); // pop rdx (key)
    code.push(0x59); // pop rcx (map copy)
    emit_call_symbol(ctx, MAP_FIND_SYMBOL, code); // rax = index or len (rcx=map, rdx=key)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]); // mov r10, [rcx + disp32]
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // if rax (found index) == r10 (len) -> append; else overwrite.
    code.extend_from_slice(&[0x4C, 0x39, 0xD0]); // cmp rax, r10
    code.extend_from_slice(&[0x0F, 0x84]); // je append (rel32)
    let append_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- overwrite branch: store value into entry `rax`'s value slot ---
    // entry addr = rcx + MAP_DATA_OFF + rax * MAP_ENTRY_SIZE (16). rax*16 = rax<<4.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    // lea r11, [rcx + rax + MAP_DATA_OFF]
    code.extend_from_slice(&[0x4C, 0x8D, 0x9C, 0x01]); // lea r11, [rcx + rax + disp32]
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // mov [r11 + MAP_VALUE_OFF], r8
    code.extend_from_slice(&[0x4D, 0x89, 0x83]); // mov [r11 + disp32], r8
    code.extend_from_slice(&MAP_VALUE_OFF.to_le_bytes());
    // rax = rcx (result map pointer) ; jmp done
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    code.push(0xE9); // jmp done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- append branch ---
    patch_rel32(code, append_site);
    // Grow if full: rcx = grow(map). Save key(rdx)/value(r8) across the call.
    code.push(0x52); // push rdx (key)
    code.push(0x41);
    code.push(0x50); // push r8 (value)
    emit_call_symbol(ctx, MAP_GROW_SYMBOL, code); // rax = grown map (rcx = map arg)
    code.push(0x41);
    code.push(0x58); // pop r8 (value)
    code.push(0x5A); // pop rdx (key)
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (rcx = grown map)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // entry addr of index `len`: r11 = rcx + MAP_DATA_OFF + len*16.
    // r10*16: shl r10, 4
    code.extend_from_slice(&[0x49, 0xC1, 0xE2, 0x04]); // shl r10, 4
    // lea r11, [rcx + r10 + MAP_DATA_OFF]  (REX.WRX: r11 dest, r10 index)
    code.extend_from_slice(&[0x4E, 0x8D, 0x9C, 0x11]); // lea r11, [rcx + r10 + disp32]
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // mov [r11], rdx  (store key word at the entry's key slot, offset 0)
    code.extend_from_slice(&[0x49, 0x89, 0x13]); // mov [r11], rdx
    // mov [r11 + MAP_VALUE_OFF], r8  (store value word)
    code.extend_from_slice(&[0x4D, 0x89, 0x83]);
    code.extend_from_slice(&MAP_VALUE_OFF.to_le_bytes());
    // len += 1: r10 currently holds len<<4; reload len and bump it.
    // mov r10, [rcx + MAP_LEN_OFF]; inc r10; mov [rcx + MAP_LEN_OFF], r10
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xFF, 0xC2]); // inc r10
    code.extend_from_slice(&[0x4C, 0x89, 0x91]); // mov [rcx + disp32], r10
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // rax = rcx (result map pointer)
    code.extend_from_slice(&[0x48, 0x89, 0xC8]);
    // done:
    patch_rel32(code, done_site);
    ctx.scratch_next = saved_scratch;
    Ok(())
}

/// Lower `map_has(m, k) -> bool`: scan for `k`, leaving `found != len` (1 if
/// present, 0 if absent) in `rax`.
pub(crate) fn lower_map_has(
    ctx: &mut NativeCtx,
    map: &BytecodeExpr,
    key: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    supported_map_kv(&map.ty).ok_or_else(|| {
        format!(
            "map_has expects a scalar-key, scalar-value map but got `{}`",
            map.ty.name
        )
    })?;
    // Evaluate key first, save it; then map into rcx; restore key into rdx. Both
    // pushes are popped before the find call, so rsp is frame-aligned there.
    // `__lullaby_map_find` preserves rcx (map) and clobbers only rax/r10/r11, so
    // the map pointer survives for the post-call `len` reload.
    lower_map_key(ctx, key, code)?;
    code.push(0x50); // push rax (key)
    lower_native_expr(ctx, map, code)?; // rax = map pointer
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (map)
    code.push(0x5A); // pop rdx (key)
    emit_call_symbol(ctx, MAP_FIND_SYMBOL, code); // rax = index or len (rcx=map, rdx=key)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // result = (rax != len) ? 1 : 0. cmp rax, r10 ; setne al ; movzx eax, al.
    code.extend_from_slice(&[0x4C, 0x39, 0xD0]); // cmp rax, r10
    code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
    Ok(())
}

/// Materialize `map_get(m, k) -> option<V>` into the aggregate words at
/// `base_slot`: word 0 = the option tag (`some`=0 / `none`=1), word 1 = the value
/// payload (for `some`). Deep-copy is NOT needed (read-only). Scans for `k`; when
/// found builds `some(value)` (loading the entry's value slot), else `none`,
/// reusing the native enum/option layout. `result_ty` is the call's `option<V>`
/// type, from which the `some`/`none` layout is resolved.
pub(crate) fn lower_map_get_into(
    ctx: &mut NativeCtx,
    base_slot: i32,
    result_ty: &TypeRef,
    map: &BytecodeExpr,
    key: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let (_k, value_ty) = supported_map_kv(&map.ty)
        .ok_or_else(|| format!("map_get expects a supported map but got `{}`", map.ty.name))?;
    let value_native = native_collection_slot(&value_ty, ctx.structs, ctx.enums, 0)
        .ok_or_else(|| format!("map value `{}` is not layable-out", value_ty.name))?;
    let deep_value = native_slot_needs_deep_copy(&value_native);
    // Resolve the option layout to get the `some`/`none` tags (some=0, none=1).
    let layout = resolve_native_type(result_ty, ctx.structs, ctx.enums)?;
    let NativeType::Enum { variants, .. } = &layout else {
        return Err(format!(
            "map_get result type `{}` is not a supported option enum",
            result_ty.name
        ));
    };
    let some_tag = variants
        .iter()
        .find(|v| v.name == "some")
        .map(|v| v.tag)
        .ok_or_else(|| "map_get result option layout missing `some` variant".to_string())?;
    let none_tag = variants
        .iter()
        .find(|v| v.name == "none")
        .map(|v| v.tag)
        .ok_or_else(|| "map_get result option layout missing `none` variant".to_string())?;

    // Evaluate key, save; map into rcx; restore key into rdx; keep map across find.
    lower_map_key(ctx, key, code)?;
    code.push(0x50); // push rax (key)
    lower_native_expr(ctx, map, code)?; // rax = map pointer
    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (map)
    code.push(0x5A); // pop rdx (key)
    // Both pushes are popped before the call; `__lullaby_map_find` preserves rcx
    // (map), so the pointer survives for the value load / `len` reload below.
    emit_call_symbol(ctx, MAP_FIND_SYMBOL, code); // rax = index or len (rcx=map, rdx=key)
    // r10 = len = [rcx + MAP_LEN_OFF]
    code.extend_from_slice(&[0x4C, 0x8B, 0x91]);
    code.extend_from_slice(&MAP_LEN_OFF.to_le_bytes());
    // if rax == len -> none; else some(value).
    code.extend_from_slice(&[0x4C, 0x39, 0xD0]); // cmp rax, r10
    code.extend_from_slice(&[0x0F, 0x84]); // je none (rel32)
    let none_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- some branch: payload = entry(rax).value, then tag = some_tag ---
    // entry addr: r11 = rcx + MAP_DATA_OFF + rax*16.
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x04]); // shl rax, 4
    code.extend_from_slice(&[0x4C, 0x8D, 0x9C, 0x01]); // lea r11, [rcx + rax + disp32]
    code.extend_from_slice(&MAP_DATA_OFF.to_le_bytes());
    // rax = [r11 + MAP_VALUE_OFF]  (the value word); store it as the payload word.
    code.extend_from_slice(&[0x49, 0x8B, 0x83]); // mov rax, [r11 + disp32]
    code.extend_from_slice(&MAP_VALUE_OFF.to_le_bytes());
    // A MUTABLE-aggregate value is returned as an INDEPENDENT deep copy (the
    // interpreters' `values[i].clone()`), so mutating the retrieved `some` payload
    // never touches the map's entry.
    if deep_value {
        emit_heap_slot_deep_copy(ctx, &value_native, code);
    }
    store_local(code, base_slot - 8); // payload word (8 bytes above the tag)
    // tag word = some_tag at base_slot.
    emit_mov_rax_imm(code, some_tag);
    store_local(code, base_slot);
    code.push(0xE9); // jmp done (rel32)
    let done_site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    // --- none branch: tag word = none_tag ---
    patch_rel32(code, none_site);
    emit_mov_rax_imm(code, none_tag);
    store_local(code, base_slot); // tag word (payload word left untouched)
    // done:
    patch_rel32(code, done_site);
    Ok(())
}

/// `parse_i64(s) -> result<i64, string>`: evaluate the string operand into `rcx`,
/// call the `__lullaby_parse_i64` helper (tag in `rax`, payload in `rdx`), and
/// store the two words into `base_slot` (tag) and `base_slot - 8` (payload — 8
/// bytes above the tag in the ascending layout). `ok`
/// is tag `0` with the parsed `i64` payload; `err` is tag `1` with a freshly-built
/// error-message string-record pointer payload, matching the `result<T, E>`
/// variant order (`ok` then `err`) the interpreters use.
pub(crate) fn lower_parse_i64_into(
    ctx: &mut NativeCtx,
    base_slot: i32,
    arg: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    if !is_string_type(&arg.ty) {
        return Err("parse_i64 expects a string argument".to_string());
    }
    lower_native_expr(ctx, arg, code)?; // rax = source string record pointer
    emit_mov_reg_reg(code, REG_RCX, REG_RAX); // rcx = source pointer (arg 0)
    emit_call_symbol(ctx, PARSE_I64_SYMBOL, code); // rax = tag, rdx = payload
    // Payload word (8 bytes above the tag): mov [rbp - (base_slot - 8)], rdx.
    code.extend_from_slice(&[0x48, 0x89, 0x95]);
    code.extend_from_slice(&(-(base_slot - 8)).to_le_bytes());
    // Tag word at base_slot (store_local writes rax, the tag).
    store_local(code, base_slot);
    Ok(())
}

/// `movq rax, xmm0` (f64) or `movd eax, xmm0` (f32, zero-extending the low four
/// bytes into rax) — move a float's bit pattern into `rax` so it can be stored as
/// a flat 8-byte list element word bit-for-bit.
pub(crate) fn emit_movq_rax_from_xmm0(code: &mut Vec<u8>, width: FloatWidth) {
    match width {
        // movq rax, xmm0 : 66 48 0F 7E C0
        FloatWidth::F64 => code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]),
        // movd eax, xmm0 : 66 0F 7E C0  (writing eax zero-extends into rax)
        FloatWidth::F32 => code.extend_from_slice(&[0x66, 0x0F, 0x7E, 0xC0]),
    }
}

/// `movq xmm0, rax` (f64) or `movd xmm0, eax` (f32) — move a raw list element
/// word's bit pattern from `rax` into `xmm0` at the element's float width, for a
/// float-element `get`.
pub(crate) fn emit_movq_xmm0_from_rax(code: &mut Vec<u8>, width: FloatWidth) {
    match width {
        // movq xmm0, rax : 66 48 0F 6E C0
        FloatWidth::F64 => code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]),
        // movd xmm0, eax : 66 0F 6E C0
        FloatWidth::F32 => code.extend_from_slice(&[0x66, 0x0F, 0x6E, 0xC0]),
    }
}

/// `mov rax, [rax + disp32]` — dereference a pointer in `rax` at a byte offset.
pub(crate) fn emit_mov_rax_from_rax_disp(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x48, 0x8B, 0x80]); // mov rax, [rax + disp32]
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `mov r8, [rcx + disp32]`.
pub(crate) fn emit_mov_r8_from_rcx_disp(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x4C, 0x8B, 0x81]); // mov r8, [rcx + disp32]
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `mov [rcx + disp32], r8`.
pub(crate) fn emit_mov_rcx_disp_from_r8(code: &mut Vec<u8>, disp: i32) {
    code.extend_from_slice(&[0x4C, 0x89, 0x81]); // mov [rcx + disp32], r8
    code.extend_from_slice(&disp.to_le_bytes());
}

/// `sub rax, imm32`.
pub(crate) fn emit_sub_rax_imm32(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x2D]); // sub rax, imm32
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `add rax, imm32`.
pub(crate) fn emit_add_rax_imm32(code: &mut Vec<u8>, imm: i32) {
    code.extend_from_slice(&[0x48, 0x05]); // add rax, imm32
    code.extend_from_slice(&imm.to_le_bytes());
}

/// `mov rcx, imm64` (10-byte form).
pub(crate) fn emit_mov_rcx_imm(code: &mut Vec<u8>, value: i64) {
    code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
    code.extend_from_slice(&value.to_le_bytes());
}
