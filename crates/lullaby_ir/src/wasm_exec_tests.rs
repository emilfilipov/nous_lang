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

// -- Out-of-bounds element access: WASM must TRAP where the interpreter errors --
//
// For an out-of-range index the IR interpreter raises `L0413` (an ERROR, not a
// value), and the WASM backend's explicit unsigned bounds check must TRAP (wasmi
// returns `Err`) at the same access — a trap-vs-error parity, not the
// value-vs-value parity `assert_parity` checks. Before the bounds-check fix the
// WASM path computed a raw linear-memory offset and returned a (corrupted) value
// instead of trapping, so each case below FAILS on the pre-fix emitter (the
// `wasm_main_traps` assert fires: it returned a value) and PASSES after it.

/// `true` iff running `main` on the IR interpreter raises the `L0413`
/// out-of-bounds error (not `Ok`, and not a different error code).
fn interp_main_is_l0413(source: &str) -> bool {
    matches!(run_main(&module_for(source)), Err(e) if e.code == "L0413")
}

/// Emit + run the WASM module for `source` and return `true` iff calling `main`
/// TRAPS (wasmi returns `Err` — the `unreachable` the bounds check emits). A
/// pipeline/instantiation failure still panics loudly, so a broken emission is a
/// test failure, never mistaken for a trap.
fn wasm_main_traps(source: &str) -> bool {
    let module = module_for(source);
    let artifact = emit_wasm_module(&module).expect("emit wasm");
    let engine = Engine::default();
    let wmodule = WasmiModule::new(&engine, &artifact.bytes).expect("wasmi parse/validate module");
    let mut store = Store::new(&engine, ());
    let mut linker = <Linker<()>>::new(&engine);
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
        .instantiate_and_start(&mut store, &wmodule)
        .expect("instantiate");
    let main = instance
        .get_typed_func::<(), i64>(&store, "main")
        .expect("main export with signature () -> i64");
    main.call(&mut store, ()).is_err()
}

/// Assert the IR interpreter raises `L0413` AND the WASM module TRAPS for
/// `source` — the out-of-bounds analogue of [`assert_parity`].
fn assert_oob_traps_like_interp(source: &str) {
    assert!(
        interp_main_is_l0413(source),
        "expected the IR interpreter to raise L0413 (out of bounds) for:\n{source}"
    );
    assert!(
        wasm_main_traps(source),
        "WASM did NOT trap on the out-of-bounds access (it returned a value — the \
         linear-memory overrun bug) for:\n{source}"
    );
}

#[test]
fn array_oob_read_traps() {
    // Reading past the end of a fixed array: interpreter L0413, WASM must trap
    // rather than load a neighboring heap word.
    let source = "\
fn main -> i64
    let a array<i64> = [10, 20, 30, 40]
    return a[4]
";
    assert_oob_traps_like_interp(source);
}

#[test]
fn array_oob_write_corrupts_neighbor_now_traps() {
    // The task's minimal repro: `a[5] = 777` on a len-4 array silently overwrote a
    // neighboring array `b` on the pre-fix WASM emitter (no trap). It must now trap
    // exactly where the interpreters raise L0413.
    let source = "\
fn main -> i64
    let a array<i64> = [1, 1, 1, 1]
    let b array<i64> = [2, 2, 2, 2]
    a[5] = 777
    return b[0] * 1000 + b[1] * 100 + a[0]
";
    assert_oob_traps_like_interp(source);
}

#[test]
fn array_negative_index_traps() {
    // A negative index must trip the SAME trap (the check is `i32.ge_u`, so a
    // negative index folds to a huge unsigned value and exceeds len). The
    // interpreters raise L0413 for a negative index too.
    let source = "\
fn main -> i64
    let a array<i64> = [10, 20, 30, 40]
    let i i64 = 0 - 1
    return a[i]
";
    assert_oob_traps_like_interp(source);
}

#[test]
fn array_oob_with_opaque_loop_index_traps() {
    // An index the optimizer cannot fold (derived by a runtime loop) still traps:
    // the check is emitted for every index, constant or dynamic. `j` ends at 9.
    let source = "\
fn main -> i64
    let a array<i64> = [10, 20, 30, 40]
    let i i64 = 0
    let j i64 = 0
    while i < 10
        j = i
        i = i + 1
    return a[j]
";
    assert_oob_traps_like_interp(source);
}

