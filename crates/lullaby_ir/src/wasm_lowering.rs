//! WASM expression/operation lowering: expressions, binary ops, string concat,
//! the index-based string-op codegen, and struct/enum/array construction. Split
//! out of wasm.rs; sees the module-assembly types and encoders via `use super::*`.

use super::*;

// -- Expression lowering -----------------------------------------------------

pub(crate) fn lower_expr(
    ctx: &mut LowerCtx,
    expr: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    match &expr.kind {
        IrExprKind::Integer(value) => {
            out.push(0x42); // i64.const
            write_sleb(out, *value);
            Ok(())
        }
        IrExprKind::Float(value) => {
            // A float literal's static type pins it to `f32` or `f64` (the type
            // checker resolves every literal to a concrete float type). An `f32`
            // literal rounds `value` to single precision first so its bits match
            // the interpreter's real `f32` store, then emits `f32.const`.
            if expr.ty.name == "f32" {
                out.push(0x43); // f32.const
                out.extend_from_slice(&(*value as f32).to_le_bytes());
            } else {
                out.push(0x44); // f64.const
                out.extend_from_slice(&value.to_le_bytes());
            }
            Ok(())
        }
        IrExprKind::Bool(value) => {
            out.push(0x41); // i32.const
            write_sleb(out, if *value { 1 } else { 0 });
            Ok(())
        }
        IrExprKind::Char(value) => {
            out.push(0x41); // i32.const
            write_sleb(out, *value as i64);
            Ok(())
        }
        // A `Local` only exists in the interpreter's resolved copy of the IR, so
        // it never reaches WASM lowering in practice; handle it identically to a
        // named `Variable` for match completeness and defensive correctness.
        IrExprKind::Variable(name) | IrExprKind::Local { name, .. } => {
            let local = ctx
                .locals
                .get(name)
                .ok_or_else(|| format!("unknown variable `{name}`"))?;
            get_local(out, local.index);
            Ok(())
        }
        IrExprKind::Unary { op, expr: inner } => match op {
            UnaryOp::Not => {
                lower_expr(ctx, inner, out)?;
                out.push(0x45); // i32.eqz (bool not)
                Ok(())
            }
            // Integer bitwise NOT (`~`): one's complement, implemented as
            // `x xor -1` (WASM has no `i64.not`). On a fixed-width kind the result
            // is re-normalized to the width, matching the interpreter's
            // `Value::int(!v, ty)`; on plain `i64` the full-width complement is
            // exact. Any other operand type is rejected (falls back to the
            // interpreters).
            UnaryOp::BitNot => {
                let kind = fixed_int_kind(inner.ty.name.as_str());
                if kind.is_none() && inner.ty.name != "i64" {
                    return Err(format!(
                        "bitwise `~` on unsupported type `{}` (wasm backend)",
                        inner.ty.name
                    ));
                }
                lower_expr(ctx, inner, out)?;
                out.push(0x42); // i64.const -1
                write_sleb(out, -1);
                out.push(0x85); // i64.xor
                if let Some(kind) = kind {
                    emit_normalize_i64(kind, out);
                }
                Ok(())
            }
            // Arithmetic negation (`-x`). A float operand uses `f64.neg`/`f32.neg`
            // (IEEE sign-bit flip, matching the interpreters); an integer operand
            // has no WASM `neg`, so it is `0 - x`, re-normalized on a fixed-width
            // kind. Detected structurally because float arithmetic nodes are
            // IR-annotated `i64`.
            UnaryOp::Negate => {
                if let Some(fty) = float_val_type_of(ctx, inner) {
                    lower_expr(ctx, inner, out)?;
                    out.push(match fty {
                        WasmValType::F64 => 0x9a, // f64.neg
                        WasmValType::F32 => 0x8c, // f32.neg
                        _ => return Err("negate float detector returned non-float".to_string()),
                    });
                    return Ok(());
                }
                let kind = fixed_int_kind(inner.ty.name.as_str());
                if kind.is_none() && inner.ty.name != "i64" {
                    return Err(format!(
                        "unary `-` on unsupported type `{}` (wasm backend)",
                        inner.ty.name
                    ));
                }
                out.push(0x42); // i64.const 0
                write_sleb(out, 0);
                lower_expr(ctx, inner, out)?;
                out.push(0x7d); // i64.sub -> 0 - x
                if let Some(kind) = kind {
                    emit_normalize_i64(kind, out);
                }
                Ok(())
            }
        },
        IrExprKind::Binary { left, op, right } => lower_binary(ctx, left, *op, right, out),
        IrExprKind::String(text) => {
            // A string literal is a constant pointer to its interned Data-section
            // layout `[len i32][utf8 bytes]`.
            let offset = ctx.pool.intern(text);
            out.push(0x41); // i32.const
            write_sleb(out, offset as i64);
            Ok(())
        }
        IrExprKind::Array(elements) => lower_array_literal(ctx, expr, elements, out),
        IrExprKind::Index { target, index } => lower_index_read(ctx, target, index, out),
        IrExprKind::Field { target, field } => lower_field_read(ctx, target, field, out),
        IrExprKind::Call { name, args } => {
            // The host log builtin lowers to a `call` of the imported
            // `env.log_i64` (WASM function index `LOG_I64_FUNC_INDEX`).
            if name == WASM_LOG {
                if args.len() != 1 {
                    return Err(format!("wasm_log expects 1 argument, got {}", args.len()));
                }
                lower_expr(ctx, &args[0], out)?;
                out.push(0x10); // call
                write_uleb(out, LOG_I64_FUNC_INDEX as u64);
                return Ok(());
            }
            // `console_log(s)` lowers to `env.console_log(ptr, len)`: push the
            // string's linear-memory pointer and its length header, then call the
            // imported host function. A browser host implements it as
            // `console.log` over the (ptr, len) slice of `memory`.
            if name == CONSOLE_LOG {
                if args.len() != 1 {
                    return Err(format!(
                        "console_log expects 1 argument, got {}",
                        args.len()
                    ));
                }
                lower_string_ptr_len(ctx, &args[0], out)?;
                out.push(0x10); // call
                write_uleb(out, CONSOLE_LOG_FUNC_INDEX as u64);
                return Ok(());
            }
            // `dom_set_text(id, text)` lowers to
            // `env.dom_set_text(id_ptr, id_len, text_ptr, text_len)`: push each
            // string's pointer and length, then call the import. A browser host
            // implements it as `document.getElementById(id).textContent = text`.
            if name == DOM_SET_TEXT {
                if args.len() != 2 {
                    return Err(format!(
                        "dom_set_text expects 2 arguments, got {}",
                        args.len()
                    ));
                }
                lower_string_ptr_len(ctx, &args[0], out)?;
                lower_string_ptr_len(ctx, &args[1], out)?;
                out.push(0x10); // call
                write_uleb(out, DOM_SET_TEXT_FUNC_INDEX as u64);
                return Ok(());
            }
            // Fixed-width integer conversions are inlined, not real calls.
            // `to_<T>(x)` normalizes the argument's `i64` cell into `T`'s width
            // (truncate + sign/zero-extend), matching the interpreter's
            // `Value::int(x, T)`. This is the same encoding the native backend
            // uses.
            if let Some(kind) = to_int_conversion_kind(name) {
                if args.len() != 1 {
                    return Err(format!("`{name}` takes exactly one argument"));
                }
                lower_expr(ctx, &args[0], out)?;
                emit_normalize_i64(kind, out);
                return Ok(());
            }
            // `to_f32(x f64) -> f32` rounds an f64 to single precision with
            // `f32.demote_f64`; `to_f64(x f32) -> f64` widens an f32 with
            // `f64.promote_f32` (exact). These builtins are inlined, not real
            // calls — the same encoding the native backend uses (`cvtsd2ss` /
            // `cvtss2sd`), so the WASM result is bit-identical to the interpreter.
            if name == "to_f32" {
                if args.len() != 1 {
                    return Err("`to_f32` takes exactly one argument".to_string());
                }
                lower_expr(ctx, &args[0], out)?;
                out.push(0xb6); // f32.demote_f64
                return Ok(());
            }
            if name == "to_f64" {
                if args.len() != 1 {
                    return Err("`to_f64` takes exactly one argument".to_string());
                }
                lower_expr(ctx, &args[0], out)?;
                out.push(0xbb); // f64.promote_f32
                return Ok(());
            }
            // `to_i64(x)` widens a fixed-width cell to `i64`; the source cell is
            // already normalized, so this is the identity on the bits.
            if name == "to_i64" {
                if args.len() != 1 {
                    return Err("`to_i64` takes exactly one argument".to_string());
                }
                lower_expr(ctx, &args[0], out)?;
                return Ok(());
            }
            // `to_string(x)` builds a `[char_len][byte_len][utf8]` string record for
            // an integer / bool / char / byte / string argument, matching the
            // interpreters' `Value::Display` bit-for-bit. A float argument
            // (`to_string(f32|f64)`) is DEFERRED — matching Rust's `Display` dtoa in
            // WASM is out of scope — so it errors here and the function falls back to
            // the interpreters.
            if name == TO_STRING_BUILTIN {
                if args.len() != 1 {
                    return Err("`to_string` takes exactly one argument".to_string());
                }
                return lower_to_string(ctx, &args[0], out);
            }
            // Index-based string operations. Each is gated on a `string` first
            // argument so the name cannot shadow a user function of the same
            // spelling: only a genuine `string`-typed call routes here. `substring`
            // and `find` are CHAR-indexed (they decode UTF-8 to map char index to
            // byte offset), while `contains`/`starts_with`/`ends_with` are byte-exact
            // substring/prefix/suffix tests — matching the interpreters bit-for-bit
            // (`builtin_substring`/`builtin_find`/`char_find`/`builtin_contains`/…).
            if name == SUBSTRING_BUILTIN && args.len() == 3 && args[0].ty.name == "string" {
                return lower_substring(ctx, &args[0], &args[1], &args[2], out);
            }
            if name == FIND_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_find(ctx, &args[0], &args[1], out);
            }
            if name == CONTAINS_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_contains(ctx, &args[0], &args[1], out);
            }
            if name == STARTS_WITH_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_starts_with(ctx, &args[0], &args[1], out);
            }
            if name == ENDS_WITH_BUILTIN && args.len() == 2 && args[0].ty.name == "string" {
                return lower_ends_with(ctx, &args[0], &args[1], out);
            }
            // Growable `list<T>` (scalar or `string` `T`) builtins. `list_new()`
            // allocates an empty header; `push`/`get`/`set`/`pop` operate on a
            // `list`-typed first argument (checked so these names cannot shadow a
            // user function or an array op). `len(l)` is NOT special-cased here — a
            // list's `len` shares offset 0 with the string/array length header, so
            // the generic `len` path below reads it. A list op whose element is a
            // MUTABLE heap type is deferred: `supported_list_element` returns
            // `None`, so lowering errors and the function is demoted to the
            // interpreters.
            if name == LIST_NEW_BUILTIN {
                return lower_list_new(ctx, args, out);
            }
            if name == LIST_PUSH_BUILTIN && args.len() == 2 && args[0].ty.list_element().is_some() {
                return lower_list_push(ctx, &args[0], &args[1], out);
            }
            if name == LIST_GET_BUILTIN && args.len() == 2 && args[0].ty.list_element().is_some() {
                return lower_list_get(ctx, &args[0], &args[1], out);
            }
            if name == LIST_SET_BUILTIN && args.len() == 3 && args[0].ty.list_element().is_some() {
                return lower_list_set(ctx, &args[0], &args[1], &args[2], out);
            }
            if name == LIST_POP_BUILTIN && args.len() == 1 && args[0].ty.list_element().is_some() {
                return lower_list_pop(ctx, &args[0], out);
            }
            // Growable `map<K, V>` (scalar `K`; scalar or `string` `V`) builtins.
            // `map_new()` allocates an empty `[len][cap][entries]` header;
            // `map_set`/`map_get`/`map_has`/`map_len` operate on a `map`-typed first
            // argument. These names are not shared with any array/list op, so they
            // dispatch on name directly (the arity/key/value types are validated in
            // each lowering). A map op whose key is a heap type, or whose value is a
            // MUTABLE heap type, is deferred: `supported_map_kv` returns `None`, so
            // lowering errors and the function is demoted to the interpreters.
            // `map_len(m)` shares offset 0 with the length header, but (unlike
            // lists) it is spelled `map_len`, so it routes here explicitly.
            if name == MAP_NEW_BUILTIN {
                return lower_map_new(ctx, args, out);
            }
            if name == MAP_SET_BUILTIN && args.len() == 3 && args[0].ty.map_args().is_some() {
                return lower_map_set(ctx, &args[0], &args[1], &args[2], out);
            }
            if name == MAP_GET_BUILTIN && args.len() == 2 && args[0].ty.map_args().is_some() {
                return lower_map_get(ctx, &expr.ty, &args[0], &args[1], out);
            }
            if name == MAP_HAS_BUILTIN && args.len() == 2 && args[0].ty.map_args().is_some() {
                return lower_map_has(ctx, &args[0], &args[1], out);
            }
            if name == MAP_LEN_BUILTIN && args.len() == 1 && args[0].ty.map_args().is_some() {
                return lower_map_len(ctx, &args[0], out);
            }
            // `len(s)`/`len(a)`/`len(l)` reads the leading i32 length header.
            if name == LEN_BUILTIN {
                return lower_len(ctx, args, out);
            }
            // Overflow-aware arithmetic builtins (`checked_*`/`saturating_*`/
            // `wrapping_*`). `wrapping_*` reuses the default fixed-width `+`/`-`/`*`;
            // `saturating_*` clamps to `T`'s bounds; `checked_*` builds an
            // `option<T>` record. Guarded by a fixed-width first operand so the
            // names cannot shadow a user function of the same spelling.
            if let Some((ovf_op, mode)) = overflow_builtin(name)
                && args.len() == 2
                && let Some(kind) = fixed_int_kind(args[0].ty.name.as_str())
            {
                if mode == OverflowMode::Wrapping {
                    lower_expr(ctx, &args[0], out)?;
                    lower_expr(ctx, &args[1], out)?;
                    return emit_fixed_binop(ctx, ovf_op.binary_op(), kind, out);
                }
                return lower_wasm_overflow(
                    ctx, ovf_op, mode, kind, &expr.ty, &args[0], &args[1], out,
                );
            }
            // Scalar math builtins that lower to inline opcode sequences (not real
            // calls), matching the interpreters bit-for-bit and mirroring the
            // native backend's decisions: `sqrt`/`abs` on `f64`, `abs` on `i64`,
            // and the `i64` suite `min`/`max`/`gcd`/`sign`/`clamp`. The `f64` cases
            // of `min`/`max`/`sign`/`clamp` are DEFERRED (they fall through to the
            // generic-call path, which errors, so the function runs on the
            // interpreters) — matching native, where `f64.min`/`f64.max`
            // NaN/`±0.0` tie-breaking diverges from Rust and the branch forms need
            // float compares. Each is gated on argument count and type so a name
            // cannot shadow a user function of the same spelling.
            if let Some(()) = try_lower_scalar_math(ctx, name, args, out)? {
                return Ok(());
            }
            // A call whose name is a declared struct is a struct construction: the
            // IR lowerer emits struct literals as positional `Call`s. For a generic
            // instantiation the call name is the struct's BASE name (`Box`) and the
            // construction node carries the base type too (`Box`, not `Box<i64>`), so
            // the concrete field types are taken from the ARGUMENT types inside
            // `lower_struct_construction` (see there).
            if ctx.structs.contains_key(name) {
                return lower_struct_construction(ctx, name, args, out);
            }
            // A call whose result type is a supported enum and whose name is one of
            // its variants is enum construction: `some(x)`/`ok(x)`/`err(e)`/`none`
            // (the built-ins) or a user `Variant(payload...)`. The IR lowerer emits
            // these as positional `Call`s (with empty `args` for a unit variant),
            // carrying the constructed enum type as `expr.ty`.
            if let Some(layout) = ctx.enum_layout(&expr.ty)
                && layout.tag_of(name).is_some()
            {
                return lower_enum_construction(ctx, &layout, name, args, out);
            }
            // A GENERIC enum construction (`present(x)`/`absent` for `Opt<T>`): the
            // construction node carries the BASE enum type (`Opt`), whose variant
            // payloads are unresolved type parameters, so `enum_layout` above cannot
            // resolve it. Build the record shape from the base declaration — the
            // variant order (tags) and the payload arity (record slot count) are
            // type-parameter-independent, so they equal the registered
            // monomorphized instantiation's — and take the constructed variant's
            // payload slot types from the ARGUMENT types (concrete). This is the
            // enum analogue of the generic struct construction above.
            if let Some(layout) = generic_enum_construction_layout(ctx, &expr.ty, name, args) {
                return lower_enum_construction(ctx, &layout, name, args, out);
            }
            let index = *ctx.func_index.get(name).ok_or_else(|| {
                format!("call to unsupported builtin or unknown function `{name}`")
            })?;
            for arg in args {
                lower_expr(ctx, arg, out)?;
                // Preserve Lullaby value semantics across the call boundary: an
                // aggregate is an `i32` pointer, so passing it raw would let the
                // callee mutate the caller's record through a shared pointer. A
                // mutable aggregate argument (struct/array/enum — never an
                // immutable `string`) is deep-copied into a fresh record here, so
                // the callee receives an independent snapshot exactly like the
                // interpreters clone the argument value. A returned aggregate is
                // the callee's own fresh record, so no copy is needed there.
                if is_mutable_aggregate(&arg.ty, ctx.structs, ctx.enums) {
                    emit_deep_copy(ctx, &arg.ty, out)?;
                }
            }
            out.push(0x10); // call
            write_uleb(out, index as u64);
            Ok(())
        }
        IrExprKind::Await { .. } => Err("await is not supported by the WASM backend".to_string()),
        // Closures are not compiled to WASM in this increment: a function that
        // constructs or calls a closure is skipped (this `Err`) and falls back to
        // the interpreters, exactly like other unsupported constructs.
        IrExprKind::Closure { .. } => {
            Err("closures are not supported by the WASM backend".to_string())
        }
    }
}

