//! Differential fuzzing for **native closure codegen** — the scalar-completeness
//! slice: float captures, float parameters/returns, interleaved integer/float
//! parameter classes, and parameter counts past the register file (stack spills).
//!
//! A submodule of `fuzz.rs` rather than a peer (that file is at the ~2500-line test
//! cap) so it keeps seeing `Rng`, `Outcome`, `run_interpreters`, `fuzz_native_exit`,
//! `ScratchDir`, and the host probes via `use super::*`.
//!
//! ## What these generators are built to catch
//!
//! The closure call ABI passes the env pointer as a **hidden first argument**, so a
//! closure's parameter `i` sits at Win64 **effective position `i + 1`**. Two
//! consequences are silent-corruption hazards, and every generator here is shaped to
//! turn each into a wrong exit code rather than a lucky pass:
//!
//! - **Float registers are positional.** A float at effective position 2 must arrive
//!   in `xmm2`, not "the next unused XMM". Generators therefore interleave integer
//!   and float parameters so floats land at positions the sequential-XMM bug would
//!   get wrong, and give every float a **distinct weight** so a swapped or stale
//!   register changes the result. A closure with a single float parameter is
//!   deliberately NOT enough on its own: the caller evaluates that argument into
//!   `xmm0` while staging it, so a callee that wrongly reads `xmm0` still sees the
//!   right value and passes. The generators bias toward multi-float, interleaved
//!   shapes for exactly this reason.
//! - **A 4th parameter is the 5th argument**, spilling above the 32-byte shadow
//!   space. Parameter counts run past the register file, with position-dependent
//!   weights, so a wrong spill displacement moves a value and shows up.
//!
//! ## Why the shapes look the way they do
//!
//! Lullaby has **no integer→float conversion** (`to_f32` is `f64→f32`, `to_f64` is
//! `f32→f64`), so a single expression cannot fold an integer parameter into a float
//! result. Each generated closure therefore keeps the two classes in their own
//! arithmetic and reports through one of three shapes, exactly mirroring how the
//! existing `array<f64>` fuzzer turns float work into an integer exit code
//! (threshold comparisons, counted):
//!
//! - [`Shape::IntWeights`] — an integer weighted sum; float parameters are present
//!   (shifting every later position) but unused by the body.
//! - [`Shape::FloatThreshold`] — a float weighted sum compared to a threshold,
//!   sampled at several argument vectors and counted into a multi-bit integer.
//! - [`Shape::MixedChain`] — an `and`-chain of a float comparison and an integer
//!   comparison, so ONE closure observes both register files and a genuinely mixed
//!   capture block (an `i64` capture and an `f64` capture in the same env).
//!
//! Division is never generated (no divide-by-zero to model) and results are folded
//! `% 251`, so a program's meaning is a pure function of the ABI being right.

use super::*;

/// Which reporting shape a generated closure program uses. See the module docs for
/// why the classes cannot be folded into one expression.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    IntWeights,
    FloatThreshold,
    MixedChain,
}

/// The scalar class of a generated parameter or capture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Int,
    F64,
    F32,
}

impl Class {
    fn type_name(self) -> &'static str {
        match self {
            Class::Int => "i64",
            Class::F64 => "f64",
            Class::F32 => "f32",
        }
    }
    fn is_float(self) -> bool {
        !matches!(self, Class::Int)
    }
}

/// A float literal of the given class. An `f32` must be written through `to_f32`
/// (there is no `f32` literal suffix in this position), which also pins the value to
/// single precision on every backend.
fn float_lit(class: Class, value: i64, half: bool) -> String {
    let frac = if half { 5 } else { 0 };
    let lit = format!("{value}.{frac}");
    match class {
        Class::F32 => format!("to_f32({lit})"),
        _ => lit,
    }
}

