use lullaby_lexer::Span;

use crate::{Expr, ExprKind};

/// Validate and remove `_` digit separators from a numeric literal. A separator
/// is only valid between two ASCII digits (`1_000`, `3.141_592`), so a leading,
/// trailing, doubled, or `.`-adjacent underscore is rejected. Returns the
/// separator-free text, or `None` when a separator is misplaced.
pub(crate) fn normalize_number_literal(value: &str) -> Option<String> {
    if !value.contains('_') {
        return Some(value.to_string());
    }
    let bytes = value.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'_' {
            let prev_digit = index
                .checked_sub(1)
                .is_some_and(|prev| bytes[prev].is_ascii_digit());
            let next_digit = bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
            if !(prev_digit && next_digit) {
                return None;
            }
        }
    }
    Some(value.chars().filter(|ch| *ch != '_').collect())
}

/// Parse a base-prefixed integer literal (`0x`/`0X`, `0b`/`0B`, `0o`/`0O`) into
/// an `i64`. The prefix is matched case-insensitively; the remaining text must be
/// non-empty radix digits with optional `_` separators strictly between two valid
/// radix digits (a leading, trailing, doubled, or prefix-adjacent underscore is
/// rejected). An out-of-radix digit, empty digits, a `.`, or an `i64` overflow all
/// return `None`. A decimal literal (no recognized base prefix) also returns
/// `None` so the caller falls through to the existing decimal/float path.
pub(crate) fn parse_radix_literal(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'0' {
        return None;
    }
    let radix = match bytes[1] {
        b'x' | b'X' => 16,
        b'b' | b'B' => 2,
        b'o' | b'O' => 8,
        _ => return None,
    };
    let digits = &value[2..];
    if digits.is_empty() {
        return None;
    }
    let digit_bytes = digits.as_bytes();
    let mut cleaned = String::with_capacity(digits.len());
    for (index, &byte) in digit_bytes.iter().enumerate() {
        if byte == b'_' {
            let prev_ok = index
                .checked_sub(1)
                .is_some_and(|prev| (digit_bytes[prev] as char).is_digit(radix));
            let next_ok = digit_bytes
                .get(index + 1)
                .is_some_and(|next| (*next as char).is_digit(radix));
            if !(prev_ok && next_ok) {
                return None;
            }
            continue;
        }
        if !(byte as char).is_digit(radix) {
            return None;
        }
        cleaned.push(byte as char);
    }
    i64::from_str_radix(&cleaned, radix).ok()
}

/// Recognized typed numeric-literal suffixes, longest first so `usize`/`isize`
/// are matched before any shorter candidate. `i64`/`f64` are the defaults; the
/// rest desugar to the corresponding `to_<T>` conversion builtin.
const NUMBER_SUFFIXES: &[&str] = &[
    "usize", "isize", "i16", "i32", "i64", "u16", "u32", "u64", "f32", "f64", "i8", "u8",
];

/// True when `s` carries a `0x`/`0b`/`0o` base prefix.
fn is_radix_prefixed(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2
        && bytes[0] == b'0'
        && matches!(bytes[1], b'x' | b'X' | b'b' | b'B' | b'o' | b'O')
}

/// The inclusive `[min, max]` range of an integer suffix as `i128`. Returns
/// `None` for the float suffixes. (The `u64`/`usize` range is the full type
/// range; a separate writable-literal cap at `i64::MAX` applies when packing the
/// desugared cell.)
fn int_suffix_range(suffix: &str) -> Option<(i128, i128)> {
    Some(match suffix {
        "i8" => (i128::from(i8::MIN), i128::from(i8::MAX)),
        "u8" => (0, i128::from(u8::MAX)),
        "i16" => (i128::from(i16::MIN), i128::from(i16::MAX)),
        "i32" => (i128::from(i32::MIN), i128::from(i32::MAX)),
        "i64" | "isize" => (i128::from(i64::MIN), i128::from(i64::MAX)),
        "u16" => (0, i128::from(u16::MAX)),
        "u32" => (0, i128::from(u32::MAX)),
        "u64" | "usize" => (0, i128::from(u64::MAX)),
        _ => return None,
    })
}