/// Whether an argument evaluates to an `f64`. A leaf is reliably typed by its
/// `TypeRef` (`f64`), but a float ARITHMETIC node is IR-annotated `i64`, so
/// [`float_val_type_of`] is consulted too (it looks through arithmetic to the
/// float leaves). Used to route `abs` to the f64 path and to keep the `i64`-only
/// `min`/`max`/`sign`/`clamp` from matching a float-arithmetic operand (which
/// would otherwise carry an `i64`-annotated node into the integer path).
fn is_f64_operand(ctx: &LowerCtx, e: &IrExpr) -> bool {
    e.ty.name == "f64" || float_val_type_of(ctx, e) == Some(WasmValType::F64)
}

/// Try to lower a scalar math builtin to an inline opcode sequence. Returns
/// `Ok(Some(()))` when it emitted the builtin, `Ok(None)` when the name/arity/type
/// is not a supported scalar-math call (the caller continues its dispatch), or
/// `Err` when a matched builtin cannot be compiled (the function is demoted to the
/// interpreters).
///
/// Ground truth is the interpreters (`builtin_sqrt`/`builtin_abs`/`builtin_min`/…
/// and `gcd_i64`); every sequence is bit-for-bit with them, and with the native
/// backend. The `f64` cases of `min`/`max`/`sign`/`clamp` are intentionally NOT
/// matched (they return `Ok(None)` and defer to the interpreters), exactly like
/// native — `f64.min`/`f64.max` NaN/`±0.0` tie-breaking diverges from Rust's
/// `f64::min`/`f64::max`, and the branch forms need float compares.
pub(crate) fn try_lower_scalar_math(
    ctx: &mut LowerCtx,
    name: &str,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<Option<()>, String> {
    match name {
        // `sqrt(x f64) -> f64`: a single `f64.sqrt`, bit-for-bit `f64::sqrt`
        // (a negative operand yields NaN, matching the interpreters and IEEE-754).
        // f64-only, like the interpreter builtin.
        "sqrt" if args.len() == 1 && is_f64_operand(ctx, &args[0]) => {
            lower_expr(ctx, &args[0], out)?;
            out.push(0x9f); // f64.sqrt
            Ok(Some(()))
        }
        // `abs(x f64) -> f64`: `f64.abs` clears the IEEE sign bit (|-0.0| = +0.0;
        // a NaN keeps its payload with the sign cleared), matching `f64::abs` /
        // the interpreters' `n.abs()`.
        "abs" if args.len() == 1 && is_f64_operand(ctx, &args[0]) => {
            lower_expr(ctx, &args[0], out)?;
            out.push(0x99); // f64.abs
            Ok(Some(()))
        }
        // `abs(x i64) -> i64`: the branchless two's-complement idiom
        // `(x ^ (x >> 63)) - (x >> 63)`, matching release `i64::abs` — which wraps
        // `abs(i64::MIN)` back to `i64::MIN` (the `i64.sub` wraps), consistent with
        // the interpreters and the native backend.
        "abs" if args.len() == 1 && args[0].ty.name == "i64" => {
            emit_i64_abs(ctx, &args[0], out)?;
            Ok(Some(()))
        }
        // `min(a, b)` / `max(a, b)` on plain `i64`: `i64.lt_s`/`i64.gt_s` +
        // `select`, matching `i64::min`/`i64::max`. For equal operands both
        // formulas yield the (equal) value, so the tie direction is irrelevant.
        "min" | "max"
            if args.len() == 2
                && args[0].ty.name == "i64"
                && args[1].ty.name == "i64"
                && !is_f64_operand(ctx, &args[0])
                && !is_f64_operand(ctx, &args[1]) =>
        {
            emit_i64_min_max(ctx, name == "min", &args[0], &args[1], out)?;
            Ok(Some(()))
        }
        // `gcd(a, b)` on `i64`: unsigned-magnitude Euclid over `u64` magnitudes with
        // `i64.rem_u`, matching `gcd_i64` — including `gcd(i64::MIN, 0) = i64::MIN`
        // (the magnitude `2^63` reinterprets back to `i64::MIN`).
        "gcd" if args.len() == 2 && args[0].ty.name == "i64" && args[1].ty.name == "i64" => {
            emit_i64_gcd(ctx, &args[0], &args[1], out)?;
            Ok(Some(()))
        }
        // `sign(x) -> i64` (`-1`/`0`/`1`) on `i64`: two nested `select`s over
        // `i64.lt_s`/`i64.gt_s`, matching `i64::signum`. The `f64` case is deferred.
        "sign" if args.len() == 1 && args[0].ty.name == "i64" && !is_f64_operand(ctx, &args[0]) => {
            emit_i64_sign(ctx, &args[0], out)?;
            Ok(Some(()))
        }
        // `clamp(x, lo, hi) -> i64` on `i64`: `x < lo ? lo : (x > hi ? hi : x)` via
        // two nested `select`s comparing the ORIGINAL `x`, matching the
        // interpreters' `if x < lo { lo } else if x > hi { hi } else { x }` for every
        // ordering of `lo`/`hi` (including `lo > hi`). The `f64` case is deferred.
        "clamp"
            if args.len() == 3
                && args[0].ty.name == "i64"
                && args[1].ty.name == "i64"
                && args[2].ty.name == "i64"
                && !is_f64_operand(ctx, &args[0]) =>
        {
            emit_i64_clamp(ctx, &args[0], &args[1], &args[2], out)?;
            Ok(Some(()))
        }
        _ => Ok(None),
    }
}

/// Emit `|x|` for an `i64` argument via the branchless two's-complement idiom,
/// leaving the magnitude on the stack. `mask = x >> 63` (arithmetic) is all-ones
/// for a negative `x` and zero otherwise, so `(x ^ mask) - mask` is `-x` when
/// negative and `x` when non-negative. `abs(i64::MIN)` wraps to `i64::MIN`
/// (the `i64.sub` wraps), matching release `i64::abs`.
pub(crate) fn emit_i64_abs(
    ctx: &mut LowerCtx,
    x: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let t = ctx.add_local(WasmValType::I64);
    let m = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, x, out)?; // [x]
    set_local(out, t); // t = x
    get_local(out, t); // [x]
    out.push(0x42); // i64.const 63
    write_sleb(out, 63);
    out.push(0x87); // i64.shr_s -> mask
    set_local(out, m); // m = mask
    get_local(out, t); // [x]
    get_local(out, m); // [x, mask]
    out.push(0x85); // i64.xor -> x ^ mask
    get_local(out, m); // [x^mask, mask]
    out.push(0x7d); // i64.sub -> |x|
    Ok(())
}

/// Emit `min`/`max` for two `i64` operands with `i64.lt_s`/`i64.gt_s` + `select`.
/// For `min`: `a < b ? a : b`; for `max`: `a > b ? a : b`. Matches
/// `i64::min`/`i64::max` bit-for-bit (equal operands yield the equal value either
/// way). Operands are spilled to locals so each is evaluated once.
pub(crate) fn emit_i64_min_max(
    ctx: &mut LowerCtx,
    is_min: bool,
    a: &IrExpr,
    b: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let la = ctx.add_local(WasmValType::I64);
    let lb = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, a, out)?;
    set_local(out, la);
    lower_expr(ctx, b, out)?;
    set_local(out, lb);
    get_local(out, la); // val1 = a
    get_local(out, lb); // val2 = b
    get_local(out, la);
    get_local(out, lb);
    out.push(if is_min { 0x53 } else { 0x55 }); // i64.lt_s (min) / i64.gt_s (max)
    out.push(0x1b); // select -> cond ? a : b
    Ok(())
}