#[test]
fn struct_array_field_oob_write_traps() {
    // OOB write through a struct's array field (`a.xs[6] = 777`) folds a Field then
    // an Index hop; the Index hop routes through the same bounds check and must
    // trap, matching the interpreters' L0413.
    let source = "\
struct Box
    xs array<i64>
    n i64

fn main -> i64
    let a Box = Box([1, 2, 3, 4], 9)
    a.xs[6] = 777
    return a.n
";
    assert_oob_traps_like_interp(source);
}

#[test]
fn list_get_oob_traps() {
    // `get(l, i)` past the live length: interpreter L0413, WASM must trap on the
    // explicit `len`-header check rather than read an unused backing slot.
    let source = "\
fn main -> i64
    let xs list<i64> = list_new()
    let a list<i64> = push(xs, 5)
    let b list<i64> = push(a, 10)
    return get(b, 5)
";
    assert_oob_traps_like_interp(source);
}

#[test]
fn list_set_oob_traps() {
    // `set(l, i, x)` past the live length must trap too (the write path shares the
    // same `emit_list_elem_offset` bounds check as `get`).
    let source = "\
fn main -> i64
    let xs list<i64> = list_new()
    let a list<i64> = push(xs, 5)
    let b list<i64> = set(a, 3, 99)
    return get(b, 0)
";
    assert_oob_traps_like_interp(source);
}

#[test]
fn list_pop_empty_traps() {
    // Popping an empty list is L0413 on the interpreters; the WASM path traps on
    // `len == 0` instead of storing a bogus -1 length.
    let source = "\
fn main -> i64
    let xs list<i64> = list_new()
    let a list<i64> = pop(xs)
    return len(a)
";
    assert_oob_traps_like_interp(source);
}

// -- In-bounds boundary parity: the check must NOT break correct programs -------

#[test]
fn array_last_valid_index_still_reads() {
    // The highest in-range index (len-1) must load normally — the unsigned check
    // is `index >= len`, so `a[3]` on a len-4 array is allowed.
    let source = "\
fn main -> i64
    let a array<i64> = [10, 20, 30, 40]
    return a[3]
";
    assert_parity(source, 40);
}

#[test]
fn array_dynamic_in_bounds_index_reads_and_writes() {
    // A runtime-derived in-range index reads and writes correctly through the
    // bounds check (no false trap on a valid dynamic index).
    let source = "\
fn main -> i64
    let a array<i64> = [10, 20, 30, 40]
    let i i64 = 0
    let sum i64 = 0
    while i < 4
        a[i] = a[i] + 1
        sum = sum + a[i]
        i = i + 1
    return sum
";
    assert_parity(source, 104);
}

#[test]
fn list_last_valid_index_get_and_set_ok() {
    // In-bounds `get`/`set` at the highest live index stay byte-correct after the
    // check is added.
    let source = "\
fn main -> i64
    let xs list<i64> = list_new()
    let a list<i64> = push(xs, 5)
    let b list<i64> = push(a, 10)
    let c list<i64> = set(b, 1, 100)
    return get(c, 1) + get(b, 1)
";
    assert_parity(source, 110);
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

// -- Generated out-of-bounds / boundary index fuzzing (trap-vs-value parity) ----
//
// The pinned OOB cases above are hand-written. This section GENERATES the class
// continuously so the memory-safety miscompile it guards (a WASM element access
// with no bounds check silently reading/writing a neighboring heap object) can
// never silently regress, and nearby variants get discovered automatically.
//
// The generator emits random small programs that build `array<i64>` /
// `list<i64>` / struct-array-field aggregates and read/write at an index drawn
// from a mix of: clearly in-bounds, the exact boundaries (`0`, `len-1`, `len`),
// negative, and far-out-of-range — some as constant literals, some as **opaque
// dynamic** expressions the optimizer cannot constant-fold (a function parameter
// or a loop-derived local). For each program it runs all four in-process tiers
// (AST, IR, bytecode interpreters + `wasmi`) and enforces the parity rule:
//
//   * in-bounds     ⇒ every tier returns the SAME `i64` value;
//   * out-of-bounds ⇒ every interpreter raises `L0413` AND `wasmi` TRAPS
//     (`main.call` returns `Err`) — never a value.
//
// The generator PREDICTS oob/in-bounds from the drawn index and the known length,
// and the oracle cross-checks that prediction against the interpreters' actual
// behavior, so a wrong prediction is itself a loud failure — the fuzzer cannot
// silently classify everything as in-bounds and pass toothless. The teeth for the
// real bug are the wasm-vs-interpreter comparisons: a tier returning a value where
// another traps, or two tiers returning different values, fails with the seed and
// the reproducing program.

/// Deterministic xorshift64 PRNG — seed-reproducible, no external `rand`, so a
/// discovered mismatch reproduces exactly from its printed seed.
struct OobRng(u64);

impl OobRng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform in `0..n` (n > 0).
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo + 1) as u64;
        lo + (self.next_u64() % span) as i64
    }
}

