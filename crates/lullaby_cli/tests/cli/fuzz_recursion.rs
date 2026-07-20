//! Deep-recursion differential oracle. A submodule of `fuzz.rs`, reusing its
//! shared `Rng`, `Outcome`, `run_interpreters`, and native `fuzz_native_exit`
//! harness via `use super::*`.
//!
//! # Why this fuzzer exists
//!
//! The three interpreters are recursive tree-walkers. Before the large-stack fix
//! they overflowed the host process (`STATUS_STACK_OVERFLOW`) on a perfectly
//! well-defined recursive program at a shallow, *tier-dependent* depth — the AST
//! tier around ~200 frames, IR around ~300, bytecode around ~1000 — while native
//! ran the same program correctly. Because the differential fuzzers treat the
//! interpreters as the reference oracle, that crash left the whole oracle **blind
//! to any program that recursed deeper than a few hundred frames**: a large hole in
//! the primary correctness net.
//!
//! The interpreters now evaluate on a dedicated large stack with a uniform
//! call-depth bound (`lullaby_runtime::interp_stack`), so a terminating recursion
//! returns the same value on every tier far past the old ceiling, and an unbounded
//! one ends with the same clean `L0466` on every interpreter tier instead of a bare
//! host abort. This fuzzer folds that discovered shape into the permanent oracle:
//! it generates recursion nested thousands of frames deep and asserts the three
//! interpreters agree with each other (and, when linkable, with native).
//!
//! Every generated program is guaranteed-terminating and divergence-free: the only
//! arithmetic is wrapping `i64` `+`/`-`/`*` and comparisons, which every backend
//! computes identically, so a mismatch is a real cross-tier bug, never undefined
//! behavior. Depths stay well below the shared interpreter bound (so every program
//! produces a value, not `L0466`), and the native variant stays well below the
//! produced exe's own OS stack limit (so native never overflows here).

use super::*;

/// Generate a guaranteed-terminating, divergence-free recursive `i64` program
/// whose recursion nests `depth` frames deep — far past the ~200-frame ceiling
/// that used to crash the interpreters. Three shapes are rotated: a single tail
/// accumulator, a value combined on the way back up, and a two-function mutual
/// recursion. Constants are derived from `seed` (decorrelated from the caller's
/// depth draw), and `depth` is embedded as the initial argument.
fn gen_recursive_program(seed: u64, depth: i64) -> String {
    // Decorrelate the constant stream from whatever draw produced `depth`.
    let mut rng = Rng(seed.rotate_left(32) | 1);
    let k1 = rng.range(1, 7);
    let k2 = rng.range(0, 5);
    let s0 = rng.range(-3, 3);
    match rng.below(3) {
        // Tail accumulator: value carried down, returned at the base case.
        // (Parameters are space-separated when their types differ; call arguments
        // are comma-separated.)
        0 => format!(
            "fn f n i64 acc i64 -> i64\n    \
                if n <= 0\n        return acc\n    \
                return f(n - 1, acc + n * {k1} - {k2})\n\n\
             fn main -> i64\n    return f({depth}, {s0})\n"
        ),
        // Post-recursion combine: value built on the way back up the stack.
        1 => format!(
            "fn f n i64 -> i64\n    \
                if n <= 0\n        return {s0}\n    \
                return {k1} * n - {k2} + f(n - 1)\n\n\
             fn main -> i64\n    return f({depth})\n"
        ),
        // Mutual recursion f -> g -> f, each decrementing by one.
        _ => format!(
            "fn g n i64 acc i64 -> i64\n    \
                if n <= 0\n        return acc\n    \
                return f(n - 1, acc + {k1})\n\n\
             fn f n i64 acc i64 -> i64\n    \
                if n <= 0\n        return acc\n    \
                return g(n - 1, acc - {k2} + n)\n\n\
             fn main -> i64\n    return f({depth}, {s0})\n"
        ),
    }
}

#[test]
fn fuzz_recursion_interpreters_agree() {
    // Cross-check the three engines on deep recursion — the exact regime that used
    // to crash them and blind this oracle. Always runs (no toolchain needed). Every
    // program terminates below the shared interpreter bound, so all three must
    // return the SAME i64 value; a divergence prints the reproducing program.
    const PROGRAMS: u64 = 60;
    let base_seed = 0x51A9_C7E3_44BD_9F02u64;
    let mut deepest = 0i64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        // Program 0 is pinned deep so a deep case always runs; the rest spread
        // across a wide band well past the old few-hundred-frame ceiling.
        let depth = if i == 0 {
            9000
        } else {
            Rng(seed | 1).range(300, 6000)
        };
        deepest = deepest.max(depth);
        let source = gen_recursive_program(seed, depth);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "recursion backend divergence on #{i} (depth {depth}, seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "recursion generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
        // The program terminates below the bound, so it yields a value (not `L0466`).
        assert!(
            matches!(ast, Outcome::Value(_)),
            "expected a value at depth {depth} (seed {seed:#x}), got {ast:?}:\n{source}"
        );
    }
    // Prove the batch actually exercised deep recursion rather than trivial depths —
    // otherwise a green result would say nothing about the regime this fuzzer targets.
    assert!(
        deepest >= 9000,
        "the recursion fuzz never generated a deep program (deepest {deepest} frames)"
    );
    eprintln!(
        "recursion fuzz: {PROGRAMS} programs agreed across AST/IR/bytecode (deepest {deepest} frames)"
    );
}

