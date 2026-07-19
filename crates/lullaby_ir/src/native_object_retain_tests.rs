//! Unit tests for the cross-call arena **retention summary** (`native_object_retain.rs`,
//! arena increment I2): the per-function `retaining` classification and the caller-side
//! [`all_callees_non_retaining`] gate wired into criterion 3 of
//! [`arena_eligible_functions`].
//!
//! These assert the CLASSIFICATION directly (the exact `retaining` boolean per channel)
//! and the eligibility MATRIX (which callers the widened criterion 3 admits/denies). The
//! end-to-end "compile a real `.exe` and check its value on every tier" proofs — the
//! positive cross-call arena, the `alloc_mode` nesting fixture, and the retaining-callee
//! UAF guard — live in `crates/lullaby_cli/tests/cli/suite18.rs`, which can run the binary.

use super::*;
use crate::{lower, lower_to_bytecode};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;
use std::collections::HashMap;

fn module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

/// The module's heap-carrying-aggregate set, built exactly as `arena_eligible_functions`
/// builds it (non-generic aggregates + heap-`T` generic instantiations), so a summary
/// query in a test sees the same R1 heap classification the real pipeline does.
fn heap_aggs_for(module: &BytecodeModule) -> std::collections::HashSet<String> {
    let mut heap_aggs = heap_carrying_aggregates(&module.structs, &module.enums);
    let generic: std::collections::HashSet<String> = module
        .functions
        .iter()
        .flat_map(|f| {
            heap_carrying_generic_instantiations(f, &module.structs, &module.enums, &heap_aggs)
        })
        .collect();
    heap_aggs.extend(generic);
    heap_aggs
}

/// The retention summary for a program's module.
fn summary_for(source: &str) -> HashMap<String, bool> {
    let module = module_for(source);
    let heap_aggs = heap_aggs_for(&module);
    retaining_summary(&module, &heap_aggs)
}

/// The native signatures of `names`, built exactly as the program driver builds them, so
/// an `arena_eligible_functions` query in a test sees the same inputs the pipeline does.
fn native_signatures_for(
    module: &BytecodeModule,
    names: &[String],
) -> HashMap<String, NativeSignature> {
    let mut signatures = HashMap::new();
    for name in names {
        let Some(function) = module.functions.iter().find(|f| &f.name == name) else {
            continue;
        };
        let lengths = infer_array_lengths(function, module, names).expect("infer array lengths");
        let sig = compute_native_signature(function, &module.structs, &module.enums, &lengths)
            .expect("compute native signature");
        signatures.insert(name.clone(), sig);
    }
    signatures
}

/// The arena-eligible function-name set for a program, driven through the real native
/// emission path (so eligibility/signatures match production).
fn arena_for(source: &str) -> std::collections::HashSet<String> {
    let module = module_for(source);
    let program = emit_native_program(&module).expect("emit native program");
    let eligible = program.compiled.clone();
    let signatures = native_signatures_for(&module, &eligible);
    arena_eligible_functions(&module, &eligible, &signatures)
}

// -- Positive classification: non-retaining leaf + non-retaining non-leaf caller ----

/// A scalar-returning heap-reading LEAF (`width`) is non-retaining, and so is a non-leaf
/// caller (`double_width`) whose only callees are that leaf plus builtins. This is the
/// core widening: the caller is admitted precisely because its callee is proven
/// non-retaining.
#[test]
fn non_leaf_caller_of_a_non_retaining_leaf_is_non_retaining() {
    let summary = summary_for(concat!(
        "fn width s string -> i64\n",
        "    len(s)\n",
        "\n",
        "fn double_width a i64 -> i64\n",
        "    let s string = to_string(a)\n",
        "    width(s) + width(to_string(a * 2))\n",
        "\n",
        "fn main -> i64\n",
        "    double_width(3)\n",
    ));
    assert_eq!(summary.get("width"), Some(&false), "leaf is non-retaining");
    assert_eq!(
        summary.get("double_width"),
        Some(&false),
        "a caller of only non-retaining callees is non-retaining"
    );
}

/// The eligibility matrix's positive row: the widened criterion 3 admits BOTH the leaf
/// and the non-leaf caller to the arena. Before I2 only the leaf qualified.
#[test]
fn cross_call_caller_and_leaf_are_both_arena_eligible() {
    let arena = arena_for(concat!(
        "fn width s string -> i64\n",
        "    len(s)\n",
        "\n",
        "fn double_width a i64 -> i64\n",
        "    let s string = to_string(a)\n",
        "    width(s) + width(to_string(a * 2))\n",
        "\n",
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 20\n",
        "        total = total + double_width(i)\n",
        "    total\n",
    ));
    assert!(
        arena.contains("width"),
        "the non-retaining leaf stays arena-eligible: {arena:?}"
    );
    assert!(
        arena.contains("double_width"),
        "the non-leaf caller is newly arena-eligible (cross-call arena): {arena:?}"
    );
}

// -- Negative classification: one control per retaining channel ---------------------

/// R1 — a callee that RETURNS a heap value is retaining, and its caller inherits that
/// through R4 (so the caller is denied the arena).
#[test]
fn heap_returning_callee_and_its_caller_are_retaining() {
    let summary = summary_for(concat!(
        "fn make_tag a i64 -> string\n",
        "    to_string(a) + \"!\"\n",
        "\n",
        "fn caller a i64 -> i64\n",
        "    len(make_tag(a))\n",
        "\n",
        "fn main -> i64\n",
        "    caller(2)\n",
    ));
    assert_eq!(
        summary.get("make_tag"),
        Some(&true),
        "a heap-returning callee is retaining (R1)"
    );
    assert_eq!(
        summary.get("caller"),
        Some(&true),
        "a caller of a retaining callee is retaining (R4)"
    );

    let arena = arena_for(concat!(
        "fn make_tag a i64 -> string\n",
        "    to_string(a) + \"!\"\n",
        "\n",
        "fn caller a i64 -> i64\n",
        "    len(make_tag(a))\n",
        "\n",
        "fn main -> i64\n",
        "    caller(2)\n",
    ));
    assert!(
        !arena.contains("caller"),
        "a caller of a heap-returning callee must NOT be arena-routed: {arena:?}"
    );
}

