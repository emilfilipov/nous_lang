//! Native expression lowering (each expression leaves its result in `rax`) and
//! the internal call-argument ABI (Win64 register + stack-spill staging). Split
//! out of native_object.rs; recurses into the op-lowering submodule and the
//! parent's statement lowering via `use super::*`.

use super::*;

// -- Expression lowering (result left in rax) --------------------------------

pub(crate) fn lower_native_expr(
    ctx: &mut NativeCtx,
    expr: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    match &expr.kind {
        BytecodeExprKind::Integer(value) => {
            emit_mov_rax_imm(code, *value);
            Ok(())
        }
        // A `bool` literal is a `0`/`1` cell; a `char` literal is its Unicode
        // scalar value (code point). Both are single normalized `i64` cells — the
        // same representation the interpreters use — so they load as an immediate.
        // These reach here mainly through `to_string(true)` / `to_string('x')`, but
        // a bool/char value is a valid `i64`-cell scalar wherever one is expected.
        BytecodeExprKind::Bool(value) => {
            emit_mov_rax_imm(code, i64::from(*value));
            Ok(())
        }
        BytecodeExprKind::Char(value) => {
            emit_mov_rax_imm(code, i64::from(u32::from(*value)));
            Ok(())
        }
        BytecodeExprKind::Variable(name) => {
            // Inside a synthesized closure body, a captured free variable resolves
            // through the env pointer: load the env pointer from its frame slot, then
            // the captured word at `[env + offset]`. Parameters and any other locals
            // fall through to the ordinary frame-slot path.
            if let Some(env) = &ctx.closure_env
                && let Some(&offset) = env.captures.get(name)
            {
                let env_slot = env.env_slot;
                load_local(code, env_slot); // mov rax, [rbp - env_slot] (env ptr)
                // mov rax, [rax + offset]  (captured word)
                code.extend_from_slice(&[0x48, 0x8B, 0x80]);
                code.extend_from_slice(&offset.to_le_bytes());
                return Ok(());
            }
            let slot = ctx.local_slot(name)?;
            match ctx.promoted_reg(slot) {
                Some(reg) => reg.to_rax(code),
                None => load_local(code, slot),
            }
            Ok(())
        }
        BytecodeExprKind::Unary { op, expr: inner } => match op {
            lullaby_parser::UnaryOp::Not => {
                // Boolean `not`: rax = (inner == 0) ? 1 : 0.
                lower_native_expr(ctx, inner, code)?;
                code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
                code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                Ok(())
            }
            // Integer bitwise NOT (`~`). On a fixed-width kind it is one's
            // complement re-normalized to the width, matching the interpreter's
            // `Value::int(!v, ty)`. On plain `i64` it is a full-width `not`.
            lullaby_parser::UnaryOp::BitNot => {
                lower_native_expr(ctx, inner, code)?;
                code.extend_from_slice(&[0x48, 0xF7, 0xD0]); // not rax
                if let Some(kind) = fixed_int_kind(inner.ty.name.as_str()) {
                    emit_normalize_rax(code, kind);
                }
                Ok(())
            }
            // Integer arithmetic negation (`-x`). Wrapping `neg`, re-normalized on
            // a fixed-width kind. (Float `-x` is handled on the float path.)
            lullaby_parser::UnaryOp::Negate => {
                lower_native_expr(ctx, inner, code)?;
                code.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
                if let Some(kind) = fixed_int_kind(inner.ty.name.as_str()) {
                    emit_normalize_rax(code, kind);
                }
                Ok(())
            }
        },
        BytecodeExprKind::Binary { left, op, right } => {
            lower_native_binary(ctx, left, *op, right, code)
        }
        BytecodeExprKind::Call { name, args } => {
            // A call whose callee name is a closure-bound local is an INDIRECT
            // closure call: load the env pointer (the block base) into `rcx`, the
            // arguments into `rdx`/`r8`/`r9`, and `call [env]` (the code pointer at
            // word 0). Detected before any builtin/name resolution so it never
            // collides with a top-level function name.
            if ctx.closure_locals.contains_key(name) {
                return lower_closure_call(ctx, name, args, code);
            }
            // Fixed-width integer conversions are emitted inline, not as calls.
            // `to_<T>(x)` normalizes the argument's `i64` cell into `T`'s width
            // (truncate + sign/zero-extend), matching the interpreter's
            // `Value::int(x, T)`. `to_i64(x)` widens a fixed-width cell to `i64`,
            // which is the identity on the already-normalized cell.
            if let Some(kind) = to_int_conversion_kind(name) {
                if args.len() != 1 {
                    return Err(format!("`{name}` takes exactly one argument"));
                }
                lower_native_expr(ctx, &args[0], code)?;
                emit_normalize_rax(code, kind);
                return Ok(());
            }
            if name == "to_i64" {
                if args.len() != 1 {
                    return Err("`to_i64` takes exactly one argument".to_string());
                }
                // The source cell is already normalized; widening to `i64` keeps
                // the bits unchanged.
                lower_native_expr(ctx, &args[0], code)?;
                return Ok(());
            }
            // `to_string(x)` produces a fresh heap string record (a pointer in
            // `rax`), matching the interpreters' `Display`/`builtin_to_string`:
            //   * an integer (`i64`/fixed-width) → decimal digits, signed or
            //     unsigned by the argument's kind (`byte` prints its 0..=255 value);
            //   * `bool` → `"true"`/`"false"`;
            //   * `char` → the code point's UTF-8 encoding;
            //   * `string` → identity (the argument's pointer is already the value).
            // Float `to_string` (dtoa) is deferred and falls back to the
            // interpreters (rejected here so the function skips gracefully).
            if name == "to_string" {
                if args.len() != 1 {
                    return Err("`to_string` takes exactly one argument".to_string());
                }
                return lower_to_string(ctx, &args[0], code);
            }
            // Growable `list<T>` (scalar `T`) builtins. `list_new()` allocates a
            // fresh `[len=0][cap=LIST_INITIAL_CAP][slots]` heap block; `push`/`set`/
            // `pop` are value-semantic (they deep-copy their source and mutate the
            // copy); `get` loads element `i`; `len(l)` loads the list's `len`
            // header. Each leaves a pointer (or, for `get`/`len`, an `i64`) in
            // `rax`. Dispatched by the (scalar-element) list type of the operand.
            if name == LIST_NEW_BUILTIN {
                if !args.is_empty() {
                    return Err("list_new expects 0 arguments".to_string());
                }
                lower_list_new(ctx, code);
                return Ok(());
            }
            if name == LIST_PUSH_BUILTIN
                && args.len() == 2
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_push(ctx, &args[0], &args[1], code)?;
                return Ok(());
            }
            if name == LIST_SET_BUILTIN
                && args.len() == 3
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_set(ctx, &args[0], &args[1], &args[2], code)?;
                return Ok(());
            }
            if name == LIST_POP_BUILTIN
                && args.len() == 1
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_pop(ctx, &args[0], code)?;
                return Ok(());
            }
            if name == LIST_GET_BUILTIN
                && args.len() == 2
                && supported_list_element(&args[0].ty).is_some()
            {
                lower_list_get(ctx, &args[0], &args[1], code)?;
                return Ok(());
            }
            // `len(l)` on a growable list loads its `len` header word.
            if name == "len" && args.len() == 1 && supported_list_element(&args[0].ty).is_some() {
                lower_native_expr(ctx, &args[0], code)?; // list pointer -> rax
                // mov rax, [rax + LIST_LEN_OFF]
                emit_mov_rax_from_rax_disp(code, LIST_LEN_OFF);
                return Ok(());
            }
            // `len(a)` on a heap `array<string>` reads the same `len` header word
            // (it shares the `list<string>` block layout).
            if name == "len" && args.len() == 1 && heap_string_array_element(&args[0].ty).is_some()
            {
                lower_native_expr(ctx, &args[0], code)?; // block pointer -> rax
                emit_mov_rax_from_rax_disp(code, LIST_LEN_OFF);
                return Ok(());
            }
            // `split(text, sep) -> array<string>`: stage the two string operands into
            // rcx/rdx and call the split helper, which builds a fresh `list<string>`
            // block of the fields. The result is a single pointer word (like
            // `list_new`), so it lowers here in the scalar path.
            if name == "split"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_SPLIT_SYMBOL, code);
            }
            // `join(a, sep) -> string`: stage the `array<string>` block and the
            // separator into rcx/rdx and call the join helper (a fresh record).
            if name == "join"
                && args.len() == 2
                && heap_string_array_element(&args[0].ty).is_some()
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_JOIN_SYMBOL, code);
            }
            // Growable `map<K, V>` (scalar key/value) builtins with a *scalar*
            // result. `map_new()` allocates an empty header; `map_set` deep-copies
            // then updates-or-appends (value-semantic, returns the fresh map
            // pointer); `map_has` scans to a `bool`; `map_len` reads the `len`
            // header. `map_get` returns `option<V>` (an aggregate), so it is lowered
            // in the aggregate paths (`lower_aggregate_init` / a `match` scrutinee),
            // not here. Each name is unique (no array/list op shares it), so they
            // dispatch by name; the key/value types are validated in each lowering.
            if name == MAP_NEW_BUILTIN {
                if !args.is_empty() {
                    return Err("map_new expects 0 arguments".to_string());
                }
                lower_map_new(ctx, code);
                return Ok(());
            }
            if name == MAP_SET_BUILTIN && args.len() == 3 && supported_map_kv(&args[0].ty).is_some()
            {
                lower_map_set(ctx, &args[0], &args[1], &args[2], code)?;
                return Ok(());
            }
            if name == MAP_HAS_BUILTIN && args.len() == 2 && supported_map_kv(&args[0].ty).is_some()
            {
                lower_map_has(ctx, &args[0], &args[1], code)?;
                return Ok(());
            }
            if name == MAP_LEN_BUILTIN && args.len() == 1 && supported_map_kv(&args[0].ty).is_some()
            {
                lower_native_expr(ctx, &args[0], code)?; // map pointer -> rax
                // mov rax, [rax + MAP_LEN_OFF]
                emit_mov_rax_from_rax_disp(code, MAP_LEN_OFF);
                return Ok(());
            }
            // `char_code(c)`: a `char` is stored as its Unicode code point in an i64
            // cell, so `char_code` is the identity on that cell (matches the
            // interpreters' `char as i64`).
            if name == "char_code" && args.len() == 1 && args[0].ty.name == "char" {
                lower_native_expr(ctx, &args[0], code)?;
                return Ok(());
            }
            // `is_digit(c)`: 1 when the code point is an ASCII digit `'0'..='9'`
            // (48..=57), else 0 — matching the interpreters' `is_ascii_digit`. One
            // unsigned range test: `(c - 48) <= 9` (a `c < 48` underflows to a huge
            // unsigned value that is not `<= 9`). Other `is_*` predicates are
            // Unicode-aware and stay on the interpreters.
            if name == "is_digit" && args.len() == 1 {
                lower_native_expr(ctx, &args[0], code)?; // c -> rax
                code.extend_from_slice(&[0x48, 0x83, 0xE8, 0x30]); // sub rax, 48
                code.extend_from_slice(&[0x48, 0x83, 0xF8, 0x09]); // cmp rax, 9
                code.extend_from_slice(&[0x0F, 0x96, 0xC0]); // setbe al
                code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                return Ok(());
            }
            // `len(arr)` over a fixed native array folds to a compile-time
            // constant (arrays never grow in the native subset).
            if name == "len"
                && args.len() == 1
                && let BytecodeExprKind::Variable(var) = &args[0].kind
                && let Ok(local) = ctx.local(var)
                && let NativeType::Array { len, .. } = &local.ty
            {
                emit_mov_rax_imm(code, *len as i64);
                return Ok(());
            }
            // `len(a)` over a fat-pointer array parameter reads the descriptor's
            // runtime length word (`[rbp - (ptr_slot + 8)]`).
            if name == "len"
                && args.len() == 1
                && let BytecodeExprKind::Variable(var) = &args[0].kind
                && let Ok(local) = ctx.local(var)
                && let NativeType::FatArray { .. } = &local.ty
            {
                let len_slot = local.slot + 8;
                load_local(code, len_slot);
                return Ok(());
            }
            // `len(string_literal)` is the first heap-backed native string op.
            // The literal's bytes live in `.rdata`; `__lullaby_strlen_copy` bump-
            // allocates a heap copy of them, scans the copy for its terminator,
            // and returns the byte length in rax (== the interpreter's char count
            // for the ASCII strings this subset accepts). This exercises the
            // whole first heap step end to end: a `.rdata` constant, a REL32
            // relocation to its address, the bump allocator, and per-byte reads
            // of both `.rdata` and the heap.
            if name == "len"
                && args.len() == 1
                && let BytecodeExprKind::String(text) = &args[0].kind
            {
                if !text.is_ascii() {
                    return Err("native string len supports ASCII string literals only".to_string());
                }
                let symbol = ctx.strings.intern(text);
                // lea rcx, [rip + __str] ; the 4-byte rel32 is a REL32 relocation.
                code.extend_from_slice(&[0x48, 0x8D, 0x0D]);
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                ctx.relocations.push(CodeRelocation {
                    offset: site as u32,
                    symbol,
                });
                // call __lullaby_strlen_copy (rel32 relocation).
                code.push(0xE8);
                let call_site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                ctx.relocations.push(CodeRelocation {
                    offset: call_site as u32,
                    symbol: HEAP_STRLEN_SYMBOL.to_string(),
                });
                return Ok(());
            }
            // `len(s)` on any other string VALUE (a variable, a parameter, a
            // concatenation result, a `to_string` result, …) reads the `char_len`
            // header of the record the string pointer addresses. This gives the
            // Unicode scalar count for arbitrary UTF-8 strings, not only ASCII
            // literals.
            if name == "len" && args.len() == 1 && is_string_type(&args[0].ty) {
                lower_native_expr(ctx, &args[0], code)?; // string pointer -> rax
                // If the operand is a uniquely-owned fresh temporary (a literal,
                // `to_string`, `substring`/`trim`/`repeat`, or a concat — never a
                // borrowed variable/field), it is dead after `len` reads its header,
                // so reclaim it via the ownership-aware helper; otherwise just read
                // the header word (borrowed value, keep it).
                if is_owning_string_alloc(&args[0]) {
                    code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (fresh-temp pointer)
                    emit_call_symbol(ctx, STR_LEN_OWN_SYMBOL, code); // rax = char_len, frees it
                } else {
                    emit_mov_rax_from_rax_disp(code, STR_CHAR_LEN_OFF);
                }
                return Ok(());
            }
            // Index-based string operations over the heap `[char_len][byte_len]
            // [utf8]` record. Each stages its string (and index) operands into the
            // Win64 argument registers and calls a `.text` helper; the helper does
            // the UTF-8-aware scanning and (for `substring`) allocation, exactly
            // matching the interpreters' semantics (char-indexed `substring`/`find`,
            // byte-wise `contains`/`starts_with`/`ends_with`). Guarded by the string
            // operand type so a same-named user function still resolves as a call.
            if name == "substring"
                && args.len() == 3
                && is_string_type(&args[0].ty)
                && args[1].ty.name == "i64"
                && args[2].ty.name == "i64"
            {
                return lower_string_substring(ctx, &args[0], &args[1], &args[2], code);
            }
            if name == "find"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_FIND_SYMBOL, code);
            }
            // `count(text, sub)`: non-overlapping occurrence count (i64). Guarded on
            // two string operands so a user-defined `count` still resolves as a call.
            if name == "count"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_COUNT_SYMBOL, code);
            }
            // `repeat(text, count)`: text repeated `count` times (a fresh record).
            if name == "repeat"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && args[1].ty.name == "i64"
            {
                return lower_string_repeat(ctx, &args[0], &args[1], code);
            }
            // `trim(text)`: leading/trailing ASCII whitespace removed (fresh record).
            if name == "trim" && args.len() == 1 && is_string_type(&args[0].ty) {
                return lower_string_trim(ctx, &args[0], code);
            }
            // `upper(text)`/`lower(text)`: ASCII case fold (fresh record). The native
            // string subset is ASCII, so a byte-wise fold matches the interpreters.
            if (name == "upper" || name == "lower")
                && args.len() == 1
                && is_string_type(&args[0].ty)
            {
                let symbol = if name == "upper" {
                    STR_UPPER_SYMBOL
                } else {
                    STR_LOWER_SYMBOL
                };
                return lower_string_case(ctx, &args[0], symbol, code);
            }
            if name == "contains"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_CONTAINS_SYMBOL, code);
            }
            if name == "starts_with"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(
                    ctx,
                    &args[0],
                    &args[1],
                    STR_STARTS_WITH_SYMBOL,
                    code,
                );
            }
            if name == "ends_with"
                && args.len() == 2
                && is_string_type(&args[0].ty)
                && is_string_type(&args[1].ty)
            {
                return lower_string_binary_op(ctx, &args[0], &args[1], STR_ENDS_WITH_SYMBOL, code);
            }
            // Overflow-aware arithmetic builtins. `wrapping_*` reuses the default
            // fixed-width `+`/`-`/`*` (wrap then normalize); `saturating_*` clamps
            // to `T`'s bounds. `checked_*` yields `option<T>` (an aggregate) and is
            // lowered in the aggregate positions (binding/return via
            // `lower_aggregate_init`, or a `match` scrutinee), never as a scalar,
            // so it is not handled here. Guarded by a fixed-width first operand so
            // the names cannot shadow a user function of the same spelling.
            if let Some((ovf_op, mode)) = overflow_builtin(name)
                && args.len() == 2
                && let Some(kind) = fixed_int_kind(args[0].ty.name.as_str())
            {
                match mode {
                    OverflowMode::Wrapping => {
                        lower_native_expr(ctx, &args[0], code)?;
                        code.push(0x50); // push rax (left)
                        lower_native_expr(ctx, &args[1], code)?; // right in rax
                        emit_fixed_binop_from_stack(code, ovf_op.binary_op(), kind)?;
                        return Ok(());
                    }
                    OverflowMode::Saturating => {
                        return lower_native_saturating(
                            ctx, ovf_op, kind, &args[0], &args[1], code,
                        );
                    }
                    OverflowMode::Checked => {}
                }
            }
            // `abs(x)` on `i64`: the branchless two's-complement abs idiom
            // (`sar` sign mask, `xor`, `sub`), matching release `i64::abs` — which
            // wraps `abs(i64::MIN)` back to `i64::MIN`, consistent with the native
            // wrapping-arithmetic contract. `abs(f64)` is handled in the float
            // lowerer (an XMM sign-bit clear) and routes there via
            // `float_width_of_expr`, so only the i64 case reaches here.
            if name == "abs" && args.len() == 1 && args[0].ty.name == "i64" {
                lower_native_expr(ctx, &args[0], code)?; // x in rax
                code.extend_from_slice(&[
                    0x48, 0x89, 0xC2, // mov rdx, rax
                    0x48, 0xC1, 0xFA, 0x3F, // sar rdx, 63   (sign mask)
                    0x48, 0x31, 0xD0, // xor rax, rdx
                    0x48, 0x29, 0xD0, // sub rax, rdx  -> rax = |x|
                ]);
                return Ok(());
            }
            // `min(a, b)` / `max(a, b)` on plain `i64`: a branchless signed
            // `cmp` + `cmov`, matching the interpreters' `i64::min`/`i64::max`
            // exactly. Evaluate left→rax (spilled), right→rax, then `pop rcx`
            // (left) and conditionally move it over the right. Only the `i64` case
            // is lowered here; an `f64` `min`/`max` is deferred (the SSE `minsd`/
            // `maxsd` NaN/`±0.0` rules diverge from `f64::min`/`f64::max`), and a
            // fixed-width kind cannot occur (the type checker admits only i64/f64).
            if (name == "min" || name == "max")
                && args.len() == 2
                && args[0].ty.name == "i64"
                && args[1].ty.name == "i64"
            {
                lower_native_expr(ctx, &args[0], code)?; // left in rax
                code.push(0x50); // push rax (left)
                lower_native_expr(ctx, &args[1], code)?; // right in rax
                code.push(0x59); // pop rcx (left)
                code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
                // min: rcx < rax (left smaller) -> cmovl takes rcx.
                // max: rcx > rax (left larger)  -> cmovg takes rcx.
                let cmov = if name == "min" { 0x4C } else { 0x4F };
                code.extend_from_slice(&[0x48, 0x0F, cmov, 0xC1]); // cmov(l|g) rax, rcx
                return Ok(());
            }
            // `gcd(a, b)` on `i64`: inlined as a branchless two's-complement `abs`
            // of each operand into an *unsigned* magnitude, then Euclid's
            // algorithm with an unsigned `div`. The interpreter computes on i128
            // magnitudes, but every magnitude is bounded by 2^63, which fits a
            // u64 — so a u64 computation is bit-identical, including the sole
            // overflow case `gcd(i64::MIN, 0)`: |i64::MIN| = 2^63 stays in the u64
            // and reinterprets back to `i64::MIN`, exactly as `gcd_i64` wraps it.
            // Self-contained (no helper, no heap); `gcd` accepts only i64 (the
            // type checker forbids other kinds), so no width handling is needed.
            if name == "gcd" && args.len() == 2 && args[0].ty.name == "i64" {
                lower_native_expr(ctx, &args[0], code)?; // a in rax
                code.push(0x50); // push rax (a)
                lower_native_expr(ctx, &args[1], code)?; // b in rax
                code.push(0x59); // pop rcx (a); b stays in rax
                code.extend_from_slice(&[
                    // x = |a| (unsigned magnitude, two's-complement abs of rcx)
                    0x48, 0x89, 0xCA, // mov rdx, rcx
                    0x48, 0xC1, 0xFA, 0x3F, // sar rdx, 63   (sign mask)
                    0x48, 0x31, 0xD1, // xor rcx, rdx
                    0x48, 0x29, 0xD1, // sub rcx, rdx  -> rcx = |a|
                    // y = |b| (abs of rax)
                    0x48, 0x89, 0xC2, // mov rdx, rax
                    0x48, 0xC1, 0xFA, 0x3F, // sar rdx, 63
                    0x48, 0x31, 0xD0, // xor rax, rdx
                    0x48, 0x29, 0xD0, // sub rax, rdx  -> rax = |b|
                    // loop: while y(rax) != 0 { r = x % y; x = y; y = r }
                    0x48, 0x85, 0xC0, // test rax, rax
                    0x0F, 0x84, 0x17, 0x00, 0x00, 0x00, // jz done (+23)
                    0x49, 0x89, 0xC0, // mov r8, rax   (divisor = y)
                    0x48, 0x89, 0xC8, // mov rax, rcx  (dividend = x)
                    0x48, 0x31, 0xD2, // xor rdx, rdx  (clear high half)
                    0x49, 0xF7, 0xF0, // div r8        (unsigned; rdx = x % y)
                    0x4C, 0x89, 0xC1, // mov rcx, r8   (x = y)
                    0x48, 0x89, 0xD0, // mov rax, rdx  (y = r)
                    0xE9, 0xE0, 0xFF, 0xFF, 0xFF, // jmp loop (-32)
                    // done: result = x
                    0x48, 0x89, 0xC8, // mov rax, rcx
                ]);
                return Ok(());
            }
            // `sign(x)` on `i64` -> `i64` (`-1`/`0`/`1`): a branchless
            // `test` + two signed `cmov`s, matching the interpreters' `i64::signum`.
            // Only the i64 arg lowers here; a `sign(f64)` (which needs float
            // comparisons and returns i64) skips gracefully.
            if name == "sign" && args.len() == 1 && args[0].ty.name == "i64" {
                lower_native_expr(ctx, &args[0], code)?; // x in rax
                code.extend_from_slice(&[
                    0x48, 0x89, 0xC1, // mov rcx, rax   (save x)
                    0x48, 0x31, 0xC0, // xor rax, rax   (result = 0)
                    0x48, 0xC7, 0xC2, 0x01, 0x00, 0x00, 0x00, // mov rdx, 1
                    0x48, 0x85, 0xC9, // test rcx, rcx
                    0x48, 0x0F, 0x4F, 0xC2, // cmovg rax, rdx  (x>0 -> 1)
                    0x48, 0xC7, 0xC2, 0xFF, 0xFF, 0xFF, 0xFF, // mov rdx, -1
                    0x48, 0x0F, 0x4C, 0xC2, // cmovl rax, rdx  (x<0 -> -1)
                ]);
                return Ok(());
            }
            // `clamp(x, lo, hi)` on `i64`: branchless, applying the upper clamp
            // then the lower clamp (lower wins), both comparing the *original* x —
            // matching the interpreters' `if x < lo { lo } else if x > hi { hi }
            // else { x }` for every ordering of `lo`/`hi` (including `lo > hi`).
            // Only the i64 case lowers here; an f64 `clamp` skips gracefully.
            if name == "clamp" && args.len() == 3 && args[0].ty.name == "i64" {
                lower_native_expr(ctx, &args[0], code)?; // x
                code.push(0x50); // push rax (x)
                lower_native_expr(ctx, &args[1], code)?; // lo
                code.push(0x50); // push rax (lo)
                lower_native_expr(ctx, &args[2], code)?; // hi in rax
                code.extend_from_slice(&[
                    0x48, 0x89, 0xC2, // mov rdx, rax   (hi)
                    0x59, // pop rcx        (lo)
                    0x58, // pop rax        (x -> result seed)
                    0x49, 0x89, 0xC1, // mov r9, rax    (r9 = original x, preserved)
                    // upper clamp: `cmp rdx, r9` sets flags for (hi - x); the `l`
                    // (less) condition is hi < x, i.e. x > hi, so take hi.
                    0x49, 0x89, 0xD0, // mov r8, rdx    (r8 = hi)
                    0x4C, 0x39, 0xCA, // cmp rdx, r9    (hi vs original x)
                    0x49, 0x0F, 0x4C, 0xC0, // cmovl rax, r8  (hi < x -> take hi)
                    // lower clamp (wins, applied last): `cmp rcx, r9` sets flags
                    // for (lo - x); the `g` (greater) condition is lo > x, i.e.
                    // x < lo, so take lo.
                    0x49, 0x89, 0xC8, // mov r8, rcx    (r8 = lo)
                    0x4C, 0x39, 0xC9, // cmp rcx, r9    (lo vs original x)
                    0x49, 0x0F, 0x4F, 0xC0, // cmovg rax, r8  (lo > x -> take lo)
                ]);
                return Ok(());
            }
            if !ctx.callable.contains(name.as_str()) {
                return Err(format!(
                    "call to non-i64-scalar or unknown function `{name}`"
                ));
            }
            // A call to a compiled function that *returns* an aggregate cannot
            // leave its result in `rax` as a value: it writes through a hidden
            // pointer. Such a call reaching here (in scalar expression position)
            // would be a use of its aggregate result that we do not handle, so it
            // is routed through `lower_aggregate_init` instead. Guard against it.
            if let Some(sig) = ctx.signatures.get(name.as_str())
                && sig.returns_aggregate()
            {
                return Err(format!(
                    "aggregate-returning call `{name}` is only supported in a binding or \
                     return position on the native backend"
                ));
            }
            // If the target is an `extern fn` (a C symbol), marshal it across the
            // Win64 C ABI via `emit_extern_call`: each argument is routed to the
            // register selected by its position and type (integer/pointer →
            // `rcx`/`rdx`/`r8`/`r9`; float → `xmm0..3`, §4.1). This scalar-
            // expression position consumes the return in `rax`, so a float-
            // returning extern here would be a value in `xmm0` we cannot use as an
            // `rax` result — reject it (the type checker never routes a float
            // return into an integer context, so this only guards a
            // miscompile). A narrow integer return is re-normalized in `rax`. A
            // non-extern call keeps the internal i64 convention below.
            if let Some(sig) = ctx.extern_sigs.get(name.as_str()) {
                let sig = *sig;
                match emit_extern_call(ctx, name, sig, args, code)? {
                    Some(FfiScalarClass::Int(Some(fixed))) => {
                        // The Win64 ABI leaves the upper bits of a narrow integer
                        // return undefined, so a returned `i8`/`i16`/`i32`/`u8`/
                        // `u16`/`u32` is re-normalized (sign/zero extended) so
                        // downstream Lullaby code sees the same cell the
                        // interpreters produce.
                        emit_normalize_rax(code, fixed);
                    }
                    // An `i64`/`u64`/`isize`/`usize`/pointer return already fills the
                    // cell; a `void` return leaves no value (the call is a discarded
                    // statement). Nothing to normalize in either case.
                    Some(FfiScalarClass::Int(None)) | None => {}
                    Some(FfiScalarClass::Float(_)) => {
                        return Err(format!(
                            "float-returning extern `{name}` cannot be used in an \
                             integer-scalar context on the native backend"
                        ));
                    }
                }
                return Ok(());
            }
            // A non-extern (compiled/builtin) call: stage every argument onto the
            // machine stack (a scalar value, a float word, or an aggregate-copy
            // pointer), then distribute the first four into the Win64 argument
            // registers and any 5th+ into the outgoing stack-argument area above
            // the callee's shadow space. No hidden return pointer here (this path
            // is scalar-returning).
            emit_native_call_args(ctx, name, args, None, code)?;
            // call rel32 -> relocation against the target symbol.
            code.push(0xE8);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            ctx.relocations.push(CodeRelocation {
                offset: site as u32,
                symbol: name.clone(),
            });
            Ok(())
        }
        // `s[i]` on a heap string yields the `i`-th Unicode scalar (a `char`), via
        // the UTF-8-aware char-at helper. Guarded on the string operand type so a
        // normal array index still resolves as a stack access below.
        BytecodeExprKind::Index { target, index } if is_string_type(&target.ty) => {
            lower_string_char_at(ctx, target, index, code)
        }
        // `a[i]` on a heap `array<string>` loads the `i`-th shared string pointer
        // from the `[len][cap][slot…]` block, bounds-checked against `len` (trapping
        // with `ud2` on an out-of-range index, mirroring the interpreters' `L0413`).
        BytecodeExprKind::Index { target, index }
            if heap_string_array_element(&target.ty).is_some() =>
        {
            lower_array_string_index(ctx, target, index, code)
        }
        BytecodeExprKind::Field { .. } | BytecodeExprKind::Index { .. } => {
            // A struct-field or array-index read yielding a flat word: an `i64`
            // scalar OR a `string` field (an immutable heap pointer stored in one
            // word). Both load the word into `rax` — for a string that is the record
            // pointer, ready to be used as a string value (e.g. by `len`, a return,
            // or a call argument). A float element takes the `lower_native_float_expr`
            // path, so the typed resolver rejecting it here never demotes a float
            // read (it is handled before this arm is reached).
            let (place, ty) = resolve_read_place_typed(ctx, expr)?;
            if !matches!(ty, NativeType::I64 | NativeType::String) {
                return Err(
                    "native scalar field/index read must be an i64 or string word".to_string(),
                );
            }
            emit_load_place(ctx, &place, code)
        }
        // A string literal used as a general VALUE (not just `len`'s argument):
        // materialize its `.rdata` bytes into a fresh heap string record at
        // runtime and leave the record pointer in `rax`. The `.rdata` bytes stay
        // NUL-terminated (shared with the `len("literal")` path); the
        // `__lullaby_str_lit` helper computes the char/byte headers and copies the
        // bytes into the record.
        BytecodeExprKind::String(text) => {
            let symbol = ctx.strings.intern(text);
            // lea rcx, [rip + __str] ; the 4-byte rel32 is a REL32 relocation.
            code.extend_from_slice(&[0x48, 0x8D, 0x0D]);
            let site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            ctx.relocations.push(CodeRelocation {
                offset: site as u32,
                symbol,
            });
            emit_call_symbol(ctx, STR_LIT_SYMBOL, code);
            Ok(())
        }
        // A closure literal (Stage 1): allocate the `[code_ptr][captures…]` heap
        // block, store the code pointer and captured scalars, and leave the block
        // pointer in `rax`. Only a Stage-1-supported closure (scalar captures, a
        // registered layout) lowers here; anything else was already rejected when
        // its binding local was classified, so the enclosing function skipped.
        BytecodeExprKind::Closure { id } => lower_closure_literal(ctx, *id, code),
        BytecodeExprKind::Float(_)
        | BytecodeExprKind::Array(_)
        | BytecodeExprKind::Await { .. } => {
            Err("expression is not in the native i64-scalar subset".to_string())
        }
    }
}

