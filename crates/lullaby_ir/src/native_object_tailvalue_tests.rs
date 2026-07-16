//! Codegen tests for VALUE-POSITION TAIL lowering — a value bound to a local
//! inside an `if`/`elif`/`else` branch or a `match` arm and yielded as that
//! branch's/arm's tail expression.
//!
//! These pin the fix for a silent native MISCOMPILE: the routing that decides
//! *where* a returned value must land (the hidden aggregate result pointer,
//! `xmm0`, or `rax`) was applied only to a function's own tail expression and to
//! `return`, never to a branch/arm tail. A branch tail fell through to the generic
//! `BytecodeInstruction::Expr` statement arm, which evaluates into `rax` and
//! DISCARDS — so an aggregate-returning function never wrote its hidden result
//! pointer at all and the caller read its own uninitialized scratch (a wrong tag
//! AND payload, or a wild pointer dereference for a heap payload), while a
//! float-returning `export fn` never wrote `xmm0`.
//!
//! `lower_return_value` is now the single routing point, used by every value
//! position; `block_yields_value` is the default-deny gate.
//!
//! These inspect the emitted `.text` bytes and the compile-vs-skip decision. The
//! end-to-end "compile a real `.exe` and check its exit code against the
//! interpreters" proofs live in `crates/lullaby_cli/tests/cli/suite16.rs`, which
//! can actually run the binary, and the whole class is swept by
//! `gen_branch_tail_program` in `crates/lullaby_cli/tests/cli/fuzz.rs`.

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

/// Compile source as a *library* (no `main` required) — for the `export fn` path.
fn library_module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = lullaby_semantics::validate(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

/// An `option<i64>` bound to a branch-local and yielded as the branch tail must
/// compile, and must write the hidden result pointer. Before the fix the function
/// compiled but emitted no store through the sret pointer at all.
#[test]
fn branch_local_option_tail_writes_hidden_result_pointer() {
    let program = emit_native_program(&module_for(concat!(
        "fn pick n i64 -> option<i64>\n",
        "    if n > 0\n",
        "        let s option<i64> = some(n)\n",
        "        s\n",
        "    else\n",
        "        let e option<i64> = none\n",
        "        e\n",
        "\n",
        "fn main -> i64\n",
        "    match pick(3)\n",
        "        some(v) -> 100 + v\n",
        "        none -> 0\n",
    )))
    .expect("emit branch-local option program");
    assert!(
        program.compiled.contains(&"pick".to_string()),
        "a branch-local option tail must compile natively: {:?}",
        program.skipped
    );
    // `lower_aggregate_return` copies each word through the hidden pointer with
    // `mov [rax + disp32], rcx` (48 89 88 + disp32). Without the fix no such store
    // exists in `pick` at all: the branch tail was evaluated into `rax` by the
    // generic statement path and discarded, leaving the hidden pointer untouched.
    assert!(
        program.bytes.windows(3).any(|w| w == [0x48, 0x89, 0x88]),
        "an aggregate branch tail must store its words through the hidden result pointer"
    );
}

/// Every aggregate return shape reached through a branch/arm tail must compile:
/// `result<T, E>`, a user enum with a payload variant, a plain struct, a nested
/// `if`, an `elif` chain, and a `match`-arm tail.
#[test]
fn value_position_tails_compile_for_every_aggregate_shape() {
    let cases: &[(&str, &str)] = &[
        (
            "result",
            concat!(
                "fn pick n i64 -> result<i64, i64>\n",
                "    if n > 0\n",
                "        let s result<i64, i64> = ok(n)\n",
                "        s\n",
                "    else\n",
                "        let e result<i64, i64> = err(9)\n",
                "        e\n",
            ),
        ),
        (
            "user enum",
            concat!(
                "enum E\n",
                "    A i64\n",
                "    B\n",
                "\n",
                "fn pick n i64 -> E\n",
                "    if n > 0\n",
                "        let s E = A(n)\n",
                "        s\n",
                "    else\n",
                "        let e E = B\n",
                "        e\n",
            ),
        ),
        (
            "struct",
            concat!(
                "struct P\n",
                "    a i64\n",
                "    b i64\n",
                "\n",
                "fn pick n i64 -> P\n",
                "    if n > 0\n",
                "        let s P = P(n, n * 2)\n",
                "        s\n",
                "    else\n",
                "        let e P = P(0, 0)\n",
                "        e\n",
            ),
        ),
        (
            "nested if",
            concat!(
                "fn pick n i64 -> option<i64>\n",
                "    if n > 0\n",
                "        if n > 100\n",
                "            let a option<i64> = some(1)\n",
                "            a\n",
                "        else\n",
                "            let b option<i64> = some(n)\n",
                "            b\n",
                "    else\n",
                "        let e option<i64> = none\n",
                "        e\n",
            ),
        ),
        (
            "elif chain",
            concat!(
                "fn pick n i64 -> option<i64>\n",
                "    if n > 100\n",
                "        let a option<i64> = some(1)\n",
                "        a\n",
                "    elif n > 0\n",
                "        let b option<i64> = some(n)\n",
                "        b\n",
                "    else\n",
                "        let e option<i64> = none\n",
                "        e\n",
            ),
        ),
        (
            "match arm tail",
            concat!(
                "fn tag n i64 -> option<i64>\n",
                "    if n > 0\n",
                "        return some(n)\n",
                "    return none\n",
                "\n",
                "fn pick n i64 -> option<i64>\n",
                "    match tag(n)\n",
                "        some(v) ->\n",
                "            let s option<i64> = some(v * 2)\n",
                "            s\n",
                "        none ->\n",
                "            let e option<i64> = none\n",
                "            e\n",
            ),
        ),
    ];
    for (label, source) in cases {
        let program =
            emit_native_program(&module_for(&format!("{source}\nfn main -> i64\n    7\n")))
                .unwrap_or_else(|e| panic!("emit {label} program: {e:?}"));
        assert!(
            program.compiled.contains(&"pick".to_string()),
            "`{label}`: a branch/arm-local aggregate tail must compile natively: {:?}",
            program.skipped
        );
    }
}

/// A float-returning `export fn` whose value comes from a branch-local tail must
/// leave its result in `xmm0`. Before the fix the branch tail loaded the f64's
/// BITS into `rax` via the integer path and `xmm0` was never written.
#[test]
fn branch_local_float_tail_writes_xmm0() {
    let program = emit_native_program(&library_module_for(concat!(
        "export fn pickf n i64 -> f64\n",
        "    if n > 0\n",
        "        let s f64 = 2.5\n",
        "        s\n",
        "    else\n",
        "        let e f64 = 1.5\n",
        "        e\n",
    )))
    .expect("emit export float program");
    assert!(
        program.compiled.contains(&"pickf".to_string()),
        "a float export with a branch-local tail must compile natively: {:?}",
        program.skipped
    );
    // `movsd xmm0, [rbp + disp32]` — F2 0F 10 85 — is how the float path loads the
    // branch local into the SSE return register.
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0xF2, 0x0F, 0x10, 0x85]),
        "a float branch tail must load its value into `xmm0` (movsd xmm0, [rbp+disp32])"
    );
}