/// Emit `gcd(a, b)` for two `i64` operands: reduce each to its `u64` magnitude
/// (the branchless `abs` idiom, whose `i64::MIN` result reinterprets to the
/// unsigned `2^63`), then run Euclid's algorithm with `i64.rem_u` in a
/// `block`/`loop`. Matches `gcd_i64` — including `gcd(i64::MIN, 0) = i64::MIN`,
/// since the loop exits immediately with `x = 2^63` whose bits are `i64::MIN`.
pub(crate) fn emit_i64_gcd(
    ctx: &mut LowerCtx,
    a: &IrExpr,
    b: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let x = ctx.add_local(WasmValType::I64);
    let y = ctx.add_local(WasmValType::I64);
    let r = ctx.add_local(WasmValType::I64);
    emit_i64_abs(ctx, a, out)?; // |a| magnitude
    set_local(out, x);
    emit_i64_abs(ctx, b, out)?; // |b| magnitude
    set_local(out, y);
    // block { loop { if y == 0 break; r = x %u y; x = y; y = r; continue } }
    out.push(0x02); // block
    out.push(0x40); // void blocktype
    out.push(0x03); // loop
    out.push(0x40); // void blocktype
    get_local(out, y);
    out.push(0x50); // i64.eqz
    out.push(0x0d); // br_if
    write_uleb(out, 1); // -> end of block (exit the loop)
    get_local(out, x);
    get_local(out, y);
    out.push(0x82); // i64.rem_u -> x %u y
    set_local(out, r);
    get_local(out, y);
    set_local(out, x); // x = y
    get_local(out, r);
    set_local(out, y); // y = r
    out.push(0x0c); // br
    write_uleb(out, 0); // -> loop top (continue)
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    get_local(out, x); // result = x
    Ok(())
}

/// Emit `sign(x) -> i64` (`-1`/`0`/`1`) for an `i64` operand as two nested
/// `select`s: `x < 0 ? -1 : (x > 0 ? 1 : 0)`, matching `i64::signum`.
pub(crate) fn emit_i64_sign(
    ctx: &mut LowerCtx,
    x: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let lx = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, x, out)?;
    set_local(out, lx);
    out.push(0x42); // i64.const -1  (outer val1)
    write_sleb(out, -1);
    out.push(0x42); // i64.const 1   (inner val1)
    write_sleb(out, 1);
    out.push(0x42); // i64.const 0   (inner val2)
    write_sleb(out, 0);
    get_local(out, lx);
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    out.push(0x55); // i64.gt_s -> x > 0
    out.push(0x1b); // select -> x > 0 ? 1 : 0
    get_local(out, lx);
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    out.push(0x53); // i64.lt_s -> x < 0
    out.push(0x1b); // select -> x < 0 ? -1 : (x > 0 ? 1 : 0)
    Ok(())
}

/// Emit `clamp(x, lo, hi) -> i64` for `i64` operands as two nested `select`s:
/// `x < lo ? lo : (x > hi ? hi : x)`, both comparing the ORIGINAL `x` — matching
/// the interpreters' `if x < lo { lo } else if x > hi { hi } else { x }` for every
/// ordering of `lo`/`hi` (including `lo > hi`, which yields `lo`).
pub(crate) fn emit_i64_clamp(
    ctx: &mut LowerCtx,
    x: &IrExpr,
    lo: &IrExpr,
    hi: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let lx = ctx.add_local(WasmValType::I64);
    let llo = ctx.add_local(WasmValType::I64);
    let lhi = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, x, out)?;
    set_local(out, lx);
    lower_expr(ctx, lo, out)?;
    set_local(out, llo);
    lower_expr(ctx, hi, out)?;
    set_local(out, lhi);
    get_local(out, llo); // outer val1 = lo
    get_local(out, lhi); // inner val1 = hi
    get_local(out, lx); // inner val2 = x
    get_local(out, lx);
    get_local(out, lhi);
    out.push(0x55); // i64.gt_s -> x > hi
    out.push(0x1b); // select -> x > hi ? hi : x
    get_local(out, lx);
    get_local(out, llo);
    out.push(0x53); // i64.lt_s -> x < lo
    out.push(0x1b); // select -> x < lo ? lo : (x > hi ? hi : x)
    Ok(())
}

/// Lower a `string` argument to the two host-import operands `[ptr, len]`: push a
/// pointer to the string's first UTF-8 byte, then its UTF-8 BYTE length. The
/// record pointer is evaluated once into a scratch `i32` local so a non-trivial
/// string expression is not lowered twice; the operand pointer is
/// `record_ptr + STR_DATA_OFF` (past the two `i32` headers) so the host slices
/// `[ptr, ptr + len)` directly, and the length is the record's byte-length header
/// (`STR_BYTE_LEN_OFF`) so multi-byte UTF-8 text decodes correctly — not the char
/// count, which only equals the byte length for ASCII.
pub(crate) fn lower_string_ptr_len(
    ctx: &mut LowerCtx,
    arg: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    if value_val_type(&arg.ty, ctx.structs, ctx.enums) != Some(WasmValType::I32)
        || arg.ty.name != "string"
    {
        return Err(format!(
            "console_log/dom_set_text expect a string but got `{}`",
            arg.ty.name
        ));
    }
    lower_expr(ctx, arg, out)?; // string record pointer (i32)
    let ptr = ctx.add_local(WasmValType::I32);
    set_local(out, ptr);
    // operand: record_ptr + STR_DATA_OFF (pointer to the first UTF-8 byte).
    get_local(out, ptr);
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    // operand: byte length (the second header).
    get_local(out, ptr); // base for the length load
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    Ok(())
}

/// Lower `len(x)` where `x` is a `string` or `array`: load the leading `i32`
/// length header (char count for strings, element count for arrays), then extend
/// to `i64` (the builtin's result type on the interpreters).
pub(crate) fn lower_len(
    ctx: &mut LowerCtx,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    if args.len() != 1 {
        return Err(format!("len expects 1 argument, got {}", args.len()));
    }
    let arg = &args[0];
    if value_val_type(&arg.ty, ctx.structs, ctx.enums) != Some(WasmValType::I32) {
        return Err(format!(
            "len expects a string or array but got `{}`",
            arg.ty.name
        ));
    }
    lower_expr(ctx, arg, out)?; // pointer (i32)
    out.push(0x28); // i32.load
    out.push(0x02); // align 2 (4-byte)
    write_uleb(out, 0); // offset 0 (the length header)
    // i64.extend_i32_s -> the builtin returns i64.
    out.push(0xac);
    Ok(())
}

/// Lower a struct construction `Struct(f0, f1, ...)`: `__alloc` a run of one
/// 8-byte slot per field, then store each field value at its slot offset. Leaves
/// the base pointer on the stack.
pub(crate) fn lower_struct_construction(
    ctx: &mut LowerCtx,
    name: &str,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let fields = ctx
        .structs
        .get(name)
        .ok_or_else(|| format!("`{name}` is not a struct"))?
        .clone();
    if args.len() != fields.len() {
        return Err(format!(
            "struct `{name}` expects {} fields, got {}",
            fields.len(),
            args.len()
        ));
    }
    let ptr = alloc_bytes(ctx, fields.len() as i32 * SLOT_SIZE, out);
    for (slot, ((_, field_ty), arg)) in fields.iter().zip(args).enumerate() {
        // The slot's value type is the declared field type when it resolves, but for
        // a GENERIC struct's base declaration the field type is an unresolved type
        // parameter (`Box<T>`'s `value T`) — the construction node carries the base
        // type, so the concrete monomorphized field type is not on this expression.
        // In that case take the slot type from the ARGUMENT's own (concrete) type,
        // which IS the value being stored and equals the monomorphized field type
        // the field-read/`match`/copy paths use for the registered instantiation. For
        // a non-generic struct `field_ty` resolves and equals `arg.ty`, so this is
        // byte-identical to before.
        let slot_ty = slot_val_type(field_ty, ctx.structs, ctx.enums)
            .or_else(|| slot_val_type(&arg.ty, ctx.structs, ctx.enums))
            .ok_or_else(|| format!("struct `{name}` field has unsupported type"))?;
        get_local(out, ptr); // base pointer
        lower_expr(ctx, arg, out)?; // field value
        // Value semantics: a field built from an aggregate LVALUE (`Outer(q, …)`)
        // must store an INDEPENDENT copy, not `q`'s pointer, so a later mutation of
        // `q` is not observable through the field. A freshly constructed operand
        // already owns its record and is stored directly (no redundant copy).
        maybe_copy_bound_value(ctx, arg, out)?;
        emit_store_at(slot_ty, slot as i32 * SLOT_SIZE, out);
    }
    get_local(out, ptr);
    Ok(())
}

/// Build the [`EnumLayout`] for a GENERIC enum construction whose node carries the
/// BASE enum type (`Opt` for an `Opt<T>` instantiation), or `None` if `ty` is not a
/// declared generic enum with `variant`. The variant order (discriminant tags) and
/// each variant's payload ARITY come from the base declaration and are
/// type-parameter-independent, so the record's slot count matches the registered
/// monomorphized instantiation exactly. The CONSTRUCTED variant's payload types are
/// taken from the argument types (concrete), so [`lower_enum_construction`] resolves
/// its slot value types; the other variants' unresolved-`T` payloads are never type-
/// resolved during construction (only their arity is read via `build_layout`).
fn generic_enum_construction_layout(
    ctx: &LowerCtx,
    ty: &TypeRef,
    variant: &str,
    args: &[IrExpr],
) -> Option<EnumLayout> {
    let def = ctx.enums.get(&ty.name)?;
    if def.type_params.is_empty() || !def.variants.iter().any(|v| v.name == variant) {
        return None;
    }
    let variants: Vec<(String, Vec<TypeRef>)> = def
        .variants
        .iter()
        .map(|v| {
            let payload = if v.name == variant {
                args.iter().map(|a| a.ty.clone()).collect()
            } else {
                v.payload.clone()
            };
            (v.name.clone(), payload)
        })
        .collect();
    Some(build_layout(variants))
}

/// Lower an enum construction (`some(x)`/`none`/`ok(x)`/`err(e)` or a user
/// `Variant(payload...)`): `__alloc` a `[tag i32 (padded)][slot0][slot1]...]`
/// record sized for the enum's widest variant, store the variant's discriminant
/// tag at offset 0, store each payload value into its leading slot, and leave the
/// base pointer (the enum value) on the stack. The discriminant is the variant's
/// index in the enum's declaration order, matching the interpreters (which
/// dispatch `match` by variant name against this same ordered layout).
pub(crate) fn lower_enum_construction(
    ctx: &mut LowerCtx,
    layout: &EnumLayout,
    variant: &str,
    args: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let tag = layout
        .tag_of(variant)
        .ok_or_else(|| format!("`{variant}` is not a variant of the enum"))?;
    let payload = layout
        .payload_of(variant)
        .ok_or_else(|| format!("`{variant}` is not a variant of the enum"))?
        .to_vec();
    if args.len() != payload.len() {
        return Err(format!(
            "enum variant `{variant}` expects {} payload value(s), got {}",
            payload.len(),
            args.len()
        ));
    }
    let ptr = alloc_bytes(ctx, layout.size_bytes(), out);
    // Tag at offset 0 (i32 discriminant).
    get_local(out, ptr);
    out.push(0x41); // i32.const tag
    write_sleb(out, tag as i64);
    emit_store_at(WasmValType::I32, 0, out);
    // Payload values into the leading slots (offset ENUM_PAYLOAD_BASE + i*SLOT).
    for (slot, (payload_ty, arg)) in payload.iter().zip(args).enumerate() {
        let slot_ty = slot_val_type(payload_ty, ctx.structs, ctx.enums)
            .ok_or_else(|| format!("enum variant `{variant}` payload has unsupported type"))?;
        get_local(out, ptr); // base pointer
        lower_expr(ctx, arg, out)?; // payload value
        // Value semantics: a payload built from an aggregate LVALUE (`some(f)`)
        // stores an INDEPENDENT copy, so mutating `f` afterward is not observable
        // through the payload; a freshly constructed operand is stored directly.
        maybe_copy_bound_value(ctx, arg, out)?;
        emit_store_at(slot_ty, ENUM_PAYLOAD_BASE + slot as i32 * SLOT_SIZE, out);
    }
    get_local(out, ptr);
    Ok(())
}

/// Lower a fixed array literal `[e0, e1, ...]`: `__alloc` a `[len i32][slots]`
/// block, write the length header and each element slot, and leave the base
/// pointer on the stack.
pub(crate) fn lower_array_literal(
    ctx: &mut LowerCtx,
    expr: &IrExpr,
    elements: &[IrExpr],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = expr
        .ty
        .array_element()
        .ok_or_else(|| format!("array literal has non-array type `{}`", expr.ty.name))?;
    let slot_ty = slot_val_type(&elem_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ty.name))?;
    let total = LEN_HEADER + elements.len() as i32 * SLOT_SIZE;
    let ptr = alloc_bytes(ctx, total, out);
    // Length header: i32.store [ptr + 0] = element count.
    get_local(out, ptr);
    out.push(0x41); // i32.const
    write_sleb(out, elements.len() as i64);
    out.push(0x36); // i32.store
    out.push(0x02); // align 2
    write_uleb(out, 0);
    for (i, element) in elements.iter().enumerate() {
        get_local(out, ptr);
        lower_expr(ctx, element, out)?;
        // Value semantics: an element built from an aggregate LVALUE (`[f]`) stores
        // an INDEPENDENT copy, so mutating `f` afterward is not observable through
        // the element; a freshly constructed element is stored directly.
        maybe_copy_bound_value(ctx, element, out)?;
        emit_store_at(slot_ty, LEN_HEADER + i as i32 * SLOT_SIZE, out);
    }
    get_local(out, ptr);
    Ok(())
}

