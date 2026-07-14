//! Differential fuzzing (WS1 of the optimization epic): generate random,
//! type-correct, guaranteed-terminating, divergence-free `i64` programs and
//! assert every backend computes the same result. A hand-written native byte
//! emitter is exactly where miscompiles hide, and hand-picked fixtures only
//! cover the cases we thought of — this oracle exercises construct combinations
//! we didn't, so optimizations can be pushed aggressively behind it.
//!
//! Two levels:
//!   * `fuzz_interpreters_agree` — always runs; cross-checks the three separate
//!     engines (AST tree-walker, IR, bytecode VM) against each other via the
//!     library API (fast, no subprocess).
//!   * `fuzz_native_matches_interpreter_when_linkable` — gated on the native
//!     link toolchain; compiles each program to a real `.exe` and checks its
//!     exit code against the interpreter result.
//!
//! The generator only emits operations every backend defines identically
//! (wrapping `+`/`-`/`*`, signed `/`/`%` with a *nonzero literal* divisor,
//! comparisons inside `if`, `let`/assignment, bounded `for`), so a mismatch is
//! a real miscompile, never undefined behavior.

use crate::*;
use lullaby_ir::{
    lower, lower_to_bytecode, run_bytecode_main_with_args, run_main_with_args as run_ir_main,
};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_runtime::{Value, run_main_with_args as run_ast_main};
use lullaby_semantics::validate_executable;

/// Deterministic xorshift64 PRNG — reproducible, no external crate. A fixed seed
/// makes every run identical, so a discovered mismatch reproduces exactly.
struct Rng(u64);

impl Rng {
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

    fn chance(&mut self, one_in: u64) -> bool {
        self.below(one_in) == 0
    }
}

/// Generates one `fn main -> i64` program over `i64` locals. Straight-line
/// bindings, a bounded `for` accumulator, and single-level `if` guards — every
/// construct produces a value all backends agree on.
struct Gen {
    rng: Rng,
    vars: Vec<String>,
    body: String,
}

impl Gen {
    fn new(seed: u64) -> Self {
        Gen {
            rng: Rng(seed | 1),
            vars: Vec::new(),
            body: String::new(),
        }
    }

    /// A leaf: a small literal or an in-scope variable.
    fn leaf(&mut self) -> String {
        if self.vars.is_empty() || self.rng.chance(2) {
            self.rng.range(-50, 50).to_string()
        } else {
            let idx = self.rng.below(self.vars.len() as u64) as usize;
            self.vars[idx].clone()
        }
    }

    /// A value-producing `i64` expression, depth-bounded. Divisors are always a
    /// nonzero positive literal, so no backend can hit a div-by-zero (which the
    /// interpreters report as an error but native traps) — keeping every
    /// generated program divergence-free.
    fn expr(&mut self, depth: u32) -> String {
        if depth == 0 || self.rng.chance(3) {
            return self.leaf();
        }
        match self.rng.below(6) {
            0 => {
                let (a, b) = (self.expr(depth - 1), self.expr(depth - 1));
                format!("({a} + {b})")
            }
            1 => {
                let (a, b) = (self.expr(depth - 1), self.expr(depth - 1));
                format!("({a} - {b})")
            }
            2 => {
                let (a, b) = (self.expr(depth - 1), self.expr(depth - 1));
                format!("({a} * {b})")
            }
            3 => {
                // Divisor: a nonzero positive literal (exercises signed idiv with
                // a varying-sign dividend, never div-by-zero).
                let a = self.expr(depth - 1);
                let d = self.rng.range(1, 20);
                format!("({a} / {d})")
            }
            4 => {
                let a = self.expr(depth - 1);
                let d = self.rng.range(1, 20);
                format!("({a} % {d})")
            }
            _ => {
                // A math builtin over subexpressions (the batch just landed).
                let a = self.expr(depth - 1);
                let b = self.expr(depth - 1);
                match self.rng.below(4) {
                    0 => format!("min({a}, {b})"),
                    1 => format!("max({a}, {b})"),
                    2 => format!("gcd({a}, {b})"),
                    _ => format!("abs({a})"),
                }
            }
        }
    }

