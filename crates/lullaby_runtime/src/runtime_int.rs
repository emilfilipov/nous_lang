//! The fixed-width integer lattice, signedness-aware integer operations, the
//! overflow-mode arithmetic builtins, and small integer helpers. Split out of
//! `lib.rs` as a behavior-preserving code move; `Value`, `RuntimeError`, and
//! `option_value` (in `lib.rs`) are reached through `crate::` paths.

use crate::{RuntimeError, Value, option_value};

/// Width/signedness tag for the fixed-width integer lattice carried by
/// [`Value::Int`]. The stored `i64` cell is always kept normalized to the kind's
/// range (truncate to width, then sign- or zero-extend). Signed kinds sit in
/// their signed range so plain `i64` ordering is correct; unsigned kinds are
/// zero-extended, so the ≤32-bit ones also order as `i64`. The 64-bit unsigned
/// kinds (`u64`/`usize`) can hold values above `i64::MAX`, stored bit-for-bit as
/// a negative `i64`, so division and ordering consult [`IntKind::is_unsigned`]
/// and operate on the `u64` reinterpretation. Every dynamic backend (AST
/// runtime, IR interpreter, bytecode VM) normalizes at the same points so
/// results agree bit-for-bit; `usize`/`isize` are 64-bit on the current targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntKind {
    /// Signed 8-bit.
    I8,
    /// Signed 16-bit.
    I16,
    /// Signed 32-bit.
    I32,
    /// Unsigned 8-bit. Distinct from `byte` (the raw-I/O octet): `u8` arithmetic
    /// wraps, whereas `byte()` construction errors outside 0-255.
    U8,
    /// Unsigned 16-bit.
    U16,
    /// Unsigned 32-bit.
    U32,
    /// Unsigned 64-bit.
    U64,
    /// Pointer-sized signed (64-bit on the current targets).
    Isize,
    /// Pointer-sized unsigned (64-bit on the current targets).
    Usize,
}

impl IntKind {
    /// The canonical Lullaby type name for this integer kind.
    pub fn type_name(self) -> &'static str {
        match self {
            IntKind::I8 => "i8",
            IntKind::I16 => "i16",
            IntKind::I32 => "i32",
            IntKind::U8 => "u8",
            IntKind::U16 => "u16",
            IntKind::U32 => "u32",
            IntKind::U64 => "u64",
            IntKind::Isize => "isize",
            IntKind::Usize => "usize",
        }
    }

    /// The bit width of this kind (8/16/32/64). `usize`/`isize` are 64-bit on the
    /// current targets. Used for shift-amount masking.
    pub fn width_bits(self) -> u32 {
        match self {
            IntKind::I8 | IntKind::U8 => 8,
            IntKind::I16 | IntKind::U16 => 16,
            IntKind::I32 | IntKind::U32 => 32,
            IntKind::U64 | IntKind::Isize | IntKind::Usize => 64,
        }
    }

    /// Whether this kind is unsigned. Division and ordering of unsigned kinds use
    /// the `u64` reinterpretation of the normalized cell.
    pub fn is_unsigned(self) -> bool {
        matches!(
            self,
            IntKind::U8 | IntKind::U16 | IntKind::U32 | IntKind::U64 | IntKind::Usize
        )
    }

    /// Normalize a mathematical `i64` result into this kind's range: truncate to
    /// the kind's width, then sign-extend (signed kinds) or zero-extend
    /// (unsigned kinds) back into the `i64` cell. Total and deterministic — this
    /// is the wrapping default shared by every backend. The 64-bit kinds occupy
    /// the whole cell, so normalization is the identity on the bits.
    pub fn normalize(self, value: i64) -> i64 {
        match self {
            IntKind::I8 => i64::from(value as i8),
            IntKind::I16 => i64::from(value as i16),
            IntKind::I32 => i64::from(value as i32),
            IntKind::U8 => i64::from(value as u8),
            IntKind::U16 => i64::from(value as u16),
            IntKind::U32 => i64::from(value as u32),
            IntKind::U64 | IntKind::Usize | IntKind::Isize => value,
        }
    }

    /// The inclusive `[min, max]` range of this kind as `i128`, wide enough to
    /// hold every kind (up to `u64::MAX`). Used by checked/saturating arithmetic
    /// to detect and clamp overflow exactly.
    pub fn range_i128(self) -> (i128, i128) {
        match self {
            IntKind::I8 => (i128::from(i8::MIN), i128::from(i8::MAX)),
            IntKind::I16 => (i128::from(i16::MIN), i128::from(i16::MAX)),
            IntKind::I32 => (i128::from(i32::MIN), i128::from(i32::MAX)),
            IntKind::Isize => (i128::from(i64::MIN), i128::from(i64::MAX)),
            IntKind::U8 => (0, i128::from(u8::MAX)),
            IntKind::U16 => (0, i128::from(u16::MAX)),
            IntKind::U32 => (0, i128::from(u32::MAX)),
            IntKind::U64 | IntKind::Usize => (0, i128::from(u64::MAX)),
        }
    }

    /// The mathematical value of a normalized `i64` cell of this kind as `i128`
    /// (unsigned kinds read the cell as `u64`, so a negative cell becomes its
    /// large positive magnitude).
    pub fn value_to_i128(self, cell: i64) -> i128 {
        if self.is_unsigned() {
            i128::from(cell as u64)
        } else {
            i128::from(cell)
        }
    }

    /// Pack an in-range `i128` value back into this kind's `i64` cell (the
    /// inverse of [`IntKind::value_to_i128`] for values within `range_i128`).
    pub fn i128_to_cell(self, value: i128) -> i64 {
        if self.is_unsigned() {
            value as u64 as i64
        } else {
            value as i64
        }
    }
}

