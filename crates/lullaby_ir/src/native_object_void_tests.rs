//! Codegen tests for VOID-RETURNING functions — a function whose signature
//! declares no return value (`fn poke p ptr<i64> v i64`).
//!
//! A void return is not "an unsupported return type"; it is the ABSENCE of one.
//! Before this surface landed, the eligibility gate ran the void return through
//! the shared signature resolver, which answered — correctly for a *parameter*,
//! wrongly for a *return* — "type `void` is not in the native stack subset", so
//! every void function skipped (`L0339`) and, through the demotion fixpoint, took
//! its callers with it. That blocked the natural driver/MMIO spelling
//! `fn poke p ptr<T> v T`, forcing a dummy `i64` return as a workaround.
//!
//! `resolve_return_native_type` is now the single place `NativeType::Void` is
//! produced, and it is reachable only from a RETURN slot — a `void` parameter,
//! local, or field still fails to resolve and its function skips cleanly.
//!
//! The invariants pinned here:
//!
//!   * a void function COMPILES, and its body's effects are emitted;
//!   * it reserves NO hidden result pointer (a void return is not an aggregate),
//!     so its first visible parameter keeps register 0 (`rcx`);
//!   * it has NO value position — its tail `if`/`match` lowers as a STATEMENT, so
//!     the `block_yields_value` default-deny gate (which a void body would fail,
//!     having no value to yield) is never consulted for it. This is the main
//!     interaction risk with the value-position routing that landed in `fe88611`;
//!   * a bare `return` is accepted in a void function and refused elsewhere;
//!   * a `void` PARAMETER is still refused.
//!
//! These inspect the emitted `.text` bytes and the compile-vs-skip decision. The
//! end-to-end "compile a real `.exe` and run it" proofs live in
//! `crates/lullaby_cli/tests/cli/suite16.rs`.

use super::*;
use crate::{lower, lower_to_bytecode};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;

/// Compile source through the full frontend into a `BytecodeModule`.
fn module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

/// The regression pin for the reported gap: a function with no declared return
/// type, whose body only has effects, must COMPILE — and so must its caller.
///
/// Before the fix both skipped: `bump` with "return type `void` is not in the
/// native subset: type `void` is not in the native stack subset", and `main`
/// with "call to non-i64-scalar or unknown function `bump`" (the demotion
/// fixpoint cascading the void function's skip into every caller).
#[test]
fn void_function_and_its_caller_compile() {
    let program = emit_native_program(&module_for(concat!(
        "fn bump n i64\n",
        "    let x i64 = n + 1\n",
        "    let y i64 = x * 2\n",
        "\n",
        "fn main -> i64\n",
        "    bump(3)\n",
        "    return 7\n",
    )))
    .expect("emit void program");
    assert!(
        program.compiled.contains(&"bump".to_string()),
        "a void-returning function must be native-eligible: {:?}",
        program.skipped
    );
    assert!(
        program.compiled.contains(&"main".to_string()),
        "a caller of a void function must not be demoted: {:?}",
        program.skipped
    );
}

/// THE driver spelling. `fn poke p ptr<i64> v i64` — a void out-parameter writer,
/// the shape every MMIO/kernel routine wants — must compile natively.
#[test]
fn void_pointer_writer_driver_spelling_compiles() {
    let program = emit_native_program(&module_for(concat!(
        "fn poke p ptr<i64> v i64\n",
        "    unsafe\n",
        "        ptr_write(p, v)\n",
        "\n",
        "fn main -> i64\n",
        "    let cell i64 = 0\n",
        "    unsafe\n",
        "        poke(addr_of(cell), 99)\n",
        "    return cell\n",
    )))
    .expect("emit driver program");
    assert!(
        program.compiled.contains(&"poke".to_string()),
        "the natural driver spelling `fn poke p ptr<i64> v i64` must compile: {:?}",
        program.skipped
    );
    assert!(
        program.compiled.contains(&"main".to_string()),
        "the driver's caller must compile: {:?}",
        program.skipped
    );
}