    fn fresh_var(&mut self) -> String {
        let name = format!("v{}", self.vars.len());
        self.vars.push(name.clone());
        name
    }

    fn push_line(&mut self, indent: usize, line: &str) {
        for _ in 0..indent {
            self.body.push_str("    ");
        }
        self.body.push_str(line);
        self.body.push('\n');
    }

    /// Build the whole program body: a handful of statements then a tail return.
    fn program(&mut self) -> String {
        let stmt_count = self.rng.range(2, 7);
        for _ in 0..stmt_count {
            match self.rng.below(8) {
                // A single-level `if` that reassigns an existing variable.
                3 if !self.vars.is_empty() => {
                    let cond_l = self.leaf();
                    let cond_r = self.leaf();
                    let cmp = ["<", "<=", ">", ">=", "==", "!="][self.rng.below(6) as usize];
                    let target = self.vars[self.rng.below(self.vars.len() as u64) as usize].clone();
                    let rhs = self.expr(2);
                    self.push_line(1, &format!("if {cond_l} {cmp} {cond_r}"));
                    self.push_line(2, &format!("{target} = {rhs}"));
                }
                // A bounded `for` accumulator into an existing variable. The
                // addend is often affine in the counter (`c1*k + c0`), exercising
                // the O(1) `for`-loop closed form; sometimes a non-affine `k*k`
                // that must fall back to the scalar loop. The range low/high are
                // constants or variables, and can be empty (hi < lo).
                4 if !self.vars.is_empty() => {
                    let target = self.vars[self.rng.below(self.vars.len() as u64) as usize].clone();
                    let lo = self.rng.range(-3, 5);
                    let hi = self.rng.range(-2, 12);
                    let addend = match self.rng.below(4) {
                        0 => "k".to_string(),
                        1 => format!(
                            "(({} * k) + {})",
                            self.rng.range(-5, 5),
                            self.rng.range(-8, 8)
                        ),
                        2 => "(k * k)".to_string(),
                        _ => format!("(k - {})", self.rng.range(-4, 4)),
                    };
                    self.push_line(1, &format!("for k from {lo} to {hi}"));
                    self.push_line(2, &format!("{target} = ({target} + {addend})"));
                }
                // A `while i < BOUND` counting-sum loop — the exact shape the
                // native ILP unroll recognizes. The bound is a small constant or a
                // small non-negative runtime variable (exercising both the const
                // and the runtime-bound fast paths, and boundary/`n <= 0` cases).
                5..=6 => {
                    let acc = self.fresh_var();
                    self.push_line(1, &format!("let {acc} i64 = 0"));
                    let ctr = self.fresh_var();
                    self.push_line(1, &format!("let {ctr} i64 = 0"));
                    let bound = if self.rng.chance(2) {
                        let b = self.fresh_var();
                        let v = self.rng.range(0, 40);
                        self.push_line(1, &format!("let {b} i64 = {v}"));
                        b
                    } else {
                        self.rng.range(0, 40).to_string()
                    };
                    // The addend is the bare counter (the closed-form counting
                    // sum) or a pure polynomial in the counter (the
                    // multi-accumulator / affine reduction paths).
                    let addend = match self.rng.below(5) {
                        0 => ctr.clone(),
                        1 => format!("({ctr} * {ctr})"),
                        2 => format!(
                            "(({} * {ctr}) + {})",
                            self.rng.range(-6, 6),
                            self.rng.range(-9, 9)
                        ),
                        3 => format!("(({ctr} - {}) * {ctr})", self.rng.range(-4, 4)),
                        _ => format!("({ctr} * {ctr} * {ctr})"),
                    };
                    self.push_line(1, &format!("while {ctr} < {bound}"));
                    self.push_line(2, &format!("{acc} = {acc} + {addend}"));
                    self.push_line(2, &format!("{ctr} = {ctr} + 1"));
                }
                // Otherwise a new binding (also the fallback when `if`/`for` guards
                // fail with no variables yet).
                _ => {
                    let expr = self.expr(3);
                    let name = self.fresh_var();
                    self.push_line(1, &format!("let {name} i64 = {expr}"));
                }
            }
        }
        let tail = self.expr(3);
        self.push_line(1, &tail);
        format!("fn main -> i64\n{}", self.body)
    }
}