/// Lower a struct field read `target.field`: push the target pointer, add the
/// field's slot offset, and load the slot.
pub(crate) fn lower_field_read(
    ctx: &mut LowerCtx,
    target: &IrExpr,
    field: &str,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (offset, slot_ty) = struct_field_slot(ctx, &target.ty, field)?;
    lower_expr(ctx, target, out)?; // base pointer
    emit_load_at(slot_ty, offset, out);
    Ok(())
}

/// Lower an array element read `target[index]`: compute the slot address, then
/// load it. [`lower_array_slot_offset`] emits an explicit unsigned bounds check
/// against the array length header, so an out-of-range index TRAPS (`unreachable`)
/// — matching native (`ud2`) and the interpreters (`L0413`) — instead of reading a
/// neighboring heap object out of WASM linear memory.
pub(crate) fn lower_index_read(
    ctx: &mut LowerCtx,
    target: &IrExpr,
    index: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let elem_ty = target
        .ty
        .array_element()
        .ok_or_else(|| format!("indexing a non-array type `{}`", target.ty.name))?;
    let slot_ty = slot_val_type(&elem_ty, ctx.structs, ctx.enums)
        .ok_or_else(|| format!("array element type `{}` is unsupported", elem_ty.name))?;
    lower_expr(ctx, target, out)?; // base pointer (i32)
    lower_array_slot_offset(ctx, index, out)?; // += header + index*SLOT_SIZE
    emit_load(slot_ty, out);
    Ok(())
}

pub(crate) fn lower_binary(
    ctx: &mut LowerCtx,
    left: &IrExpr,
    op: BinaryOp,
    right: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // String ordering (`< <= > >=`) is lexicographic by content; the scalar
    // backend would compare heap pointers, so defer the function to the
    // interpreters. Concatenation `+` and equality use their own paths.
    if matches!(
        op,
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
    ) && (left.ty.name == "string" || right.ty.name == "string")
    {
        return Err("string ordering comparison is not supported on the wasm backend".to_string());
    }
    // Short-circuit `and`/`or` via WASM `if`/`else` producing i32.
    match op {
        BinaryOp::And => {
            lower_expr(ctx, left, out)?;
            out.push(0x04); // if
            out.push(0x7f); // result i32
            lower_expr(ctx, right, out)?;
            out.push(0x05); // else
            out.push(0x41); // i32.const 0
            write_sleb(out, 0);
            out.push(0x0b); // end
            return Ok(());
        }
        BinaryOp::Or => {
            lower_expr(ctx, left, out)?;
            out.push(0x04); // if
            out.push(0x7f); // result i32
            out.push(0x41); // i32.const 1
            write_sleb(out, 1);
            out.push(0x05); // else
            lower_expr(ctx, right, out)?;
            out.push(0x0b); // end
            return Ok(());
        }
        _ => {}
    }

    // Runtime string concatenation: `a + b` where both operands are `string`
    // allocates a fresh `[char_len][byte_len][utf8 bytes]` record whose bytes are
    // the two operands' byte ranges joined and whose char/byte headers are the
    // sums of the operands' headers. Strings are immutable, so the result is a new
    // record with no aliasing. Any other `+` operand type falls through to the
    // scalar arithmetic paths below.
    if op == BinaryOp::Add && left.ty.name == "string" && right.ty.name == "string" {
        return lower_string_concat(ctx, left, right, out);
    }

    // A fixed-width operand kind (both operands share it; the type checker forbids
    // mixing widths) selects width- and signedness-correct codegen that
    // re-normalizes width-producing results, mirroring the interpreter free
    // functions and the native backend.
    if let Some(kind) = fixed_int_kind(left.ty.name.as_str()) {
        lower_expr(ctx, left, out)?;
        lower_expr(ctx, right, out)?;
        return emit_fixed_binop(ctx, op, kind, out);
    }

    // Integer bitwise/shift operators on plain `i64` map directly to the WASM
    // opcodes (no width normalization needed). f64/bool/char/byte cannot carry
    // them, so a bitwise/shift op on a non-integer type is rejected (the function
    // falls back to the interpreters).
    if matches!(
        op,
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Shl | BinaryOp::Shr
    ) {
        if left.ty.name != "i64" {
            return Err(format!(
                "bitwise/shift operator on unsupported type `{}` (wasm backend)",
                left.ty.name
            ));
        }
        lower_expr(ctx, left, out)?;
        lower_expr(ctx, right, out)?;
        return emit_i64_bitwise_or_shift(op, out);
    }

    // The operand value type drives the opcode family. For a FLOAT operand this
    // must be derived structurally: the IR annotates a float ARITHMETIC node with
    // `i64` (see the IR binary lowerer), so `if a + b > c` would otherwise pick an
    // integer compare over f32/f64 values. `float_val_type_of` looks through
    // arithmetic to the reliably-typed leaves (float literals, float locals, and
    // the `to_f32`/`to_f64` conversions); when neither operand is a float it falls
    // back to the left operand's own value type (i64/i32).
    let operand_ty = match float_val_type_of(ctx, left).or_else(|| float_val_type_of(ctx, right)) {
        Some(ft) => ft,
        None => expr_val_type(ctx, left)?
            .ok_or_else(|| "binary operand has no scalar value".to_string())?,
    };
    lower_expr(ctx, left, out)?;
    lower_expr(ctx, right, out)?;
    // Plain `i64` signed division goes through the wrapping guard so `i64::MIN /
    // -1` yields `i64::MIN` instead of trapping, matching the interpreters.
    if matches!((op, operand_ty), (BinaryOp::Divide, WasmValType::I64)) {
        emit_i64_signed_div_guarded(ctx, out);
        return Ok(());
    }
    emit_binary_op_typed(op, operand_ty, out)
}

/// Lower runtime string concatenation `a + b` (both `string`) into a fresh
/// `[char_len: i32][byte_len: i32][utf8 bytes]` record and leave its pointer on
/// the stack.
///
/// Strings are immutable, so concatenation always builds a NEW record (no
/// aliasing): read each operand's char-count and byte-count headers, `__alloc` a
/// record of `STR_DATA_OFF + byte_a + byte_b` bytes, write the summed headers
/// (char count = `char_a + char_b`, byte count = `byte_a + byte_b`), then
/// `memory.copy` each operand's UTF-8 byte range into place. Working in BYTE
/// ranges (not char counts) keeps multi-byte UTF-8 correct; the result's `len`
/// (its char-count header) is `len(a) + len(b)`, matching the interpreters
/// bit-for-bit. Chained `a + b + c` nests naturally: the inner `+` yields a normal
/// string record consumed by the outer `+`.
pub(crate) fn lower_string_concat(
    ctx: &mut LowerCtx,
    left: &IrExpr,
    right: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate both operands once into scratch record-pointer locals (each may be
    // a non-trivial expression — a variable, a literal, or a nested concat).
    lower_expr(ctx, left, out)?;
    let a = ctx.add_local(WasmValType::I32);
    set_local(out, a);
    lower_expr(ctx, right, out)?;
    let b = ctx.add_local(WasmValType::I32);
    set_local(out, b);

    // Read the four headers into locals: char and byte counts of each operand.
    let char_a = ctx.add_local(WasmValType::I32);
    get_local(out, a);
    emit_load_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    set_local(out, char_a);
    let byte_a = ctx.add_local(WasmValType::I32);
    get_local(out, a);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_a);
    let char_b = ctx.add_local(WasmValType::I32);
    get_local(out, b);
    emit_load_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    set_local(out, char_b);
    let byte_b = ctx.add_local(WasmValType::I32);
    get_local(out, b);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_b);

    // dst = __alloc(STR_DATA_OFF + byte_a + byte_b): header + both byte ranges.
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    get_local(out, byte_a);
    out.push(0x6a); // i32.add
    get_local(out, byte_b);
    out.push(0x6a); // i32.add
    let dst = alloc_runtime(ctx, out);

    // dst[char_len] = char_a + char_b.
    get_local(out, dst);
    get_local(out, char_a);
    get_local(out, char_b);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    // dst[byte_len] = byte_a + byte_b.
    get_local(out, dst);
    get_local(out, byte_a);
    get_local(out, byte_b);
    out.push(0x6a); // i32.add
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);

    // memory.copy(dst + STR_DATA_OFF, a + STR_DATA_OFF, byte_a): first operand's
    // bytes. `memory.copy` pops size, src, dest (pushed dest, src, size).
    get_local(out, dst);
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> dest
    get_local(out, a);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> src
    get_local(out, byte_a); // size
    emit_memory_copy(out);

    // memory.copy(dst + STR_DATA_OFF + byte_a, b + STR_DATA_OFF, byte_b): second
    // operand's bytes appended after the first range.
    get_local(out, dst);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    get_local(out, byte_a);
    out.push(0x6a); // i32.add -> dest = dst + STR_DATA_OFF + byte_a
    get_local(out, b);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add -> src
    get_local(out, byte_b); // size
    emit_memory_copy(out);

    // The concatenated record's pointer is the value of the expression.
    get_local(out, dst);
    Ok(())
}

/// Emit the `memory.copy` bulk-memory instruction, copying `size` bytes from `src`
/// to `dest` within the single linear memory (all three operands already on the
/// stack in dest, src, size order). Encoded as the `0xfc` misc prefix, sub-opcode
/// `0x0a`, then the destination and source memory indices (both `0` — the module
/// has exactly one memory).
pub(crate) fn emit_memory_copy(out: &mut Vec<u8>) {
    out.push(0xfc); // misc-op prefix
    write_uleb(out, 0x0a); // memory.copy
    out.push(0x00); // dest memory index
    out.push(0x00); // src memory index
}

// -- Index-based string-operation codegen ------------------------------------
//
// These lower the char-indexed `substring`/`find` and the byte-exact
// `contains`/`starts_with`/`ends_with` builtins over the `[char_len][byte_len]
// [utf8 bytes]` string record. The byte scans compare `memory[hay + i]` against
// `memory[needle + j]` with `i32.load8_u`; `find`/`substring` additionally decode
// UTF-8 lead bytes (a byte is a char start iff `(b & 0xC0) != 0x80`) to map char
// indices to byte offsets. Every scan is an inline WASM loop over the UTF-8 bytes,
// matching the interpreters' `str::find`/`str::contains`/`chars()` bit-for-bit.

/// Push a pointer to a string record's first UTF-8 byte: `record_ptr + STR_DATA_OFF`.
/// The record pointer must already be on the stack.
pub(crate) fn emit_add_data_off(out: &mut Vec<u8>) {
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
}

/// Emit `i32.load8_u` reading the byte at the address on the stack (offset 0).
pub(crate) fn emit_load8_u(out: &mut Vec<u8>) {
    out.push(0x2d); // i32.load8_u
    write_uleb(out, 0); // align 0 (1-byte)
    write_uleb(out, 0); // offset 0
}

/// Evaluate a `string` expression into a fresh scratch triple of `i32` locals and
/// return them as `(data_ptr, byte_len)`: `data_ptr` points at the first UTF-8
/// byte (`record + STR_DATA_OFF`) and `byte_len` is the UTF-8 byte-length header.
/// The record pointer is evaluated once so a non-trivial string expression is not
/// lowered twice.
pub(crate) fn lower_string_data_len(
    ctx: &mut LowerCtx,
    arg: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(u32, u32), String> {
    if arg.ty.name != "string" {
        return Err(format!(
            "index-based string op expects a string but got `{}`",
            arg.ty.name
        ));
    }
    lower_expr(ctx, arg, out)?; // record pointer (i32)
    let record = ctx.add_local(WasmValType::I32);
    set_local(out, record);
    // data = record + STR_DATA_OFF
    let data = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_add_data_off(out);
    set_local(out, data);
    // byte_len = i32.load [record + STR_BYTE_LEN_OFF]
    let byte_len = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_len);
    Ok((data, byte_len))
}

/// Emit an expression that leaves an `i32` bool (`1`/`0`) on the stack: whether the
/// `needle` bytes match the haystack bytes starting at byte position `pos`. The
/// caller guarantees `pos + needle_len <= hay_len`, so no bounds check is needed
/// inside; an empty needle (`needle_len == 0`) yields `1` (the inner loop runs zero
/// times), matching Rust's `""` prefix/substring semantics. Emitted as a
/// self-contained `block (result i32)` holding a byte-compare loop.
pub(crate) fn emit_bytes_match_at(
    ctx: &mut LowerCtx,
    hay_data: u32,
    needle_data: u32,
    needle_len: u32,
    pos: u32,
    out: &mut Vec<u8>,
) {
    // result block: j = 0; loop { if j >= needle_len -> push 1, break;
    //   if hay[pos+j] != needle[j] -> push 0, break; j += 1; continue }
    let j = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, j);
    out.push(0x02); // block (result i32)
    out.push(0x7f);
    out.push(0x03); // loop (result i32)
    out.push(0x7f);
    // if j >= needle_len -> matched: push 1 and break out of both.
    get_local(out, j);
    get_local(out, needle_len);
    out.push(0x4e); // i32.ge_s
    out.push(0x04); // if (no result — a branch exits the enclosing blocks)
    out.push(0x40);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x0c); // br 2 (leave block with 1 on the stack)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // if hay[pos + j] != needle[j] -> mismatch: push 0 and break.
    get_local(out, hay_data);
    get_local(out, pos);
    out.push(0x6a); // i32.add
    get_local(out, j);
    out.push(0x6a); // i32.add -> hay_data + pos + j
    emit_load8_u(out);
    get_local(out, needle_data);
    get_local(out, j);
    out.push(0x6a); // i32.add -> needle_data + j
    emit_load8_u(out);
    out.push(0x47); // i32.ne
    out.push(0x04); // if
    out.push(0x40);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    out.push(0x0c); // br 2 (leave block with 0)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // j += 1; continue the loop.
    get_local(out, j);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, j);
    out.push(0x0c); // br 0 (repeat loop)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block -> i32 bool on the stack
}