/// The three arithmetic operations that the overflow-aware builtins provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
}

/// Signedness-aware quotient of two normalized `i64` cells tagged `ty`; the
/// caller guarantees a non-zero divisor. Unsigned kinds divide on the `u64`
/// reinterpretation so 64-bit unsigned values above `i64::MAX` divide correctly.
pub fn int_div(left: i64, right: i64, ty: IntKind) -> i64 {
    if ty.is_unsigned() {
        (left as u64).wrapping_div(right as u64) as i64
    } else {
        left.wrapping_div(right)
    }
}

/// Signedness-aware remainder of two normalized `i64` cells tagged `ty`; the
/// caller guarantees a non-zero divisor. Signed kinds use the truncated
/// remainder (sign of the dividend, like C/Rust `%`); unsigned kinds take the
/// remainder on the `u64` reinterpretation. The result magnitude is smaller than
/// the divisor, so it is already normalized to the kind's width.
pub fn int_rem(left: i64, right: i64, ty: IntKind) -> i64 {
    if ty.is_unsigned() {
        (left as u64).wrapping_rem(right as u64) as i64
    } else {
        left.wrapping_rem(right)
    }
}

/// Left shift of a normalized fixed-width cell, with the shift amount masked to
/// the kind's width (`amount & (width-1)` — total and deterministic, like the
/// `i64` shift) and the result re-normalized to the width.
pub fn int_shl(value: i64, amount: i64, ty: IntKind) -> i64 {
    let masked = (amount as u64 & u64::from(ty.width_bits() - 1)) as u32;
    ty.normalize(value.wrapping_shl(masked))
}

/// Right shift of a normalized fixed-width cell: logical (zero-filling) for
/// unsigned kinds, arithmetic (sign-preserving) for signed kinds, with the same
/// masked amount as [`int_shl`].
pub fn int_shr(value: i64, amount: i64, ty: IntKind) -> i64 {
    let masked = (amount as u64 & u64::from(ty.width_bits() - 1)) as u32;
    let shifted = if ty.is_unsigned() {
        (value as u64).wrapping_shr(masked) as i64
    } else {
        value.wrapping_shr(masked)
    };
    ty.normalize(shifted)
}

/// Signedness-aware ordering of two normalized `i64` cells tagged `ty`. Unsigned
/// kinds compare on the `u64` reinterpretation (correct for the 64-bit unsigned
/// kinds whose cells may be negative `i64`s); signed kinds compare as `i64`.
pub fn int_cmp(left: i64, right: i64, ty: IntKind) -> std::cmp::Ordering {
    if ty.is_unsigned() {
        (left as u64).cmp(&(right as u64))
    } else {
        left.cmp(&right)
    }
}

/// Overflow behaviour selector for the `checked_*`/`saturating_*`/`wrapping_*`
/// arithmetic builtins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowMode {
    /// `option<T>`: `none` when the true result is outside `T`.
    Checked,
    /// `T`: clamp to `T`'s bounds.
    Saturating,
    /// `T`: wrap modulo the type width (the default `+`/`-`/`*` behaviour).
    Wrapping,
}

/// Unwrap a `Value::Int`, returning its normalized cell and kind, or an `L0407`
/// runtime error otherwise.
pub fn expect_fixed_int(name: &str, value: &Value) -> Result<(i64, IntKind), RuntimeError> {
    match value {
        Value::Int { value, ty } => Ok((*value, *ty)),
        other => Err(RuntimeError::new(
            "L0407",
            format!("{name} expects a fixed-width integer but got `{other}`"),
        )),
    }
}