/// Parse a (possibly base-prefixed) integer literal body into `i128`. Radix
/// bodies reuse [`parse_radix_literal`] (values up to `i64::MAX`); decimal bodies
/// parse the full `i128` range so large `u64`/`usize` literals validate exactly.
fn literal_base_to_i128(base: &str) -> Option<i128> {
    if is_radix_prefixed(base) {
        parse_radix_literal(base).map(i128::from)
    } else {
        normalize_number_literal(base)?.parse::<i128>().ok()
    }
}

/// Build a `to_<name>(literal)` call expression, the desugaring of a typed
/// numeric-literal suffix; the synthetic literal argument carries the same span.
fn conversion_call(name: &str, argument: ExprKind, span: Span) -> ExprKind {
    ExprKind::Call {
        name: name.to_string(),
        args: vec![Expr {
            kind: argument,
            span,
        }],
    }
}

/// Turn a `Number` token's text into an expression. A recognized type suffix
/// (`0u8`… → err, `1i32`, `2.0f32`, `0xFFu16`, …) is range-checked and desugared
/// to the matching `to_<T>` conversion; `i64`/`f64` suffixes and unsuffixed
/// literals produce a plain `Integer`/`Float`. Unsigned 64-bit literals above
/// `i64::MAX` are supported (their `i64` bit pattern is passed to `to_u64`).
pub(crate) fn parse_number_literal(value: &str, span: Span) -> Result<ExprKind, String> {
    for &suffix in NUMBER_SUFFIXES {
        let Some(base) = value.strip_suffix(suffix) else {
            continue;
        };
        if base.is_empty() {
            continue;
        }
        let is_float_suffix = suffix.starts_with('f');
        // A float suffix never applies to a base-prefixed literal — `0xABF32` is
        // the hex number 0xABF32, not `0xAB` with an `f32` suffix.
        if is_float_suffix && is_radix_prefixed(base) {
            continue;
        }
        if is_float_suffix {
            let normalized = normalize_number_literal(base)
                .ok_or_else(|| format!("invalid float literal `{value}`"))?;
            let parsed = normalized
                .parse::<f64>()
                .map_err(|_| format!("invalid float literal `{value}`"))?;
            if suffix == "f64" {
                return Ok(ExprKind::Float(parsed));
            }
            return Ok(conversion_call("to_f32", ExprKind::Float(parsed), span));
        }
        // Integer suffix.
        if base.contains('.') {
            return Err(format!("integer literal `{value}` must not contain `.`"));
        }
        let (min, max) = int_suffix_range(suffix).expect("integer suffix has a range");
        let magnitude = literal_base_to_i128(base)
            .ok_or_else(|| format!("invalid integer literal `{value}`"))?;
        if magnitude < min || magnitude > max {
            return Err(format!(
                "integer literal `{value}` is out of range for `{suffix}`"
            ));
        }
        if suffix == "i64" {
            return Ok(ExprKind::Integer(magnitude as i64));
        }
        // The literal is desugared to `to_<T>(<i64>)`, so its magnitude must be
        // expressible as a non-negative `i64` literal. Every fixed-width value is
        // — except a `u64`/`usize` above `i64::MAX`, whose cell would be a
        // negative `i64` that has no round-trippable literal form. Reject those
        // with a precise pointer to the conversion builtin (the value is valid
        // for the type, just not writable as a literal).
        if magnitude > i128::from(i64::MAX) {
            return Err(format!(
                "`{suffix}` literal `{value}` exceeds the writable maximum {}; \
                 build larger `{suffix}` values with `to_{suffix}`",
                i64::MAX
            ));
        }
        return Ok(conversion_call(
            &format!("to_{suffix}"),
            ExprKind::Integer(magnitude as i64),
            span,
        ));
    }
    // No recognized suffix: base-prefixed integer, else decimal integer or float.
    if is_radix_prefixed(value) {
        let parsed = parse_radix_literal(value)
            .ok_or_else(|| format!("invalid integer literal `{value}`"))?;
        return Ok(ExprKind::Integer(parsed));
    }
    let normalized = normalize_number_literal(value)
        .ok_or_else(|| format!("invalid numeric literal `{value}`"))?;
    if normalized.contains('.') {
        let parsed = normalized
            .parse::<f64>()
            .map_err(|_| format!("invalid float literal `{value}`"))?;
        Ok(ExprKind::Float(parsed))
    } else {
        let parsed = normalized
            .parse::<i64>()
            .map_err(|_| format!("invalid integer literal `{value}`"))?;
        Ok(ExprKind::Integer(parsed))
    }
}