/// A void return consumes NO hidden result pointer, so the first visible
/// parameter must arrive in register 0 (`rcx`) exactly as it would for an
/// `i64`-returning function — not shifted to `rdx` as an aggregate return shifts
/// it.
///
/// This pins the classification (`NativeType::Void` is deliberately not an
/// aggregate): had `Void` been treated as an aggregate, the callee would read its
/// parameters one register late while the caller passed them one register early —
/// a silent argument-shift miscompile on every void call.
#[test]
fn void_return_reserves_no_hidden_result_pointer() {
    let program = emit_native_program(&module_for(concat!(
        "fn store_it p ptr<i64> v i64\n",
        "    unsafe\n",
        "        ptr_write(p, v)\n",
        "\n",
        "fn main -> i64\n",
        "    let cell i64 = 0\n",
        "    unsafe\n",
        "        store_it(addr_of(cell), 5)\n",
        "    return cell\n",
    )))
    .expect("emit program");
    assert!(program.compiled.contains(&"store_it".to_string()));
    // The prologue spills parameter 0 from `rcx`: `mov [rbp - disp32], rcx` is
    // `48 89 8D`. An aggregate return would have consumed `rcx` for the hidden
    // pointer and spilled parameter 0 from `rdx` (`48 89 95`) instead.
    assert!(
        program.bytes.windows(3).any(|w| w == [0x48, 0x89, 0x8D]),
        "a void function's first visible parameter must arrive in `rcx` (no hidden \
         result pointer is reserved for a void return)"
    );
}

/// A void function whose body ENDS IN AN `if` lowers that `if` in STATEMENT
/// position (`is_value: false`), never through `lower_return_value`.
///
/// This is the direct interaction with the value-position routing from `fe88611`:
/// `block_yields_value` is a default-deny gate that a void body cannot pass (it
/// has no value to yield), so routing a void tail through the value path would
/// refuse a perfectly lowerable function. The `if` here is deliberately
/// NON-EXHAUSTIVE (no `else`) — the shape `block_yields_value` explicitly rejects
/// — which must be irrelevant for a void function and therefore must compile.
#[test]
fn void_body_ending_in_a_non_exhaustive_if_compiles_as_a_statement() {
    let program = emit_native_program(&module_for(concat!(
        "fn poke_if p ptr<i64> n i64\n",
        "    unsafe\n",
        "        if n > 10\n",
        "            ptr_write(p, 1)\n",
        "        elif n > 5\n",
        "            ptr_write(p, 2)\n",
        "\n",
        "fn main -> i64\n",
        "    let cell i64 = 0\n",
        "    unsafe\n",
        "        poke_if(addr_of(cell), 20)\n",
        "    return cell\n",
    )))
    .expect("emit program");
    assert!(
        program.compiled.contains(&"poke_if".to_string()),
        "a void body ending in a non-exhaustive `if` is a STATEMENT tail and must \
         compile — the value-position `block_yields_value` gate must not apply: {:?}",
        program.skipped
    );
}

/// The same interaction for a tail `match`: statement position, must compile.
#[test]
fn void_body_ending_in_a_match_compiles_as_a_statement() {
    let program = emit_native_program(&module_for(concat!(
        "fn poke_match p ptr<i64> o option<i64>\n",
        "    unsafe\n",
        "        match o\n",
        "            some(v) -> ptr_write(p, v)\n",
        "            none -> ptr_write(p, 0)\n",
        "\n",
        "fn main -> i64\n",
        "    let cell i64 = 0\n",
        "    unsafe\n",
        "        poke_match(addr_of(cell), some(5))\n",
        "    return cell\n",
    )))
    .expect("emit program");
    assert!(
        program.compiled.contains(&"poke_match".to_string()),
        "a void body ending in a `match` is a STATEMENT tail and must compile: {:?}",
        program.skipped
    );
}

/// A bare `return` (no value) is legal in a void function and must compile: there
/// is no value to route, so it emits the epilogue directly.
#[test]
fn bare_return_in_a_void_function_compiles() {
    let program = emit_native_program(&module_for(concat!(
        "fn early p ptr<i64> n i64\n",
        "    if n < 0\n",
        "        return\n",
        "    unsafe\n",
        "        ptr_write(p, n)\n",
        "\n",
        "fn main -> i64\n",
        "    let cell i64 = 7\n",
        "    unsafe\n",
        "        early(addr_of(cell), -1)\n",
        "    return cell\n",
    )))
    .expect("emit program");
    assert!(
        program.compiled.contains(&"early".to_string()),
        "a bare `return` in a void function must compile: {:?}",
        program.skipped
    );
}

/// A void function called inside a LOOP compiles — the call is a statement whose
/// (undefined) `rax` is discarded on every iteration.
#[test]
fn void_call_in_a_loop_compiles() {
    let program = emit_native_program(&module_for(concat!(
        "fn add_to p ptr<i64> v i64\n",
        "    unsafe\n",
        "        ptr_write(p, ptr_read(p) + v)\n",
        "\n",
        "fn main -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 1\n",
        "    while i <= 4\n",
        "        unsafe\n",
        "            add_to(addr_of(acc), i)\n",
        "        i = i + 1\n",
        "    return acc\n",
    )))
    .expect("emit program");
    assert!(
        program.compiled.contains(&"add_to".to_string()),
        "a void function called in a loop must compile: {:?}",
        program.skipped
    );
    assert!(
        program.compiled.contains(&"main".to_string()),
        "the loop caller must compile: {:?}",
        program.skipped
    );
}

