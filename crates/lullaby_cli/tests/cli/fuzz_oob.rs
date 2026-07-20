//! Out-of-bounds / boundary index differential oracle (native tier). A submodule
//! of `fuzz.rs`, reusing its shared `Rng`, `ScratchDir`, `ensure_msvc_env`,
//! `native_exe_runnable`, and `fuzz_native_exit` harness via `use super::*`.
//!
//! # Why this fuzzer exists
//!
//! A phase-1 hardening sweep found a real memory-safety miscompile: WASM
//! `array`/`list` element access had no bounds check, so an out-of-bounds index
//! silently read/WROTE a neighboring heap object while the interpreters raise
//! `L0413` and native traps with `ud2`. It was green across the entire existing
//! suite because the differential fuzzers generate NO out-of-bounds indices — the
//! class was reachable only by hand. The WASM half of the permanent oracle lives
//! in `lullaby_ir::wasm::exec_tests` (`fuzz_oob_wasm_and_interpreters_parity`,
//! four-tier AST/IR/bytecode + `wasmi`). This file is the **native** half: it
//! generates the same class and asserts a trap-vs-value parity between the three
//! interpreters and a real `.exe`.
//!
//! # The parity rule
//!
//! For each generated program, run every applicable tier and require EITHER all
//! tiers produce the same value (in-bounds) OR all tiers fail consistently
//! (out-of-bounds ⇒ every interpreter raises `L0413`, native aborts with the
//! defined `ud2` bounds-trap `STATUS_ILLEGAL_INSTRUCTION` = `0xC000001D`, empty
//! stdout — never a wrong value). Native never returns a value where an
//! interpreter traps, and never traps where they return a value.
//!
//! # Native eligibility note
//!
//! A *constant* out-of-bounds `array` index is statically refused by the native
//! i64-scalar subset (L0339, no exe), so this generator delivers every
//! out-of-bounds `array` index **opaquely** — through a function parameter or a
//! loop-derived local the subset analysis cannot fold — which forces a runtime
//! `ud2` bounds-trap instead. `list` indices route through the runtime `len`-header
//! check regardless, so those use constant indices directly. Every generated
//! program therefore lowers to a real exe (asserted by `fuzz_native_exit`), so a
//! regression that stopped emitting the check is a loud failure, not a skip.

use super::*;

/// The defined `ud2` bounds-trap exit status on Windows (`STATUS_ILLEGAL_INSTRUCTION`).
/// A native out-of-bounds access aborts with exactly this — never a wrong value
/// (exit 0/N) and never a heap-corrupting access violation (`0xC0000005`). Matches
/// the pins in `suite13.rs`.
const STATUS_ILLEGAL_INSTRUCTION: i32 = 0xC000_001Du32 as i32;

/// Normalize an interpreter result to `Ok(i64)` on any integer/bool value or
/// `Err(code)` on a runtime error (the diagnostic code, e.g. `"L0413"`).
fn oob_norm(r: Result<Value, lullaby_runtime::RuntimeError>) -> Result<i64, String> {
    match r {
        Ok(Value::I64(n)) => Ok(n),
        Ok(Value::Int { value, .. }) => Ok(value),
        Ok(Value::Bool(b)) => Ok(b as i64),
        Ok(other) => Err(format!("non-i64:{other:?}")),
        Err(e) => Err(e.code.to_string()),
    }
}

/// Run `source` on the AST, IR, and bytecode interpreters, returning each tier's
/// normalized result. Unlike `run_interpreters` (which collapses every error to
/// one `Outcome::Error`), this preserves the diagnostic CODE so the oracle can
/// require `L0413` specifically for an out-of-bounds access.
fn oob_interp_results(source: &str) -> [Result<i64, String>; 3] {
    let tokens = lex(source).expect("lex oob program");
    let program = parse(&tokens).expect("parse oob program");
    let checked = validate_executable(&program).expect("validate oob program");
    let ast = oob_norm(run_ast_main(&checked.program, Vec::new()));
    let module = lower(&checked).expect("lower oob program");
    let ir = oob_norm(run_ir_main(&module, Vec::new()));
    let bc = lower_to_bytecode(&module);
    let bcr = oob_norm(run_bytecode_main_with_args(&bc, Vec::new()));
    [ast, ir, bcr]
}