/// Shared implementation of the overflow-aware arithmetic builtins. Both operands
/// must be the same integer kind (enforced by the type checker); the true result
/// is computed in `i128` — wide enough that no fixed-width add/sub/mul overflows
/// it — then resolved per `mode`. Plain `i64` is its own full-width `Value::I64`
/// cell (outside the fixed-width `IntKind` lattice) and takes the dedicated branch
/// below, using Rust's native `i64` checked/saturating/wrapping methods. Identical
/// on every backend.
pub fn overflow_arith(
    name: &str,
    args: Vec<Value>,
    op: ArithOp,
    mode: OverflowMode,
) -> Result<Value, RuntimeError> {
    let [a, b]: [Value; 2] = args.try_into().map_err(|args: Vec<Value>| {
        RuntimeError::new(
            "L0407",
            format!("{name} expects 2 arguments but got {}", args.len()),
        )
    })?;
    // Plain `i64` — its own full-width `Value::I64` cell, not part of the
    // fixed-width `IntKind` lattice. The type checker guarantees both operands
    // share a type, so a single `I64` operand means both are `I64`. Rust's own
    // `checked_*`/`saturating_*`/`wrapping_*` `i64` methods give the exact result;
    // `wrapping_*` matches the default `+`/`-`/`*` on `i64`.
    if let (Value::I64(la), Value::I64(lb)) = (&a, &b) {
        let (la, lb) = (*la, *lb);
        return Ok(match mode {
            OverflowMode::Wrapping => Value::I64(match op {
                ArithOp::Add => la.wrapping_add(lb),
                ArithOp::Sub => la.wrapping_sub(lb),
                ArithOp::Mul => la.wrapping_mul(lb),
            }),
            OverflowMode::Saturating => Value::I64(match op {
                ArithOp::Add => la.saturating_add(lb),
                ArithOp::Sub => la.saturating_sub(lb),
                ArithOp::Mul => la.saturating_mul(lb),
            }),
            OverflowMode::Checked => option_value(
                match op {
                    ArithOp::Add => la.checked_add(lb),
                    ArithOp::Sub => la.checked_sub(lb),
                    ArithOp::Mul => la.checked_mul(lb),
                }
                .map(Value::I64),
            ),
        });
    }
    let (la, ta) = expect_fixed_int(name, &a)?;
    let (lb, tb) = expect_fixed_int(name, &b)?;
    if ta != tb {
        return Err(RuntimeError::new(
            "L0407",
            format!("{name} operands must have the same integer type"),
        ));
    }
    // The exact result in `i128`. Add/Sub of any two fixed-width operands always
    // fit `i128`; only unsigned 64-bit `Mul` can exceed it (`u64::MAX^2` is just
    // over `i128::MAX`). Because unsigned operands are non-negative, such a
    // product is unambiguously above `max`, so `checked_mul` returning `None`
    // exactly IS the overflow signal — no `i128` multiply ever panics.
    let (la128, lb128) = (ta.value_to_i128(la), ta.value_to_i128(lb));
    let exact = match op {
        ArithOp::Add => la128.checked_add(lb128),
        ArithOp::Sub => la128.checked_sub(lb128),
        ArithOp::Mul => la128.checked_mul(lb128),
    };
    let (min, max) = ta.range_i128();
    Ok(match mode {
        // Two's-complement wrap on the raw normalized cells (bit-level identical
        // for signed/unsigned add/sub/mul); `Value::int` then re-normalizes to the
        // kind's width. This is total even when the exact `i128` would overflow.
        OverflowMode::Wrapping => {
            let low = match op {
                ArithOp::Add => la.wrapping_add(lb),
                ArithOp::Sub => la.wrapping_sub(lb),
                ArithOp::Mul => la.wrapping_mul(lb),
            };
            Value::int(low, ta)
        }
        OverflowMode::Saturating => match exact {
            Some(wide) => Value::int(ta.i128_to_cell(wide.clamp(min, max)), ta),
            // Only unsigned 64-bit mul reaches `None`; the true product is
            // positive and above `max`, so saturation clamps to `max`.
            None => Value::int(ta.i128_to_cell(max), ta),
        },
        OverflowMode::Checked => match exact {
            Some(wide) if wide >= min && wide <= max => {
                option_value(Some(Value::int(ta.i128_to_cell(wide), ta)))
            }
            _ => option_value(None),
        },
    })
}