/// Emit a scan for the FIRST byte position at which `needle` matches `hay`, storing
/// that byte position into `found_pos` and a `1`/`0` flag into `found_flag`. Mirrors
/// Rust's `str::find` at the byte level: it tries every start `pos` in
/// `0..=(hay_len - needle_len)` and stops at the first full byte match. An empty
/// needle matches at `pos = 0` (the match loop runs zero iterations), matching
/// `"...".find("") == Some(0)`. When `needle_len > hay_len` the outer loop never
/// runs and `found_flag` stays `0`. Returns `(found_pos, found_flag)`: fresh
/// caller-visible `i32` locals holding the matched byte position and the found
/// flag, initialized by this function.
pub(crate) fn emit_byte_search(
    ctx: &mut LowerCtx,
    hay_data: u32,
    hay_len: u32,
    needle_data: u32,
    needle_len: u32,
    out: &mut Vec<u8>,
) -> (u32, u32) {
    let found_pos = ctx.add_local(WasmValType::I32);
    let found_flag = ctx.add_local(WasmValType::I32);
    // found_flag = 0; found_pos = 0.
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, found_flag);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, found_pos);
    // limit = hay_len - needle_len (last valid start position, inclusive). When
    // needle_len > hay_len this is negative, so the `pos <= limit` guard fails
    // immediately and the search reports "not found".
    let limit = ctx.add_local(WasmValType::I32);
    get_local(out, hay_len);
    get_local(out, needle_len);
    out.push(0x6b); // i32.sub
    set_local(out, limit);
    // pos = 0; loop { if pos > limit break; if match_at(pos) { found; break }; pos += 1 }
    let pos = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, pos);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when pos > limit (signed; limit may be negative).
    get_local(out, pos);
    get_local(out, limit);
    out.push(0x4a); // i32.gt_s
    out.push(0x0d); // br_if 1 (out of the block)
    write_uleb(out, 1);
    // if bytes_match_at(pos) { found_pos = pos; found_flag = 1; break }
    emit_bytes_match_at(ctx, hay_data, needle_data, needle_len, pos, out);
    out.push(0x04); // if
    out.push(0x40);
    get_local(out, pos);
    set_local(out, found_pos);
    out.push(0x41);
    write_sleb(out, 1);
    set_local(out, found_flag);
    out.push(0x0c); // br 2 (out of the block)
    write_uleb(out, 2);
    out.push(0x0b); // end if
    // pos += 1; continue.
    get_local(out, pos);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, pos);
    out.push(0x0c); // br 0 (repeat)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    (found_pos, found_flag)
}

/// Emit a loop that counts the number of UTF-8 characters in `data[0..byte_end)`
/// and leaves that count (an `i32`) on the stack. A byte begins a character iff
/// `(b & 0xC0) != 0x80` (it is not a continuation byte), so the char count is the
/// number of non-continuation bytes in the range — exactly what
/// `text[..byte_index].chars().count()` yields in the interpreters' `char_find`.
/// `data` and `byte_end` are `i32` locals; `byte_end` is a byte offset relative to
/// `data`.
pub(crate) fn emit_char_count_upto(
    ctx: &mut LowerCtx,
    data: u32,
    byte_end: u32,
    out: &mut Vec<u8>,
) {
    let count = ctx.add_local(WasmValType::I32);
    let bi = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, count);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, bi);
    out.push(0x02); // block
    out.push(0x40);
    out.push(0x03); // loop
    out.push(0x40);
    // break when bi >= byte_end.
    get_local(out, bi);
    get_local(out, byte_end);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1
    write_uleb(out, 1);
    // if (mem[data + bi] & 0xC0) != 0x80 -> count += 1 (a char start).
    get_local(out, data);
    get_local(out, bi);
    out.push(0x6a); // i32.add
    emit_load8_u(out);
    out.push(0x41); // i32.const 0xC0
    write_sleb(out, 0xC0);
    out.push(0x71); // i32.and
    out.push(0x41); // i32.const 0x80
    write_sleb(out, 0x80);
    out.push(0x47); // i32.ne
    out.push(0x04); // if
    out.push(0x40);
    get_local(out, count);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, count);
    out.push(0x0b); // end if
    // bi += 1; continue.
    get_local(out, bi);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, bi);
    out.push(0x0c); // br 0
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block
    get_local(out, count); // leave the count on the stack
}

/// Emit a loop that advances a byte offset from the start of `data` past exactly
/// `target_char` whole UTF-8 characters, storing the resulting byte offset into the
/// caller-owned `i32` local `out_byte`. Each step moves past one lead byte and then
/// over all following continuation bytes (`(b & 0xC0) == 0x80`). For
/// `target_char == char_count` this lands on `byte_len` (one past the last byte).
/// The string is well-formed UTF-8 and `target_char <= char_count` is guaranteed by
/// the caller's bounds check, so the walk terminates in range.
pub(crate) fn emit_char_index_to_byte(
    ctx: &mut LowerCtx,
    data: u32,
    byte_len: u32,
    target_char: u32,
    out_byte: u32,
    out: &mut Vec<u8>,
) {
    // bi = 0; c = 0; loop { if c >= target_char break; bi += 1;
    //   while bi < byte_len and (mem[data+bi] & 0xC0)==0x80 { bi += 1 }; c += 1 }
    let c = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, out_byte); // bi
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, c);
    out.push(0x02); // outer block
    out.push(0x40);
    out.push(0x03); // outer loop (over chars)
    out.push(0x40);
    // break when c >= target_char.
    get_local(out, c);
    get_local(out, target_char);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of outer block)
    write_uleb(out, 1);
    // bi += 1 (past the lead byte).
    get_local(out, out_byte);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, out_byte);
    // inner loop: while bi < byte_len and mem[data+bi] is a continuation byte { bi += 1 }
    out.push(0x02); // inner block
    out.push(0x40);
    out.push(0x03); // inner loop
    out.push(0x40);
    // break inner when bi >= byte_len.
    get_local(out, out_byte);
    get_local(out, byte_len);
    out.push(0x4e); // i32.ge_s
    out.push(0x0d); // br_if 1 (out of inner block)
    write_uleb(out, 1);
    // break inner when NOT a continuation byte: (mem[data+bi] & 0xC0) != 0x80.
    get_local(out, data);
    get_local(out, out_byte);
    out.push(0x6a); // i32.add
    emit_load8_u(out);
    out.push(0x41);
    write_sleb(out, 0xC0);
    out.push(0x71); // i32.and
    out.push(0x41);
    write_sleb(out, 0x80);
    out.push(0x47); // i32.ne
    out.push(0x0d); // br_if 1 (out of inner block — reached the next char start)
    write_uleb(out, 1);
    // bi += 1; continue inner.
    get_local(out, out_byte);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, out_byte);
    out.push(0x0c); // br 0 (repeat inner)
    write_uleb(out, 0);
    out.push(0x0b); // end inner loop
    out.push(0x0b); // end inner block
    // c += 1; continue outer.
    get_local(out, c);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, c);
    out.push(0x0c); // br 0 (repeat outer)
    write_uleb(out, 0);
    out.push(0x0b); // end outer loop
    out.push(0x0b); // end outer block
}

/// Lower `substring(s, start, end) -> string`: the char-indexed half-open
/// `[start, end)` slice. Matches `builtin_substring` exactly: `start`/`end` are
/// char indices; if `start < 0 || end < 0 || start > end || end > char_count` the
/// range is out of bounds and the interpreters raise `L0413`, so the WASM path
/// traps (`unreachable`) rather than producing a wrong value. Otherwise the slice's
/// char indices are mapped to byte offsets by walking the UTF-8, a fresh
/// `[char_len][byte_len][utf8]` record is allocated, and the byte range is
/// `memory.copy`'d in.
pub(crate) fn lower_substring(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    start: &IrExpr,
    end: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate the source string into (data, byte_len); also read its char-count
    // header for the bounds check.
    lower_expr(ctx, s, out)?; // record pointer
    let record = ctx.add_local(WasmValType::I32);
    set_local(out, record);
    let char_count = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_load_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    set_local(out, char_count);
    let byte_len = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_load_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    set_local(out, byte_len);
    let data = ctx.add_local(WasmValType::I32);
    get_local(out, record);
    emit_add_data_off(out);
    set_local(out, data);

    // start/end are i64 char indices; narrow to i32 for offset math (a valid char
    // index fits in i32 — a string cannot hold more than 2^31 chars in a wasm32
    // linear memory). Keep the i64 values for the bounds comparisons so a huge or
    // negative index is rejected exactly like the interpreters.
    let start64 = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, start, out)?;
    set_local(out, start64);
    let end64 = ctx.add_local(WasmValType::I64);
    lower_expr(ctx, end, out)?;
    set_local(out, end64);

    // Bounds check (traps on failure): start < 0 || end < 0 || start > end ||
    // end > char_count. char_count is an i32 count; extend it to i64 for the
    // comparison.
    // cond = (start < 0) | (end < 0) | (start > end) | (end > char_count)
    get_local(out, start64);
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    out.push(0x53); // i64.lt_s -> start < 0
    get_local(out, end64);
    out.push(0x42);
    write_sleb(out, 0);
    out.push(0x53); // end < 0
    out.push(0x72); // i32.or
    get_local(out, start64);
    get_local(out, end64);
    out.push(0x55); // i64.gt_s -> start > end
    out.push(0x72); // i32.or
    get_local(out, end64);
    get_local(out, char_count);
    out.push(0xac); // i64.extend_i32_s (char_count -> i64)
    out.push(0x55); // i64.gt_s -> end > char_count
    out.push(0x72); // i32.or
    out.push(0x04); // if (out-of-bounds) { unreachable }
    out.push(0x40);
    out.push(0x00); // unreachable (trap — mirrors the interpreters' L0413)
    out.push(0x0b); // end if

    // start_char / end_char as i32 char indices.
    let start_char = ctx.add_local(WasmValType::I32);
    get_local(out, start64);
    out.push(0xa7); // i32.wrap_i64
    set_local(out, start_char);
    let end_char = ctx.add_local(WasmValType::I32);
    get_local(out, end64);
    out.push(0xa7); // i32.wrap_i64
    set_local(out, end_char);

    // Map char indices to byte offsets by walking the UTF-8.
    let start_byte = ctx.add_local(WasmValType::I32);
    emit_char_index_to_byte(ctx, data, byte_len, start_char, start_byte, out);
    let end_byte = ctx.add_local(WasmValType::I32);
    emit_char_index_to_byte(ctx, data, byte_len, end_char, end_byte, out);

    // slice_bytes = end_byte - start_byte; slice_chars = end_char - start_char.
    let slice_bytes = ctx.add_local(WasmValType::I32);
    get_local(out, end_byte);
    get_local(out, start_byte);
    out.push(0x6b); // i32.sub
    set_local(out, slice_bytes);

    // dst = __alloc(STR_DATA_OFF + slice_bytes).
    out.push(0x41); // i32.const STR_DATA_OFF
    write_sleb(out, STR_DATA_OFF as i64);
    get_local(out, slice_bytes);
    out.push(0x6a); // i32.add
    let dst = alloc_runtime(ctx, out);
    // dst.char_len = end_char - start_char.
    get_local(out, dst);
    get_local(out, end_char);
    get_local(out, start_char);
    out.push(0x6b); // i32.sub
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    // dst.byte_len = slice_bytes.
    get_local(out, dst);
    get_local(out, slice_bytes);
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);
    // memory.copy(dst + STR_DATA_OFF, data + start_byte, slice_bytes).
    get_local(out, dst);
    emit_add_data_off(out); // dest
    get_local(out, data);
    get_local(out, start_byte);
    out.push(0x6a); // i32.add -> src
    get_local(out, slice_bytes); // size
    emit_memory_copy(out);

    get_local(out, dst); // the slice record's pointer is the value
    Ok(())
}

/// Lower `find(haystack, needle) -> i64`: the CHAR index of the first byte-level
/// occurrence of `needle`, or `-1` if absent. Matches `char_find` exactly: byte
/// search for the first match, then count the UTF-8 characters preceding that byte
/// offset (`text[..byte_index].chars().count()`). An empty needle finds at byte 0,
/// whose preceding char count is 0, so `find(s, "") == 0` — matching Rust's
/// `find("") == Some(0)`.
pub(crate) fn lower_find(
    ctx: &mut LowerCtx,
    haystack: &IrExpr,
    needle: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, haystack, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, needle, out)?;
    let (found_pos, found_flag) =
        emit_byte_search(ctx, hay_data, hay_len, needle_data, needle_len, out);
    // if found_flag { char_count(hay[0..found_pos]) as i64 } else { -1 }
    get_local(out, found_flag);
    out.push(0x04); // if (result i64)
    out.push(0x7e);
    emit_char_count_upto(ctx, hay_data, found_pos, out); // i32 char index
    out.push(0xac); // i64.extend_i32_s
    out.push(0x05); // else
    out.push(0x42); // i64.const -1
    write_sleb(out, -1);
    out.push(0x0b); // end if -> i64 on the stack
    Ok(())
}