/// The `block_yields_value` gate must not OVER-refuse: a function that mixes an
/// early `return` inside an `if` with an ordinary aggregate tail expression is the
/// normal supported path and must still compile.
///
/// (This test was previously misnamed `non_exhaustive_value_tail_if_skips_cleanly`
/// — it never checked a non-exhaustive tail `if`, and asserted the opposite of what
/// the name said. A genuinely non-exhaustive value tail `if` cannot be built from
/// source at all: semantics rejects it with `L0301` before it can reach the
/// backend, which is why the gate has no source-level negative to point at. See the
/// reachability note on `block_yields_value`.)
#[test]
fn value_tail_gate_does_not_over_refuse_ordinary_tails() {
    let module = module_for(concat!(
        "fn pick n i64 -> option<i64>\n",
        "    let fallback option<i64> = none\n",
        "    if n > 0\n",
        "        let s option<i64> = some(n)\n",
        "        return s\n",
        "    fallback\n",
        "\n",
        "fn main -> i64\n",
        "    match pick(3)\n",
        "        some(v) -> 100 + v\n",
        "        none -> 0\n",
    ));
    let program = emit_native_program(&module).expect("emit fallback program");
    assert!(
        program.compiled.contains(&"pick".to_string()),
        "an ordinary aggregate tail expression must still compile: {:?}",
        program.skipped
    );
}