fn gen_program(seed: u64) -> String {
    Gen::new(seed).program()
}

/// The result of running a program on one backend, reduced to a comparable form.
#[derive(PartialEq, Eq, Debug)]
enum Outcome {
    Value(i64),
    /// A runtime error (should not occur with the divergence-free generator; if
    /// it does on one tier but not another that is itself a finding).
    Error,
    /// The program returned a non-i64 (the generator only makes i64 mains, so
    /// this is a generator bug, surfaced rather than silently ignored).
    Other,
}

fn outcome(value: Result<Value, lullaby_runtime::RuntimeError>) -> Outcome {
    match value {
        Ok(Value::I64(n)) => Outcome::Value(n),
        Ok(_) => Outcome::Other,
        Err(_) => Outcome::Error,
    }
}

/// Run `source` on the AST, IR, and bytecode tiers via the library API.
fn run_interpreters(source: &str) -> (Outcome, Outcome, Outcome) {
    let tokens = lex(source).expect("lex generated program");
    let program = parse(&tokens).expect("parse generated program");
    let checked = validate_executable(&program).expect("semantic-check generated program");

    let ast = outcome(run_ast_main(&checked.program, Vec::new()));

    let module = lower(&checked).expect("lower generated program");
    let ir = outcome(run_ir_main(&module, Vec::new()));

    let bytecode = lower_to_bytecode(&module);
    let bc = outcome(run_bytecode_main_with_args(&bytecode, Vec::new()));

    (ast, ir, bc)
}

#[test]
fn fuzz_interpreters_agree() {
    // Cross-check the three engines on a large batch. Always runs (no toolchain
    // needed). A divergence prints the exact reproducing program.
    const PROGRAMS: u64 = 3000;
    let base_seed = 0x9E37_79B9_7F4A_7C15u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_native_matches_interpreter_when_linkable() {
    // Compile each generated program to a real `.exe` and check its exit code
    // against the interpreter result — the high-value oracle for the native
    // emitter. Gated on the link toolchain; skips cleanly when absent.
    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native differential fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 120;
    let base_seed = 0xD1B5_4A32_D192_ED03u64;
    let dir = std::env::temp_dir().join("lullaby_fuzz_native");
    let _ = std::fs::create_dir_all(&dir);

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_program(seed);

        // Interpreter ground truth (all three must already agree; assert it here
        // too so a native mismatch is never blamed on interpreter disagreement).
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let src_path = dir.join(format!("fuzz_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_{i}.exe"));
        std::fs::write(&src_path, &source).expect("write fuzz source");
        let _ = std::fs::remove_file(&exe_path);

        let emit = lullaby()
            .args([
                "native",
                "-o",
                exe_path.to_str().expect("exe path"),
                src_path.to_str().expect("src path"),
            ])
            .output()
            .expect("run native");
        assert!(
            emit.status.success(),
            "native emit failed on #{i} (seed {seed:#x}):\n{source}\n{}",
            stderr(&emit)
        );
        assert!(
            exe_path.is_file(),
            "no linked exe on #{i} (seed {seed:#x}):\n{source}\n{}",
            stdout(&emit)
        );

        let run = Command::new(&exe_path).output().expect("run fuzz exe");
        let exit = run.status.code().expect("native exit code");
        // The entry stub forwards main's i64 to ExitProcess; on Windows the full
        // 32-bit value round-trips, so the exit code equals `expected as i32`.
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}