/// Normalize an interpreter result to `Ok(i64)` on any integer/bool value or
/// `Err(code)` on a runtime error (the diagnostic code string, e.g. `"L0413"`).
fn oob_norm(r: Result<Value, lullaby_runtime::RuntimeError>) -> Result<i64, String> {
    match r {
        Ok(Value::I64(n)) => Ok(n),
        Ok(Value::Int { value, .. }) => Ok(value),
        Ok(Value::Bool(b)) => Ok(b as i64),
        Ok(other) => Err(format!("non-i64:{other:?}")),
        Err(e) => Err(e.code.to_string()),
    }
}

/// Run `source` on the three interpreter tiers (AST tree-walker, IR, bytecode VM)
/// and return each tier's normalized result. A value-vs-error divergence between
/// the three is itself a finding the caller asserts against.
fn oob_interp_results(source: &str) -> [Result<i64, String>; 3] {
    let tokens = lex(source).expect("lex oob program");
    let program = parse(&tokens).expect("parse oob program");
    let checked = validate(&program).expect("validate oob program");
    let ast = oob_norm(lullaby_runtime::run_main_with_args(
        &checked.program,
        Vec::new(),
    ));
    let module = lower(&checked).expect("lower oob program");
    let ir = oob_norm(crate::run_main_with_args(&module, Vec::new()));
    let bc = crate::lower_to_bytecode(&module);
    let bcr = oob_norm(crate::run_bytecode_main_with_args(&bc, Vec::new()));
    [ast, ir, bcr]
}

/// Emit + run the WASM module for `source` under `wasmi`, returning `Ok(value)`
/// when `main` returns and `Err(())` when it TRAPS (the `unreachable` the bounds
/// check emits). A pipeline/instantiation failure still panics loudly, so a broken
/// emission is a test failure, never mistaken for a trap.
fn oob_wasm_result(source: &str) -> Result<i64, ()> {
    let module = module_for(source);
    let artifact = emit_wasm_module(&module).expect("emit wasm");
    let engine = Engine::default();
    let wmodule = WasmiModule::new(&engine, &artifact.bytes).expect("wasmi parse/validate module");
    let mut store = Store::new(&engine, ());
    let mut linker = <Linker<()>>::new(&engine);
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
        .instantiate_and_start(&mut store, &wmodule)
        .expect("instantiate");
    let main = instance
        .get_typed_func::<(), i64>(&store, "main")
        .expect("main export with signature () -> i64");
    main.call(&mut store, ()).map_err(|_| ())
}

/// Draw an index for a container of length `len` (>= 1) from the mixed pool:
/// interior in-bounds, the exact boundaries (`0`, `len-1`, `len`), negatives, and
/// far-out-of-range on both sides. Roughly half the draws land out of bounds.
fn oob_draw_index(rng: &mut OobRng, len: i64) -> i64 {
    match rng.below(9) {
        0 => rng.range(0, len - 1),  // interior, in-bounds
        1 => 0,                      // low boundary, in-bounds
        2 => len - 1,                // high boundary, in-bounds
        3 => len,                    // just past the end, OOB
        4 => -1,                     // negative boundary, OOB
        5 => rng.range(-40, -2),     // far negative, OOB
        6 => len + rng.range(1, 20), // far positive, OOB
        7 => len + 1,                // just-past + 1, OOB
        _ => rng.range(0, len + 3),  // mixed: may land either side
    }
}

/// Build a `list<i64>` of `len` random elements via a `list_new()` + `push` chain,
/// leaving the final list in local `l{len}`. Returns the source lines.
fn oob_build_list(rng: &mut OobRng, len: i64) -> String {
    let mut s = String::from("    let l0 list<i64> = list_new()\n");
    for k in 0..len {
        let e = rng.range(-40, 40);
        s.push_str(&format!("    let l{} list<i64> = push(l{k}, {e})\n", k + 1));
    }
    s
}