/// Generate one closure program for `seed`.
///
/// The closure is always a direct `let` literal called directly — the supported
/// native shape — so every generated program is expected to COMPILE natively, not
/// skip. That matters: a generator that mostly produced skipping programs would
/// still pass while proving nothing about codegen.
fn gen_closure_program(seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let shape = match rng.below(3) {
        0 => Shape::IntWeights,
        1 => Shape::FloatThreshold,
        _ => Shape::MixedChain,
    };
    // Float width for this program: exercise f32 sometimes, f64 mostly (f64 is the
    // common shape and has the richer arithmetic surface).
    let fclass = if rng.chance(4) {
        Class::F32
    } else {
        Class::F64
    };

    // Which classes this shape's body actually READS. A body that reads a class must
    // get at least one parameter of it, or the generated term list would be empty and
    // the program would not parse.
    let needs_float = matches!(shape, Shape::FloatThreshold | Shape::MixedChain);
    let needs_int = matches!(shape, Shape::IntWeights | Shape::MixedChain);

    // Parameter classes. Counts run up to 6 so positions 4+ spill to the stack (a 4th
    // parameter is already the 5th argument once the env pointer is counted). A shape
    // that reads BOTH classes needs at least two parameters to hold one of each.
    let min_params = if needs_float && needs_int { 2 } else { 1 };
    let nparams = rng.range(min_params, 6).max(min_params) as usize;
    let mut classes: Vec<Class> = Vec::new();
    for _ in 0..nparams {
        // Bias toward the class this shape reports on, but keep the other class
        // present so it shifts positions — that is the point of interleaving.
        let want_float = match shape {
            Shape::IntWeights => rng.chance(3),
            Shape::FloatThreshold => !rng.chance(3),
            Shape::MixedChain => rng.chance(2),
        };
        classes.push(if want_float { fclass } else { Class::Int });
    }
    // Pin the invariants deterministically at OPPOSITE ends, so the two can never
    // overwrite each other (a single shared slot would let the second guard undo the
    // first and emit an empty term list).
    //
    // The integer goes FIRST and the float LAST on purpose: an integer at effective
    // position 1 pushes every float to position >= 2, which is exactly where the
    // sequential-XMM bug diverges from the positional rule, and a float last maximizes
    // the chance it lands in a stack-spilled position.
    if needs_int && !classes.contains(&Class::Int) {
        classes[0] = Class::Int;
    }
    if needs_float && !classes.iter().any(|c| c.is_float()) {
        classes[nparams - 1] = fclass;
    }
    debug_assert!(
        !needs_int || classes.contains(&Class::Int),
        "shape {shape:?} reads integers but generated none"
    );
    debug_assert!(
        !needs_float || classes.iter().any(|c| c.is_float()),
        "shape {shape:?} reads floats but generated none"
    );

    let names: Vec<String> = (0..nparams).map(|i| format!("p{i}")).collect();
    let sig_types: Vec<&str> = classes.iter().map(|c| c.type_name()).collect();
    let params: Vec<String> = names
        .iter()
        .zip(classes.iter())
        .map(|(n, c)| format!("{n} {}", c.type_name()))
        .collect();

    // Captures. Every shape captures at least one value of the class it reads;
    // `MixedChain` captures BOTH classes, so its env block interleaves an integer
    // word and a float word and each must be read through the right register file.
    let icap = rng.range(2, 9);
    let fcap_v = rng.range(1, 6);
    let mut preamble = String::new();
    if needs_int {
        preamble.push_str(&format!("    let k i64 = {icap}\n"));
    }
    if needs_float {
        preamble.push_str(&format!(
            "    let w {} = {}\n",
            fclass.type_name(),
            float_lit(fclass, fcap_v, true)
        ));
    }

    // Body. Distinct per-position weights make any swapped register or wrong spill
    // displacement produce a different number.
    let int_terms: Vec<String> = names
        .iter()
        .zip(classes.iter())
        .enumerate()
        .filter(|(_, (_, c))| **c == Class::Int)
        .map(|(i, (n, _))| format!("{n} * {}", 1 + i * 7))
        .collect();
    let float_terms: Vec<String> = names
        .iter()
        .zip(classes.iter())
        .enumerate()
        .filter(|(_, (_, c))| c.is_float())
        .map(|(i, (n, _))| format!("{n} * {}", float_lit(fclass, 1 + i as i64 * 3, false)))
        .collect();

    let (ret_ty, body) = match shape {
        Shape::IntWeights => ("i64".to_string(), format!("{} + k", int_terms.join(" + "))),
        Shape::FloatThreshold => {
            let t = float_lit(fclass, rng.range(-6, 30), true);
            (
                "bool".to_string(),
                format!("{} + w > {t}", float_terms.join(" + ")),
            )
        }
        Shape::MixedChain => {
            let t = float_lit(fclass, rng.range(-6, 30), true);
            let n = rng.range(-10, 40);
            (
                "bool".to_string(),
                format!(
                    "{} + w > {t} and {} + k > {n}",
                    float_terms.join(" + "),
                    int_terms.join(" + ")
                ),
            )
        }
    };

    let decl = format!(
        "    let f fn({}) -> {ret_ty} = fn {} -> {body}\n",
        sig_types.join(", "),
        params.join(" ")
    );

    // Call sites. An integer-returning closure is read once; a bool-returning one is
    // sampled at three argument vectors and counted, so the exit code carries several
    // independent bits of the float computation rather than a single flag.
    let arg_vec = |rng: &mut Rng| -> String {
        let args: Vec<String> = classes
            .iter()
            .map(|c| match c {
                Class::Int => format!("{}", rng.range(-9, 9)),
                _ => float_lit(*c, rng.range(-9, 9), rng.chance(2)),
            })
            .collect();
        args.join(", ")
    };

    let tail = if ret_ty == "i64" {
        let a = arg_vec(&mut rng);
        format!("    let r i64 = f({a})\n    r % 251\n")
    } else {
        let (a1, a2, a3) = (arg_vec(&mut rng), arg_vec(&mut rng), arg_vec(&mut rng));
        format!(
            "    let total i64 = 0\n\
             \x20   if f({a1})\n        total = total + 1\n\
             \x20   if f({a2})\n        total = total + 2\n\
             \x20   if f({a3})\n        total = total + 4\n\
             \x20   total\n"
        )
    };

    format!("fn main -> i64\n{preamble}{decl}{tail}")
}

