//! Typing for the freestanding-tier **port-mapped I/O** surface (stage 3):
//! `port_in8` / `port_in16` / `port_in32` and `port_out8` / `port_out16` /
//! `port_out32`. Kept out of `semantics_checker_calls.rs` (and `lib.rs`, both
//! already over the size cap) as a cohesive `impl Checker` block, mirroring how
//! `semantics_raw_ptr.rs` owns the raw-pointer addressing surface. See
//! `documents/freestanding_tier_design.md` §4.
//!
//! # The surface
//!
//! The design doc §4 fixes the spelling, and it is what ships:
//!
//! ```text
//! port_in8(port u16)  -> u8       port_out8(port u16, value u8)   -> void
//! port_in16(port u16) -> u16      port_out16(port u16, value u16) -> void
//! port_in32(port u16) -> u32      port_out32(port u16, value u32) -> void
//! ```
//!
//! Plain call syntax, no turbofish — the data width is baked into the builtin's
//! *name* rather than carried by a type parameter, matching the delivered
//! raw-pointer builtins' style. That naming is what makes the width checkable:
//! `port_out8` can only ever mean an 8-bit `out`, so a `u32` data argument is a
//! definite error rather than an inference puzzle.
//!
//! # Typing
//!
//! An x86 port number is a **16-bit unsigned** value — the architectural port
//! space is exactly `0..=0xFFFF`, and `DX` is the register that carries it — so
//! the port parameter is typed `u16` on every one of the six builtins. Lullaby
//! has no implicit integer coercion, so a literal port is written with the
//! delivered typed-literal suffix (`port_out8(0x3F8u16, b)`) or an explicit
//! `to_u16(...)`. That is deliberate: a port number silently truncating from an
//! `i64` is exactly the class of bug this tier cannot afford.
//!
//! The data width is fixed by the builtin name (8/16/32 → `u8`/`u16`/`u32`), and
//! it is **unsigned** in both directions: a port read yields a raw device byte /
//! word / dword with no sign to extend, so `port_in8` returns `u8`, never `i8`.
//! A wrong port or data width is [`L0442`] — a dedicated code rather than the
//! generic argument-type diagnostics, so the message can explain *why* the width
//! is fixed (see [`Checker::port_io_type_error`]).
//!
//! # Gating
//!
//! Every port builtin is `unsafe`-gated exactly like the delivered raw-pointer
//! builtins, reusing **`L0330`** — no new code, because "this is a raw hardware
//! operation and needs an `unsafe` block" is precisely what `L0330` already says.
//!
//! They are **available in a `no-runtime` module**: the `L0441` freestanding gate
//! rejects heap/runtime *types* and the host-allocator builtins, and a port
//! builtin is neither — it names only `u8`/`u16`/`u32`, allocates nothing, and
//! introduces no hidden control flow. It is kernel core, like `ptr_read` /
//! `volatile_*`. `semantics_no_runtime.rs` therefore needs no change, and
//! `port_io_is_available_in_a_no_runtime_module` pins that.
//!
//! # Native-only
//!
//! `in`/`out` are privileged instructions (they fault at CPL 3 and are
//! meaningless in a hosted process), so they are **native-only**: the three
//! interpreters refuse them at run time with `L0444` rather than invent a value.
//! That refusal lives with the interpreters; this module only types the calls.

use super::*;

/// The Lullaby type of a port number: an x86 I/O port is a 16-bit unsigned value
/// (`0..=0xFFFF`), carried architecturally in `DX`.
pub(crate) const PORT_TYPE: &str = "u16";

/// The signature of a port-I/O builtin: whether it *reads* a port (`in`, arity 1)
/// or *writes* one (`out`, arity 2), and the unsigned Lullaby type of its data
/// operand, which the builtin's name fixes. `None` for any other name.
pub(crate) fn port_io_signature(name: &str) -> Option<(bool, &'static str)> {
    Some(match name {
        "port_in8" => (true, "u8"),
        "port_in16" => (true, "u16"),
        "port_in32" => (true, "u32"),
        "port_out8" => (false, "u8"),
        "port_out16" => (false, "u16"),
        "port_out32" => (false, "u32"),
        _ => return None,
    })
}

/// Whether `name` is one of the six port-I/O builtins.
pub(crate) fn is_port_io_builtin(name: &str) -> bool {
    port_io_signature(name).is_some()
}