/// Generate one OOB/boundary program and predict whether its access is out of
/// bounds. Shapes rotate over `array<i64>` reads/writes (constant, opaque-param,
/// and loop-derived indices), `list<i64>` `get`/`set`/`pop`, and a struct
/// array-field read — every construct the WASM emitter and all three interpreters
/// support and agree on. The returned `bool` is the predicted-OOB flag the oracle
/// cross-checks against the interpreters' actual behavior.
fn gen_oob_program(seed: u64) -> (String, bool) {
    let mut rng = OobRng(seed | 1);
    let len = rng.range(1, 6);
    let idx = oob_draw_index(&mut rng, len);
    let oob = idx < 0 || idx >= len;
    let elems: Vec<String> = (0..len).map(|_| rng.range(-40, 40).to_string()).collect();
    let arr = format!("[{}]", elems.join(", "));
    let elems2: Vec<String> = (0..len).map(|_| rng.range(-40, 40).to_string()).collect();
    let arr2 = format!("[{}]", elems2.join(", "));
    let v = rng.range(-99, 99);
    let n = rng.range(-30, 30);

    // A loop that lands `j` exactly on `idx` — an index the optimizer cannot
    // constant-fold. Only valid for idx >= 0 (a `for` up-count reaches it).
    let loop_to_idx = |target: i64| -> String {
        format!("    let j i64 = 0\n    for k from 0 to {target}\n        j = k\n")
    };

    match rng.below(9) {
        // array read, constant index.
        0 => (
            format!("fn main -> i64\n    let xs array<i64> = {arr}\n    return xs[{idx}]\n"),
            oob,
        ),
        // array read, OPAQUE parameter index (any sign — the index is a callee
        // parameter, so no caller-side folding can prove it in range).
        1 => (
            format!(
                "fn at a array<i64> i i64 -> i64\n    return a[i]\n\n\
                 fn main -> i64\n    let xs array<i64> = {arr}\n    return at(xs, {idx})\n"
            ),
            oob,
        ),
        // array read, LOOP-DERIVED index (idx >= 0); else fall back to opaque param.
        2 if idx >= 0 => (
            format!(
                "fn main -> i64\n    let xs array<i64> = {arr}\n{loop}    return xs[j]\n",
                loop = loop_to_idx(idx)
            ),
            oob,
        ),
        2 => (
            format!(
                "fn at a array<i64> i i64 -> i64\n    return a[i]\n\n\
                 fn main -> i64\n    let xs array<i64> = {arr}\n    return at(xs, {idx})\n"
            ),
            oob,
        ),
        // array WRITE + neighbor read: the exact heap-corruption shape. A constant
        // OOB write into `xs` must trap, not scribble on the neighbor `ys`.
        3 => (
            format!(
                "fn main -> i64\n    let xs array<i64> = {arr}\n    \
                 let ys array<i64> = {arr2}\n    xs[{idx}] = {v}\n    return ys[0]\n"
            ),
            oob,
        ),
        // array WRITE, loop-derived index (idx >= 0); else a constant write.
        4 if idx >= 0 => (
            format!(
                "fn main -> i64\n    let xs array<i64> = {arr}\n{loop}    xs[j] = {v}\n    \
                 return xs[0]\n",
                loop = loop_to_idx(idx)
            ),
            oob,
        ),
        4 => (
            format!(
                "fn main -> i64\n    let xs array<i64> = {arr}\n    \
                 let ys array<i64> = {arr2}\n    xs[{idx}] = {v}\n    return ys[0]\n"
            ),
            oob,
        ),
        // list get, constant index.
        5 => (
            format!(
                "fn main -> i64\n{list}    return get(l{len}, {idx})\n",
                list = oob_build_list(&mut rng, len)
            ),
            oob,
        ),
        // list set, constant index (the write path shares the same bounds check).
        6 => (
            format!(
                "fn main -> i64\n{list}    let m list<i64> = set(l{len}, {idx}, {v})\n    \
                 return get(m, 0)\n",
                list = oob_build_list(&mut rng, len)
            ),
            oob,
        ),
        // list pop: pop `pop_count` times; popping past empty (pop_count > len) is
        // the L0413 shape. This shape computes its OWN oob flag from pop_count.
        7 => {
            let pop_count = rng.range(1, len + 1);
            let oob_pop = pop_count > len;
            let mut body = format!(
                "fn main -> i64\n{list}    let p0 list<i64> = l{len}\n",
                list = oob_build_list(&mut rng, len)
            );
            for k in 0..pop_count {
                body.push_str(&format!("    let p{} list<i64> = pop(p{k})\n", k + 1));
            }
            body.push_str(&format!("    return len(p{pop_count})\n"));
            (body, oob_pop)
        }
        // struct array-field read: `a.xs[idx]` folds a Field then an Index hop.
        _ => (
            format!(
                "struct Box\n    xs array<i64>\n    n i64\n\n\
                 fn main -> i64\n    let a Box = Box({arr}, {n})\n    return a.xs[{idx}]\n"
            ),
            oob,
        ),
    }
}