// -- Internal call argument ABI (registers + stack spill) --------------------
//
// A compiled Lullaby callee receives its first four **effective** arguments in
// the Win64 registers (`rcx`/`rdx`/`r8`/`r9` for integer/pointer/aggregate-copy
// pointers; `xmm0..3` positionally for floats) and its 5th+ arguments on the
// stack, pushed above the callee's 32-byte shadow space. When the callee returns
// an aggregate, its hidden result pointer consumes register 0, shifting the
// visible arguments down by one effective position. `emit_native_call_args`
// stages every visible argument onto the machine stack, then distributes each to
// its register or outgoing stack slot before the `call`.

/// Load the staged word at machine-stack offset `disp` into effective integer
/// register `pos` (`rcx`/`rdx`/`r8`/`r9`).
const GPR_ARG_INDEX: [u8; 4] = [0, 1, 2, 3];

/// Stage a call's arguments and place them into the Win64 argument registers and
/// (for a 5th+ argument) the outgoing stack area, then leave the machine stack as
/// the emitter found it so the `call` sees the reserved outgoing area intact.
///
/// `sret` is the caller-allocated destination slot when the callee returns an
/// aggregate (its address is passed as the hidden first argument, register 0),
/// otherwise `None`. A scalar argument stages its value word; a float argument
/// stages its raw float word; an aggregate argument stages a *pointer* to a fresh
/// caller-owned copy in scratch (value semantics). After staging all `n` words on
/// the stack (argument `i` at `[rsp + 8*(n-1-i)]`), the first four effective
/// positions load into registers and each later position is copied into the
/// outgoing area at `[rsp + 8*n + 32 + 8*(pos-4)]` (which becomes
/// `[rsp' + 32 + 8*(pos-4)]` once the staging words are discarded).
pub(crate) fn emit_native_call_args(
    ctx: &mut NativeCtx,
    callee: &str,
    args: &[BytecodeExpr],
    sret: Option<i32>,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // The callee's parameter layouts (when it is a compiled function) tell us
    // which arguments are aggregates or floats. An `extern`/builtin call has no
    // aggregate/float parameters on this path (guarded elsewhere), so treat a
    // missing signature as all-scalar-integer.
    let param_tys: Vec<Option<NativeType>> = match ctx.signatures.get(callee) {
        Some(sig) => sig.params.iter().map(|t| Some(t.clone())).collect(),
        None => args.iter().map(|_| None).collect(),
    };
    // Fast path: a single scalar integer/pointer argument with no hidden
    // aggregate-return pointer. Staging exists only to keep an already-placed
    // register from being clobbered while a *later* argument is evaluated — with
    // one argument there is nothing to clobber, so evaluate it straight into the
    // first argument register (`rcx`) instead of the stack round-trip.
    let single_agg_or_float = matches!(
        param_tys.first(),
        Some(Some(t)) if t.is_aggregate()
            || matches!(t, NativeType::F64 | NativeType::F32 | NativeType::FatArray { .. })
    );
    if sret.is_none() && args.len() == 1 && !single_agg_or_float {
        // `f(reg ± const)` (the recursive `fib(n - 1)` / `fib(n - 2)` idiom):
        // compute the argument with a single `lea rcx, [reg ± imm]`, exactly as C
        // does, instead of `mov rax, reg; add/sub rax, imm; mov rcx, rax`.
        if let Some((reg, disp)) = promoted_reg_plus_const(ctx, &args[0]) {
            emit_lea_rcx_reg_disp(code, reg, disp);
            return Ok(());
        }
        lower_native_expr(ctx, &args[0], code)?; // arg → rax
        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
        return Ok(());
    }
    // Stage every argument onto the machine stack as one 8-byte word, left to
    // right, so evaluating a later argument cannot clobber an already-placed
    // register. Reset the scratch cursor so each call reuses the shared region.
    let saved_scratch = ctx.scratch_next;
    for (arg, param_ty) in args.iter().zip(param_tys.iter()) {
        match param_ty {
            Some(ty) if ty.is_aggregate() => {
                // Materialize the argument aggregate into scratch, then push its
                // address (the callee copies-in from this snapshot).
                let base = ctx.alloc_scratch(ty.words());
                lower_aggregate_init(ctx, base, ty, arg, code)?;
                emit_lea_rax_slot(code, base); // rax = &scratch copy
                code.push(0x50); // push rax
            }
            Some(NativeType::FatArray { .. }) => {
                // A fat-pointer array argument: build a two-word `(data_ptr, length)`
                // descriptor in scratch and push its address (the callee copies the
                // two descriptor words in, then reads the array through the shared
                // data pointer). The data pointer is the caller's array storage, so
                // no array body is copied — value-semantically safe because the
                // callee parameter is read-only.
                let base = ctx.alloc_scratch(2);
                emit_fat_array_descriptor(ctx, base, arg, code)?;
                emit_lea_rax_slot(code, base); // rax = &descriptor
                code.push(0x50); // push rax
            }
            Some(NativeType::F64) | Some(NativeType::F32) => {
                // A float argument evaluates into `xmm0`; spill it as one raw word.
                lower_native_float_expr(ctx, arg, code)?;
                code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8
                code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x04, 0x24]); // movsd [rsp], xmm0
            }
            _ => {
                // A scalar integer/pointer argument: evaluate into rax and push it.
                lower_native_expr(ctx, arg, code)?;
                code.push(0x50); // push rax
            }
        }
    }
    ctx.scratch_next = saved_scratch;

    let n = args.len();
    let hidden = usize::from(sret.is_some());
    // Distribute each staged word to its effective position. Register positions
    // (< 4) load into the GPR/XMM chosen by position and class; stack positions
    // (>= 4) copy into the outgoing area above the shadow space.
    for (i, param_ty) in param_tys.iter().enumerate() {
        let staged_disp = 8 * (n - 1 - i) as i32; // arg i at [rsp + staged_disp]
        let pos = i + hidden;
        let is_float = matches!(param_ty, Some(NativeType::F64) | Some(NativeType::F32));
        if pos < 4 {
            if is_float {
                emit_load_xmm_from_rsp_disp(code, pos as u8, staged_disp);
            } else {
                emit_load_gpr_from_rsp_disp(code, GPR_ARG_INDEX[pos], staged_disp);
            }
        } else {
            // Copy the staged word into the outgoing stack slot. After the staging
            // words are discarded (`add rsp, 8*n`), the slot at
            // `[rsp + 8*n + 32 + 8*(pos-4)]` becomes `[rsp' + 32 + 8*(pos-4)]`,
            // exactly where the callee reads its `(pos-4)`-th stack parameter from
            // `[rbp + 16 + 8*(pos-4)]`.
            let out_disp = 8 * n as i32 + 32 + 8 * (pos as i32 - 4);
            // mov rax, [rsp + staged_disp] ; mov [rsp + out_disp], rax.
            code.extend_from_slice(&[0x48, 0x8B, 0x84, 0x24]); // mov rax, [rsp + disp32]
            code.extend_from_slice(&staged_disp.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]); // mov [rsp + disp32], rax
            code.extend_from_slice(&out_disp.to_le_bytes());
        }
    }
    // The hidden aggregate-return pointer occupies register 0 (`rcx`).
    if let Some(dest_slot) = sret {
        emit_lea_rcx_slot(code, dest_slot);
    }
    // Discard the staging words; the outgoing area and shadow space remain.
    if n > 0 {
        emit_add_rsp(code, 8 * n as i32);
    }
    Ok(())
}

