//! WASM **execution**-parity harness and tests.
//!
//! `wasm_tests.rs` asserts the *bytes* of the emitted module (magic, section
//! order, import/export counts) but never runs it, so a WASM miscompile that
//! still produces structurally valid bytes is invisible to `cargo test`. This
//! module closes that gap: it emits the module through the real pipeline, runs
//! it in-process under the pure-Rust [`wasmi`] interpreter with stub host
//! imports, and compares the `i64` returned by `main` against the value the IR
//! interpreter (`crate::run_main`) computes for the same source. It is the WASM
//! analogue of the native execution-parity tests that compile+run a real `.exe`
//! and check the process exit code — only here the "process" is an in-process
//! wasmi instance and the "exit code" is `main`'s `i64` return.
//!
//! The three stub host imports match the emitter's fixed import set (see
//! `wasm.rs`: `env.log_i64 (param i64)`, `env.console_log (param i32 i32)`,
//! `env.dom_set_text (param i32 i32 i32 i32)`); they discard their arguments so
//! a program that logs still runs to completion. Everything is deterministic and
//! self-contained — no Node, no `wasm-pack`, no external toolchain.

use super::*;
use crate::{lower, run_main};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_runtime::Value;
use lullaby_semantics::validate;
use wasmi::{Engine, Linker, Module as WasmiModule, Store};

/// Lower `source` all the way to an [`IrModule`] through the real front end.
fn module_for(source: &str) -> IrModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate(&program).expect("semantic");
    lower(&checked).expect("lower")
}

/// Emit the WASM module for `source`, instantiate it under `wasmi` with stub host
/// imports, call `main`, and return its `i64`. Panics with a descriptive message
/// on any pipeline/instantiation failure so a broken emission is a loud test
/// failure, never a silent skip.
fn run_wasm_main_i64(source: &str) -> i64 {
    let module = module_for(source);
    let artifact = emit_wasm_module(&module).expect("emit wasm");
    run_wasm_bytes_main_i64(&artifact.bytes)
}

/// Instantiate a raw `.wasm` byte module under `wasmi` and call `main -> i64`.
/// Split out so a teeth test can hand in a deliberately corrupted module.
fn run_wasm_bytes_main_i64(bytes: &[u8]) -> i64 {
    let engine = Engine::default();
    let module = WasmiModule::new(&engine, bytes).expect("wasmi parse/validate module");
    let mut store = Store::new(&engine, ());
    let mut linker = <Linker<()>>::new(&engine);
    // Stub the three fixed host imports; they discard their arguments.
    linker
        .func_wrap("env", "log_i64", |_: i64| {})
        .expect("link log_i64");
    linker
        .func_wrap("env", "console_log", |_: i32, _: i32| {})
        .expect("link console_log");
    linker
        .func_wrap("env", "dom_set_text", |_: i32, _: i32, _: i32, _: i32| {})
        .expect("link dom_set_text");
    let instance = linker
        .instantiate_and_start(&mut store, &module)
        .expect("instantiate");
    let main = instance
        .get_typed_func::<(), i64>(&store, "main")
        .expect("main export with signature () -> i64");
    main.call(&mut store, ()).expect("call main")
}

/// Run `main` on the IR interpreter and extract its `i64` result. Any integer
/// [`Value`] shape (`I64` or a fixed-width `Int`) is accepted so a program that
/// returns e.g. an `i32` still compares.
fn run_interp_main_i64(source: &str) -> i64 {
    match run_main(&module_for(source)).expect("interpret main") {
        Value::I64(v) => v,
        Value::Int { value, .. } => value,
        Value::Bool(b) => b as i64,
        other => panic!("main did not return an integer value: {other:?}"),
    }
}