/// Lower `contains(s, sub) -> bool`: byte-exact substring test. Emits the same
/// byte search as `find` and yields its found flag (`1`/`0`). An empty `sub` is
/// contained (matches at byte 0), matching Rust's `str::contains("")`.
pub(crate) fn lower_contains(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    sub: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, s, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, sub, out)?;
    let (_found_pos, found_flag) =
        emit_byte_search(ctx, hay_data, hay_len, needle_data, needle_len, out);
    get_local(out, found_flag); // i32 bool result
    Ok(())
}

/// Lower `starts_with(s, prefix) -> bool`: byte-exact prefix test. If
/// `prefix_len > s_len` the result is `0`; otherwise it is whether the prefix bytes
/// match at byte position 0. An empty prefix matches, mirroring
/// `str::starts_with("")`.
pub(crate) fn lower_starts_with(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    prefix: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, s, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, prefix, out)?;
    // if needle_len > hay_len { 0 } else { bytes_match_at(pos = 0) }
    get_local(out, needle_len);
    get_local(out, hay_len);
    out.push(0x4a); // i32.gt_s
    out.push(0x04); // if (result i32)
    out.push(0x7f);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    out.push(0x05); // else
    let pos = ctx.add_local(WasmValType::I32);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, pos);
    emit_bytes_match_at(ctx, hay_data, needle_data, needle_len, pos, out);
    out.push(0x0b); // end if -> i32 bool
    Ok(())
}

/// Lower `ends_with(s, suffix) -> bool`: byte-exact suffix test. If
/// `suffix_len > s_len` the result is `0`; otherwise it is whether the suffix bytes
/// match at byte position `s_len - suffix_len`. An empty suffix matches, mirroring
/// `str::ends_with("")`.
pub(crate) fn lower_ends_with(
    ctx: &mut LowerCtx,
    s: &IrExpr,
    suffix: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let (hay_data, hay_len) = lower_string_data_len(ctx, s, out)?;
    let (needle_data, needle_len) = lower_string_data_len(ctx, suffix, out)?;
    // if needle_len > hay_len { 0 } else { bytes_match_at(pos = hay_len - needle_len) }
    get_local(out, needle_len);
    get_local(out, hay_len);
    out.push(0x4a); // i32.gt_s
    out.push(0x04); // if (result i32)
    out.push(0x7f);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    out.push(0x05); // else
    let pos = ctx.add_local(WasmValType::I32);
    get_local(out, hay_len);
    get_local(out, needle_len);
    out.push(0x6b); // i32.sub
    set_local(out, pos);
    emit_bytes_match_at(ctx, hay_data, needle_data, needle_len, pos, out);
    out.push(0x0b); // end if -> i32 bool
    Ok(())
}

// -- to_string codegen -------------------------------------------------------
//
// `to_string(x)` produces a fresh `[char_len: i32][byte_len: i32][utf8 bytes]`
// string record (see the string record layout notes near `STR_DATA_OFF`),
// interchangeable with string literals and concatenation results. The output
// matches the interpreters' `Value::Display`:
//   - `i64`/signed fixed-width/`isize`: decimal, leading `-` for negatives.
//   - `u64`/unsigned fixed-width/`usize`/`byte`: unsigned decimal magnitude.
//   - `bool`: `"true"` / `"false"` (interned literals).
//   - `char`: the 1–4 byte UTF-8 encoding of the scalar (char_len = 1).
//   - `string`: identity — strings are immutable, so the same pointer is returned.
// A float argument is deferred (see the caller).

/// Lower `to_string(x)` for the supported argument types, leaving the resulting
/// string record's `i32` pointer on the stack. Dispatches on the argument's IR
/// type. A float argument errors so the enclosing function falls back to the
/// interpreters.
pub(crate) fn lower_to_string(
    ctx: &mut LowerCtx,
    arg: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    match arg.ty.name.as_str() {
        // A `string` is already a record; strings are immutable, so returning the
        // same pointer is value-equivalent to the interpreters' clone.
        "string" => lower_expr(ctx, arg, out),
        // `bool` prints `true`/`false`: select the interned literal pointer.
        "bool" => lower_bool_to_string(ctx, arg, out),
        // `char` prints its UTF-8 encoding (1–4 bytes, char_len = 1).
        "char" => lower_char_to_string(ctx, arg, out),
        // `byte` is a 0–255 magnitude held in an `i32` cell: unsigned itoa.
        "byte" => {
            lower_expr(ctx, arg, out)?;
            // Widen the i32 byte cell to an i64 magnitude (unsigned: 0..255).
            out.push(0xad); // i64.extend_i32_u
            emit_itoa_unsigned(ctx, out);
            Ok(())
        }
        // `i64` (plain signed) and the fixed-width integer kinds. Unsigned kinds
        // print the u64 reinterpretation of their normalized cell; signed kinds
        // print the signed value with a leading `-` for negatives.
        "i64" => {
            lower_expr(ctx, arg, out)?;
            emit_itoa_signed(ctx, out);
            Ok(())
        }
        name => match fixed_int_kind(name) {
            Some(kind) if kind.is_unsigned() => {
                lower_expr(ctx, arg, out)?;
                emit_itoa_unsigned(ctx, out);
                Ok(())
            }
            Some(_) => {
                lower_expr(ctx, arg, out)?;
                emit_itoa_signed(ctx, out);
                Ok(())
            }
            // Floats and everything else are deferred to the interpreters.
            None => Err(format!(
                "to_string of `{name}` is not supported by the WASM backend"
            )),
        },
    }
}

/// Lower `to_string(b)` for a `bool`: push the pointer of the interned `"true"`
/// literal when `b` is nonzero, else the interned `"false"` literal, via a typed
/// `if`/`else` yielding an `i32`.
pub(crate) fn lower_bool_to_string(
    ctx: &mut LowerCtx,
    arg: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let true_ptr = ctx.pool.intern("true");
    let false_ptr = ctx.pool.intern("false");
    lower_expr(ctx, arg, out)?; // bool condition (i32 0/1)
    out.push(0x04); // if
    out.push(WasmValType::I32.byte()); // block type: yields i32
    out.push(0x41); // i32.const true_ptr
    write_sleb(out, true_ptr as i64);
    out.push(0x05); // else
    out.push(0x41); // i32.const false_ptr
    write_sleb(out, false_ptr as i64);
    out.push(0x0b); // end
    Ok(())
}

/// Lower `to_string(c)` for a `char`: encode the Unicode scalar (an `i32` code
/// point) to its 1–4 byte UTF-8 sequence in a fresh record with `char_len == 1`
/// and `byte_len` the encoded length. The scalar is guaranteed valid (the type
/// checker only admits real `char` values), so the four ranges below are
/// exhaustive over Unicode scalars.
pub(crate) fn lower_char_to_string(
    ctx: &mut LowerCtx,
    arg: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    lower_expr(ctx, arg, out)?;
    let code = ctx.add_local(WasmValType::I32);
    set_local(out, code);

    // Allocate the maximum record (header + 4 UTF-8 bytes). Only `byte_len` bytes
    // are meaningful; the bump allocator never reclaims, so an over-allocation of
    // a few bytes is harmless and keeps the size a compile-time constant.
    let dst = alloc_bytes(ctx, STR_DATA_OFF + 4, out);
    // char_len is always 1 for a single scalar.
    get_local(out, dst);
    out.push(0x41); // i32.const 1
    write_sleb(out, 1);
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);

    // byte_len local, computed alongside the byte writes.
    let byte_len = ctx.add_local(WasmValType::I32);

    // if code < 0x80 { 1-byte } else if < 0x800 { 2-byte } else if < 0x10000
    // { 3-byte } else { 4-byte }. Each arm writes its bytes at dst+STR_DATA_OFF..
    // and sets byte_len.
    // --- code < 0x80 ---
    get_local(out, code);
    out.push(0x41);
    write_sleb(out, 0x80);
    out.push(0x48); // i32.lt_s
    out.push(0x04); // if
    out.push(0x40); // block type: void
    // dst[data+0] = code
    emit_store_byte_at(dst, STR_DATA_OFF, |o| get_local(o, code), out);
    set_byte_len(byte_len, 1, out);
    out.push(0x05); // else
    // --- code < 0x800 ---
    get_local(out, code);
    out.push(0x41);
    write_sleb(out, 0x800);
    out.push(0x48); // i32.lt_s
    out.push(0x04); // if
    out.push(0x40);
    // b0 = 0xC0 | (code >> 6)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF,
        |o| {
            push_or(o, 0xC0, |o| push_shr_u(o, code, 6));
        },
        out,
    );
    // b1 = 0x80 | (code & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 1,
        |o| {
            push_or(o, 0x80, |o| push_and(o, code, 0x3F));
        },
        out,
    );
    set_byte_len(byte_len, 2, out);
    out.push(0x05); // else
    // --- code < 0x10000 ---
    get_local(out, code);
    out.push(0x41);
    write_sleb(out, 0x10000);
    out.push(0x48); // i32.lt_s
    out.push(0x04); // if
    out.push(0x40);
    // b0 = 0xE0 | (code >> 12)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF,
        |o| {
            push_or(o, 0xE0, |o| push_shr_u(o, code, 12));
        },
        out,
    );
    // b1 = 0x80 | ((code >> 6) & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 1,
        |o| {
            push_or(o, 0x80, |o| push_and_of_shr(o, code, 6, 0x3F));
        },
        out,
    );
    // b2 = 0x80 | (code & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 2,
        |o| {
            push_or(o, 0x80, |o| push_and(o, code, 0x3F));
        },
        out,
    );
    set_byte_len(byte_len, 3, out);
    out.push(0x05); // else
    // --- 4-byte: code >= 0x10000 ---
    // b0 = 0xF0 | (code >> 18)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF,
        |o| {
            push_or(o, 0xF0, |o| push_shr_u(o, code, 18));
        },
        out,
    );
    // b1 = 0x80 | ((code >> 12) & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 1,
        |o| {
            push_or(o, 0x80, |o| push_and_of_shr(o, code, 12, 0x3F));
        },
        out,
    );
    // b2 = 0x80 | ((code >> 6) & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 2,
        |o| {
            push_or(o, 0x80, |o| push_and_of_shr(o, code, 6, 0x3F));
        },
        out,
    );
    // b3 = 0x80 | (code & 0x3F)
    emit_store_byte_at(
        dst,
        STR_DATA_OFF + 3,
        |o| {
            push_or(o, 0x80, |o| push_and(o, code, 0x3F));
        },
        out,
    );
    set_byte_len(byte_len, 4, out);
    // Close the three nested `if`s (`< 0x10000`, `< 0x800`, `< 0x80`); the 4-byte
    // case is the innermost `else`, so it needs no `end` of its own.
    out.push(0x0b); // end (`< 0x10000` if)
    out.push(0x0b); // end (`< 0x800` if)
    out.push(0x0b); // end (`< 0x80` if)

    // dst[byte_len] = byte_len local.
    get_local(out, dst);
    get_local(out, byte_len);
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);

    // The record pointer is the result.
    get_local(out, dst);
    Ok(())
}

/// Store one byte at `dst + offset`: push `dst + offset`, then `value_fn` pushes
/// the byte value, then `i32.store8`.
pub(crate) fn emit_store_byte_at(
    dst: u32,
    offset: i32,
    value_fn: impl FnOnce(&mut Vec<u8>),
    out: &mut Vec<u8>,
) {
    get_local(out, dst);
    value_fn(out);
    out.push(0x3a); // i32.store8
    write_uleb(out, 0); // align 0 (1-byte)
    write_uleb(out, offset as u64);
}

/// Push `constant | inner(...)` as an `i32`: push `constant`, run `inner` (which
/// leaves an i32), `i32.or`.
pub(crate) fn push_or(out: &mut Vec<u8>, constant: i64, inner: impl FnOnce(&mut Vec<u8>)) {
    out.push(0x41); // i32.const constant
    write_sleb(out, constant);
    inner(out);
    out.push(0x72); // i32.or
}

/// Push `local >> shift` (logical) as an `i32`.
pub(crate) fn push_shr_u(out: &mut Vec<u8>, local: u32, shift: i64) {
    get_local(out, local);
    out.push(0x41); // i32.const shift
    write_sleb(out, shift);
    out.push(0x76); // i32.shr_u
}

/// Push `local & mask` as an `i32`.
pub(crate) fn push_and(out: &mut Vec<u8>, local: u32, mask: i64) {
    get_local(out, local);
    out.push(0x41); // i32.const mask
    write_sleb(out, mask);
    out.push(0x71); // i32.and
}

/// Push `(local >> shift) & mask` as an `i32`.
pub(crate) fn push_and_of_shr(out: &mut Vec<u8>, local: u32, shift: i64, mask: i64) {
    push_shr_u(out, local, shift);
    out.push(0x41); // i32.const mask
    write_sleb(out, mask);
    out.push(0x71); // i32.and
}