/// Build a fat-pointer array **descriptor** `[data_ptr, length]` in the two scratch
/// words at `base_slot` (word 0) and `base_slot + 8` (word 1), for a fat-array call
/// argument. In this increment the argument must be a bare variable bound to a
/// **stack array local** (the common `let arr array<i64> = [..]; f(arr)` shape);
/// anything else demotes the caller gracefully. The data pointer is the address of
/// the array's element 0 (its highest stack word), so the callee reads the caller's
/// storage in place with no array-body copy.
pub(crate) fn emit_fat_array_descriptor(
    ctx: &mut NativeCtx,
    base_slot: i32,
    arg: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let BytecodeExprKind::Variable(name) = &arg.kind else {
        return Err("a fat-pointer array argument must be an array variable".to_string());
    };
    let local = ctx.local(name)?.clone();
    let NativeType::Array { len, .. } = &local.ty else {
        return Err("a fat-pointer array argument must reference a stack array local".to_string());
    };
    let len = *len as i64;
    // Descriptor word 0: data pointer = address of the array's element 0 (its
    // highest stack word, `[rbp - arr_slot]`).
    emit_lea_rax_slot(code, local.slot); // rax = rbp - arr_slot
    store_local(code, base_slot); // descriptor word 0 = data_ptr
    // Descriptor word 1: runtime length (a compile-time constant for a stack array).
    emit_mov_rax_imm(code, len);
    store_local(code, base_slot + 8); // descriptor word 1 = length
    Ok(())
}