/// Assert WASM == IR interpreter for `source`, and that both equal `expected`.
/// This is the core cross-tier parity check every case below funnels through.
fn assert_parity(source: &str, expected: i64) {
    let interp = run_interp_main_i64(source);
    let wasm = run_wasm_main_i64(source);
    assert_eq!(
        interp, expected,
        "IR interpreter disagrees with the expected value for:\n{source}"
    );
    assert_eq!(
        wasm, interp,
        "WASM diverged from the IR interpreter (wasm={wasm}, interp={interp}) for:\n{source}"
    );
}

// -- Harness teeth: it must reproduce known-good answers AND be able to fail ---

#[test]
fn harness_runs_plain_arithmetic_main() {
    // A plain scalar `main` with no aggregates: proves the emit->wasmi->call
    // path returns the real computed value.
    let source = "fn main -> i64\n    let a = 40\n    let b = 2\n    return a + b\n";
    assert_parity(source, 42);
}

#[test]
fn harness_reads_a_struct_field() {
    // A correct struct field-read (no aliasing involved): the harness handles
    // linear-memory aggregates end to end.
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let f P = P(100, 4)
    return f.a + f.b
";
    assert_parity(source, 104);
}

#[test]
fn harness_detects_a_corrupted_emission() {
    // Prove the harness can FAIL: truncate the module mid-body so wasmi rejects
    // it. A harness that cannot fail is worthless. (The stronger teeth proof is
    // the baseline struct-alias bug below, which the parity check catches by
    // returning the *wrong value* from a structurally valid module.)
    let module = module_for("fn main -> i64\n    return 7\n");
    let good = emit_wasm_module(&module).expect("emit").bytes;
    let engine = Engine::default();
    assert!(
        WasmiModule::new(&engine, &good).is_ok(),
        "the pristine module must instantiate"
    );
    let mut truncated = good.clone();
    truncated.truncate(truncated.len() - 4);
    assert!(
        WasmiModule::new(&engine, &truncated).is_err(),
        "wasmi accepted a truncated module — the harness has no teeth"
    );
}

// -- The bug: struct value-semantics on `let g = f` (copy, not alias) ----------

/// The minimal scalar-struct copy-isolation repro from the task. On baseline
/// `98cc1fe` WASM returns 202 (mutation of the copy leaked back into `f`) where
/// the interpreters and native return 104.
const SCALAR_STRUCT_COPY_REPRO: &str = "\
struct P
    a i64
    b i64

fn main -> i64
    let f P = P(100, 4)
    let g P = f
    g.a = 99
    return f.a + f.b
";

#[test]
fn let_binding_struct_is_value_copy_not_alias() {
    assert_parity(SCALAR_STRUCT_COPY_REPRO, 104);
}

// -- Blast-radius parity: every observable value-copy shape --------------------

#[test]
fn assign_struct_is_value_copy_not_alias() {
    // `g = f` (both already declared) must copy, exactly like `let g = f`.
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let f P = P(100, 4)
    let g P = P(0, 0)
    g = f
    g.a = 99
    return f.a + f.b
";
    assert_parity(source, 104);
}

#[test]
fn let_binding_struct_copy_source_mutation_does_not_leak_into_copy() {
    // Opposite direction: mutating the SOURCE after the copy must not change the
    // copy. Proves the copy is a genuine snapshot, not a shared alias either way.
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let f P = P(100, 4)
    let g P = f
    f.a = 7
    return g.a + g.b
";
    assert_parity(source, 104);
}

#[test]
fn let_binding_array_is_value_copy_not_alias() {
    // Arrays are value types with in-place element mutation, so `let ys = xs`
    // then `ys[0] = 99` must not touch `xs`.
    let source = "\
fn main -> i64
    let xs array<i64> = [10, 20, 30]
    let ys array<i64> = xs
    ys[0] = 99
    return xs[0] + xs[1] + xs[2]
";
    assert_parity(source, 60);
}

#[test]
fn let_binding_struct_with_array_field_deep_copies_the_field() {
    // A struct with a nested mutable array field: copying the struct must deep-
    // copy the array too, so mutating the copy's element leaves the source intact.
    let source = "\
struct Box
    items array<i64>
    n i64

fn main -> i64
    let a Box = Box([1, 2, 3], 10)
    let b Box = a
    b.items[0] = 99
    return a.items[0] + a.items[1] + a.items[2] + a.n
";
    assert_parity(source, 16);
}