/// A void function that itself CALLS another void function compiles — the void
/// return classification composes through the demotion fixpoint rather than
/// demoting at each hop.
#[test]
fn void_calling_void_compiles() {
    let program = emit_native_program(&module_for(concat!(
        "fn inner p ptr<i64> v i64\n",
        "    unsafe\n",
        "        ptr_write(p, v)\n",
        "\n",
        "fn outer p ptr<i64> v i64\n",
        "    inner(p, v * 2)\n",
        "\n",
        "fn main -> i64\n",
        "    let cell i64 = 0\n",
        "    unsafe\n",
        "        outer(addr_of(cell), 21)\n",
        "    return cell\n",
    )))
    .expect("emit program");
    for name in ["inner", "outer", "main"] {
        assert!(
            program.compiled.contains(&name.to_string()),
            "`{name}` must compile in a void->void call chain: {:?}",
            program.skipped
        );
    }
}

/// DEFAULT-DENY: a `void` return is admitted, but a `void` *value* is not.
///
/// `resolve_return_native_type` is the only producer of `NativeType::Void`, and
/// only a return slot reaches it — so `void` remains unresolvable as a parameter,
/// local, or field. The frontend independently rejects a void binding (`L0303`,
/// probed directly), which is why this asserts the *type resolver's* boundary
/// rather than compiling a program: it pins that the backend's own gate stays
/// closed even if the frontend's ever opened.
#[test]
fn void_is_not_resolvable_as_a_non_return_type() {
    let void_ty = TypeRef::new("void");
    let lengths = ArrayLengths::new();

    // As a local/field type: refused.
    assert!(
        resolve_native_type(&void_ty, &[], &[]).is_err(),
        "`void` must not resolve as a local/field type"
    );
    // As a parameter (signature) type: refused.
    assert!(
        resolve_signature_native_type(&void_ty, &[], &[], &lengths, "p").is_err(),
        "`void` must not resolve as a parameter type"
    );
    // As a RETURN type: admitted, as `Void`.
    assert_eq!(
        resolve_return_native_type(&void_ty, &[], &[], &lengths).expect("void return resolves"),
        NativeType::Void,
        "`void` must resolve to `NativeType::Void` in return position"
    );
}

// -- Void `main`: the entry stub must not read `rax` ---------------------------
//
// REGRESSION PIN for a real MISCOMPILE this surface newly admitted. `main` is the
// one void function whose "no value" is externally observable: the entry stub is
// a CALLER of it, and every stub unconditionally read `eax` as the process exit
// code. So a void `main` exited with whatever its body last left in `rax` —
// `fn main -> void` after a call returning 77 exited 77, and 1234 exited 210
// (1234 & 0xFF), where all three interpreters exit 0. The old `has_main: bool`
// was keyed on the function NAME alone and carried no return-shape information,
// so no stub could have known better; `EntryStub` replaces it.
//
// The fix is at the STUB, not the epilogue: `rax` genuinely IS undefined after a
// void function, so the stub must stop reading it. That covers every return path
// of `main` at once (fallthrough, an explicit `return`, and a `return` nested in a
// branch or loop) rather than needing each to zero `rax` itself.

/// The direct-PE entry stub — the DEFAULT path, and the one that produced the
/// observed exit 77 — must zero the exit-code register for a void `main` and read
/// `eax` for a value-returning one.
///
/// `FF 15` is the stub's indirect `call [rip + __imp_ExitProcess]`, so the
/// two-byte sequence immediately before it is exactly the exit-code setup; both
/// directions are asserted, so a stub that always emitted one form fails here.
#[test]
fn void_main_entry_stub_zeroes_the_exit_code() {
    let void_main = emit_native_program(&module_for(concat!(
        "fn f -> i64\n",
        "    77\n",
        "\n",
        "fn main -> void\n",
        "    let x i64 = f()\n",
        "    return\n",
    )))
    .expect("emit void-main program");
    let image = void_main
        .pe_image
        .expect("a void main is still a runnable image");
    assert!(
        image.windows(4).any(|w| w == [0x31, 0xC9, 0xFF, 0x15]),
        "a void `main`'s entry stub must `xor ecx, ecx` — `rax` is undefined after it"
    );
    assert!(
        !image.windows(4).any(|w| w == [0x89, 0xC1, 0xFF, 0x15]),
        "a void `main`'s entry stub must NOT `mov ecx, eax`: that leaks whatever the \
         body last computed as the process exit code (the observed 77-vs-0 miscompile)"
    );

    // The value-returning stub is unchanged — the fix must not zero a real result.
    let value_main = emit_native_program(&module_for(concat!(
        "fn f -> i64\n",
        "    77\n",
        "\n",
        "fn main -> i64\n",
        "    return f()\n",
    )))
    .expect("emit value-main program");
    let image = value_main
        .pe_image
        .expect("a value main is a runnable image");
    assert!(
        image.windows(4).any(|w| w == [0x89, 0xC1, 0xFF, 0x15]),
        "a value-returning `main`'s entry stub must still `mov ecx, eax`"
    );
    assert!(
        !image.windows(4).any(|w| w == [0x31, 0xC9, 0xFF, 0x15]),
        "a value-returning `main`'s exit code must not be zeroed"
    );
}