/// R2 — a callee that captures into a RETURNED closure is retaining (a closure literal
/// is present, and it returns a `fn` value that carries the capture).
#[test]
fn closure_capturing_callee_is_retaining() {
    let summary = summary_for(concat!(
        "fn trap a i64 -> fn() -> i64\n",
        "    let f fn() -> i64 = fn -> a + 1\n",
        "    f\n",
        "\n",
        "fn main -> i64\n",
        "    let g fn() -> i64 = trap(5)\n",
        "    g()\n",
    ));
    assert_eq!(
        summary.get("trap"),
        Some(&true),
        "a closure-capturing/fn-returning callee is retaining (R2/R1)"
    );
}

/// R4 — a callee that calls an `extern` C function is retaining (C can stash the
/// pointer), and its caller inherits that.
#[test]
fn extern_calling_callee_is_retaining() {
    let summary = summary_for(concat!(
        "extern fn c_add a i64 -> i64\n",
        "\n",
        "fn uses_extern a i64 -> i64\n",
        "    let s string = to_string(a)\n",
        "    c_add(len(s))\n",
        "\n",
        "fn main -> i64\n",
        "    uses_extern(3)\n",
    ));
    assert_eq!(
        summary.get("uses_extern"),
        Some(&true),
        "a callee that calls `extern` is retaining (R4 — extern is default-deny)"
    );
}

/// Cycle pre-poison — a self-recursive function is retaining WITHOUT inspecting its
/// callees (default-deny), and so is every mutually-recursive member of an SCC.
#[test]
fn recursive_and_mutually_recursive_functions_are_retaining() {
    let self_rec = summary_for(concat!(
        "fn rec a i64 -> i64\n",
        "    let s string = to_string(a)\n",
        "    if a <= 0\n",
        "        len(s)\n",
        "    else\n",
        "        len(s) + rec(a - 1)\n",
        "\n",
        "fn main -> i64\n",
        "    rec(3)\n",
    ));
    assert_eq!(
        self_rec.get("rec"),
        Some(&true),
        "a self-recursive function is pre-poisoned retaining"
    );

    let mutual = summary_for(concat!(
        "fn ping a i64 -> i64\n",
        "    let s string = to_string(a)\n",
        "    if a <= 0\n",
        "        len(s)\n",
        "    else\n",
        "        len(s) + pong(a - 1)\n",
        "\n",
        "fn pong a i64 -> i64\n",
        "    let t string = to_string(a)\n",
        "    if a <= 0\n",
        "        len(t)\n",
        "    else\n",
        "        len(t) + ping(a - 1)\n",
        "\n",
        "fn main -> i64\n",
        "    ping(4)\n",
    ));
    assert_eq!(
        mutual.get("ping"),
        Some(&true),
        "an SCC member is pre-poisoned retaining"
    );
    assert_eq!(
        mutual.get("pong"),
        Some(&true),
        "every SCC member is pre-poisoned retaining"
    );
}

/// R4 — a callee reached through a `fn`-typed PARAMETER (an indirect target) is
/// retaining: the default-deny inversion treats an unresolvable indirect call as
/// retaining, never as a harmless builtin.
#[test]
fn indirect_fn_param_call_makes_the_caller_retaining() {
    let summary = summary_for(concat!(
        "fn apply_hof f fn(i64) -> i64 x i64 -> i64\n",
        "    f(x)\n",
        "\n",
        "fn main -> i64\n",
        "    apply_hof(fn y i64 -> y + 1, 5)\n",
    ));
    assert_eq!(
        summary.get("apply_hof"),
        Some(&true),
        "an indirect call through a fn-param is retaining (default-deny)"
    );
}

/// R3 — a callee that uses `alloc` (a raw heap box invisible to the escape analysis) is
/// retaining, and its caller is denied the arena. This is the cross-call generalization
/// of the `alloc_defeats_arena` gate.
#[test]
fn alloc_using_callee_is_retaining() {
    let summary = summary_for(concat!(
        "fn boxed a i64 -> i64\n",
        "    unsafe\n",
        "        let p = alloc(a)\n",
        "        ptr_read(p)\n",
        "\n",
        "fn main -> i64\n",
        "    boxed(9)\n",
    ));
    assert_eq!(
        summary.get("boxed"),
        Some(&true),
        "an alloc/raw-pointer callee is retaining (R3)"
    );
}

/// A caller that calls one non-retaining and one retaining callee is denied: the arena
/// admits a caller only when ALL its callees are non-retaining.
#[test]
fn caller_of_a_mixed_callee_set_is_denied() {
    let arena = arena_for(concat!(
        "fn width s string -> i64\n",
        "    len(s)\n",
        "\n",
        "fn tag a i64 -> string\n",
        "    to_string(a) + \"!\"\n",
        "\n",
        "fn mixed a i64 -> i64\n",
        "    let s string = to_string(a)\n",
        "    width(s) + len(tag(a))\n",
        "\n",
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 12\n",
        "        total = total + mixed(i)\n",
        "    total\n",
    ));
    assert!(
        arena.contains("width"),
        "the non-retaining leaf is still admitted: {arena:?}"
    );
    assert!(
        !arena.contains("mixed"),
        "a caller of ANY retaining callee (here the heap-returning `tag`) is denied: {arena:?}"
    );
}