#[test]
fn fuzz_closure_interpreters_agree() {
    // Cross-check the three engines on closure programs. Always runs (no toolchain
    // needed): it establishes the ground truth the native oracle below compares to,
    // so a divergence HERE would invalidate that oracle.
    const PROGRAMS: u64 = 600;
    let base_seed = 0x51D3_9C1B_7A44_02E5u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_closure_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "closure backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "closure generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_closure_native_matches_interpreter_when_linkable() {
    // THE oracle for the closure ABI: compile each closure program to a real `.exe`
    // and check its exit code against the interpreters. This is what catches a
    // positional-XMM error (a float read from the wrong register) or a wrong
    // stack-spill displacement — both of which are silent, producing a plausible
    // number rather than a crash.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native closure fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 128;
    let base_seed = 0x6B19_44C7_20E5_1D8Fu64;
    let dir = ScratchDir::new("closure");

    let mut ran = 0u64;
    let mut compiled = 0u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_closure_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on closure native fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("closure_{i}")) else {
            eprintln!("closure native fuzz: no exe path on this host; ran {ran} before skipping");
            return;
        };
        ran += 1;
        compiled += 1;
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE on closure #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
    // Report the count so a run can be audited: this oracle is only meaningful if it
    // actually executed real binaries. Every generated program is a supported native
    // closure shape, so on a Windows host this prints the full count — if it ever
    // prints fewer, the generator has drifted into producing skipping programs and
    // is no longer testing codegen.
    eprintln!("closure native fuzz: ran {ran}/{PROGRAMS} real exes ({compiled} compiled natively)");
}