/// The oracle: run all four tiers on `source`, cross-check the predicted-OOB flag
/// against the interpreters, and enforce the trap-vs-value parity rule. Returns
/// `true` iff the program's access was out of bounds (for coverage counting).
fn check_oob_parity(seed: u64, source: &str, predicted_oob: bool) -> bool {
    let [ast, ir, bc] = oob_interp_results(source);
    // The three interpreters must agree with each other first — a value on one and
    // an error on another is a backend divergence, never blamed on WASM.
    assert!(
        ast == ir && ir == bc,
        "interpreter divergence on oob-fuzz (seed {seed:#x}):\n{source}\n\
         ast={ast:?} ir={ir:?} bytecode={bc:?}"
    );
    let wasm = oob_wasm_result(source);

    if predicted_oob {
        // Predicted OOB: every interpreter must raise L0413, and WASM must TRAP.
        // A mismatch here is EITHER a generator misprediction (interp returned a
        // value) OR the miscompile this fuzzer exists to catch (WASM returned a
        // value where the interpreters trap).
        assert_eq!(
            ast,
            Err("L0413".to_string()),
            "predicted OOB but the interpreters did not raise L0413 (seed {seed:#x}):\n{source}\n\
             got {ast:?} — the generator mispredicted, or the interpreter bounds check regressed"
        );
        assert!(
            wasm.is_err(),
            "WASM did NOT trap on an out-of-bounds access — it returned {wasm:?}, the \
             linear-memory overrun miscompile (seed {seed:#x}):\n{source}"
        );
        true
    } else {
        // Predicted in-bounds: every tier must return the SAME value.
        let expected = match &ast {
            Ok(n) => *n,
            Err(code) => panic!(
                "predicted in-bounds but the interpreters raised {code} (seed {seed:#x}):\n{source}\n\
                 the generator mispredicted an in-bounds index as reachable"
            ),
        };
        assert_eq!(
            wasm,
            Ok(expected),
            "WASM diverged from the interpreters on an in-bounds access (wasm={wasm:?}, \
             expected {expected}) (seed {seed:#x}):\n{source}"
        );
        false
    }
}

#[test]
fn fuzz_oob_wasm_and_interpreters_parity() {
    // The four-tier (AST/IR/bytecode + wasmi) out-of-bounds oracle. Always runs —
    // everything is in-process, no toolchain needed. Asserts a nonzero count of
    // BOTH out-of-bounds and in-bounds programs, so a batch that generated only
    // in-bounds indices (and would prove nothing about the trap) is itself a
    // failure.
    const PROGRAMS: u64 = 500;
    let base_seed = 0x0B0D_5A17_C3E9_1F42u64;
    let mut oob_count = 0u64;
    let mut inbounds_count = 0u64;

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let (source, predicted_oob) = gen_oob_program(seed);
        if check_oob_parity(seed, &source, predicted_oob) {
            oob_count += 1;
        } else {
            inbounds_count += 1;
        }
    }

    // Coverage must be legible AND real: both classes must have been exercised, or
    // the parity assertion above proved nothing about the regime it targets.
    assert!(
        oob_count > 0,
        "the OOB fuzz generated NO out-of-bounds programs — it is toothless (in-bounds \
         {inbounds_count})"
    );
    assert!(
        inbounds_count > 0,
        "the OOB fuzz generated NO in-bounds programs — the bounds check's correctness \
         (no false trap) went unchecked (oob {oob_count})"
    );
    eprintln!(
        "oob wasm/interp fuzz: {PROGRAMS} programs — {oob_count} out-of-bounds (interp L0413 + \
         wasmi trap), {inbounds_count} in-bounds (four-tier value parity)"
    );
}