/// Store the constant `value` into the `byte_len` local (an i32).
pub(crate) fn set_byte_len(byte_len: u32, value: i64, out: &mut Vec<u8>) {
    out.push(0x41); // i32.const value
    write_sleb(out, value);
    set_local(out, byte_len);
}

/// Emit signed integer-to-decimal: consume the `i64` value on the stack and leave
/// a fresh string record pointer. A negative value writes a leading `-` and
/// formats its magnitude; `i64::MIN` is handled by computing the magnitude in
/// unsigned space (`0 - value` wraps to the correct unsigned magnitude), so the
/// unformattable positive `-i64::MIN` is never needed.
pub(crate) fn emit_itoa_signed(ctx: &mut LowerCtx, out: &mut Vec<u8>) {
    let value = ctx.add_local(WasmValType::I64);
    set_local(out, value);
    // sign = (value < 0) as i32.
    let sign = ctx.add_local(WasmValType::I32);
    get_local(out, value);
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    out.push(0x53); // i64.lt_s
    set_local(out, sign);
    // magnitude = value < 0 ? (0 - value) : value, computed via unsigned wrap so
    // `i64::MIN` yields its correct u64 magnitude (0x8000000000000000).
    let mag = ctx.add_local(WasmValType::I64);
    get_local(out, sign);
    out.push(0x04); // if
    out.push(WasmValType::I64.byte()); // yields i64
    out.push(0x42); // i64.const 0
    write_sleb(out, 0);
    get_local(out, value);
    out.push(0x7d); // i64.sub  -> 0 - value (wrapping)
    out.push(0x05); // else
    get_local(out, value);
    out.push(0x0b); // end
    set_local(out, mag);
    emit_itoa_core(ctx, mag, sign, out);
}

/// Emit unsigned integer-to-decimal: consume the `i64` magnitude on the stack
/// (interpreted as `u64`) and leave a fresh string record pointer. No sign is
/// written.
pub(crate) fn emit_itoa_unsigned(ctx: &mut LowerCtx, out: &mut Vec<u8>) {
    let mag = ctx.add_local(WasmValType::I64);
    set_local(out, mag);
    // sign = 0 (no leading `-`).
    let sign = ctx.add_local(WasmValType::I32);
    out.push(0x41); // i32.const 0
    write_sleb(out, 0);
    set_local(out, sign);
    emit_itoa_core(ctx, mag, sign, out);
}

/// The shared itoa core: format the unsigned `u64` magnitude in `mag` with an
/// optional leading `-` when `sign` is nonzero, leaving a fresh
/// `[char_len][byte_len][utf8]` record pointer on the stack. All output is ASCII,
/// so `char_len == byte_len == sign + digit_count`.
///
/// Two passes over the magnitude: pass one counts decimal digits (`0` is one
/// digit), pass two writes them least-significant-first into the tail of the data
/// region, moving a write cursor backward from the last byte so the digits land
/// in print order. The record is allocated once the digit count is known.
pub(crate) fn emit_itoa_core(ctx: &mut LowerCtx, mag: u32, sign: u32, out: &mut Vec<u8>) {
    // --- Pass 1: ndigits = number of decimal digits in `mag` (>= 1). ---
    // A do-while counting loop (`block { loop { body; br_if 1 exit; br 0 } }`, the
    // same idiom the list/map loops use): each iteration counts one digit and
    // divides `scratch` down, so `mag == 0` still counts a single digit.
    let ndigits = ctx.add_local(WasmValType::I32);
    let scratch = ctx.add_local(WasmValType::I64);
    out.push(0x41);
    write_sleb(out, 0);
    set_local(out, ndigits);
    get_local(out, mag);
    set_local(out, scratch);
    out.push(0x02); // block
    out.push(0x40); // void
    out.push(0x03); // loop
    out.push(0x40); // void
    // ndigits += 1
    get_local(out, ndigits);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6a); // i32.add
    set_local(out, ndigits);
    // scratch /= 10
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 10);
    out.push(0x80); // i64.div_u
    set_local(out, scratch);
    // exit the block when scratch == 0.
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 0);
    out.push(0x51); // i64.eq
    out.push(0x0d); // br_if 1 (exit block)
    write_uleb(out, 1);
    out.push(0x0c); // br 0 (repeat loop)
    write_uleb(out, 0);
    out.push(0x0b); // end loop
    out.push(0x0b); // end block

    // total_len = sign + ndigits.
    let total = ctx.add_local(WasmValType::I32);
    get_local(out, sign);
    get_local(out, ndigits);
    out.push(0x6a); // i32.add
    set_local(out, total);

    // dst = __alloc(STR_DATA_OFF + total).
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    get_local(out, total);
    out.push(0x6a); // i32.add
    let dst = alloc_runtime(ctx, out);

    // Headers: char_len = byte_len = total (all ASCII).
    get_local(out, dst);
    get_local(out, total);
    emit_store_at(WasmValType::I32, STR_CHAR_LEN_OFF, out);
    get_local(out, dst);
    get_local(out, total);
    emit_store_at(WasmValType::I32, STR_BYTE_LEN_OFF, out);

    // Optional leading '-' at dst + STR_DATA_OFF (only when sign != 0).
    get_local(out, sign);
    out.push(0x04); // if
    out.push(0x40);
    get_local(out, dst);
    out.push(0x41);
    write_sleb(out, b'-' as i64);
    out.push(0x3a); // i32.store8
    write_uleb(out, 0);
    write_uleb(out, STR_DATA_OFF as u64);
    out.push(0x0b); // end if

    // --- Pass 2: write digits from the tail backward. ---
    // cursor = dst + STR_DATA_OFF + total - 1  (address of the last byte).
    let cursor = ctx.add_local(WasmValType::I32);
    get_local(out, dst);
    out.push(0x41);
    write_sleb(out, STR_DATA_OFF as i64);
    out.push(0x6a); // i32.add
    get_local(out, total);
    out.push(0x6a); // i32.add
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6b); // i32.sub  -> last byte address
    set_local(out, cursor);

    // scratch = mag; then a do-while writing one digit per iteration (so `0`
    // writes a single '0').
    get_local(out, mag);
    set_local(out, scratch);
    out.push(0x03); // loop
    out.push(0x40);
    // *cursor = '0' + (scratch % 10).
    get_local(out, cursor);
    out.push(0x41);
    write_sleb(out, b'0' as i64);
    // (scratch % 10) as i32
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 10);
    out.push(0x82); // i64.rem_u
    out.push(0xa7); // i32.wrap_i64
    out.push(0x6a); // i32.add -> '0' + digit
    out.push(0x3a); // i32.store8
    write_uleb(out, 0);
    write_uleb(out, 0);
    // cursor -= 1.
    get_local(out, cursor);
    out.push(0x41);
    write_sleb(out, 1);
    out.push(0x6b); // i32.sub
    set_local(out, cursor);
    // scratch /= 10.
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 10);
    out.push(0x80); // i64.div_u
    set_local(out, scratch);
    // continue while scratch != 0.
    get_local(out, scratch);
    out.push(0x42);
    write_sleb(out, 0);
    out.push(0x52); // i64.ne
    out.push(0x0d); // br_if 0 -> repeat while nonzero
    write_uleb(out, 0);
    out.push(0x0b); // end loop

    // The record pointer is the result.
    get_local(out, dst);
}

/// The WASM float value type (`F32`/`F64`) an expression evaluates to, or `None`
/// if it is not a float. Mirrors the native backend's `float_width_of_expr`: it
/// reads only the leaf nodes the IR types correctly — float literals, float
/// locals/params, and the `to_f32`/`to_f64` conversions — and recurses through
/// float arithmetic (`+ - * /`), whose own node type the IR annotates `i64`. A
/// comparison yields a `bool` (not a float), so it reports `None`.
pub(crate) fn float_val_type_of(ctx: &LowerCtx, expr: &IrExpr) -> Option<WasmValType> {
    match &expr.kind {
        IrExprKind::Float(_) => match scalar_val_type(&expr.ty) {
            Some(ft @ (WasmValType::F32 | WasmValType::F64)) => Some(ft),
            _ => None,
        },
        IrExprKind::Variable(name) => match ctx.locals.get(name)?.ty {
            ft @ (WasmValType::F32 | WasmValType::F64) => Some(ft),
            _ => None,
        },
        IrExprKind::Call { name, args } => match name.as_str() {
            "to_f32" => Some(WasmValType::F32),
            // `to_f64(x)` widens to f64; `sqrt(x f64) -> f64` is f64-only (its
            // argument is always f64), so a `sqrt` node is reliably f64 — matching
            // the native backend's `float_width_of_expr`.
            "to_f64" | "sqrt" => Some(WasmValType::F64),
            // `abs` follows its argument's width, but only the f64 case is a float
            // result; an `abs(i64)` is an integer (`None`), so `abs` reports a
            // float type only when its argument is f64 — mirroring native.
            "abs" if args.len() == 1 => match float_val_type_of(ctx, &args[0]) {
                Some(WasmValType::F64) => Some(WasmValType::F64),
                _ => None,
            },
            _ => None,
        },
        IrExprKind::Binary {
            left,
            op: BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide,
            right,
        } => float_val_type_of(ctx, left).or_else(|| float_val_type_of(ctx, right)),
        _ => None,
    }
}