/// Draw an index for a container of length `len` (>= 1) from the mixed pool:
/// interior in-bounds, the exact boundaries (`0`, `len-1`, `len`), negatives, and
/// far-out-of-range on both sides. Roughly half the draws land out of bounds.
fn oob_draw_index(rng: &mut Rng, len: i64) -> i64 {
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
/// leaving the final list in local `l{len}`.
fn oob_build_list(rng: &mut Rng, len: i64) -> String {
    let mut s = String::from("    let l0 list<i64> = list_new()\n");
    for k in 0..len {
        let e = rng.range(-40, 40);
        s.push_str(&format!("    let l{} list<i64> = push(l{k}, {e})\n", k + 1));
    }
    s
}

/// Generate one native-eligible OOB/boundary program and predict whether its
/// access is out of bounds. `array` indices are always delivered opaquely (a
/// parameter or a loop-derived local) so a constant OOB index is never statically
/// refused; `list` indices are constant (they route through the runtime `len`
/// check). Every shape lowers to a real exe. The returned `bool` is the
/// predicted-OOB flag the oracle cross-checks against the interpreters.
fn gen_oob_native_program(seed: u64) -> (String, bool) {
    let mut rng = Rng(seed | 1);
    let len = rng.range(1, 6);
    let idx = oob_draw_index(&mut rng, len);
    let oob = idx < 0 || idx >= len;
    let elems: Vec<String> = (0..len).map(|_| rng.range(-40, 40).to_string()).collect();
    let arr = format!("[{}]", elems.join(", "));
    let v = rng.range(-99, 99);

    // A loop that lands `j` exactly on `idx` (opaque to the subset's fold), valid
    // only for idx >= 0.
    let loop_to_idx = |target: i64| -> String {
        format!("    let j i64 = 0\n    for k from 0 to {target}\n        j = k\n")
    };

    match rng.below(7) {
        // array read via an OPAQUE parameter index — works for any sign, so this is
        // the universal array OOB carrier (constant OOB array indices are refused
        // by native eligibility; a parameter index forces a runtime trap instead).
        0 | 1 => (
            format!(
                "fn at a array<i64> i i64 -> i64\n    return a[i]\n\n\
                 fn main -> i64\n    let xs array<i64> = {arr}\n    return at(xs, {idx})\n"
            ),
            oob,
        ),
        // array read via a LOOP-DERIVED index (idx >= 0); else the opaque param.
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
        // array WRITE via a loop-derived index (idx >= 0); else the opaque read.
        3 if idx >= 0 => (
            format!(
                "fn main -> i64\n    let xs array<i64> = {arr}\n{loop}    xs[j] = {v}\n    \
                 return xs[0]\n",
                loop = loop_to_idx(idx)
            ),
            oob,
        ),
        3 => (
            format!(
                "fn at a array<i64> i i64 -> i64\n    return a[i]\n\n\
                 fn main -> i64\n    let xs array<i64> = {arr}\n    return at(xs, {idx})\n"
            ),
            oob,
        ),
        // list get, constant index (the runtime `len`-header check traps on OOB).
        4 => (
            format!(
                "fn main -> i64\n{list}    return get(l{len}, {idx})\n",
                list = oob_build_list(&mut rng, len)
            ),
            oob,
        ),
        // list set, constant index (shares the same bounds check as `get`).
        5 => (
            format!(
                "fn main -> i64\n{list}    let m list<i64> = set(l{len}, {idx}, {v})\n    \
                 return get(m, 0)\n",
                list = oob_build_list(&mut rng, len)
            ),
            oob,
        ),
        // list pop: pop `pop_count` times; popping past empty is the L0413 shape.
        // Computes its OWN oob flag from pop_count.
        _ => {
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
    }
}

/// Cross-check the predicted-OOB flag against the three interpreters (a wrong
/// prediction is itself a loud failure) and return the ground-truth expectation:
/// `Err(())` for an out-of-bounds program (every interpreter raises `L0413`) or
/// `Ok(value)` for an in-bounds one (all three agree on the value).
fn oob_interp_expectation(seed: u64, source: &str, predicted_oob: bool) -> Result<i64, ()> {
    let [ast, ir, bc] = oob_interp_results(source);
    assert!(
        ast == ir && ir == bc,
        "interpreter divergence on native-oob-fuzz (seed {seed:#x}):\n{source}\n\
         ast={ast:?} ir={ir:?} bytecode={bc:?}"
    );
    if predicted_oob {
        assert_eq!(
            ast,
            Err("L0413".to_string()),
            "predicted OOB but the interpreters did not raise L0413 (seed {seed:#x}):\n{source}\n\
             got {ast:?} — the generator mispredicted, or the interpreter bounds check regressed"
        );
        Err(())
    } else {
        match ast {
            Ok(n) => Ok(n),
            Err(code) => panic!(
                "predicted in-bounds but the interpreters raised {code} (seed {seed:#x}):\n{source}"
            ),
        }
    }
}

#[test]
fn fuzz_oob_native_matches_interpreter_when_linkable() {
    // Compile each generated OOB/boundary program to a real `.exe` and enforce the
    // trap-vs-value parity against the interpreters. Gated on the ability to run a
    // Windows exe; every program is native-eligible (the array OOB indices are
    // opaque, so the runtime bounds check is always emitted) so `fuzz_native_exit`
    // produces an exe for every one — a regression that stopped emitting the check
    // fails here loudly rather than skipping.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native OOB differential fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 80;
    let base_seed = 0x4F1B_9C27_A3E5_0D61u64;
    let dir = ScratchDir::new("oob");
    let mut ran = 0u64;
    let mut oob_count = 0u64;
    let mut inbounds_count = 0u64;

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let (source, predicted_oob) = gen_oob_native_program(seed);
        let expectation = oob_interp_expectation(seed, &source, predicted_oob);

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("oob_{i}")) else {
            break;
        };
        ran += 1;

        match expectation {
            // Out of bounds: native must abort with the defined `ud2` bounds-trap —
            // never a value (which would be the linear-memory overrun miscompile).
            Err(()) => {
                assert_eq!(
                    exit,
                    STATUS_ILLEGAL_INSTRUCTION,
                    "NATIVE OOB MISCOMPILE on #{i} (seed {seed:#x}): expected a clean bounds-trap \
                     (STATUS_ILLEGAL_INSTRUCTION 0x{status:08X}), got exit {exit:#010x} — native \
                     returned a value where the interpreters raise L0413:\n{source}",
                    status = STATUS_ILLEGAL_INSTRUCTION as u32
                );
                oob_count += 1;
            }
            // In bounds: native must return the same value every interpreter did,
            // through the bounds check (no false trap on a valid index).
            Ok(expected) => {
                assert_eq!(
                    exit, expected as i32,
                    "NATIVE MISCOMPILE on in-bounds #{i} (seed {seed:#x}):\n{source}\n\
                     interpreter={expected}, native exit={exit}"
                );
                inbounds_count += 1;
            }
        }
    }

    // A green result is only meaningful if the batch actually ran binaries AND
    // exercised BOTH classes — a run that generated only in-bounds indices would
    // say nothing about the trap this fuzzer targets.
    assert!(
        ran > 0,
        "the native OOB fuzz executed NO programs on a Windows host — a green result \
         here would prove nothing about the emitter"
    );
    assert!(
        oob_count > 0,
        "the native OOB fuzz produced NO out-of-bounds programs — it is toothless \
         (in-bounds {inbounds_count}, ran {ran})"
    );
    assert!(
        inbounds_count > 0,
        "the native OOB fuzz produced NO in-bounds programs — the bounds check's \
         correctness (no false trap) went unchecked (oob {oob_count}, ran {ran})"
    );
    eprintln!(
        "oob native fuzz: ran {ran}/{PROGRAMS} real exes — {oob_count} out-of-bounds (interp \
         L0413 + native ud2-trap), {inbounds_count} in-bounds (value parity)"
    );
}