/// `EntryStub::classify` keys on `main`'s declared RETURN SHAPE, not just its
/// name — the hardening of the old `has_main = lowered.iter().any(|f| f.name ==
/// "main")`, which could not distinguish the two.
#[test]
fn entry_stub_classifies_main_by_return_shape() {
    let void_module = module_for("fn main -> void\n    let x i64 = 1\n");
    let value_module = module_for("fn main -> i64\n    7\n");

    let void_lowered = [LoweredNativeFunction {
        name: "main".to_string(),
        code: Vec::new(),
        relocations: Vec::new(),
        line: 1,
    }];
    assert_eq!(
        EntryStub::classify(&void_lowered, &void_module),
        EntryStub::MainVoid,
        "a `main` with no declared return type must classify as MainVoid"
    );
    assert_eq!(
        EntryStub::classify(&void_lowered, &value_module),
        EntryStub::MainValue,
        "an i64-returning `main` must classify as MainValue"
    );
    // No compiled `main` -> a library object with no stub and no exit dependency.
    assert_eq!(
        EntryStub::classify(&[], &value_module),
        EntryStub::None,
        "a program with no compiled `main` must emit no entry stub"
    );
    assert!(!EntryStub::None.emits());
    assert!(EntryStub::MainVoid.emits());
    assert!(EntryStub::MainValue.emits());
}

/// A void `main` must compile on every return-path shape the stub has to cover:
/// fallthrough (no `return` at all), an explicit tail `return`, a `return` inside
/// a branch, and a `return` inside a loop. The stub fix makes all four exit 0 by
/// construction; the exit codes themselves are asserted end-to-end in
/// `crates/lullaby_cli/tests/cli/suite16.rs`.
#[test]
fn void_main_compiles_on_every_return_path() {
    let cases: &[(&str, &str)] = &[
        ("fallthrough", "fn main -> void\n    let x i64 = f()\n"),
        (
            "tail return",
            "fn main -> void\n    let x i64 = f()\n    return\n",
        ),
        (
            "return in a branch",
            "fn main -> void\n    let x i64 = f()\n    if x > 0\n        return\n    let y i64 = 5\n",
        ),
        (
            "return in a loop",
            "fn main -> void\n    let i i64 = 0\n    while i < 4\n        let x i64 = f()\n        if x > 0\n            return\n        i = i + 1\n",
        ),
    ];
    for (label, main_src) in cases {
        let source = format!("fn f -> i64\n    77\n\n{main_src}");
        let program = emit_native_program(&module_for(&source))
            .unwrap_or_else(|e| panic!("emit void main ({label}): {}", e.message));
        assert!(
            program.compiled.contains(&"main".to_string()),
            "a void `main` ({label}) must compile: {:?}",
            program.skipped
        );
        let image = program
            .pe_image
            .unwrap_or_else(|| panic!("void main ({label}) must still produce an image"));
        assert!(
            image.windows(4).any(|w| w == [0x31, 0xC9, 0xFF, 0x15]),
            "a void `main` ({label}) must zero its exit code"
        );
    }
}

/// `NativeType::Void` is not an aggregate and occupies no words — the two
/// properties the frame planner and call ABI depend on to leave a void call's
/// registers and scratch untouched.
#[test]
fn void_layout_is_zero_words_and_not_an_aggregate() {
    assert!(
        !NativeType::Void.is_aggregate(),
        "a void return must not be classified as an aggregate (it reserves no hidden \
         result pointer)"
    );
    assert_eq!(
        NativeType::Void.words(),
        0,
        "a void return occupies no stack words"
    );
}