/// The `checked_div`/`checked_rem` builtins: signedness-aware division and
/// remainder returning `option<T>`. Yields `none` when the operation is undefined
/// or overflows `T` — a **zero divisor** (both div and rem), or the signed
/// `MIN / -1` **division** overflow (whose true quotient `|MIN|` is outside `T`);
/// otherwise `some(result)`. Remainder by a non-zero divisor is always defined, so
/// `checked_rem(MIN, -1)` is `some(0)` (the truncated remainder), matching
/// Lullaby's default `%`. Handles plain `i64` (`Value::I64`) and every fixed-width
/// kind; both operands share a type (enforced by the type checker). `is_rem`
/// selects remainder; otherwise division. Identical on every interpreter backend.
pub fn checked_div_rem(name: &str, args: Vec<Value>, is_rem: bool) -> Result<Value, RuntimeError> {
    let [a, b]: [Value; 2] = args.try_into().map_err(|args: Vec<Value>| {
        RuntimeError::new(
            "L0407",
            format!("{name} expects 2 arguments but got {}", args.len()),
        )
    })?;
    // Plain `i64`: a zero divisor and (for division) the `i64::MIN / -1` overflow
    // yield `none`; remainder by a non-zero divisor is always defined, so the
    // `MIN % -1 == 0` case is `some(0)` (via `wrapping_rem`), consistent with the
    // default `%` operator rather than Rust's overflow-tied `checked_rem`.
    if let (Value::I64(la), Value::I64(lb)) = (&a, &b) {
        let (la, lb) = (*la, *lb);
        let result = if lb == 0 {
            None
        } else if is_rem {
            Some(la.wrapping_rem(lb))
        } else {
            la.checked_div(lb)
        };
        return Ok(option_value(result.map(Value::I64)));
    }
    let (la, ta) = expect_fixed_int(name, &a)?;
    let (lb, tb) = expect_fixed_int(name, &b)?;
    if ta != tb {
        return Err(RuntimeError::new(
            "L0407",
            format!("{name} operands must have the same integer type"),
        ));
    }
    let (la128, lb128) = (ta.value_to_i128(la), ta.value_to_i128(lb));
    if lb128 == 0 {
        return Ok(option_value(None));
    }
    // Both div and rem are exact in `i128` (divisor non-zero, `i128` is wide
    // enough). The remainder is always within `T`'s range; the quotient is out of
    // range only in the signed `MIN / -1` case (e.g. `i8::MIN / -1 == 128`), which
    // the range check turns into `none`.
    let exact = if is_rem { la128 % lb128 } else { la128 / lb128 };
    let (min, max) = ta.range_i128();
    Ok(if exact >= min && exact <= max {
        option_value(Some(Value::int(ta.i128_to_cell(exact), ta)))
    } else {
        option_value(None)
    })
}

/// Left shift of an `i64` with a total, deterministic shift amount: the amount
/// is masked to its low 6 bits (`amount & 63`), matching x86/Java `long`
/// semantics, so a large or negative amount never panics or errors. Every
/// backend (AST, IR interpreter, bytecode VM) must use this exact rule.
pub fn shift_left(value: i64, amount: i64) -> i64 {
    value.wrapping_shl(((amount as u64) & 63) as u32)
}

/// Arithmetic (sign-preserving) right shift of an `i64` with the same masked,
/// deterministic shift amount as [`shift_left`].
pub fn shift_right(value: i64, amount: i64) -> i64 {
    value.wrapping_shr(((amount as u64) & 63) as u32)
}

/// Greatest common divisor of two `i64` values, over their absolute values.
///
/// Total on every input: `gcd(0, 0) == 0`, `gcd(0, n) == |n|`, and the result
/// is always non-negative. `i64::MIN.abs()` overflows, so absolute values are
/// taken in the wider `i128` domain and the Euclidean loop runs there before the
/// (always in-range) result is narrowed back to `i64`.
pub fn gcd_i64(a: i64, b: i64) -> i64 {
    let mut x = (a as i128).unsigned_abs();
    let mut y = (b as i128).unsigned_abs();
    while y != 0 {
        let r = x % y;
        x = y;
        y = r;
    }
    // `x` is bounded by `max(|a|, |b|) <= 2^63`, so `2^63` (the `i64::MIN` case)
    // is the only value that would not fit; but it can only appear when the
    // other operand is `0`, and `gcd(0, i64::MIN) == 2^63` overflows `i64`. Wrap
    // that single case to `i64::MIN` (its own magnitude) to stay total.
    x as i64
}
