# Numeric Type Completeness Design

Canonical language rules: see [core_language_rules.md](core_language_rules.md). This design
covers the full scalar numeric lattice required for Lullaby 1.0 (see
[roadmap_1_0.md](roadmap_1_0.md), Phase 1). It is the linchpin of primitive completeness:
systems code, binary protocols, graphics, and FFI all require precise fixed-width integers and
`f32`. Related: [[generics_design.md]] (runtime type erasure precedent), the FFI marshalling
rules in [[ffi_design.md]], and the bitwise operators work.

## Types

Add the complete fixed-width scalar set alongside the existing `i64`/`f64`:

- Signed integers: `i8`, `i16`, `i32`, `i64`.
- Unsigned integers: `u8`, `u16`, `u32`, `u64`.
- Pointer-sized: `usize`, `isize` (target-width; 64-bit on the current targets).
- Floats: `f32`, `f64`.

`byte` becomes a documented alias of `u8` (its existing 0–255 semantics are exactly `u8`), and
`char` remains a distinct Unicode-scalar type (not an integer). This keeps existing programs
valid while unifying byte handling with the integer lattice.

## Runtime representation (parity strategy)

The dynamic backends (AST runtime, IR interpreter, bytecode VM) **erase integer width at run
time**, exactly as generics are erased: every integer value is stored in a single
`Value::Int(i64)` (the existing `Value::I64`, kept as the storage cell) and every float in
`Value::F64`, with `f32` values stored as an `f64` that is *rounded to `f32` precision* after
each `f32`-producing operation. The **static type checker** is the source of truth for widths;
the interpreters apply **width normalization** (masking/truncation for ints, `f32` rounding for
floats) after each operation so that observable results match a real fixed-width machine.

This choice keeps the three interpreters light and identical, avoids a combinatorial explosion of
`Value` variants, and matches the existing erasure philosophy. The **native backend** uses real
machine widths (a 1-byte `u8`, a 4-byte `i32`, etc.); the **WASM backend** uses `i32`/`i64` with
the same normalization the interpreters apply. All four must agree bit-for-bit — enforced by the
parity harness.

### Normalization rules

For an integer type `T` of width `w` bits and signedness `s`, after any operation that can leave
the mathematical result outside `T`'s range, normalize the stored `i64` by:

- **Unsigned:** keep the low `w` bits (`value & mask(w)`), then zero-extend into the `i64` cell.
- **Signed:** keep the low `w` bits, then sign-extend from bit `w-1` into the `i64` cell.

`i64`/`u64` use the full cell (`u64` values above `i64::MAX` are stored via `as i64`
reinterpretation and printed/compared using their unsigned interpretation — the type tag decides).
`f32` normalization is `(x as f32) as f64` after every `f32` operation and on every `f32` literal.

## Literals and inference

- An unsuffixed integer literal defaults to `i64`; an unsuffixed float literal defaults to `f64`
  — preserving all current programs.
- **Typed suffixes** let any scalar be written directly: `0u8`, `255u8`, `-1i32`, `1_000u32`,
  `0xFFu16`, `3.5f32`, `2.0f64`. The lexer already scans suffix characters into the number token
  (they are alphanumeric); the parser recognizes a trailing type suffix, checks the literal is in
  range for that type (out-of-range is a parse/semantic error), and strips it before conversion.
- Context can also pin an unsuffixed literal: `let x u8 = 200` type-checks (200 fits `u8`), and a
  literal that does not fit its annotated/context type is rejected.

## Conversions and casts

No implicit numeric coercion (keeping the existing "`i64` and `f64` do not mix" rule, generalized
to the whole lattice). Explicit conversion is via a `convert` builtin family (or a `to_T` set),
production-specified per pair:

- **Integer → wider/narrower integer:** `convert(x, T)` truncates (narrowing) or sign/zero-extends
  (widening) deterministically per the normalization rules.
- **Integer ↔ float:** round-to-nearest for int→float; truncation toward zero for float→int, with
  saturation at the target bounds (never UB; a documented, total function).
- **Same-width signed/unsigned reinterpret:** bit-preserving.

The exact builtin surface (one generic `convert`/`as` operator vs. named `to_i32`/`to_u8`/… ) is
decided in the implementation ticket; whichever is chosen must be total and identical across
backends.

## Arithmetic and overflow

Default arithmetic (`+ - * /`) on a fixed-width integer **wraps** modulo the type width (the
deterministic, total default, matching the normalization rules — no hidden traps, no UB). In
addition, provide explicit variants for the cases where wrapping is wrong:

- **Checked:** `checked_add(a, b) -> option<T>` (etc.) — `none` on overflow.
- **Saturating:** `saturating_add(a, b) -> T` — clamps to `T`'s bounds.
- **Wrapping:** `wrapping_add(a, b) -> T` — explicit form of the default.

Division/remainder by zero remains the existing runtime error; signed `MIN / -1` overflow wraps
(consistent with the wrapping default) rather than trapping. `f32`/`f64` follow IEEE-754
(NaN/inf), already the case for `f64`.

Bitwise operators (separate ticket) extend to every integer width, using each type's width for
shift masking (`amount & (w-1)`) and `u`-types using logical (zero-filling) right shift while
`i`-types use arithmetic right shift.

## Operator type rules

Binary numeric operators require **both operands to have the same numeric type** (as today) and
produce that type; there is no implicit widening. Comparisons produce `bool`. `usize`/`isize`
participate as ordinary integer types. The type checker reports an operand-type mismatch with the
existing arithmetic-operand diagnostic family, extended to name the specific widths.

## Diagnostics

Reuse the existing numeric operand/conversion diagnostic codes where possible; new conditions to
cover (propose codes in the registry during implementation, do not duplicate): literal out of
range for its (suffixed/annotated) type; conversion target not a numeric type; mixed-width binary
operands.

## Backend parity and testing

A `run_numeric_types.lby` parity fixture must exercise: each width's wrap-around at its boundary,
signed vs unsigned right shift and comparison, `f32` rounding vs `f64`, checked/saturating/wrapping
variants, suffixed literals, and conversions in both directions — returning a deterministic `i64`
identical on AST/IR/bytecode (and, once available, native/WASM). Width normalization must be
applied at exactly the same points on every backend.

## Scope and sequencing

1. Type system: the lattice, `byte = u8` alias, literal suffixes + range checking, operator rules.
2. Runtime/IR/bytecode: the erased representation + normalization after each op; wrapping default.
3. Conversions + checked/saturating/wrapping builtins.
4. Bitwise operators extended to all widths (coordinated with the bitwise ticket).
5. Native + WASM backends adopt real/normalized widths at parity.

Deferred beyond 1.0: 128-bit integers, decimal/fixed-point types, and SIMD vector types (these
are specialized modules, not core primitives).

## Why these choices

- **Erased runtime width + static enforcement** keeps the interpreters simple and identical while
  the type checker guarantees correctness — the same lever that made generics free.
- **Wrapping default + explicit checked/saturating** is total and deterministic (no UB, no hidden
  panics) and gives systems programmers the exact control they need.
- **No implicit coercion** preserves Lullaby's existing strictness and makes FFI marshalling and
  binary-protocol code unambiguous.
- **`byte = u8`** unifies the byte story with the integer lattice without breaking existing code.