/// The marshalling class of one C-ABI scalar crossing the FFI boundary: an
/// integer/pointer value (Win64 GPR `rcx`/`rdx`/`r8`/`r9`, an optional
/// re-normalization on a narrow return) or a float value (`f64`/`f32` in the SSE
/// registers `xmm0..3`, returned in `xmm0`). Positional routing (§4.1) chooses
/// the register for argument N by N's *position and type*: a float at position N
/// consumes `xmm N`, an integer at position N consumes integer register N, and
/// each position consumes exactly one slot in exactly one sequence.
#[derive(Clone, Copy)]
pub(crate) enum FfiScalarClass {
    /// An integer/pointer scalar; `Some(kind)` needs a narrow-return normalization
    /// in `rax`, `None` already fills the 64-bit cell (`i64`/`u64`/`isize`/`usize`).
    Int(Option<IntKind>),
    /// An `f64`/`f32` float scalar routed through the SSE registers.
    Float(FloatWidth),
}

/// One `extern fn` parameter's marshalling class across the Win64 C ABI: a scalar
/// value (integer/pointer/float, routed by [`FfiScalarClass`]), a `cstr` — a
/// Lullaby `string` the boundary materializes into a fresh NUL-terminated C buffer
/// (`__lullaby_to_cstr`) whose pointer is then routed like any pointer word — or a
/// `Callback`: a Lullaby top-level function passed to C as a C-ABI function
/// pointer. The callback's address is taken directly (`lea`, no trampoline)
/// because a fully-marshallable-signature Lullaby function already uses the Win64
/// C calling convention, and the resulting pointer word routes like any pointer
/// (§7).
#[derive(Clone, Copy)]
enum FfiParam {
    Scalar(FfiScalarClass),
    Cstr,
    Callback,
}