impl Checker<'_> {
    /// Type a port-I/O builtin call. Returns `None` when `name` is not a port
    /// builtin, so the caller falls through to its other dispatch arms.
    ///
    /// * `port_in<N>(port u16) -> u<N>`
    /// * `port_out<N>(port u16, value u<N>) -> void`
    ///
    /// Both operands are width-checked (`L0442`) and the call is `unsafe`-gated
    /// (`L0330`).
    pub(crate) fn check_port_io(
        &mut self,
        name: &str,
        args: &[Expr],
        call_span: Span,
        scope: &Scope,
        function: &Function,
    ) -> Option<TypeRef> {
        let (is_read, data_type) = port_io_signature(name)?;
        let arity = if is_read { 1 } else { 2 };
        self.expect_arg_count(name, args, arity, function)?;

        // The port number: `u16` on every builtin, read or write.
        self.expect_port_operand(
            name,
            &args[0],
            PORT_TYPE,
            "a port number is a 16-bit unsigned value (`u16`), the full x86 I/O \
             port space `0..=0xFFFF`",
            scope,
            function,
        )?;

        // The data operand of an `out`: its width is fixed by the builtin's name.
        if !is_read {
            let width = &name["port_out".len()..];
            let article = if width == "8" { "an" } else { "a" };
            self.expect_port_operand(
                name,
                &args[1],
                data_type,
                &format!(
                    "`{name}` writes {article} {width}-bit port, so its value must be \
                     `{data_type}`; use `port_out8`/`port_out16`/`port_out32` to select the width"
                ),
                scope,
                function,
            )?;
        }

        self.require_unsafe_port_io(name, call_span, function)?;
        Some(TypeRef::new(if is_read { data_type } else { "void" }))
    }

    /// The `unsafe` gate for port I/O. Reuses **`L0330`** — the delivered
    /// raw-operation gate — because "this is a raw hardware operation and needs an
    /// `unsafe` block" is exactly what that code means (the design doc §2.2 frames
    /// it as covering the raw-pointer *and* hardware operations, and the once-
    /// proposed `L0442` was not needed for gating).
    ///
    /// The message is spelled here rather than through the shared
    /// [`Checker::require_unsafe`] helper only so it can say *port I/O* instead of
    /// *raw pointer operation*: a port builtin touches no pointer, and telling a
    /// kernel author to "use safe `rc<T>`/`ref<T>` references instead" of an `out`
    /// would be nonsense. Same code, accurate noun.
    fn require_unsafe_port_io(
        &mut self,
        name: &str,
        call_span: Span,
        function: &Function,
    ) -> Option<()> {
        if self.unsafe_depth > 0 {
            return Some(());
        }
        self.diagnostics.push(SemanticDiagnostic::at(
            "L0330",
            format!(
                "port I/O operation `{name}` requires an `unsafe` block: `in`/`out` drive \
                 hardware directly and cannot be checked by the compiler"
            ),
            Some(function.name.clone()),
            call_span,
        ));
        None
    }

    /// Check one port-I/O operand against its required fixed-width type,
    /// reporting `L0442` with `why` (an explanation of *why* the width is fixed)
    /// when it does not match.
    fn expect_port_operand(
        &mut self,
        name: &str,
        arg: &Expr,
        expected: &str,
        why: &str,
        scope: &Scope,
        function: &Function,
    ) -> Option<()> {
        let actual = self.check_expr(arg, scope, function)?;
        if actual.name == expected {
            return Some(());
        }
        self.port_io_type_error(name, arg, expected, why, &actual, function);
        None
    }

    /// Report an `L0442` port-I/O width error. The message names the required
    /// type, what was found, and why the width is not negotiable — and, for the
    /// overwhelmingly common case of an unsuffixed integer literal, points at the
    /// typed-literal suffix that fixes it.
    fn port_io_type_error(
        &mut self,
        name: &str,
        arg: &Expr,
        expected: &str,
        why: &str,
        actual: &TypeRef,
        function: &Function,
    ) {
        let hint = if actual.name == "i64" {
            format!(
                " (an integer literal is `i64`; write it with the `{expected}` suffix, \
                 e.g. `0x3F8{expected}`, or convert it with `to_{expected}(...)`)"
            )
        } else {
            String::new()
        };
        self.diagnostics.push(SemanticDiagnostic::at(
            "L0442",
            format!(
                "`{name}` expects `{expected}` but got `{}`: {why}{hint}",
                actual.name
            ),
            Some(function.name.clone()),
            arg.span,
        ));
    }
}