#[test]
fn fuzz_recursion_native_matches_interpreter_when_linkable() {
    // Compile each deep-recursion program to a real `.exe` and check its exit code
    // against the interpreter result — the four-tier oracle for deep recursion.
    // Gated on the ability to run a Windows exe; depths stay well below the produced
    // exe's OS stack limit so native never overflows here (that is native's own
    // stack-size concern, not this fuzzer's).
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native recursion differential fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 40;
    let base_seed = 0x9D2C_11F6_A73E_0451u64;
    let dir = ScratchDir::new("recursion");
    let mut ran = 0u64;

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        // Conservative depth: comfortably past the old ~200 ceiling, comfortably
        // below the native exe's default stack capacity, so native runs to a value.
        let depth = Rng(seed | 1).range(300, 1500);
        let source = gen_recursive_program(seed, depth);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-recursion-fuzz #{i} (depth {depth}, seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("recursion generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("rec_{i}")) else {
            break;
        };
        ran += 1;
        // The entry stub forwards main's i64 low 32 bits to ExitProcess.
        assert_eq!(
            exit, expected as i32,
            "NATIVE RECURSION MISCOMPILE on #{i} (depth {depth}, seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
    assert!(
        ran > 0,
        "the native recursion fuzz executed NO programs on a Windows host — a green \
         result here would prove nothing"
    );
    eprintln!("recursion native fuzz: ran {ran}/{PROGRAMS} real exes");
}

#[test]
fn deep_recursion_parity_pin() {
    // Regression pin: a 5000-deep recursion that USED to host-abort the AST (~200)
    // and IR (~300) interpreters with STATUS_STACK_OVERFLOW now runs identically on
    // all four tiers. `1 + f(n-1)` is the exact shape from the adversarial finding.
    let source = "fn f n i64 -> i64\n    if n <= 0\n        return 0\n    \
                  return 1 + f(n - 1)\n\nfn main -> i64\n    return f(5000)\n";
    let (ast, ir, bc) = run_interpreters(source);
    assert_eq!(ast, Outcome::Value(5000), "AST deep-recursion value");
    assert_eq!(ir, Outcome::Value(5000), "IR deep-recursion value");
    assert_eq!(bc, Outcome::Value(5000), "bytecode deep-recursion value");

    if native_exe_runnable() {
        ensure_msvc_env();
        let dir = ScratchDir::new("deep_rec");
        if let Some(exit) = fuzz_native_exit(source, &dir, "deep_rec_5000") {
            assert_eq!(exit, 5000, "native deep-recursion value");
        }
    }
}

#[test]
fn unbounded_recursion_uniform_diagnostic() {
    // Non-terminating recursion (no base case): every interpreter tier stops at the
    // shared bound with the SAME clean, catchable `L0466` — never a bare host
    // stack-overflow abort, and never a tier-dependent depth. This is the other half
    // of the invariant: a terminating recursion agrees on a value (above), an
    // unbounded one agrees on a diagnostic.
    let source = "fn f n i64 -> i64\n    return 1 + f(n + 1)\n\nfn main -> i64\n    return f(0)\n";
    let tokens = lex(source).expect("lex unbounded-recursion program");
    let program = parse(&tokens).expect("parse unbounded-recursion program");
    let checked = validate_executable(&program).expect("check unbounded-recursion program");

    let ast = run_ast_main(&checked.program, Vec::new());
    let module = lower(&checked).expect("lower unbounded-recursion program");
    let ir = run_ir_main(&module, Vec::new());
    let bytecode = lower_to_bytecode(&module);
    let bc = run_bytecode_main_with_args(&bytecode, Vec::new());

    for (tier, result) in [("ast", &ast), ("ir", &ir), ("bytecode", &bc)] {
        let error = result.as_ref().expect_err(&format!(
            "{tier}: unbounded recursion must raise, not return"
        ));
        assert_eq!(
            error.code, "L0466",
            "{tier}: expected the uniform recursion-limit diagnostic L0466, got {}: {}",
            error.code, error.message
        );
    }
}