#[test]
fn field_assign_of_aggregate_is_value_copy_not_alias() {
    // `o.inner = q` stores an independent copy of `q` into the field, so a later
    // mutation of `q` is not observable through `o.inner` (the path-assign site).
    let source = "\
struct Inner
    v i64

struct Outer
    inner Inner
    tag i64

fn main -> i64
    let q Inner = Inner(5)
    let o Outer = Outer(Inner(0), 10)
    o.inner = q
    q.v = 99
    return o.inner.v + o.tag
";
    assert_parity(source, 15);
}

#[test]
fn param_locally_copied_inside_callee_is_isolated() {
    // A struct passed by value into a function, then locally copied and mutated:
    // the local copy must not disturb the (already independent) parameter record.
    let source = "\
struct P
    a i64
    b i64

fn tweak p P -> i64
    let q P = p
    q.a = 1
    return p.a + p.b

fn main -> i64
    let f P = P(100, 4)
    return tweak(f)
";
    assert_parity(source, 104);
}

#[test]
fn returned_struct_then_copied_is_isolated() {
    // A struct returned from a call (fresh record) then bound and copied: the
    // copy mutation must not leak into the original binding.
    let source = "\
struct P
    a i64
    b i64

fn make -> P
    return P(100, 4)

fn main -> i64
    let x P = make()
    let y P = x
    y.a = 1
    return x.a + x.b
";
    assert_parity(source, 104);
}

#[test]
fn chained_copies_stay_independent() {
    // A -> B -> C copy chain: mutating each copy must leave every earlier value
    // untouched. Exercises repeated deep-copy of the same source shape.
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let f P = P(10, 1)
    let g P = f
    let h P = g
    g.a = 20
    h.a = 30
    return f.a + g.a + h.a + f.b
";
    assert_parity(source, 61);
}

#[test]
fn fresh_construction_binding_is_correct() {
    // A `let p = Struct(...)` (fresh construction, NOT an alias) is left uncopied
    // by the fix; confirm it still produces the right value (no double alloc bug).
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let p P = P(100, 4)
    p.a = 200
    return p.a + p.b
";
    assert_parity(source, 204);
}

// -- Non-observable shapes: parity must still hold (regression guards) ---------

#[test]
fn enum_copy_and_match_parity() {
    // Enums have no in-place payload mutation, so `let g = e` aliasing is not
    // observable; the fix deep-copies them for consistency. Confirm the copy +
    // match still yields the correct value on both tiers.
    let source = "\
fn main -> i64
    let e option<i64> = some(41)
    let g option<i64> = e
    match g
        some(v) -> v + 1
        none -> 0
";
    assert_parity(source, 42);
}

#[test]
fn list_copy_and_set_parity() {
    // Lists mutate functionally (`set` returns a new list), so `let ys = xs`
    // aliasing is not observable, but the value-semantic result must match.
    let source = "\
fn main -> i64
    let xs list<i64> = list_new()
    let a list<i64> = push(xs, 5)
    let b list<i64> = push(a, 10)
    let c list<i64> = set(b, 0, 100)
    return get(b, 0) + get(c, 0) + len(b)
";
    assert_parity(source, 107);
}

#[test]
fn list_variable_copy_then_functional_mutation_parity() {
    // Bind a list to a second variable and mutate through the copy functionally;
    // the original must be unchanged and both tiers must agree.
    let source = "\
fn main -> i64
    let xs list<i64> = list_new()
    let a list<i64> = push(xs, 7)
    let b list<i64> = push(a, 8)
    let c list<i64> = b
    let d list<i64> = set(c, 0, 99)
    return get(b, 0) + get(d, 0)
";
    assert_parity(source, 106);
}