impl FfiParam {
    /// A `cstr`/`Callback` argument occupies a pointer word in an integer register;
    /// a scalar float occupies an SSE register. This selects the register class at a
    /// given argument position.
    fn is_float(self) -> bool {
        matches!(self, FfiParam::Scalar(FfiScalarClass::Float(_)))
    }
}

/// Whether a Lullaby type name spells a raw pointer: the modern `ptr<T>` form or
/// the legacy `ptr_T` form that `alloc` produces. A raw pointer is a single
/// machine-address word (a C `T*`) at the FFI boundary.
pub(crate) fn is_raw_pointer_type_name(name: &str) -> bool {
    name.starts_with("ptr<") || name.starts_with("ptr_")
}

/// Classify a Lullaby type name as a marshallable FFI scalar (integer, raw
/// pointer, or float), or `None` for a type outside the scalar marshalling set
/// (`string`/`list`/`map`, non-`repr(C)` structs, callbacks), which demotes the
/// extern caller so it runs on the interpreters. A raw pointer `ptr<T>` marshals
/// to a C `T*`: a 64-bit address passed/returned in a GPR with no narrow-return
/// normalization, i.e. the same class as `i64` (`Int(None)`).
pub(crate) fn ffi_scalar_class(type_name: &str) -> Option<FfiScalarClass> {
    if is_raw_pointer_type_name(type_name) {
        return Some(FfiScalarClass::Int(None));
    }
    if let Some(kind) = ffi_scalar_int_kind(type_name) {
        return Some(FfiScalarClass::Int(kind));
    }
    FloatWidth::from_type_name(type_name).map(FfiScalarClass::Float)
}