/// An `asm` block as an `if`-BRANCH tail must compile. `asm` is native-only (the
/// interpreters reject it with `L0425`), so refusing this shape does not demote it
/// to an interpreter — it makes the program unbuildable ANYWHERE, breaking the
/// freestanding/kernel tier.
///
/// This is a REGRESSION PIN: adding the `block_yields_value` default-deny gate
/// without an `Asm` arm silently made this shape `L0339` "no functions were
/// eligible", even though the function-level tail-`asm` path had always trusted an
/// `asm` block to leave its value in `rax`. No fixture used an `asm` branch tail, so
/// the whole test suite stayed green while the shape broke.
#[test]
fn asm_branch_tail_compiles_and_emits_its_bytes() {
    let program = emit_native_program(&module_for(concat!(
        "fn pick n i64 -> i64\n",
        "    if n > 0\n",
        "        unsafe\n",
        "            asm 72, 199, 192, 42, 0, 0, 0\n",
        "    else\n",
        "        unsafe\n",
        "            asm 72, 199, 192, 7, 0, 0, 0\n",
        "\n",
        "fn main -> i64\n",
        "    pick(1)\n",
    )))
    .expect("emit asm branch-tail program");
    assert!(
        program.compiled.contains(&"pick".to_string()),
        "an `asm` branch tail must compile — `asm` is native-only, so refusing it \
         makes the program unbuildable anywhere: {:?}",
        program.skipped
    );
    // Both branches' verbatim bytes must be present: `mov rax, 42` and `mov rax, 7`.
    assert!(
        program
            .bytes
            .windows(7)
            .any(|w| w == [0x48, 0xC7, 0xC0, 42, 0, 0, 0]),
        "the then-branch `asm` bytes (mov rax, 42) must be emitted verbatim"
    );
    assert!(
        program
            .bytes
            .windows(7)
            .any(|w| w == [0x48, 0xC7, 0xC0, 7, 0, 0, 0]),
        "the else-branch `asm` bytes (mov rax, 7) must be emitted verbatim"
    );
}

/// The `block_yields_value` gate accepts the shapes the value-position lowering
/// handles and rejects a chain that can fall through without producing a value.
#[test]
fn block_yields_value_gate_matches_lowerable_shapes() {
    let yielding = module_for(concat!(
        "fn pick n i64 -> option<i64>\n",
        "    if n > 0\n",
        "        let s option<i64> = some(n)\n",
        "        s\n",
        "    else\n",
        "        let e option<i64> = none\n",
        "        e\n",
        "\n",
        "fn main -> i64\n",
        "    7\n",
    ));
    let pick = yielding
        .functions
        .iter()
        .find(|f| f.name == "pick")
        .expect("pick present");
    assert!(
        block_yields_value(&pick.instructions),
        "an exhaustive if/else whose branches end in tail expressions must yield"
    );

    // An `asm` branch tail yields: the block leaves its value in `rax`, the same
    // contract the function-level tail-`asm` path relies on.
    let asm_tail = module_for(concat!(
        "fn pick n i64 -> i64\n",
        "    if n > 0\n",
        "        unsafe\n",
        "            asm 72, 199, 192, 42, 0, 0, 0\n",
        "    else\n",
        "        unsafe\n",
        "            asm 72, 199, 192, 7, 0, 0, 0\n",
        "\n",
        "fn main -> i64\n",
        "    pick(1)\n",
    ));
    let asm_pick = asm_tail
        .functions
        .iter()
        .find(|f| f.name == "pick")
        .expect("pick present");
    assert!(
        block_yields_value(&asm_pick.instructions),
        "an exhaustive if/else whose branches end in `asm` blocks must yield"
    );

    // A body whose tail is a `while` loop produces no value on any path.
    let non_yielding = module_for(concat!(
        "fn count n i64 -> void\n",
        "    let i i64 = 0\n",
        "    while i < n\n",
        "        i = i + 1\n",
        "\n",
        "fn main -> i64\n",
        "    7\n",
    ));
    let count = non_yielding
        .functions
        .iter()
        .find(|f| f.name == "count")
        .expect("count present");
    assert!(
        !block_yields_value(&count.instructions),
        "a body whose tail is a `while` loop must not be treated as yielding"
    );
}

/// The one shape that actually reaches the `block_yields_value` refusal: a `throw`
/// branch tail (it passes `lullaby check`, unlike every other non-yielding tail,
/// which semantics rejects with `L0301`). It must skip cleanly — never miscompile.
///
/// `throw`/`try` is outside the native subset regardless, so `lower_native_stmt`
/// would refuse this function anyway; the gate simply refuses it earlier. Either
/// way the compile-vs-skip decision is the same as before the gate existed, which
/// is what this pins.
#[test]
fn throw_branch_tail_skips_cleanly() {
    // `pick` skips, and `main` demotes through the fixpoint because it calls `pick`,
    // so nothing is eligible and the emitter reports `L0339` — the clean-skip path,
    // never a miscompiled `pick`.
    let err = emit_native_program(&module_for(concat!(
        "fn pick n i64 -> i64\n",
        "    if n > 0\n",
        "        throw \"boom\"\n",
        "    else\n",
        "        7\n",
        "\n",
        "fn main -> i64\n",
        "    pick(-1)\n",
    )))
    .expect_err("a throw branch tail must not compile");
    assert_eq!(err.code, "L0339", "the skip must be reported as L0339");
    assert!(
        err.skipped.iter().any(|s| s.name == "pick"),
        "`pick` must be recorded as skipped with a reason: {:?}",
        err.skipped
    );
}