// -- Construction from an lvalue operand must value-copy, not alias -----------
// A literal/construction built from an aggregate LVALUE aliases that operand
// unless each such operand is deep-copied at the store. A freshly constructed
// operand already owns its record and must NOT be copied (parity would still
// hold, but the redundant copy would be wasteful and change bytes).

#[test]
fn array_literal_with_struct_element_copies_the_operand() {
    // `[f]` must snapshot `f`; mutating `f` afterward must not change `g[0]`.
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let f P = P(1, 2)
    let g array<P> = [f]
    f.a = 99
    return g[0].a + g[0].b
";
    assert_parity(source, 3);
}

#[test]
fn struct_construction_with_struct_field_copies_the_operand() {
    // `Outer(q, 2)` must snapshot `q`; mutating `q` afterward must not change
    // `o.inner`.
    let source = "\
struct Inner
    v i64

struct Outer
    inner Inner
    tag i64

fn main -> i64
    let q Inner = Inner(1)
    let o Outer = Outer(q, 2)
    q.v = 99
    return o.inner.v + o.tag
";
    assert_parity(source, 3);
}

#[test]
fn enum_construction_with_struct_payload_copies_the_operand() {
    // `some(f)` (an `option<struct>`) must snapshot `f`; mutating `f` afterward
    // must not change the payload observed through `match`.
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let f P = P(1, 2)
    let e option<P> = some(f)
    f.a = 99
    match e
        some(p) -> p.a + p.b
        none -> 0
";
    assert_parity(source, 3);
}

#[test]
fn list_push_of_struct_copies_the_operand() {
    // `push(xs, f)` on a `list<struct>` must snapshot `f`; mutating `f` afterward
    // must not change the stored element.
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let xs list<P> = list_new()
    let f P = P(1, 2)
    let ys list<P> = push(xs, f)
    f.a = 99
    let g P = get(ys, 0)
    return g.a + g.b
";
    assert_parity(source, 3);
}

#[test]
fn map_set_of_struct_copies_the_operand() {
    // `map_set(m, k, f)` on a `map<i64, struct>` must snapshot `f`; mutating `f`
    // afterward must not change the stored value (map_set already deep-copies —
    // regression guard confirming the whole value-copy class is covered).
    let source = "\
struct P
    a i64
    b i64

fn main -> i64
    let m map<i64, P> = map_new()
    let f P = P(1, 2)
    let m2 map<i64, P> = map_set(m, 7, f)
    f.a = 99
    match map_get(m2, 7)
        some(p) -> p.a + p.b
        none -> 0
";
    assert_parity(source, 3);
}

#[test]
fn nested_construction_operand_binds_directly_and_is_correct() {
    // A construction operand that is itself a FRESH construction (not an lvalue)
    // owns its record; confirm nested construction still yields the right value.
    let source = "\
struct Inner
    v i64

struct Outer
    inner Inner
    tag i64

fn main -> i64
    let o Outer = Outer(Inner(5), 2)
    o.inner.v = 40
    return o.inner.v + o.tag
";
    assert_parity(source, 42);
}

#[test]
fn region_block_runs_value_neutrally_under_wasm() {
    // The explicit `region` block runs value-neutrally on WASM: its body executes as
    // an ordinary nested scope and the linear-memory heap never reclaims, so WASM
    // agrees with the IR interpreter. `len("7!!")` folded into an outer scalar = 3.
    let source = "\
fn main -> i64
    let total i64 = 0
    region
        let s string = to_string(7) + \"!!\"
        total = total + len(s)
    total
";
    assert_parity(source, 3);
}

#[test]
fn region_block_shadow_keeps_outer_under_wasm() {
    // A region-block `let v` shadows an outer `v`: the WASM lowering snapshots and
    // restores the name->local maps around the region body, so the block-local
    // shadow does not leak its slot past dedent — the WASM counterpart of the native
    // scope-renamer / interpreter scope push. The outer `v` (17) survives.
    let source = "\
fn main -> i64
    let v i64 = 17
    region
        let v i64 = 5
        v = v + 100
    v
";
    assert_parity(source, 17);
}