/// Marshal and emit a call to an `extern fn` C symbol across the Win64 C ABI.
/// Validates that every parameter and the return type is a marshallable scalar
/// (integer/pointer or `f64`/`f32`); stages each argument's value on the machine
/// stack; loads each argument into the register selected by its **position and
/// type** (integer/pointer → `rcx`/`rdx`/`r8`/`r9` at that position; float →
/// `xmm0..3` at that position, §4.1); then emits the `call rel32` relocation. An
/// integer return is left in `rax` (the caller re-normalizes a narrow width); a
/// float return is left in `xmm0`. Returns the return-value class so the caller
/// can finish the return normalization for its result context.
///
/// There is no fixed argument-count cap: the first four arguments use the Win64
/// argument registers and the 5th+ spill onto the stack above the callee's 32-byte
/// shadow space (the outgoing area the frame reserved). A non-marshallable
/// parameter/return type demotes the caller gracefully.
pub(crate) fn emit_extern_call(
    ctx: &mut NativeCtx,
    name: &str,
    sig: &crate::IrExternSignature,
    args: &[BytecodeExpr],
    code: &mut Vec<u8>,
) -> Result<Option<FfiScalarClass>, String> {
    if args.len() != sig.params.len() {
        return Err(format!(
            "extern `{name}` expects {} argument(s) but got {}",
            sig.params.len(),
            args.len()
        ));
    }
    // Classify each parameter and the return type. Any non-marshallable type
    // demotes the caller to the interpreters (which reject the extern call with
    // `L0423`). A `cstr` parameter materializes a NUL-terminated C buffer from a
    // Lullaby `string`; a raw pointer / scalar routes by its Win64 register class.
    let param_classes: Vec<FfiParam> = sig
        .params
        .iter()
        .map(|param_ty| {
            if param_ty.name == "cstr" {
                return Ok(FfiParam::Cstr);
            }
            // A callback (function-pointer) parameter `fn(A...) -> R`: a Lullaby
            // top-level function is passed to C as a C-ABI function pointer. The
            // semantic checker already guaranteed the callback's own signature is
            // C-marshallable (`is_marshallable_callback`), so here we only route the
            // resulting pointer word; the argument-side address-of is emitted below.
            if param_ty.is_function() {
                return Ok(FfiParam::Callback);
            }
            ffi_scalar_class(&param_ty.name)
                .map(FfiParam::Scalar)
                .ok_or_else(|| {
                    format!(
                        "extern `{name}` parameter type `{}` is not a native FFI \
                         parameter (aggregates are deferred)",
                        param_ty.name
                    )
                })
        })
        .collect::<Result<_, _>>()?;
    // A `cstr` cannot be *returned* (an inbound C string is received as `ptr<byte>`
    // and copied explicitly), so the return type is `void` (no value) or a plain
    // FFI scalar/pointer.
    if sig.return_type.name == "cstr" {
        return Err(format!(
            "extern `{name}` returns `cstr`; an inbound C string must be typed \
             `ptr<byte>` (owned-string conversion is deferred)"
        ));
    }
    let return_class = if sig.return_type.is_void() {
        None
    } else {
        Some(ffi_scalar_class(&sig.return_type.name).ok_or_else(|| {
            format!(
                "extern `{name}` return type `{}` is not a native FFI scalar/pointer \
                 (aggregates/strings are deferred)",
                sig.return_type.name
            )
        })?)
    };

    // Stage each argument onto the machine stack as one 8-byte word, left to
    // right. An integer/pointer argument evaluates into `rax` and is `push`ed; a
    // float argument evaluates into `xmm0` and is spilled into a reserved 8-byte
    // word; a `cstr` argument evaluates its `string` into a record pointer, then
    // `__lullaby_to_cstr` materializes a NUL-terminated buffer whose pointer is
    // pushed. After the loop, argument at position i sits at `[rsp + 8*(n-1-i)]`
    // (the first-pushed argument is deepest). Staging first, then loading
    // registers, avoids one argument's evaluation clobbering an already-loaded
    // register.
    for (arg, class) in args.iter().zip(param_classes.iter()) {
        match class {
            FfiParam::Scalar(FfiScalarClass::Int(_)) => {
                lower_native_expr(ctx, arg, code)?;
                code.push(0x50); // push rax
            }
            FfiParam::Scalar(FfiScalarClass::Float(_)) => {
                lower_native_float_expr(ctx, arg, code)?;
                // sub rsp, 8 ; movsd [rsp], xmm0  (spill one 8-byte float word).
                code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]);
                code.extend_from_slice(&[0xF2, 0x0F, 0x11, 0x04, 0x24]);
            }
            FfiParam::Cstr => {
                // Evaluate the `string` argument into a heap record pointer, then
                // materialize a fresh NUL-terminated UTF-8 buffer. The helper only
                // calls the leaf bump allocator, so it tolerates the mid-staging
                // (possibly unaligned) `rsp`; the real C `call` below is realigned.
                lower_native_expr(ctx, arg, code)?; // rax = string record ptr
                code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
                emit_call_symbol(ctx, TO_CSTR_SYMBOL, code); // rax = C buffer ptr
                code.push(0x50); // push rax
            }
            FfiParam::Callback => {
                // A callback argument must be a **bare top-level function name**: a
                // compiled Lullaby function whose `.text` symbol we take the address
                // of (`lea rax, [rip + fn]`, a REL32 relocation) and pass as a C
                // function pointer. This is sound with no trampoline because a
                // fully-marshallable-signature Lullaby function already uses the
                // Win64 C calling convention (integer args in rcx/rdx/r8/r9, floats
                // in xmm0..3 positionally, result in rax/xmm0, callee-saved rbx/rsi/
                // rbp saved+restored), so C can call it directly (§7).
                //
                // Anything else — a closure, a `fn`-typed local/parameter holding a
                // runtime pointer, or an unknown name — is not an address-takeable
                // top-level function, so the whole extern caller demotes cleanly to
                // the interpreters (which reject the extern call with `L0423`)
                // rather than emitting a wrong pointer.
                let BytecodeExprKind::Variable(fname) = &arg.kind else {
                    return Err(format!(
                        "extern `{name}` callback argument must be a top-level function name \
                         (closures / function-valued locals are deferred)"
                    ));
                };
                if ctx.locals.contains_key(fname) || !ctx.callable.contains(fname.as_str()) {
                    return Err(format!(
                        "extern `{name}` callback argument `{fname}` is not a compiled \
                         top-level function (only a non-capturing top-level function can be \
                         passed to C as a callback)"
                    ));
                }
                // lea rax, [rip + fname] ; the 4-byte rel32 is a REL32 relocation
                // against the callee's `.text` symbol (same model as a `call`).
                code.extend_from_slice(&[0x48, 0x8D, 0x05]);
                let site = code.len();
                code.extend_from_slice(&[0, 0, 0, 0]);
                ctx.relocations.push(CodeRelocation {
                    offset: site as u32,
                    symbol: fname.clone(),
                });
                code.push(0x50); // push rax
            }
        }
    }
    let n = args.len();
    // Distribute each staged word to its Win64 position. Positions 0..4 load into
    // the argument register selected by position and class (GPR for int/pointer/
    // cstr, XMM for float); position 4+ copies into the outgoing stack-argument
    // area above the 32-byte shadow space, exactly where the callee reads it. The
    // staged words are still on the stack here (they are discarded after), so a
    // stack slot sits at `[rsp + 8*n + 32 + 8*(pos-4)]`.
    for (i, class) in param_classes.iter().enumerate() {
        let staged_disp = 8 * (n - 1 - i) as i32;
        if i < 4 {
            if class.is_float() {
                emit_load_xmm_from_rsp_disp(code, i as u8, staged_disp);
            } else {
                emit_load_gpr_from_rsp_disp(code, i as u8, staged_disp);
            }
        } else {
            let out_disp = 8 * n as i32 + 32 + 8 * (i as i32 - 4);
            // mov rax, [rsp + staged_disp] ; mov [rsp + out_disp], rax.
            code.extend_from_slice(&[0x48, 0x8B, 0x84, 0x24]); // mov rax, [rsp + disp32]
            code.extend_from_slice(&staged_disp.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x89, 0x84, 0x24]); // mov [rsp + disp32], rax
            code.extend_from_slice(&out_disp.to_le_bytes());
        }
    }
    // Discard the staged words: add rsp, 8*n. `rsp` returns to the frame's
    // call-ready position (16-byte aligned, 32-byte shadow + outgoing area below).
    if n > 0 {
        emit_add_rsp(code, 8 * n as i32);
    }
    // call rel32 -> relocation against the (undefined external) C symbol.
    code.push(0xE8);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: name.to_string(),
    });
    Ok(return_class)
}