/// Emit a fixed-width binary op whose operands (both normalized `i64` cells of
/// `kind`) are already on the stack (left then right), leaving the result (a
/// normalized cell for arithmetic/bitwise/shift, a canonical `0`/`1` for
/// comparisons) on the stack. This mirrors the interpreter free functions
/// exactly: arithmetic wraps then re-normalizes (`Value::int`), division and
/// comparison are signedness-aware (`int_div`/`int_cmp`), and shifts mask the
/// count to the width and honor signedness (`int_shl`/`int_shr`).
pub(crate) fn emit_fixed_binop(
    ctx: &mut LowerCtx,
    op: BinaryOp,
    kind: IntKind,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    match op {
        BinaryOp::Add => {
            out.push(0x7c); // i64.add
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Subtract => {
            out.push(0x7d); // i64.sub
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Multiply => {
            out.push(0x7e); // i64.mul
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Divide => {
            // Divide on the full 64-bit cell (signedness-correct: signed cells are
            // sign-extended, unsigned cells zero-extended), matching `int_div`.
            // WASM `div_s`/`div_u` traps on a zero divisor, exactly like the
            // existing `i64` divide path.
            if kind.is_unsigned() {
                out.push(0x80); // i64.div_u
            } else {
                // Signed division guards `i64::MIN / -1` (and, after
                // normalization, each width's MIN / -1) against the WASM trap.
                emit_i64_signed_div_guarded(ctx, out);
            }
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Remainder => {
            // WASM `rem_s`/`rem_u` need no overflow guard: `rem_s` returns 0 for
            // `MIN % -1` (matching `wrapping_rem`) and traps only on a zero
            // divisor, exactly like the interpreters' remainder path.
            if kind.is_unsigned() {
                out.push(0x82); // i64.rem_u
            } else {
                out.push(0x81); // i64.rem_s
            }
            emit_normalize_i64(kind, out);
        }
        // Equality is width-agnostic on the normalized cells.
        BinaryOp::Equal => out.push(0x51),    // i64.eq
        BinaryOp::NotEqual => out.push(0x52), // i64.ne
        // Ordering uses unsigned comparisons for unsigned kinds, signed for
        // signed kinds, on the normalized cells.
        BinaryOp::Less => out.push(if kind.is_unsigned() { 0x54 } else { 0x53 }), // lt_u/lt_s
        BinaryOp::LessEqual => out.push(if kind.is_unsigned() { 0x58 } else { 0x57 }), // le_u/le_s
        BinaryOp::Greater => out.push(if kind.is_unsigned() { 0x56 } else { 0x55 }), // gt_u/gt_s
        BinaryOp::GreaterEqual => out.push(if kind.is_unsigned() { 0x5a } else { 0x59 }), // ge_u/ge_s
        BinaryOp::BitAnd => {
            out.push(0x83); // i64.and
            emit_normalize_i64(kind, out);
        }
        BinaryOp::BitOr => {
            out.push(0x84); // i64.or
            emit_normalize_i64(kind, out);
        }
        BinaryOp::BitXor => {
            out.push(0x85); // i64.xor
            emit_normalize_i64(kind, out);
        }
        BinaryOp::Shl | BinaryOp::Shr => {
            // Mask the shift count to `width-1` (matching `int_shl`/`int_shr`):
            // `right & (width-1)`. The count is already on the stack; AND it with
            // the mask, then shift the left operand and re-normalize. `<<` is
            // `shl`; `>>` is `shr_u` (logical) for unsigned kinds, `shr_s`
            // (arithmetic) for signed kinds.
            let mask = i64::from(kind.width_bits() - 1); // 7/15/31/63
            out.push(0x42); // i64.const mask
            write_sleb(out, mask);
            out.push(0x83); // i64.and (masked count)
            let shift_opcode = match (op, kind.is_unsigned()) {
                (BinaryOp::Shl, _) => 0x86,     // i64.shl
                (BinaryOp::Shr, true) => 0x88,  // i64.shr_u (logical)
                (BinaryOp::Shr, false) => 0x87, // i64.shr_s (arithmetic)
                _ => unreachable!("outer match restricts to shifts"),
            };
            out.push(shift_opcode);
            emit_normalize_i64(kind, out);
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
// `saturating_*` and `checked_*` detect overflow with comparison-only formulas
// on the normalized operands (no host carry flags exist in WASM), producing the
// same clamp/`none`/`some` result as the interpreters' `overflow_arith` for every
// width and sign. Division appears only in the 64-bit `mul` overflow tests and is
// always guarded (by a structured `if` on a zero divisor, plus the signed
// `MIN / -1` guard) so no case can trap.

/// The arithmetic operation of an overflow-aware builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverflowOp {
    Add,
    Sub,
    Mul,
}

impl OverflowOp {
    /// The wrapping [`BinaryOp`] this operation shares with the default `+`/`-`/`*`
    /// (used to route `wrapping_*` through the fixed-width binary-op emitter).
    fn binary_op(self) -> BinaryOp {
        match self {
            OverflowOp::Add => BinaryOp::Add,
            OverflowOp::Sub => BinaryOp::Subtract,
            OverflowOp::Mul => BinaryOp::Multiply,
        }
    }

    /// The bare `i64.add`/`i64.sub`/`i64.mul` opcode.
    fn wasm_opcode(self) -> u8 {
        match self {
            OverflowOp::Add => 0x7c,
            OverflowOp::Sub => 0x7d,
            OverflowOp::Mul => 0x7e,
        }
    }
}

/// The overflow behaviour of an overflow-aware builtin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverflowMode {
    Wrapping,
    Saturating,
    Checked,
}

/// Recognize an overflow-aware arithmetic builtin name (`checked_add`,
/// `saturating_mul`, `wrapping_sub`, …), returning its `(op, mode)`.
pub(crate) fn overflow_builtin(name: &str) -> Option<(OverflowOp, OverflowMode)> {
    let (mode, op) = name.split_once('_')?;
    let mode = match mode {
        "checked" => OverflowMode::Checked,
        "saturating" => OverflowMode::Saturating,
        "wrapping" => OverflowMode::Wrapping,
        _ => return None,
    };
    let op = match op {
        "add" => OverflowOp::Add,
        "sub" => OverflowOp::Sub,
        "mul" => OverflowOp::Mul,
        _ => return None,
    };
    Some((op, mode))
}

/// `i64.const v`.
pub(crate) fn push_i64_const(out: &mut Vec<u8>, v: i64) {
    out.push(0x42);
    write_sleb(out, v);
}

/// `i32.const v`.
pub(crate) fn push_i32_const(out: &mut Vec<u8>, v: i32) {
    out.push(0x41);
    write_sleb(out, i64::from(v));
}

/// Push an `i32` boolean (`1` iff `a <op> b` overflows `kind`), leaving it on the
/// stack. `a`/`b` are the normalized operands; `wrapped` is `normalize(a op b)`
/// (used by the 64-bit signed `mul` division test). Comparison-only, matching
/// [`lullaby_runtime`]'s `overflow_arith` exactly.
pub(crate) fn push_wasm_overflow_flag(
    ctx: &mut LowerCtx,
    op: OverflowOp,
    kind: IntKind,
    a: u32,
    b: u32,
    wrapped: u32,
    out: &mut Vec<u8>,
) {
    let (min_i128, max_i128) = kind.range_i128();
    let min = min_i128 as i64;
    let max = max_i128 as i64;
    let w64 = matches!(kind, IntKind::U64 | IntKind::Usize | IntKind::Isize);
    let unsigned = kind.is_unsigned();
    match op {
        OverflowOp::Add if unsigned => {
            // a >u (MAX - b)
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x7d); // i64.sub
            out.push(0x56); // i64.gt_u
        }
        OverflowOp::Add => {
            // pos = (b > 0) & (a > MAX - b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x55); // i64.gt_s
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x7d); // i64.sub
            out.push(0x55); // i64.gt_s
            out.push(0x71); // i32.and
            // neg = (b < 0) & (a < MIN - b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            get_local(out, a);
            push_i64_const(out, min);
            get_local(out, b);
            out.push(0x7d); // i64.sub
            out.push(0x53); // i64.lt_s
            out.push(0x71); // i32.and
            out.push(0x72); // i32.or
        }
        OverflowOp::Sub if unsigned => {
            // a <u b
            get_local(out, a);
            get_local(out, b);
            out.push(0x54); // i64.lt_u
        }
        OverflowOp::Sub => {
            // pos = (b < 0) & (a > MAX + b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x7c); // i64.add
            out.push(0x55); // i64.gt_s
            out.push(0x71); // i32.and
            // neg = (b > 0) & (a < MIN + b)
            get_local(out, b);
            push_i64_const(out, 0);
            out.push(0x55); // i64.gt_s
            get_local(out, a);
            push_i64_const(out, min);
            get_local(out, b);
            out.push(0x7c); // i64.add
            out.push(0x53); // i64.lt_s
            out.push(0x71); // i32.and
            out.push(0x72); // i32.or
        }
        OverflowOp::Mul if !w64 => {
            // Narrow: the exact product fits i64; range-check it against [min, max].
            if unsigned {
                get_local(out, a);
                get_local(out, b);
                out.push(0x7e); // i64.mul
                push_i64_const(out, max);
                out.push(0x56); // i64.gt_u
            } else {
                let prod = ctx.add_local(WasmValType::I64);
                get_local(out, a);
                get_local(out, b);
                out.push(0x7e); // i64.mul
                set_local(out, prod);
                get_local(out, prod);
                push_i64_const(out, max);
                out.push(0x55); // i64.gt_s
                get_local(out, prod);
                push_i64_const(out, min);
                out.push(0x53); // i64.lt_s
                out.push(0x72); // i32.or
            }
        }
        OverflowOp::Mul if unsigned => {
            // 64-bit unsigned: overflow iff a*b > MAX iff (b != 0) & (a > MAX/u b).
            // Guard the divide-by-zero with a structured `if` (WASM `i32.and` does
            // not short-circuit).
            get_local(out, b);
            out.push(0x50); // i64.eqz
            out.push(0x04); // if
            out.push(0x7f); // result i32
            push_i32_const(out, 0);
            out.push(0x05); // else
            get_local(out, a);
            push_i64_const(out, max);
            get_local(out, b);
            out.push(0x80); // i64.div_u
            out.push(0x56); // i64.gt_u
            out.push(0x0b); // end
        }
        OverflowOp::Mul => {
            // 64-bit signed (isize): if a == 0 no overflow, else overflow iff the
            // wrapped product divided by `a` does not recover `b` — plus the
            // `-1 * MIN` case the wrapping division cannot distinguish. The guarded
            // signed division avoids the `MIN / -1` trap; `a != 0` avoids div-by-0.
            get_local(out, a);
            out.push(0x50); // i64.eqz
            out.push(0x04); // if
            out.push(0x7f); // result i32
            push_i32_const(out, 0);
            out.push(0x05); // else
            // (a == -1) & (b == MIN)
            get_local(out, a);
            push_i64_const(out, -1);
            out.push(0x51); // i64.eq
            get_local(out, b);
            push_i64_const(out, min);
            out.push(0x51); // i64.eq
            out.push(0x71); // i32.and
            // (guarded_div_s(wrapped, a) != b)
            get_local(out, wrapped);
            get_local(out, a);
            emit_i64_signed_div_guarded(ctx, out);
            get_local(out, b);
            out.push(0x52); // i64.ne
            out.push(0x72); // i32.or
            out.push(0x0b); // end
        }
    }
}

/// Push the `i64` saturation target for `a <op> b` (the bound the true result
/// crosses on overflow). Read only when the overflow flag is set.
pub(crate) fn push_wasm_saturation_target(
    op: OverflowOp,
    kind: IntKind,
    a: u32,
    b: u32,
    wrapped: u32,
    out: &mut Vec<u8>,
) {
    let (min_i128, max_i128) = kind.range_i128();
    let min = min_i128 as i64;
    let max = max_i128 as i64;
    let unsigned = kind.is_unsigned();
    match (op, unsigned) {
        // Unsigned subtraction underflows to the minimum (0); unsigned add/mul
        // saturate up to the maximum.
        (OverflowOp::Sub, true) => push_i64_const(out, min),
        (_, true) => push_i64_const(out, max),
        // Signed multiply: the true product's sign is sign(a) ^ sign(b); a negative
        // product saturates to MIN, else MAX. `select(MIN, MAX, (a ^ b) < 0)`.
        (OverflowOp::Mul, false) => {
            push_i64_const(out, min);
            push_i64_const(out, max);
            get_local(out, a);
            get_local(out, b);
            out.push(0x85); // i64.xor
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            out.push(0x1b); // select
        }
        // Signed add/sub: a signed overflow flips the wrapped result's sign, so a
        // negative wrapped value means positive overflow (target MAX), else MIN.
        // `select(MAX, MIN, wrapped < 0)`.
        (_, false) => {
            push_i64_const(out, max);
            push_i64_const(out, min);
            get_local(out, wrapped);
            push_i64_const(out, 0);
            out.push(0x53); // i64.lt_s
            out.push(0x1b); // select
        }
    }
}

/// Lower an overflow-aware arithmetic builtin. `wrapping_*` leaves the wrapped
/// `T` value on the stack; `saturating_*` the clamped `T`; `checked_*` a fresh
/// `option<T>` record pointer (`some(result)`/`none`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_wasm_overflow(
    ctx: &mut LowerCtx,
    op: OverflowOp,
    mode: OverflowMode,
    kind: IntKind,
    result_ty: &TypeRef,
    left: &IrExpr,
    right: &IrExpr,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    // Evaluate both operands into `i64` locals so the overflow tests can read them
    // several times.
    lower_expr(ctx, left, out)?;
    let a = ctx.add_local(WasmValType::I64);
    set_local(out, a);
    lower_expr(ctx, right, out)?;
    let b = ctx.add_local(WasmValType::I64);
    set_local(out, b);
    // wrapped = normalize(a op b) — the wrapping result and the `some` payload.
    let wrapped = ctx.add_local(WasmValType::I64);
    get_local(out, a);
    get_local(out, b);
    out.push(op.wasm_opcode());
    emit_normalize_i64(kind, out);
    set_local(out, wrapped);

    match mode {
        OverflowMode::Wrapping => {
            get_local(out, wrapped);
            Ok(())
        }
        OverflowMode::Saturating => {
            let ovf = ctx.add_local(WasmValType::I32);
            push_wasm_overflow_flag(ctx, op, kind, a, b, wrapped, out);
            set_local(out, ovf);
            // result = ovf ? target : wrapped.
            push_wasm_saturation_target(op, kind, a, b, wrapped, out);
            get_local(out, wrapped);
            get_local(out, ovf);
            out.push(0x1b); // select
            Ok(())
        }
        OverflowMode::Checked => {
            let ovf = ctx.add_local(WasmValType::I32);
            push_wasm_overflow_flag(ctx, op, kind, a, b, wrapped, out);
            set_local(out, ovf);
            // Build the `option<T>` record: tag = ovf ? none : some, payload = wrapped.
            let inner = result_ty.option_element().ok_or_else(|| {
                format!(
                    "checked_* result type `{}` is not an `option<T>` enum",
                    result_ty.name
                )
            })?;
            let slot_ty = slot_val_type(&inner, ctx.structs, ctx.enums).ok_or_else(|| {
                format!("checked_* option payload `{}` is unsupported", inner.name)
            })?;
            let layout = build_layout(vec![
                ("some".to_string(), vec![inner]),
                ("none".to_string(), Vec::new()),
            ]);
            let some_tag = layout
                .tag_of("some")
                .ok_or_else(|| "checked_* option layout missing `some` variant".to_string())?;
            let none_tag = layout
                .tag_of("none")
                .ok_or_else(|| "checked_* option layout missing `none` variant".to_string())?;
            let opt = alloc_bytes(ctx, layout.size_bytes(), out);
            // tag = select(none, some, ovf).
            get_local(out, opt);
            push_i32_const(out, none_tag as i32);
            push_i32_const(out, some_tag as i32);
            get_local(out, ovf);
            out.push(0x1b); // select
            emit_store_at(WasmValType::I32, 0, out);
            // payload slot = wrapped.
            get_local(out, opt);
            get_local(out, wrapped);
            emit_store_at(slot_ty, ENUM_PAYLOAD_BASE, out);
            get_local(out, opt);
            Ok(())
        }
    }
}

/// Emit a bitwise/shift binary op on plain `i64` operands already on the stack
/// (left then right). No width normalization is needed: `i64` fills the cell.
pub(crate) fn emit_i64_bitwise_or_shift(op: BinaryOp, out: &mut Vec<u8>) -> Result<(), String> {
    let opcode = match op {
        BinaryOp::BitAnd => 0x83, // i64.and
        BinaryOp::BitOr => 0x84,  // i64.or
        BinaryOp::BitXor => 0x85, // i64.xor
        BinaryOp::Shl => 0x86,    // i64.shl (WASM masks the count modulo 64)
        BinaryOp::Shr => 0x87,    // i64.shr_s (arithmetic, matching the i64 shift)
        _ => unreachable!("caller restricts to bitwise/shift"),
    };
    out.push(opcode);
    Ok(())
}