/// Emit `add rsp, imm` (imm > 0). Uses the imm8 form when it fits, else imm32.
pub(crate) fn emit_add_rsp(code: &mut Vec<u8>, amount: i32) {
    if (0..=127).contains(&amount) {
        code.extend_from_slice(&[0x48, 0x83, 0xC4, amount as u8]);
    } else {
        code.extend_from_slice(&[0x48, 0x81, 0xC4]);
        code.extend_from_slice(&amount.to_le_bytes());
    }
}

/// Lower a call to a compiled function that returns an aggregate, writing the
/// result into the words at `dest_slot`. The caller-allocated destination address
/// is passed as the hidden first integer argument (`rcx`); the visible arguments
/// (scalar values / aggregate-copy pointers) follow in `rdx`/`r8`/`r9`. The callee
/// writes the result through the hidden pointer, so after the call `dest_slot`
/// holds the returned aggregate.
pub(crate) fn lower_aggregate_returning_call(
    ctx: &mut NativeCtx,
    dest_slot: i32,
    ty: &NativeType,
    value: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let BytecodeExprKind::Call { name, args } = &value.kind else {
        return Err("expected a call expression".to_string());
    };
    let sig = ctx
        .signatures
        .get(name.as_str())
        .ok_or_else(|| format!("call target `{name}` has no native signature"))?;
    if !sig.returns_aggregate() {
        return Err(format!("call `{name}` does not return an aggregate"));
    }
    // The callee writes `sig.ret.words()` words into the destination; the caller's
    // destination must reserve at least that many. (An enum `match` scrutinee
    // constructs an equivalent layout with a synthetic name, so compare by word
    // count rather than exact type equality.)
    if sig.ret.words() != ty.words() {
        return Err(format!(
            "call `{name}` return layout ({} words) does not match the destination ({} words)",
            sig.ret.words(),
            ty.words()
        ));
    }
    // Stage the visible arguments and distribute them past the hidden return
    // pointer: the pointer consumes register 0 (`rcx`), the visible args follow in
    // `rdx`/`r8`/`r9` and then the outgoing stack area for a 5th+ effective arg.
    emit_native_call_args(ctx, name, args, Some(dest_slot), code)?;
    // call rel32 -> relocation against the callee.
    code.push(0xE8);
    let site = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);
    ctx.relocations.push(CodeRelocation {
        offset: site as u32,
        symbol: name.clone(),
    });
    // The callee wrote the result words into `[rcx]` == `dest_slot`; nothing more
    // to copy.
    Ok(())
}

/// Lower a `return <aggregate>` (or an aggregate tail expression): materialize the
/// value into scratch, then copy its words through the hidden return pointer and
/// leave that pointer in `rax` (the by-pointer return convention). A direct
/// aggregate-returning call is special-cased to write straight into the hidden
/// pointer's destination (no scratch round-trip).
pub(crate) fn lower_aggregate_return(
    ctx: &mut NativeCtx,
    expr: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    let sret_slot = ctx
        .sret_slot
        .ok_or("aggregate return without a hidden result pointer")?;
    let ty = ctx.return_ty.clone();
    // Materialize into a scratch region, then copy the words to `[sret]`. Using
    // scratch (rather than writing straight through the pointer) keeps the
    // materialization code — which addresses `[rbp - slot]` frame slots — reusable
    // for every initializer shape (constructor, literal, local copy, call).
    let saved_scratch = ctx.scratch_next;
    let base = ctx.alloc_scratch(ty.words());
    lower_aggregate_init(ctx, base, &ty, expr, code)?;
    // rax = hidden result pointer (the caller-allocated destination, addressing
    // word 0). Aggregate words descend in memory, so word k is written at
    // `[rax - 8*k]`, matching the destination's `[rbp - (slot + 8*k)]` layout.
    emit_mov_rax_from_slot(code, sret_slot);
    // Copy each word: rcx = [rbp - (base + 8k)]; [rax - 8k] = rcx.
    for word in 0..ty.words() as i32 {
        emit_mov_rcx_from_slot(code, base + word * 8);
        emit_mov_rax_disp_from_rcx(code, -word * 8);
    }
    ctx.scratch_next = saved_scratch;
    // Per the ABI, an aggregate return leaves the result pointer in rax.
    emit_mov_rax_from_slot(code, sret_slot);
    Ok(())
}

/// `lea rax, [rbp - slot]` — the effective address of a frame slot.
pub(crate) fn emit_lea_rax_slot(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8D, 0x85]); // lea rax, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `lea rcx, [rbp - slot]` — the effective address of a frame slot.
pub(crate) fn emit_lea_rcx_slot(code: &mut Vec<u8>, slot: i32) {
    code.extend_from_slice(&[0x48, 0x8D, 0x8D]); // lea rcx, [rbp + disp32]
    code.extend_from_slice(&(-slot).to_le_bytes());
}

/// `mov rax, [rbp - slot]`.
pub(crate) fn emit_mov_rax_from_slot(code: &mut Vec<u8>, slot: i32) {
    load_local(code, slot);
}

/// Lower a binary expression. `and`/`or` short-circuit; other operators evaluate
/// left (pushed), right (in rax), then combine popping the left back.
pub(crate) fn lower_native_binary(
    ctx: &mut NativeCtx,
    left: &BytecodeExpr,
    op: BinaryOp,
    right: &BytecodeExpr,
    code: &mut Vec<u8>,
) -> Result<(), String> {
    // String ordering (`< <= > >=`) is lexicographic by content; the scalar
    // backend would compare heap pointers, so defer the whole function to the
    // interpreters (which compare correctly). Concatenation `+` and equality are
    // handled by their own paths.
    if matches!(
        op,
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual
    ) && (left.ty.name == "string" || right.ty.name == "string")
    {
        return Err(
            "string ordering comparison is not supported on the native backend".to_string(),
        );
    }
    match op {
        BinaryOp::And => {
            // rax = left ? (right != 0 ? 1 : 0) : 0
            lower_native_expr(ctx, left, code)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.extend_from_slice(&[0x0F, 0x84]); // jz false (patched)
            let false_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            lower_native_expr(ctx, right, code)?;
            normalize_bool(code); // rax = (rax != 0) ? 1 : 0
            code.push(0xE9); // jmp done
            let done_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            patch_rel32(code, false_site);
            code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
            patch_rel32(code, done_site);
            Ok(())
        }
        BinaryOp::Or => {
            // rax = left ? 1 : (right != 0 ? 1 : 0)
            lower_native_expr(ctx, left, code)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.extend_from_slice(&[0x0F, 0x85]); // jnz true (patched)
            let true_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            lower_native_expr(ctx, right, code)?;
            normalize_bool(code);
            code.push(0xE9); // jmp done
            let done_site = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);
            patch_rel32(code, true_site);
            emit_mov_rax_imm(code, 1);
            patch_rel32(code, done_site);
            Ok(())
        }
        // String concatenation: `a + b` on two strings allocates a fresh record
        // and byte-copies both operands' UTF-8 ranges. Detected by the result type
        // being `string` (the type checker only allows `+` between two strings).
        BinaryOp::Add if is_string_type(&left.ty) && is_string_type(&right.ty) => {
            lower_string_concat(ctx, left, right, code)
        }
        _ => {
            // A float comparison (`f64`/`f32` operands, ordered relational or
            // equality op) produces an `i64`/`bool` result in rax via SSE compare
            // + set. Float *arithmetic* never reaches here in an i64-return
            // function (it flows through the float-expr lowerer), so only
            // comparisons need handling on this i64-producing path.
            //
            // Detect float-ness from the operands' structure rather than the
            // comparison node's own `ty`: the IR annotates an arithmetic float
            // node (e.g. `a + b`) with `i64`, so the leaf-derived width is the
            // reliable signal (float literals, float locals, and `to_f32`/`to_f64`
            // conversions all carry a correct concrete float type).
            if let Some(width) =
                float_width_of_expr(ctx, left).or_else(|| float_width_of_expr(ctx, right))
            {
                return lower_native_float_compare(ctx, left, op, right, width, code);
            }
            // Strength-reduce `*` by a constant on plain `i64`: a power of two
            // becomes `shl` and 3/5/9 a single `lea [x + x*scale]` (exactly as C
            // does), each 1-cycle vs `imul`'s 3-cycle latency — a win on a
            // dependency chain. All keep the low 64 bits (wrapping). Other
            // constants fall through to the immediate `imul` below.
            if fixed_int_kind(left.ty.name.as_str()).is_none()
                && op == BinaryOp::Multiply
                && let BytecodeExprKind::Integer(m) = &right.kind
            {
                if *m >= 2 && (*m & (*m - 1)) == 0 {
                    let k = m.trailing_zeros() as u8; // 1..=62 for a positive i64
                    lower_native_expr(ctx, left, code)?; // x in rax
                    code.extend_from_slice(&[0x48, 0xC1, 0xE0, k]); // shl rax, k
                    return Ok(());
                }
                // `lea rax, [rax + rax*scale]`: 3 (scale 2), 5 (scale 4), 9 (scale 8).
                if let Some(sib) = match *m {
                    3 => Some(0x40u8),
                    5 => Some(0x80),
                    9 => Some(0xC0),
                    _ => None,
                } {
                    lower_native_expr(ctx, left, code)?; // x in rax
                    code.extend_from_slice(&[0x48, 0x8D, 0x04, sib]); // lea rax, [rax + rax*scale]
                    return Ok(());
                }
            }
            // Constant right operand on a plain `i64` add/sub/mul: fold into an
            // immediate (`add`/`sub rax, imm32`, or `imul rax, rax, imm32`),
            // skipping the operand-stack shuffle. x86 `add`/`sub`/`imul` keep the
            // low 64 bits, matching the interpreters' wrapping arithmetic. Only for
            // plain `i64` (fixed-width kinds need width re-normalization) with an
            // i32-range immediate; anything else uses the general path below.
            if fixed_int_kind(left.ty.name.as_str()).is_none()
                && let BytecodeExprKind::Integer(rhs) = &right.kind
                && let Ok(imm) = i32::try_from(*rhs)
            {
                let opcode: Option<&[u8]> = match op {
                    BinaryOp::Add => Some(&[0x48, 0x05]),      // add rax, imm32
                    BinaryOp::Subtract => Some(&[0x48, 0x2D]), // sub rax, imm32
                    BinaryOp::Multiply => Some(&[0x48, 0x69, 0xC0]), // imul rax, rax, imm32
                    _ => None,
                };
                if let Some(prefix) = opcode {
                    lower_native_expr(ctx, left, code)?; // left in rax
                    code.extend_from_slice(prefix);
                    code.extend_from_slice(&imm.to_le_bytes());
                    return Ok(());
                }
            }
            // Strength-reduce `/` and `%` by a positive power-of-two constant on
            // plain `i64` into shifts, avoiding the ~20-40 cycle `idiv` (exactly
            // as C does). The sign-bias `(x + (x>>63 >>> (64-k)))` makes the
            // arithmetic shift round toward zero, matching `wrapping_div`/
            // `wrapping_rem` bit-for-bit (including `i64::MIN`). Non-power-of-two
            // divisors and the fixed-width kinds fall through to `idiv`.
            if fixed_int_kind(left.ty.name.as_str()).is_none()
                && matches!(op, BinaryOp::Divide | BinaryOp::Remainder)
                && let BytecodeExprKind::Integer(divisor) = &right.kind
                && *divisor >= 2
                && (*divisor & (*divisor - 1)) == 0
            {
                let k = divisor.trailing_zeros() as u8; // 1..=62 for a positive i64
                lower_native_expr(ctx, left, code)?; // x in rax
                match op {
                    BinaryOp::Divide => {
                        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax
                        code.extend_from_slice(&[0x48, 0xC1, 0xF9, 63]); // sar rcx, 63
                        code.extend_from_slice(&[0x48, 0xC1, 0xE9, 64 - k]); // shr rcx, 64-k
                        code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx
                        code.extend_from_slice(&[0x48, 0xC1, 0xF8, k]); // sar rax, k
                    }
                    BinaryOp::Remainder => {
                        // rem = x - (x / 2^k) * 2^k, reusing the same rounded quotient.
                        code.extend_from_slice(&[0x48, 0x89, 0xC1]); // mov rcx, rax (save x)
                        code.extend_from_slice(&[0x48, 0x89, 0xC2]); // mov rdx, rax
                        code.extend_from_slice(&[0x48, 0xC1, 0xFA, 63]); // sar rdx, 63
                        code.extend_from_slice(&[0x48, 0xC1, 0xEA, 64 - k]); // shr rdx, 64-k
                        code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx
                        code.extend_from_slice(&[0x48, 0xC1, 0xF8, k]); // sar rax, k
                        code.extend_from_slice(&[0x48, 0xC1, 0xE0, k]); // shl rax, k
                        code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
                        code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
                    }
                    _ => unreachable!(),
                }
                return Ok(());
            }
            lower_native_expr(ctx, left, code)?;
            code.push(0x50); // push rax (left)
            lower_native_expr(ctx, right, code)?; // right in rax
            // A fixed-width operand kind (both operands share it; the type checker
            // forbids mixing widths) selects width- and signedness-correct codegen
            // that re-normalizes the result. Plain `i64` uses the full-width path.
            match fixed_int_kind(left.ty.name.as_str()) {
                Some(kind) => emit_fixed_binop_from_stack(code, op, kind),
                None => emit_i64_binop_from_stack(code, op),
            }
        }
    }
}
