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
    /// Accumulated `fn hN ...` declarations for the VOID helpers `main` calls for
    /// effect, emitted ahead of `main`. See [`Gen::void_helper`].
    void_helpers: String,
    /// How many void helpers have been emitted (names them, and lets a helper call
    /// the previously-emitted one).
    helper_count: usize,
}

impl Gen {
    fn new(seed: u64) -> Self {
        Gen {
            rng: Rng(seed | 1),
            vars: Vec::new(),
            body: String::new(),
            void_helpers: String::new(),
            helper_count: 0,
        }
    }

    /// Emit one VOID helper (`fn hN a i64 b i64` — no declared return type) and
    /// return its name.
    ///
    /// The helper's work is deliberately **unobservable**: it computes into its own
    /// locals and returns nothing, so `main`'s value must be **identical whether or
    /// not the call is there**. The interpreters model the call as a pure no-op on
    /// the caller, so any native deviation diverges.
    ///
    /// **What this actually catches (measured, not assumed):**
    ///
    /// * a void call that **misaligns the stack**, scribbles on the caller's frame,
    ///   or returns down a wrong epilogue path — `main`'s value changes;
    /// * a regression that makes the shape **stop compiling**, via
    ///   `fuzz_native_exit`'s "an exe must be produced" assertion. This is the live
    ///   one: injecting `NativeType::Void::is_aggregate() -> true` makes the fuzzer
    ///   FAIL, because the void call statement then trips the "aggregate-returning
    ///   call is only supported in a binding or return position" gate, its caller
    ///   skips, and no exe is produced.
    ///
    /// **What it does NOT catch, despite the intuition:**
    ///
    /// * a **clobbered callee-saved register**. A void call disables register
    ///   promotion in the *entire calling function*: `plan_register_promotion`
    ///   requires ALL instructions to pass `instr_reg_promotable`, and a void call
    ///   statement is an `Expr` whose `Call` arm demands `expr.ty.name == "i64"`.
    ///   So `rbx`/`rsi` are never live across a void call — there is nothing to
    ///   clobber. (Verified by injection: the bug is invisible to this fuzzer,
    ///   correctly.) See the register-promotion footgun in
    ///   `documents/native_backend_contract.md`.
    /// * an **argument-register shift** from a wrongly reserved hidden result
    ///   pointer. That never reaches codegen — it is refused earlier as a clean
    ///   skip, which is why the `is_aggregate` injection above surfaces as a
    ///   compile failure rather than a wrong value.
    ///
    /// The body shapes cover the void-specific lowering paths: a body ending in a
    /// NON-EXHAUSTIVE `if` and one ending in a `match` (both STATEMENT tails — a
    /// void function has no value position, so the `block_yields_value` gate must
    /// never apply), a bare `return`, a `while` loop, and a void→void call. Only
    /// `+`/`-`/`*` and literals appear, so no shape can divide by zero or otherwise
    /// diverge between the tiers.
    fn void_helper(&mut self) -> String {
        let name = format!("h{}", self.helper_count);
        self.helper_count += 1;
        // A previously emitted helper this one may call (void -> void).
        let callee = (self.helper_count > 1).then(|| format!("h{}", self.helper_count - 2));

        let mut decl = format!("fn {name} a i64 b i64\n");
        decl.push_str("    let t0 i64 = (a + b)\n");
        decl.push_str(&format!(
            "    let t1 i64 = (t0 * {})\n",
            self.rng.range(1, 9)
        ));

        match self.rng.below(6) {
            // Ends in a NON-EXHAUSTIVE `if` — a statement tail.
            0 => {
                decl.push_str("    if a < b\n        t1 = (t1 + 1)\n");
                decl.push_str(&format!(
                    "    elif a > {}\n        t1 = (t1 - 1)\n",
                    self.rng.range(-9, 9)
                ));
            }
            // Ends in a `match` — a statement tail. Each arm body is a void call.
            1 => {
                decl.push_str("    let o option<i64> = some(t0)\n");
                decl.push_str("    match o\n");
                decl.push_str(&format!("        some(v) -> {name}_sink(v)\n"));
                decl.push_str(&format!("        none -> {name}_sink(0)\n"));
                // The arm bodies need a void callee of their own.
                self.void_helpers.push_str(&format!(
                    "fn {name}_sink x i64\n    let s i64 = (x + 1)\n\n"
                ));
            }
            // A bare `return` (early exit, no value to route) on a condition that
            // varies with the arguments, so both paths occur across seeds.
            2 => {
                decl.push_str("    if a < b\n        return\n");
                decl.push_str("    let t2 i64 = (t1 - a)\n");
            }
            // A `while` loop.
            3 => {
                decl.push_str("    let i i64 = 0\n");
                decl.push_str(&format!("    while i < {}\n", self.rng.range(0, 8)));
                decl.push_str("        t1 = (t1 + i)\n        i = i + 1\n");
            }
            // A void -> void call, when there is an earlier helper to call.
            4 if callee.is_some() => {
                let callee = callee.expect("guarded by the match arm");
                decl.push_str(&format!("    {callee}(t0, t1)\n"));
            }
            // Straight-line only (also the fallback when there is no earlier
            // helper to call).
            _ => {
                decl.push_str(&format!(
                    "    let t2 i64 = (t1 - {})\n",
                    self.rng.range(-20, 20)
                ));
            }
        }
        decl.push('\n');
        self.void_helpers.push_str(&decl);
        name
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
            match self.rng.below(9) {
                // A call to a fresh VOID helper, for effect. It contributes nothing
                // to the result, so `main`'s value must be unchanged by its
                // presence — see `void_helper`.
                7..=8 if !self.vars.is_empty() => {
                    let (a, b) = (self.leaf(), self.leaf());
                    let name = self.void_helper();
                    self.push_line(1, &format!("{name}({a}, {b})"));
                }
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
        // Void helpers (if any) are declared ahead of `main`.
        format!("{}fn main -> i64\n{}", self.void_helpers, self.body)
    }
}

fn gen_program(seed: u64) -> String {
    Gen::new(seed).program()
}

/// Generates one program that exercises **fat-pointer `array<i64>` parameters**:
/// `main` builds an `array<i64>` literal and passes it to a helper that reads it
/// **read-only** — via `for x in a`, `a[i]`, and/or `len(a)` — and returns an i64.
///
/// This is the differential net for the native calling-convention change. The
/// helper compiles to native code as a fat pointer (a `(data_ptr, length)`
/// descriptor) rather than demoting because its length is not inferable, and the
/// linked result must match the interpreters. Every generated access is in bounds
/// (the separate-length shape uses `n` in `1..=len`, index reads run `0..len-1`),
/// so the program is divergence-free just like the scalar generator.
fn gen_array_program(seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    // A small array of random i64 literals (length 2..=8).
    let len = rng.range(2, 8) as usize;
    let elems: Vec<String> = (0..len).map(|_| rng.range(-40, 40).to_string()).collect();
    let arr = format!("[{}]", elems.join(", "));
    // Pick a read-only helper body shape.
    let shape = rng.below(5);
    let k = rng.range(-20, 20);
    // `n` for the separate-length shape: in `1..=len` (always in bounds).
    let n = rng.range(1, len as i64);
    let helper = match shape {
        // for-each sum
        0 => "fn helper a array<i64> -> i64\n    let acc i64 = 0\n    for x in a\n        \
              acc = acc + x\n    acc\n"
            .to_string(),
        // for-each predicate count
        1 => format!(
            "fn helper a array<i64> -> i64\n    let c i64 = 0\n    for x in a\n        \
             if x > {k}\n            c = c + 1\n    c\n"
        ),
        // indexed sum via len(a)
        2 => "fn helper a array<i64> -> i64\n    let acc i64 = 0\n    \
              for i from 0 to len(a) - 1\n        acc = acc + a[i]\n    acc\n"
            .to_string(),
        // max scan (a[0] plus for-each)
        3 => "fn helper a array<i64> -> i64\n    let best i64 = a[0]\n    for x in a\n        \
              if x > best\n            best = x\n    best\n"
            .to_string(),
        // separate-length indexed count (`count_frequency_of` shape)
        _ => "fn helper a array<i64> n, x i64 -> i64\n    let c i64 = 0\n    \
              for i from 0 to n - 1\n        if a[i] == x\n            c = c + 1\n    c\n"
            .to_string(),
    };
    let call = if shape == 4 {
        let x = elems[rng.below(len as u64) as usize].clone();
        format!("helper(xs, {n}, {x})")
    } else {
        "helper(xs)".to_string()
    };
    let bias = rng.range(-100, 100);
    format!(
        "{helper}\nfn main -> i64\n    let xs array<i64> = {arr}\n    \
         let r i64 = {call}\n    r + {bias}\n"
    )
}

/// Generates one program that exercises **fat-pointer `array<f64>` parameters**:
/// `main` builds an `array<f64>` literal and passes it to a read-only helper that
/// reads each element through an XMM register (via `for x in a`, `a[i]`, `len(a)`)
/// and returns an i64 (a comparison count or a summed-then-compared boolean —
/// there is no `f64`→`i64` cast in the native subset, so the helper never returns a
/// float).
///
/// Every literal is a `N.5` value (exact in IEEE-754 binary, no dtoa rounding), and
/// native `f64` arithmetic/comparison is bit-exact with the interpreters (no
/// `--fast-math`), so the comparisons are deterministic and the program is
/// divergence-free.
fn gen_array_f64_program(seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let len = rng.range(2, 8) as usize;
    let elems: Vec<String> = (0..len)
        .map(|_| format!("{}.5", rng.range(-20, 20)))
        .collect();
    let arr = format!("[{}]", elems.join(", "));
    let shape = rng.below(3);
    let t = format!("{}.5", rng.range(-20, 20));
    let n = rng.range(1, len as i64);
    let helper = match shape {
        // for-each comparison count
        0 => format!(
            "fn helper a array<f64> -> i64\n    let c i64 = 0\n    for x in a\n        \
             if x > {t}\n            c = c + 1\n    c\n"
        ),
        // indexed comparison count with a separate length
        1 => format!(
            "fn helper a array<f64> n i64 -> i64\n    let c i64 = 0\n    \
             for i from 0 to n - 1\n        if a[i] > {t}\n            c = c + 1\n    c\n"
        ),
        // summed via len(a) then compared to a threshold
        _ => format!(
            "fn helper a array<f64> -> i64\n    let acc f64 = 0.0\n    \
             for i from 0 to len(a) - 1\n        acc = acc + a[i]\n    \
             if acc > {t}\n        1\n    else\n        0\n"
        ),
    };
    let call = if shape == 1 {
        format!("helper(xs, {n})")
    } else {
        "helper(xs)".to_string()
    };
    let bias = rng.range(-100, 100);
    format!(
        "{helper}\nfn main -> i64\n    let xs array<f64> = {arr}\n    \
         let r i64 = {call}\n    r + {bias}\n"
    )
}

/// Generates one program that exercises **inline, by-value fixed-extent arrays as
/// struct fields** (road_to_1_0_stable A2, increments 2 and 4): a `struct` with a
/// scalar field, a fixed `array<i64, N>` FIELD, and a trailing scalar field is
/// constructed (via an element list or a `[v; k]` fill), copied by value
/// (`let g = f`), has a random in-bounds copy element mutated, is passed BY VALUE
/// into a helper that mutates its own parameter's fields, and is finally read
/// element-by-element with a folded `len`. Increment 4's two whole-field shapes are
/// woven in: a whole-FIELD by-value copy (`let c = f.xs; c[cj] = cv`) whose mutation
/// must not touch `f.xs`, and a `for x in f.xs` reduction — both reading `f.xs` after
/// the copy's mutation, so an aliasing whole-field copy corrupts `sum_f`/`fsum` and
/// diverges.
///
/// This is the differential net for the extent-survival channel and the inline
/// aggregate layout. Every access is in bounds (indices drawn from `0..N`), so the
/// program is divergence-free; the by-value copy and by-value parameter make the
/// caller's original independent of every mutation, so any aliasing/partial-copy/
/// wrong-stride miscompile surfaces as a native/interpreter exit-code divergence.
/// The interpreters are the ground truth (all three must already agree); the shape
/// is chosen to always LOWER natively (scalar `i64` element, in-bounds indices,
/// direct field ops, the whole-field copy, and `for x in f.xs`), so
/// `fuzz_native_exit` produces a real exe for every program rather than skipping.
fn gen_struct_array_field_program(seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let len = rng.range(2, 6) as usize;
    // Field sum helper: `<recv>.xs[0] + <recv>.xs[1] + ...` over all `len` elements.
    let xs_sum = |recv: &str| -> String {
        (0..len)
            .map(|i| format!("{recv}.xs[{i}]"))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    // Sum over a bare array LOCAL (the whole-field copy `c`), indexed directly.
    let arr_sum = |name: &str| -> String {
        (0..len)
            .map(|i| format!("{name}[{i}]"))
            .collect::<Vec<_>>()
            .join(" + ")
    };

    let a0 = rng.range(-30, 30);
    let b0 = rng.range(-30, 30);
    // Construction: an explicit element list, or a `[v; len]` fill (both lower).
    let construct = if rng.chance(2) {
        let elems: Vec<String> = (0..len).map(|_| rng.range(-30, 30).to_string()).collect();
        format!("[{}]", elems.join(", "))
    } else {
        let v = rng.range(-30, 30);
        format!("[{v}; {len}]")
    };

    // The whole-struct copy's mutation (a random in-bounds element and the scalar `a`).
    let gj = rng.below(len as u64) as usize;
    let gv = rng.range(-30, 30);
    // The whole-FIELD copy's mutation (`let c = f.xs; c[cj] = cv`): its own snapshot,
    // so it must NOT touch `f.xs` — an aliasing copy would corrupt `sum_f`/`fsum`
    // below (both read `f.xs` after this mutation) and diverge from the interpreters.
    let cj = rng.below(len as u64) as usize;
    let cv = rng.range(-30, 30);
    // The helper's parameter mutations (its own copy — must not touch the caller).
    let mj = rng.below(len as u64) as usize;
    let mv = rng.range(-30, 30);
    let da = rng.range(-30, 30);
    let bias = rng.range(-100, 100);

    format!(
        "struct S\n\
         \x20   a i64\n\
         \x20   xs array<i64, {len}>\n\
         \x20   b i64\n\n\
         fn touch s S -> i64\n\
         \x20   s.xs[{mj}] = {mv}\n\
         \x20   s.a = s.a + {da}\n\
         \x20   {sum_s} + s.a + s.b\n\n\
         fn main -> i64\n\
         \x20   let f = S(a: {a0}, xs: {construct}, b: {b0})\n\
         \x20   let g = f\n\
         \x20   g.xs[{gj}] = {gv}\n\
         \x20   let c = f.xs\n\
         \x20   c[{cj}] = {cv}\n\
         \x20   let fsum i64 = 0\n\
         \x20   for x in f.xs\n\
         \x20       fsum = fsum + x\n\
         \x20   let t i64 = touch(f)\n\
         \x20   t + {sum_f} + f.a + f.b + {sum_g} + g.a + g.b + len(f.xs) + {sum_c} \
         + fsum + {bias}\n",
        sum_s = xs_sum("s"),
        sum_f = xs_sum("f"),
        sum_g = xs_sum("g"),
        sum_c = arr_sum("c"),
    )
}

/// Generates one program that exercises **RC drop insertion on loop EARLY-EXIT
/// edges** (memory-model stage 2): a loop allocates a fresh, uniquely-owned `string`
/// each iteration, borrows it via `len`, and may `break`/`continue` before reaching
/// the fallthrough back-edge. Drop insertion is behavior-neutral to the *computed
/// value* — it only reclaims memory — so any drop-induced miscompile (a clobbered
/// accumulator across the `call rc_dec`, a corrupted still-live value, or an
/// unbalanced free that corrupts the heap and returns garbage) surfaces as a
/// native/interpreter exit-code divergence. The nested shape additionally checks that
/// an inner `break`/`continue` drops only the inner loop's owned string while the
/// outer string stays live across the inner loop and is dropped on the outer edge.
fn gen_string_loop_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x5DEE_CE66_D1CE_B00Du64);
    let hi = rng.range(3, 30);
    // A fresh, uniquely-owned string expression in the counter `i` (each of these
    // native builtins always allocates a new record, so it is droppable).
    let fresh = |rng: &mut Rng| -> String {
        match rng.below(4) {
            0 => "to_string(i) + \"!!\"".to_string(),
            1 => "trim(to_string(i * 3))".to_string(),
            2 => "substring(to_string(i * 7 + 1), 0, 1)".to_string(),
            _ => "repeat(\"ab\", 2)".to_string(),
        }
    };

    if rng.chance(3) {
        // Nested shape: an outer owned string live across an inner loop whose own
        // owned string exits early.
        let hi2 = rng.range(2, 12);
        let k = rng.range(2, 5);
        let inner_exit = if rng.chance(2) {
            format!("            if j % {k} == 0\n                continue\n")
        } else {
            format!(
                "            if j >= {c}\n                break\n",
                c = rng.range(1, hi2)
            )
        };
        let of = fresh(&mut rng);
        let jf = match rng.below(3) {
            0 => "to_string(j) + \".\"".to_string(),
            1 => "trim(to_string(j * 2))".to_string(),
            _ => "substring(to_string(j + 5), 0, 1)".to_string(),
        };
        return format!(
            "fn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
             let outer string = {of}\n        total = total + len(outer)\n        \
             for j from 0 to {hi2}\n            let inner string = {jf}\n            \
             total = total + len(inner)\n{inner_exit}            total = total + 1\n        \
             total = total + len(outer)\n    total\n"
        );
    }

    // Single-loop shape with an optional early exit before the fallthrough drop.
    let f = fresh(&mut rng);
    let exit = match rng.below(3) {
        0 => {
            let k = rng.range(2, 5);
            format!("        if i % {k} == 0\n            continue\n")
        }
        1 => format!(
            "        if i >= {c}\n            break\n",
            c = rng.range(1, hi)
        ),
        // No early exit — a plain fallthrough loop (still a valid oracle case).
        _ => String::new(),
    };
    format!(
        "fn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
         let s string = {f}\n        total = total + len(s)\n{exit}        \
         total = total + 1\n    total\n"
    )
}

/// Generates one program that exercises **arena-first memory**: an arena-eligible
/// LEAF helper `h a i64 -> i64` (scalar return, no user calls, allocates strings
/// that stay local) called from `main`'s bounded loop. `h` routes its heap through
/// the function-scoped arena and rewinds the bump pointer on return; the arena is
/// behavior-neutral to the computed value (it only changes WHEN memory is
/// reclaimed), so ANY native/interpreter divergence here is an arena-induced
/// miscompile (a clobbered return value across the reset, or an unsound rewind that
/// corrupts a still-live value).
///
/// Two shapes, chosen per seed:
/// * **straight-line** (stage 1): `h` builds a couple of local strings and returns
///   a scalar derived from them — a function-scoped region reclaimed at return.
/// * **confined loop** (stage 2): `h` ITSELF loops, allocating per-iteration scratch
///   that stays local (a fresh `string` read only by `len`, accumulating a SCALAR),
///   so the loop gets a per-iteration bump-pointer **sub-region**. This is the
///   stage-2 oracle: a mis-scoped rewind (freeing a still-live accumulator, or
///   leaking so the fixed heap exhausts) surfaces as an exit-code divergence.
///
/// Every string op is one the native subset supports and all backends agree on
/// (ASCII, so `len` counts identically), and the bounds are always valid, so the
/// program is divergence-free like the other generators.
fn gen_arena_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0xA5A5_1234_DEAD_BEEFu64);
    let hi = rng.range(5, 40);
    let bias = rng.range(-50, 50);
    let main = format!(
        "\n\nfn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
         total = total + h(i)\n    total + {bias}\n"
    );

    // Confined-loop shape: `h` allocates non-escaping per-iteration scratch. Half
    // the batch, so both the stage-1 (function region) and stage-2 (loop sub-region)
    // paths are covered.
    if rng.below(2) == 0 {
        // A few iterations; the per-iteration string is uniquely owned and read only
        // by `len` (borrow-only), so the loop is confined and gets a sub-region.
        let k = rng.range(2, 12);
        let scratch = match rng.below(4) {
            0 => "to_string(a + j) + \"__x__\"".to_string(),
            1 => "trim(to_string(a * 2 + j))".to_string(),
            2 => "upper(to_string(a + j * 3))".to_string(),
            _ => "repeat(\"ab\", 2) + to_string(j)".to_string(),
        };
        let acc = match rng.below(3) {
            0 => "total = total + len(s)".to_string(),
            1 => "total = total + len(s) * 2".to_string(),
            _ => "total = total + len(s) - 1".to_string(),
        };
        return match rng.below(3) {
            // Borrow-only per-iteration scratch (`let s` read only by `len`): confined
            // under both the pre-I4 and I4 rules.
            0 => {
                let h = format!(
                    "fn h a i64 -> i64\n    let total i64 = 0\n    for j from 0 to {k}\n        \
                     let s string = {scratch}\n        {acc}\n    total\n"
                );
                format!("{h}{main}")
            }
            // I4 target-aware widening: REBIND the loop-local `s` with a fresh
            // allocation each iteration (`s = s + <suffix>`). `s` is a top-level `let`
            // of the loop body, so the rebind is iteration-local and the loop stays
            // confined — newly admitted by I4. Value-neutral: the accumulator reads
            // the (now longer) `len(s)`, which every tier computes identically.
            1 => {
                let suffix = match rng.below(3) {
                    0 => "?",
                    1 => "__",
                    _ => "!!",
                };
                let h = format!(
                    "fn h a i64 -> i64\n    let total i64 = 0\n    for j from 0 to {k}\n        \
                     let s string = {scratch}\n        s = s + \"{suffix}\"\n        {acc}\n    \
                     total\n"
                );
                format!("{h}{main}")
            }
            // I4 nested-loop iteration-local scratch: the inner loop rebinds its own
            // top-level `let t`, so BOTH loop levels are confined and each gets a
            // per-iteration sub-region. Newly admitted by I4 (the inner heap rebind
            // used to deny both loops).
            _ => {
                let inner = rng.range(2, 6);
                let inner_scratch = match rng.below(3) {
                    0 => "to_string(a + j + m)".to_string(),
                    1 => "upper(to_string(a + j * 2 + m))".to_string(),
                    _ => "repeat(\"c\", 1) + to_string(j + m)".to_string(),
                };
                let h = format!(
                    "fn h a i64 -> i64\n    let total i64 = 0\n    for j from 0 to {k}\n        \
                     for m from 0 to {inner}\n            let t string = {inner_scratch}\n            \
                     t = t + \"!\"\n            total = total + len(t)\n    total\n"
                );
                format!("{h}{main}")
            }
        };
    }

    // Straight-line shape (stage 1): `s`, then `t` derived from `s`, then a scalar.
    let s_expr = match rng.below(4) {
        0 => "to_string(a) + \"__suffix__\"".to_string(),
        1 => "trim(to_string(a * 3))".to_string(),
        2 => "substring(to_string(a * 7 + 1), 0, 1)".to_string(),
        _ => "repeat(\"ab\", 3)".to_string(),
    };
    let t_expr = match rng.below(3) {
        0 => "upper(s)".to_string(),
        1 => "lower(s)".to_string(),
        _ => "s + \"?\"".to_string(),
    };
    let result = match rng.below(3) {
        0 => "len(s) + len(t)".to_string(),
        1 => "len(t) - len(s)".to_string(),
        _ => "len(s) * 2 + len(t)".to_string(),
    };
    let h = format!(
        "fn h a i64 -> i64\n    let s string = {s_expr}\n    let t string = {t_expr}\n    \
         {result}\n"
    );
    format!("{h}{main}")
}

/// Generates one program that exercises **arena-first memory with a heap-typed
/// aggregate field**: an arena-eligible LEAF helper `h a i64 -> i64` that constructs
/// a `struct Rec { name string, id i64 }` — a struct whose `string` field is a
/// one-level heap-typed field — keeps it local (read only via `len(r.name)` and the
/// scalar `r.id`), and returns a scalar. `h` routes its heap through the
/// function-scoped arena (straight-line shape) or a per-iteration sub-region
/// (confined-loop shape), so the struct's `string` record is reclaimed by the bump
/// rewind. The recursive drop-glue (`rc_dec` per string field) coexists with the
/// arena (`rc_free` no-ops in arena mode), so ANY native/interpreter divergence is a
/// reclamation or layout miscompile of the heap-typed aggregate field.
///
/// Every string op is native-subset and all-backend-agreeing (ASCII `len`, valid
/// bounds), and the struct stays borrow-only, so the program is divergence-free like
/// the other generators.
fn gen_arena_struct_string_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x51C7_0F33_2B9E_1D47u64);
    let hi = rng.range(5, 40);
    let bias = rng.range(-50, 50);
    let rec = "struct Rec\n    name string\n    id i64\n\n";
    let main = format!(
        "\n\nfn main -> i64\n    let total i64 = 0\n    for i from 0 to {hi}\n        \
         total = total + h(i)\n    total + {bias}\n"
    );

    // Confined-loop shape (stage 2): `h` allocates a non-escaping per-iteration
    // struct-string temp, so the loop is confined and gets a sub-region.
    if rng.below(2) == 0 {
        let k = rng.range(2, 12);
        let name_expr = match rng.below(4) {
            0 => "to_string(a + j) + \"__x__\"".to_string(),
            1 => "trim(to_string(a * 2 + j))".to_string(),
            2 => "upper(to_string(a + j * 3))".to_string(),
            _ => "repeat(\"ab\", 2) + to_string(j)".to_string(),
        };
        let acc = match rng.below(3) {
            0 => "total = total + len(r.name) + r.id".to_string(),
            1 => "total = total + len(r.name) * 2 - r.id".to_string(),
            _ => "total = total + len(r.name)".to_string(),
        };
        // I4: half of the confined struct-string batch REBINDS the loop-local `r` with
        // a fresh heap-carrying struct each iteration (`r = Rec(r.name + "!", …)`). `r`
        // is a top-level `let` of the loop body, so the rebind is iteration-local and
        // the loop stays confined — a heap-CARRYING-aggregate store newly admitted by
        // I4. Value-neutral: the accumulator reads the rebuilt fields identically on
        // every tier.
        if rng.below(2) == 0 {
            let h = format!(
                "fn h a i64 -> i64\n    let total i64 = 0\n    for j from 0 to {k}\n        \
                 let r Rec = Rec({name_expr}, a + j)\n        r = Rec(r.name + \"!\", r.id + 1)\n        \
                 {acc}\n    total\n"
            );
            return format!("{rec}{h}{main}");
        }
        let h = format!(
            "fn h a i64 -> i64\n    let total i64 = 0\n    for j from 0 to {k}\n        \
             let r Rec = Rec({name_expr}, a + j)\n        {acc}\n    total\n"
        );
        return format!("{rec}{h}{main}");
    }

    // Straight-line shape (stage 1): build a struct-string local, derive a scalar.
    let name_expr = match rng.below(4) {
        0 => "to_string(a) + \"__suffix__\"".to_string(),
        1 => "trim(to_string(a * 3))".to_string(),
        2 => "substring(to_string(a * 7 + 1), 0, 1)".to_string(),
        _ => "repeat(\"ab\", 3)".to_string(),
    };
    let result = match rng.below(3) {
        0 => "len(r.name) + r.id".to_string(),
        1 => "len(r.name) * 2 - r.id".to_string(),
        _ => "len(r.name) + r.id * 3".to_string(),
    };
    let h = format!("fn h a i64 -> i64\n    let r Rec = Rec({name_expr}, a)\n    {result}\n");
    format!("{rec}{h}{main}")
}

/// Generates a program that builds a `list<Rec>` where `Rec` carries a `string`
/// field, in one of three push styles, then reads a retrieved element's fields:
///
/// - **inline constructor** (`bucket = push(bucket, Rec(expr, k))`) — native-eligible;
///   the fresh struct block is built directly on the heap, so it must run
///   native==interpreter;
/// - **struct variable** (`let r Rec = Rec(expr, k)` then `push(bucket, r)`) — the
///   native backend must SKIP cleanly (a stack-flattened struct value has no
///   `[nwords]` heap block for `__lullaby_struct_copy` to deep-copy);
/// - **per-iteration temp in a loop** (the `esc_list` shape) — same struct-variable
///   push, inside a `while`, which must also skip cleanly.
///
/// The struct-variable styles are exactly the previously-unexercised composition that
/// used to SIGSEGV in native codegen. After the soundness fix each program either runs
/// native==interpreter (inline) or is a clean skip (variable) — never a produced exe
/// that crashes. The differential harness ([`fuzz_list_struct_string_native_no_crash_when_linkable`])
/// accepts both outcomes and only fails on a produced exe that diverges.
fn gen_list_struct_string_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x6D2B_79F5_A1C3_04E9u64);
    let rec = "struct Rec\n    name string\n    id i64\n\n";
    let bias = rng.range(-30, 30);
    let n = rng.range(2, 5);
    // 0 = inline constructors (native-eligible), 1 = struct variables (skip),
    // 2 = per-iteration struct temp pushed in a loop (skip; the `esc_list` shape).
    let style = rng.below(3);

    fn name_expr(rng: &mut Rng, k: &str) -> String {
        match rng.below(4) {
            0 => format!("to_string({k})"),
            1 => format!("to_string({k}) + \"!\""),
            2 => format!("trim(to_string({k} * 2))"),
            _ => "\"item\"".to_string(),
        }
    }

    let mut body = String::from("\n\nfn main -> i64\n    let bucket list<Rec> = list_new()\n");
    match style {
        2 => {
            let ne = name_expr(&mut rng, "i");
            body.push_str(&format!(
                "    let i i64 = 0\n    while i < {n}\n        let r Rec = Rec({ne}, i)\n        \
                 bucket = push(bucket, r)\n        i += 1\n"
            ));
        }
        1 => {
            for k in 0..n {
                let ne = name_expr(&mut rng, &k.to_string());
                body.push_str(&format!(
                    "    let r{k} Rec = Rec({ne}, {k})\n    bucket = push(bucket, r{k})\n"
                ));
            }
        }
        _ => {
            for k in 0..n {
                let ne = name_expr(&mut rng, &k.to_string());
                body.push_str(&format!("    bucket = push(bucket, Rec({ne}, {k}))\n"));
            }
        }
    }
    let idx = rng.range(0, n - 1);
    body.push_str(&format!(
        "    let got Rec = get(bucket, {idx})\n    len(got.name) + got.id + {bias}\n"
    ));
    format!("{rec}{body}")
}

/// Generates a program exercising **native monomorphization of user-defined
/// generic types with SCALAR type arguments**. Fixed generic declarations
/// (`Box<T>`, `Pair<A, B>`, `Opt<T>`) are instantiated with scalar arguments
/// (`i64`/`bool`) and randomized values, exercising construction (positional and
/// named), field read, value-semantic field write (mutating one copy must not
/// affect another), `match` over a generic enum, value-semantic copy, and
/// passing/returning generic values across function boundaries. Every
/// instantiation is scalar-only, so it must compile natively AND agree with the
/// interpreters — a monomorphized `Box<i64>` is byte-identical to the erased
/// `Box<i64>`. All arithmetic is wrapping `+`/`-`/`*` over `i64`/`bool` cells, so
/// the program is divergence-free and its `main` returns an `i64`.
fn gen_generic_scalar_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x2B1D_66E4_9F07_5A3Cu64);
    let decls = "struct Box<T>\n    value T\n\n\
                 struct Pair<A, B>\n    first A\n    second B\n\n\
                 enum Opt<T>\n    present T\n    absent\n\n\
                 fn unbox b Box<i64> -> i64\n    b.value\n\n\
                 fn opt_or o Opt<i64> f i64 -> i64\n    match o\n        \
                 present(x) -> x\n        absent -> f\n\n";

    let v1 = rng.range(-1000, 1000);
    let v2 = rng.range(-1000, 1000);
    let v3 = rng.range(-1000, 1000);
    let flag = rng.chance(2);
    let pair_bool = rng.chance(2);
    let present = rng.chance(2);
    let mutate = rng.chance(2);
    let mutval = rng.range(-500, 500);
    let fallback = rng.range(-500, 500);

    let mut body = String::from("fn main -> i64\n");
    // A boxed i64, a value-semantic copy, and a (maybe) mutated third copy: the
    // final sum reads `unbox(a)` to prove `a` survived every copy/mutation.
    body.push_str(&format!("    let a Box<i64> = Box({v1})\n"));
    body.push_str("    let c Box<i64> = a\n");
    body.push_str("    let m Box<i64> = a\n");
    if mutate {
        body.push_str(&format!("    m.value = {mutval}\n"));
    }
    // A second scalar instantiation of `Box` (bool) and a two-parameter `Pair`.
    body.push_str(&format!("    let fb Box<bool> = Box({flag})\n"));
    body.push_str(&format!(
        "    let p Pair<i64, bool> = Pair(first: {v2}, second: {pair_bool})\n"
    ));
    let opt_ctor = if present {
        format!("present({v3})")
    } else {
        "absent".to_string()
    };
    body.push_str(&format!("    let o Opt<i64> = {opt_ctor}\n"));
    // The bool field / bool box guard scalar contributions.
    body.push_str("    let pg i64 = p.first if p.second else 0\n");
    body.push_str("    let fg i64 = 10 if fb.value else 20\n");
    body.push_str(&format!(
        "    unbox(a) + c.value + m.value + pg + fg + opt_or(o, {fallback})\n"
    ));
    format!("{decls}{body}")
}

/// Generates a program exercising **native inherent-method dispatch** (A1): a
/// non-generic `struct Counter` with inherent methods (`bump` returns a fresh
/// `Counter`, `get` reads the scalar field) and a generic `Box<T>`/`Opt<T>` with
/// methods (`peek`/`rewrap`, `unwrap_or`), called over the `i64`/`bool`/`string`
/// instantiations. Each `recv.method(args)` monomorphizes to a direct call whose
/// `self` is a copied aggregate, so the receiver is unchanged after the call —
/// `c.get()` after `c.bump(..)` reads the original value, the value-semantics
/// oracle. A chained call (`c.bump(v).get()`) exercises method-call chaining. All
/// arithmetic is wrapping `+`/`-`/`*` over `i64`/`bool` cells (ASCII `len` for the
/// string box), so the program is divergence-free and `main` returns an `i64`.
fn gen_method_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x7D14_9C3B_5E62_08AFu64);
    let decls = "struct Counter\n    n i64\n\n\
                 impl Counter\n    fn bump self d i64 -> Counter\n        \
                 Counter(self.n + d)\n    fn get self -> i64\n        self.n\n\n\
                 struct Box<T>\n    value T\n\n\
                 impl Box<T>\n    fn peek self -> T\n        self.value\n    \
                 fn rewrap self v T -> Box<T>\n        Box(v)\n\n\
                 enum Opt<T>\n    present T\n    absent\n\n\
                 impl Opt<T>\n    fn unwrap_or self f T -> T\n        match self\n            \
                 present(x) -> x\n            absent -> f\n\n";

    let v1 = rng.range(-1000, 1000);
    let v2 = rng.range(-1000, 1000);
    let v3 = rng.range(-1000, 1000);
    let v4 = rng.range(-1000, 1000);
    let v5 = rng.range(-1000, 1000);
    let v6 = rng.range(-1000, 1000);
    let flag = rng.chance(2);
    let present = rng.chance(2);
    let fallback = rng.range(-500, 500);
    let word = match rng.below(4) {
        0 => "hello",
        1 => "abc",
        2 => "",
        _ => "native",
    };

    let mut body = String::from("fn main -> i64\n");
    // Non-generic method + value semantics: `c` is read AFTER `c.bump(..)`, so it
    // must be unchanged (self is a copied aggregate).
    body.push_str(&format!("    let c Counter = Counter({v1})\n"));
    body.push_str(&format!("    let d Counter = c.bump({v2})\n"));
    // Generic struct method over two scalar instantiations + a fresh-aggregate return.
    body.push_str(&format!("    let a Box<i64> = Box({v3})\n"));
    body.push_str(&format!("    let b Box<i64> = a.rewrap({v4})\n"));
    body.push_str(&format!("    let fb Box<bool> = Box({flag})\n"));
    // A heap-field (`string`) receiver method read.
    body.push_str(&format!("    let sb Box<string> = Box(\"{word}\")\n"));
    let opt_ctor = if present {
        format!("present({v5})")
    } else {
        "absent".to_string()
    };
    body.push_str(&format!("    let o Opt<i64> = {opt_ctor}\n"));
    body.push_str("    let fg i64 = 7 if fb.peek() else 3\n");
    // A chained method call `c.bump(v6).get()` plus reads that prove `c` survived.
    body.push_str(&format!(
        "    c.get() + d.get() + a.peek() + b.peek() + fg + len(sb.peek()) \
         + o.unwrap_or({fallback}) + c.bump({v6}).get()\n"
    ));
    format!("{decls}{body}")
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

/// Whether this host can run a natively-produced `.exe` at all.
///
/// The native differential fuzzers used to gate on `rust-lld` + `kernel32.lib`.
/// That gate is **obsolete**: direct-PE emission is the default for every eligible
/// executable build, so the CLI writes a runnable image in-house with no external
/// linker and no import library (see `native_default_direct_pe_runs_without_linker`
/// in `suite2.rs`). Gating on the linker silently turned all nine native fuzzers
/// into no-ops on a host without it — a vacuous green.
///
/// The honest gate is: can we execute a Windows `.exe`? Whether an individual
/// program reaches the direct-PE path or the linker path is then asserted
/// per-program by [`fuzz_native_exit`], which fails loudly if NEITHER produced an
/// exe and the linker was available — rather than skipping.
fn native_exe_runnable() -> bool {
    cfg!(windows)
}

/// Compile `source` to a real `.exe` and return its exit code, or `None` when the
/// program legitimately could not be turned into an executable on this host (no
/// direct-PE image AND no linker). Panics on a native emit failure, so a
/// regression that makes a previously-compiling program skip is a FAILURE, not a
/// silent pass.
///
/// Takes a [`ScratchDir`] rather than a `&Path` **on purpose**: the exe written
/// here is executed here, so its path must not be shared with any other process.
/// `ScratchDir` is the only way to name a directory for this function, and it
/// cannot be pointed at a fixed path — so a new fuzzer gets process isolation by
/// construction instead of by remembering to ask for it.
fn fuzz_native_exit(source: &str, dir: &ScratchDir, tag: &str) -> Option<i32> {
    let src_path = dir.join(format!("{tag}.lby"));
    let exe_path = dir.join(format!("{tag}.exe"));
    std::fs::write(&src_path, source).expect("write fuzz source");
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
        "native emit failed for {tag}:\n{source}\n{}",
        stderr(&emit)
    );
    if !exe_path.is_file() {
        // No direct-PE image AND the linker could not run. Only excusable when the
        // link toolchain is genuinely absent; otherwise it is a real failure.
        if rust_lld_path().is_none() || !kernel32_available() {
            eprintln!("{tag}: no direct-PE image and no link toolchain; skipping");
            return None;
        }
        panic!(
            "no exe produced for {tag} despite an available linker:\n{source}\n{}",
            stdout(&emit)
        );
    }
    let run = Command::new(&exe_path).output().expect("run fuzz exe");
    Some(run.status.code().expect("native exit code"))
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
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native differential fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 120;
    let base_seed = 0xD1B5_4A32_D192_ED03u64;
    let dir = ScratchDir::new("scalar");

    // Counts what ACTUALLY executed, so the batch cannot silently do nothing and
    // still pass green (asserted after the loop).
    let mut ran = 0u64;
    let mut with_void = 0u64;

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_program(seed);
        // Void helpers are generated into a fraction of the programs; track how
        // many so the void surface's differential coverage is visible rather than
        // assumed. Helpers are prepended, so the first one starts the source with
        // no preceding newline — check both, or the count silently undercounts.
        if source.starts_with("fn h") || source.contains("\nfn h") {
            with_void += 1;
        }

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

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_{i}")) else {
            break;
        };
        ran += 1;
        // The entry stub forwards main's i64 to ExitProcess; on Windows the full
        // 32-bit value round-trips, so the exit code equals `expected as i32`.
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
    // ASSERT, don't just report. The `native_exe_runnable()` gate above already
    // established a Windows host, and every program this generator emits is
    // direct-PE eligible (it has a `main` and needs no C runtime), so its exe is
    // written in-house with no external linker — `fuzz_native_exit` cannot take its
    // no-toolchain escape here, and all `PROGRAMS` must have run. Merely REPORTING
    // the count still let `ran == 0` pass green, which would prove nothing about
    // the emitter; this makes an empty batch a failure.
    assert!(
        ran > 0,
        "the native differential fuzz executed NO programs on a Windows host — a \
         green result here would prove nothing about the emitter"
    );
    // Visible under `--nocapture`. `with_void` counts programs carrying a void
    // helper called for effect: such a call contributes nothing to `main`'s value,
    // so any divergence it causes is a void-codegen bug. See `Gen::void_helper` for
    // what that does and does not catch.
    eprintln!(
        "scalar native fuzz: ran {ran}/{PROGRAMS} real exes ({with_void} carried void helpers)"
    );
}

#[test]
fn fuzz_array_interpreters_agree() {
    // Cross-check the three engines on fat-pointer `array<i64>`-parameter programs.
    // Always runs (no toolchain needed); a divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x2545_F491_4F6C_DD1Du64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_array_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "array backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "array generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_array_f64_interpreters_agree() {
    // Cross-check the three engines on fat-pointer `array<f64>`-parameter programs.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0xB5A9_7D3C_11E6_82F7u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_array_f64_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "array<f64> backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "array<f64> generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_array_native_matches_interpreter_when_linkable() {
    // Compile each fat-pointer `array<i64>`-parameter program to a real `.exe` and
    // check its exit code against the interpreter result — the high-value oracle for
    // the calling-convention change. Gated on the link toolchain; skips cleanly when
    // absent. This ASSERTS the helper (a fat-pointer array parameter) compiles
    // natively, so a regression that demoted it would fail here, not silently pass.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native array differential fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 80;
    let base_seed = 0x7C6A_2F13_88BE_4D05u64;
    let dir = ScratchDir::new("array");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        // Alternate `array<i64>` and `array<f64>` element types so the fat-pointer
        // path is exercised for both the integer (GPR) and float (XMM) element read.
        let source = if i % 2 == 0 {
            gen_array_program(seed)
        } else {
            gen_array_f64_program(seed)
        };

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-array-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("array generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_arr_{i}")) else {
            return;
        };
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (fat array) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}

#[test]
fn fuzz_struct_array_field_interpreters_agree() {
    // Cross-check the three engines on struct-with-fixed-array-field programs
    // (construct, by-value copy + mutation, by-value parameter mutation, field
    // reads, `len`). Always runs (no toolchain needed); a divergence prints the
    // reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x51F3_9C2A_7D48_6E11u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_struct_array_field_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "struct-array-field backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "struct-array-field generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_struct_array_field_native_matches_interpreter_when_linkable() {
    // Compile each struct-with-fixed-array-field program to a real `.exe` and check
    // its exit code against the interpreter result — the high-value oracle for the
    // inline aggregate layout and the extent-survival channel. Gated on the native
    // toolchain; skips cleanly when absent. Every generated program is chosen to
    // LOWER inline (a scalar-element fixed-array field, in-bounds indices, direct
    // field ops), so `fuzz_native_exit` produces a real exe and the shape can never
    // silently demote here — a regression that un-compiled it would fail loudly.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native struct-array-field fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 120;
    let base_seed = 0xAF21_6BD0_35C7_9E4Du64;
    let dir = ScratchDir::new("struct_array_field");

    // Counts what ACTUALLY executed, so the batch cannot silently do nothing and
    // still pass green (asserted after the loop).
    let mut ran = 0u64;

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_struct_array_field_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-struct-array-field-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!(
                "struct-array-field generator produced {other:?} on seed {seed:#x}:\n{source}"
            ),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_saf_{i}")) else {
            break;
        };
        ran += 1;
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (struct array field) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
    // ASSERT, don't just report. Every program this generator emits is direct-PE
    // eligible (a `main`, no C runtime), so its exe is written in-house with no
    // external linker — `fuzz_native_exit` cannot take its no-toolchain escape here,
    // and all `PROGRAMS` must have run. An empty batch is a failure, not a pass.
    assert!(
        ran > 0,
        "the native struct-array-field fuzz executed NO programs on a Windows host — a \
         green result here would prove nothing about the inline aggregate layout"
    );
    eprintln!("struct-array-field native fuzz: ran {ran}/{PROGRAMS} real exes");
}

#[test]
fn fuzz_string_loop_interpreters_agree() {
    // Cross-check the three engines on loop programs that allocate/borrow a fresh
    // per-iteration string with `break`/`continue`. Always runs (no toolchain needed).
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x3C79_AC49_2E6D_1F55u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_string_loop_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "string-loop backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "string-loop generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_string_loop_native_matches_interpreter_when_linkable() {
    // The high-value oracle for RC drop insertion on loop early-exit edges: compile
    // each generated string-loop program to a real `.exe` and check its exit code
    // against the interpreter. Because drops are behavior-neutral to the computed
    // value, ANY divergence here is a drop-induced miscompile (a clobbered live
    // accumulator across `call rc_dec`, or an unbalanced free that corrupts the heap).
    // ASSERTS `main` compiles natively so a regression that demoted it (hiding the
    // drop path) fails loudly rather than silently passing on the interpreter.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native string-loop fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x6EA3_55C1_9B02_7DF1u64;
    let dir = ScratchDir::new("string_loop");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_string_loop_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-string-loop-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => {
                panic!("string-loop generator produced {other:?} on seed {seed:#x}:\n{source}")
            }
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_sl_{i}")) else {
            return;
        };
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (string-loop RC drop) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}

#[test]
fn fuzz_arena_interpreters_agree() {
    // Cross-check the three engines on arena-eligible-function programs. Always runs
    // (no toolchain needed); a divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x41E5_9A2C_7B3D_0F19u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_arena_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "arena backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "arena generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_arena_native_matches_interpreter_when_linkable() {
    // The high-value oracle for arena-first memory (stage 1): compile each generated
    // program (an arena-eligible leaf helper called from a loop) to a real `.exe` and
    // check its exit code against the interpreter. The arena is behavior-neutral to the
    // computed value, so ANY divergence here is an arena-induced miscompile (a return
    // value clobbered by the return-edge reset, or an unsound bump-pointer rewind).
    // Gated on the link toolchain; skips cleanly when absent.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native arena fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x9D2B_C4E7_16A0_38F5u64;
    let dir = ScratchDir::new("arena");

    let mut ran = 0usize;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_arena_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-arena-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("arena generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_arena_{i}")) else {
            break;
        };
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (arena) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
        ran += 1;
    }
    // The arena programs are all direct-PE eligible, so on a Windows host every one
    // produces and runs an exe — a batch that executed nothing would mean the oracle
    // proved nothing (a silent-pass regression). The I4 rebind/nested shapes are part
    // of this batch, so this also guards that the newly-admitted shapes were run.
    assert!(
        ran > 0,
        "the native arena oracle executed no program — it must run a non-empty batch"
    );
}

#[test]
fn fuzz_arena_struct_string_interpreters_agree() {
    // Cross-check the three engines on arena-eligible programs that build a struct
    // with a heap-typed (`string`) field. Always runs (no toolchain needed); a
    // divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x7C3E_15A9_D40B_2E86u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_arena_struct_string_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "arena struct-string backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "arena struct-string generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_arena_struct_string_native_matches_interpreter_when_linkable() {
    // The high-value oracle for arena reclamation of a **heap-typed aggregate field**:
    // compile each generated program (an arena-eligible leaf that builds a
    // struct-with-`string`-field, called from a loop) to a real `.exe` and check its
    // exit code against the interpreter. The arena rewind reclaims the struct's string
    // record while the recursive drop-glue's `rc_free` no-ops, so ANY divergence is a
    // reclamation/layout miscompile of the heap-field aggregate. Also asserts the
    // arena-eligible helper compiles natively (a regression that demoted it fails here
    // rather than silently passing). Gated on the link toolchain; skips when absent.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native arena struct-string fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x2F81_66DA_9C40_7B15u64;
    let dir = ScratchDir::new("arena_struct_string");

    let mut ran = 0usize;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_arena_struct_string_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-arena-struct-string-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!(
                "arena struct-string generator produced {other:?} on seed {seed:#x}:\n{source}"
            ),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("fuzz_arena_ss_{i}")) else {
            break;
        };
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (arena struct-string) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
        ran += 1;
    }
    // A non-empty batch must have executed — the I4 heap-carrying-aggregate rebind
    // shape is part of it, so an empty batch would silently prove nothing.
    assert!(
        ran > 0,
        "the native arena struct-string oracle executed no program — it must run a non-empty batch"
    );
}

#[test]
fn fuzz_list_struct_string_interpreters_agree() {
    // Cross-check the three engines on `list<struct-with-string-field>` programs that
    // build the list via inline constructors, struct variables, or a per-iteration
    // loop temp. Always runs (no toolchain needed).
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x4C7B_2E91_D3A6_08F1u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_list_struct_string_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "backend divergence on list-struct-string-fuzz #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_list_struct_string_native_no_crash_when_linkable() {
    // The regression oracle for `list<struct-with-string-field>` when a struct
    // VARIABLE (or a per-iteration loop temp) is pushed — the previously-unexercised
    // shape that used to SIGSEGV in native codegen. After the soundness fix each
    // program must EITHER compile and run native==interpreter (the inline-constructor
    // shape) OR skip cleanly (the struct-variable shape: no exe emitted, a controlled
    // L0339 diagnostic). The ONLY failure is a produced exe whose exit diverges from
    // the interpreter (a real miscompile — including a crash, which surfaces as a
    // non-matching exit code). Gated on the link toolchain; skips when absent.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native list-struct-string fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x7A3F_D18C_44B0_92E5u64;
    let dir = ScratchDir::new("list_struct_string");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_list_struct_string_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-list-struct-string-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!(
                "list-struct-string generator produced {other:?} on seed {seed:#x}:\n{source}"
            ),
        };

        let src_path = dir.join(format!("fuzz_lss_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_lss_{i}.exe"));
        std::fs::write(&src_path, &source).expect("write fuzz source");
        let _ = std::fs::remove_file(&exe_path);

        let _emit = lullaby()
            .args([
                "native",
                "-o",
                exe_path.to_str().expect("exe path"),
                src_path.to_str().expect("src path"),
            ])
            .output()
            .expect("run native");

        // A produced exe MUST match the interpreter; no exe is a clean default-deny
        // skip of the struct-variable push (the correct outcome, not a miscompile).
        if exe_path.is_file() {
            let run = Command::new(&exe_path).output().expect("run fuzz exe");
            let exit = run.status.code().expect("native exit code");
            assert_eq!(
                exit, expected as i32,
                "NATIVE MISCOMPILE (list struct-string) on #{i} (seed {seed:#x}):\n{source}\n\
                 interpreter={expected}, native exit={exit}"
            );
        }
    }
}

#[test]
fn fuzz_generic_scalar_interpreters_agree() {
    // Cross-check the three engines on user-generic-type programs (scalar `T`).
    // Always runs (no toolchain needed); a divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x1F83_D9AB_5BE0_CD19u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_generic_scalar_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "generic-scalar backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "generic-scalar generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_generic_scalar_native_matches_interpreter_when_linkable() {
    // The value-neutrality oracle for native monomorphization of user generic types
    // with SCALAR type arguments: each generated program instantiates `Box<T>`,
    // `Pair<A, B>`, and `Opt<T>` with scalar arguments, so every function MUST
    // compile natively AND its `.exe` exit code MUST equal the interpreter result —
    // a monomorphized `Box<i64>` is byte-identical to the erased `Box<i64>` the
    // interpreters run. Gated on the link toolchain; skips cleanly when absent.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native generic-scalar fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 120;
    let base_seed = 0xC3D2_E1F0_A9B8_7654u64;
    let dir = ScratchDir::new("generic_scalar");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_generic_scalar_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-generic-scalar-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => {
                panic!("generic-scalar generator produced {other:?} on seed {seed:#x}:\n{source}")
            }
        };

        let src_path = dir.join(format!("fuzz_gs_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_gs_{i}.exe"));
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
        // Every scalar generic instantiation is native-eligible, so an exe MUST be
        // produced (unlike the heap-element regression oracle, a clean skip here
        // would mean monomorphization regressed).
        assert!(
            emit.status.success() && exe_path.is_file(),
            "expected a native exe for scalar generics on #{i} (seed {seed:#x}):\n{source}\n{}",
            stderr(&emit)
        );

        let run = Command::new(&exe_path).output().expect("run fuzz exe");
        let exit = run.status.code().expect("native exit code");
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (generic scalar) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}

#[test]
fn fuzz_method_interpreters_agree() {
    // Cross-check the three engines on inherent-method-dispatch programs. Always
    // runs (no toolchain needed); a divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x3C6E_F372_FE94_F82Du64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_method_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "method backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "method generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_method_native_matches_interpreter_when_linkable() {
    // The correctness oracle for native inherent-method dispatch: each program
    // calls non-generic and monomorphized generic methods (receiver passed by the
    // aggregate ABI, copy-in value semantics), so every function MUST compile
    // natively AND its `.exe` exit code MUST equal the interpreter result. A clean
    // skip here would mean method dispatch regressed. Gated on the link toolchain.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native method fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 120;
    let base_seed = 0x6A09_E667_F3BC_C908u64;
    let dir = ScratchDir::new("method");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_method_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-method-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("method generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let src_path = dir.join(format!("fuzz_m_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_m_{i}.exe"));
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
        // Every method instance is native-eligible, so an exe MUST be produced; a
        // clean skip would mean method dispatch regressed.
        assert!(
            emit.status.success() && exe_path.is_file(),
            "expected a native exe for method dispatch on #{i} (seed {seed:#x}):\n{source}\n{}",
            stderr(&emit)
        );

        let run = Command::new(&exe_path).output().expect("run fuzz exe");
        let exit = run.status.code().expect("native exit code");
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (method dispatch) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}

/// Generates a program exercising **native monomorphization of user generic types
/// with a HEAP (`string`) type argument** — the one-level heap-`T` increment. Fixed
/// generic declarations (`Box<T>`, `Pair<K, V>`, `Opt<T>`) are instantiated with a
/// `string` argument and randomized ASCII strings (so `len` is the byte count on
/// both native and the interpreters), exercising construction, string field/payload
/// read, value-semantic copy, `match` over a string-payload enum, passing/returning
/// a heap-`T` value across a boundary, and a per-iteration RECLAIM loop (a fresh
/// `Box<string>` each iteration, borrow-only). Half the programs (a coin flip) add
/// an ESCAPING reassignment of an outer `Box<string>` inside the loop — the
/// adversarial case for the arena escape analysis: native must recognize the
/// heap-carrying generic store, stay off the arena path, and still agree with the
/// interpreters (a wrong classification would arena-reclaim a live record and
/// diverge/crash). All arithmetic is wrapping `i64`, so `main` returns an `i64`.
fn gen_generic_heap_string_program(seed: u64) -> String {
    let mut rng = Rng(seed ^ 0x51ED_270B_2E07_6E11u64);
    let decls = "struct Box<T>\n    value T\n\n\
                 struct Pair<K, V>\n    key K\n    value V\n\n\
                 enum Opt<T>\n    present T\n    absent\n\n\
                 fn box_len b Box<string> -> i64\n    len(b.value)\n\n\
                 fn pair_sum p Pair<string, i64> -> i64\n    len(p.key) + p.value\n\n\
                 fn opt_len o Opt<string> f i64 -> i64\n    match o\n        \
                 present(s) -> len(s)\n        absent -> f\n\n";

    let la = rng.range(1, 12) as usize;
    let lb = rng.range(1, 12) as usize;
    let lc = rng.range(1, 12) as usize;
    let n = rng.range(3, 40);
    let val = rng.range(-500, 500);
    let present = rng.chance(2);
    let fallback = rng.range(-200, 200);
    let escape = rng.chance(2);

    let sa = "a".repeat(la);
    let sb = "b".repeat(lb);
    let sc = "c".repeat(lc);

    let mut body = String::from("fn main -> i64\n");
    body.push_str(&format!("    let a Box<string> = Box(\"{sa}\")\n"));
    // Value-semantic copy: `c` shares the immutable string word with `a`.
    body.push_str("    let c Box<string> = a\n");
    body.push_str(&format!(
        "    let p Pair<string, i64> = Pair(\"{sb}\", {val})\n"
    ));
    let opt_ctor = if present {
        format!("present(\"{sc}\")")
    } else {
        "absent".to_string()
    };
    body.push_str(&format!("    let o Opt<string> = {opt_ctor}\n"));
    body.push_str("    let acc i64 = 0\n");
    if escape {
        // An outer heap-`T` generic reassigned inside the loop — an ESCAPE, so the
        // function must stay off the arena path (RC). Read after the loop.
        body.push_str("    let esc Box<string> = Box(\"seed\")\n");
    }
    body.push_str(&format!("    for i from 0 to {n}\n"));
    // A confined per-iteration reclaim temp (borrow-only, dropped each edge).
    body.push_str("        let t Box<string> = Box(to_string(i) + \"xyz\")\n");
    body.push_str("        acc = acc + len(t.value)\n");
    if escape {
        body.push_str("        if i == 0\n");
        body.push_str("            esc = Box(to_string(i) + \"qq\")\n");
    }
    let esc_term = if escape { " + box_len(esc)" } else { "" };
    body.push_str(&format!(
        "    box_len(a) + len(c.value) + pair_sum(p) + opt_len(o, {fallback}) + acc{esc_term}\n"
    ));
    format!("{decls}{body}")
}

#[test]
fn fuzz_generic_heap_string_interpreters_agree() {
    // Cross-check the three engines on heap-`T` user-generic programs (`string` T).
    // Always runs (no toolchain needed); a divergence prints the reproducing program.
    const PROGRAMS: u64 = 2000;
    let base_seed = 0x2545_F491_4F6C_DD1Du64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_generic_heap_string_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "generic-heap-string backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "generic-heap-string generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_generic_heap_string_native_matches_interpreter_when_linkable() {
    // The value-neutrality + reclamation-soundness oracle for native monomorphization
    // of heap-`T` user generics. Each program instantiates `Box<string>`,
    // `Pair<string, i64>`, and `Opt<string>`, with a per-iteration reclaim loop and
    // (half the time) an escaping outer-`Box<string>` reassignment. A produced exe
    // MUST match the interpreter exit code; a clean skip (no exe) is also acceptable
    // (default-deny) — the ONLY failure is a produced exe whose exit diverges (a real
    // miscompile, including a crash from a bad arena reclaim). Gated on the link
    // toolchain; skips cleanly when absent.
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native generic-heap-string fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0xB5C0_FBCF_EC4A_3E5Fu64;
    let dir = ScratchDir::new("generic_heap_string");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_generic_heap_string_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native-generic-heap-string-fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!(
                "generic-heap-string generator produced {other:?} on seed {seed:#x}:\n{source}"
            ),
        };

        let src_path = dir.join(format!("fuzz_ghs_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_ghs_{i}.exe"));
        std::fs::write(&src_path, &source).expect("write fuzz source");
        let _ = std::fs::remove_file(&exe_path);

        let _emit = lullaby()
            .args([
                "native",
                "-o",
                exe_path.to_str().expect("exe path"),
                src_path.to_str().expect("src path"),
            ])
            .output()
            .expect("run native");

        // A produced exe MUST match the interpreter; no exe is a clean default-deny
        // skip (never a miscompile).
        if exe_path.is_file() {
            let run = Command::new(&exe_path).output().expect("run fuzz exe");
            let exit = run.status.code().expect("native exit code");
            assert_eq!(
                exit, expected as i32,
                "NATIVE MISCOMPILE (generic heap-string) on #{i} (seed {seed:#x}):\n{source}\n\
                 interpreter={expected}, native exit={exit}"
            );
        }
    }
}

// -- Raw-pointer surface differential fuzz ------------------------------------
//
// The freestanding raw-pointer surface has native codegen (see
// `crates/lullaby_ir/src/native_object_rawptr.rs`). Its READ half — `addr_of` of
// a scalar local / struct field, `ptr_read`, `ptr_cast` chains, `ptr_offset`'s
// size law, and `ptr_to_int`/`int_to_ptr` round-trips — runs on every tier, so it
// has a real differential oracle: native must agree with the interpreters exactly.
//
// The STORE half (`ptr_write`/`volatile_store` through an `addr_of` pointer) has
// no interpreter oracle: the interpreters refuse it with a runtime `L0459`
// because their by-value snapshot model cannot alias. It is covered instead by
// the fixed, hand-computed fixtures in `suite15.rs`, whose expected exit codes
// follow from the source by construction.

/// Generate a program that reads a handful of `i64` locals and struct fields
/// through `addr_of` pointers, mixing in `ptr_cast` chains, size-law terms, and
/// integer round-trips. Every construct is one the interpreters model, so the
/// result is a valid cross-tier oracle.
fn gen_raw_pointer_read_program(seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let mut src = String::new();
    src.push_str("struct Cell\n    lo i64\n    hi i64\n\n");
    src.push_str("fn main -> i64\n");

    // A few scalar locals plus one struct, all with small values so the summed
    // result stays far from overflow.
    let locals = 2 + rng.below(3) as usize; // 2..=4
    for i in 0..locals {
        src.push_str(&format!("    let v{i} i64 = {}\n", rng.range(-50, 50)));
    }
    src.push_str(&format!(
        "    let cell Cell = Cell({}, {})\n",
        rng.range(-50, 50),
        rng.range(-50, 50)
    ));
    src.push_str("    let acc i64 = 0\n");
    src.push_str("    unsafe\n");

    let terms = 3 + rng.below(5) as usize; // 3..=7
    for t in 0..terms {
        match rng.below(5) {
            // A plain scalar read through addr_of.
            0 => {
                let i = rng.below(locals as u64);
                src.push_str(&format!("        let p{t} ptr<i64> = addr_of(v{i})\n"));
                src.push_str(&format!("        acc = acc + ptr_read(p{t})\n"));
            }
            // A read through a ptr_cast round-trip (a machine-level no-op).
            1 => {
                let i = rng.below(locals as u64);
                src.push_str(&format!("        let a{t} ptr<i64> = addr_of(v{i})\n"));
                src.push_str(&format!("        let b{t} ptr<byte> = ptr_cast(a{t})\n"));
                src.push_str(&format!("        let c{t} ptr<i64> = ptr_cast(b{t})\n"));
                src.push_str(&format!("        acc = acc + ptr_read(c{t})\n"));
            }
            // A struct-field read through addr_of.
            2 => {
                let field = if rng.chance(2) { "lo" } else { "hi" };
                src.push_str(&format!(
                    "        let f{t} ptr<i64> = addr_of(cell.{field})\n"
                ));
                src.push_str(&format!("        acc = acc + ptr_read(f{t})\n"));
            }
            // The size law: (p + n) - p == n * size_of(i64) == n * 8.
            3 => {
                let i = rng.below(locals as u64);
                let n = rng.range(-4, 4);
                let n_src = if n < 0 {
                    format!("(0 - {})", -n)
                } else {
                    n.to_string()
                };
                src.push_str(&format!("        let s{t} ptr<i64> = addr_of(v{i})\n"));
                src.push_str(&format!(
                    "        acc = acc + (ptr_to_int(ptr_offset(s{t}, {n_src})) - ptr_to_int(s{t}))\n"
                ));
            }
            // A ptr_to_int / int_to_ptr round-trip, then a read back.
            _ => {
                let i = rng.below(locals as u64);
                src.push_str(&format!("        let r{t} ptr<i64> = addr_of(v{i})\n"));
                src.push_str(&format!("        let n{t} i64 = ptr_to_int(r{t})\n"));
                src.push_str(&format!("        let q{t} ptr<i64> = int_to_ptr(n{t})\n"));
                src.push_str(&format!("        acc = acc + ptr_read(q{t})\n"));
            }
        }
    }
    src.push_str("    acc\n");
    src
}

/// Every generated raw-pointer read program must agree across the three
/// interpreters (the model is self-consistent before native is involved).
#[test]
fn fuzz_raw_pointer_reads_agree_across_interpreters() {
    const PROGRAMS: u64 = 150;
    let base_seed = 0x51C7_9E20_B3A4_16D9u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_raw_pointer_read_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on raw-pointer-read fuzz #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

/// The differential oracle for the raw-pointer READ surface: each generated
/// program must EITHER compile natively and exit with exactly the interpreter's
/// value, OR skip cleanly (no exe, an `L0339` diagnostic). The only failure is a
/// produced exe whose exit code diverges — a real miscompile.
///
/// Uses the direct-PE writer (no linker needed), so there is no toolchain gate —
/// only a Windows host to execute the image on.
#[test]
fn fuzz_raw_pointer_reads_native_matches_interpreter() {
    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping the raw-pointer native differential fuzz");
        return;
    }
    const PROGRAMS: u64 = 100;
    let base_seed = 0x2E64_A11B_7F03_C58Du64;
    let dir = ScratchDir::new("rawptr");

    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_raw_pointer_read_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on native raw-pointer fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let src_path = dir.join(format!("rawptr_{i}.lby"));
        let exe_path = dir.join(format!("rawptr_{i}.exe"));
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
        if !emit.status.success() {
            // A clean skip is an acceptable outcome (default-deny); it must carry
            // L0339 and must NOT have produced an executable.
            let errors = stderr(&emit);
            assert!(
                errors.contains("L0339"),
                "a native raw-pointer failure must be the clean L0339 skip, not a hard error \
                 (#{i}, seed {seed:#x}):\n{source}\n{errors}"
            );
            assert!(
                !exe_path.is_file(),
                "a skipped program must not leave an exe (#{i}, seed {seed:#x})"
            );
            continue;
        }

        assert!(exe_path.is_file(), "expected a direct-PE exe for #{i}");
        let run = std::process::Command::new(&exe_path)
            .output()
            .expect("run native exe");
        let exit = run.status.code().expect("native exit code");
        // The exe reports `main` through the Windows process exit code, whose
        // full 32-bit value round-trips, so the exit code equals `expected as i32`
        // (the same convention as the other native differential fuzzers above).
        assert_eq!(
            exit, expected as i32,
            "native/interpreter divergence on raw-pointer fuzz #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}

/// Generates one program that exercises a **VALUE-POSITION BRANCH/ARM TAIL**: an
/// aggregate (`option<T>` / `result<T, E>` / a user enum / `option<struct>`) bound
/// to a local INSIDE an `if`/`elif`/`else` branch or a `match` arm, and yielded as
/// that branch's/arm's tail expression.
///
/// This shape is why the fuzzers missed a real native miscompile: the routing that
/// decides where a returned value must land (the hidden aggregate result pointer)
/// was applied only to a function's own tail expression and to `return`, never to a
/// branch tail. An aggregate-returning function whose value came from a branch tail
/// wrote its hidden result pointer NOWHERE, so the caller read its own
/// uninitialized scratch — a wrong tag AND payload for a scalar payload, and a wild
/// pointer dereference (`0xC0000005`) for `option<struct>`. The generator covers the
/// four aggregate kinds crossed with four tail shapes at two nesting depths, so the
/// whole class stays covered rather than the single reported instance.
///
/// Every generated program is total and divergence-free: the payloads are small
/// positive literals, no arithmetic can overflow or divide, and the `match` in
/// `main` is exhaustive.
fn gen_branch_tail_program(seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let kind = rng.below(4);
    let shape = rng.below(4);
    // Payloads: small positive literals, so every result is a small positive i64
    // that round-trips through the process exit code unambiguously.
    let deep = rng.range(1, 40);
    let mid = rng.range(1, 40);
    let alt = rng.range(1, 9);
    // `-5` takes the none/err path, `3` the middle path, `200` the deep path (the
    // `n > 100` guard in the nested/elif shapes).
    let arg = [-5i64, 3, 200][rng.below(3) as usize];

    // Per aggregate kind: how to build the "value" variant...
    let some_of: fn(&str) -> String = match kind {
        0 => |e| format!("some({e})"),
        1 => |e| format!("ok({e})"),
        2 => |e| format!("A({e})"),
        _ => |e| format!("some(P({e}, 2))"),
    };
    // ...and the declarations, return type, "empty" variant, and the `match` arms
    // `main` reads the result back with.
    let (decls, ty, none_ctor, arm_some, arm_none): (&str, &str, String, &str, String) = match kind
    {
        0 => (
            "",
            "option<i64>",
            "none".to_string(),
            "some(v) -> 100 + v",
            format!("none -> {alt}"),
        ),
        1 => (
            "",
            "result<i64, i64>",
            format!("err({alt})"),
            "ok(v) -> 100 + v",
            "err(e) -> e".to_string(),
        ),
        2 => (
            "enum E\n    A i64\n    B\n\n",
            "E",
            "B".to_string(),
            "A(v) -> 100 + v",
            format!("B -> {alt}"),
        ),
        // `option<struct>`: a HEAP payload. This is the variant that segfaulted —
        // the unwritten payload word was dereferenced as a struct pointer.
        _ => (
            "struct P\n    a i64\n    b i64\n\n",
            "option<P>",
            "none".to_string(),
            "some(p) -> 100 + p.a + p.b",
            format!("none -> {alt}"),
        ),
    };

    let deep_v = some_of(&deep.to_string());
    let mid_v = some_of(&mid.to_string());

    // The four tail shapes, at increasing nesting depth. In every one the aggregate
    // is bound to a LOCAL and that local is the branch's/arm's tail expression.
    let (helper, body) = match shape {
        // Flat `if`/`else` — the originally reported instance.
        0 => (
            String::new(),
            format!(
                "    if n > 0\n        let s {ty} = {mid_v}\n        s\n    \
                 else\n        let e {ty} = {none_ctor}\n        e\n"
            ),
        ),
        // A nested `if` inside the taken branch (depth 2).
        1 => (
            String::new(),
            format!(
                "    if n > 0\n        if n > 100\n            let a {ty} = {deep_v}\n            a\n        \
                 else\n            let b {ty} = {mid_v}\n            b\n    \
                 else\n        let e {ty} = {none_ctor}\n        e\n"
            ),
        ),
        // An `elif` chain.
        2 => (
            String::new(),
            format!(
                "    if n > 100\n        let a {ty} = {deep_v}\n        a\n    \
                 elif n > 0\n        let b {ty} = {mid_v}\n        b\n    \
                 else\n        let e {ty} = {none_ctor}\n        e\n"
            ),
        ),
        // A `match`-ARM tail: the aggregate is built inside an arm of a `match` that
        // is itself the function's tail.
        _ => {
            let from_v = some_of("v");
            (
                "fn tag n i64 -> option<i64>\n    if n > 0\n        return some(n)\n    return none\n\n"
                    .to_string(),
                format!(
                    "    match tag(n)\n        some(v) ->\n            let s {ty} = {from_v}\n            s\n        \
                     none ->\n            let e {ty} = {none_ctor}\n            e\n"
                ),
            )
        }
    };

    format!(
        "{decls}{helper}fn pick n i64 -> {ty}\n{body}\nfn main -> i64\n    \
         match pick({arg})\n        {arm_some}\n        {arm_none}\n"
    )
}

#[test]
fn fuzz_branch_tail_interpreters_agree() {
    // Cross-check the three engines on branch/arm-local aggregate-tail programs.
    // Always runs (no toolchain needed).
    const PROGRAMS: u64 = 400;
    let base_seed = 0x7F4A_7C15_9E37_79B9u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_branch_tail_program(seed);
        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "branch-tail backend divergence on program #{i} (seed {seed:#x}):\n{source}\n\
             ast={ast:?} ir={ir:?} bytecode={bc:?}"
        );
        assert!(
            ast != Outcome::Other,
            "branch-tail generator produced a non-i64 main on seed {seed:#x}:\n{source}"
        );
    }
}

#[test]
fn fuzz_branch_tail_native_matches_interpreter_when_linkable() {
    // THE oracle for this shape: compile each branch/arm-local aggregate-tail
    // program to a real `.exe` and check its exit code against the interpreters.
    // Before the value-position routing fix this failed on the very first program
    // (native returned the caller's stale buffer instead of the built aggregate,
    // and the `option<struct>` variant crashed with 0xC0000005).
    if !native_exe_runnable() {
        eprintln!("not a Windows host; skipping native branch-tail fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 96;
    let base_seed = 0x2D19_2ED0_3D1B_54A3u64;
    let dir = ScratchDir::new("branch_tail");

    let mut ran = 0u64;
    for i in 0..PROGRAMS {
        let seed = base_seed.wrapping_add(i.wrapping_mul(0xA076_1D64_78BD_642F));
        let source = gen_branch_tail_program(seed);

        let (ast, ir, bc) = run_interpreters(&source);
        assert!(
            ast == ir && ir == bc,
            "interpreter divergence on branch-tail native fuzz #{i} (seed {seed:#x}):\n{source}"
        );
        let expected = match ast {
            Outcome::Value(n) => n,
            other => panic!("generator produced {other:?} on seed {seed:#x}:\n{source}"),
        };

        let Some(exit) = fuzz_native_exit(&source, &dir, &format!("branch_tail_{i}")) else {
            // This host can produce no exe at all (no direct-PE image AND no
            // linker). `fuzz_native_exit` has already established that and panics
            // if a linker WAS available, so reaching here is a genuine skip.
            eprintln!(
                "branch-tail native fuzz: no exe path on this host; ran {ran} before skipping"
            );
            return;
        };
        ran += 1;
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE on branch-tail #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
    // Report the count so a run can be audited: this oracle is only meaningful if it
    // actually executed real binaries, and a silent skip would hide exactly the
    // class of bug it exists to catch. (Direct-PE emission is the default, so on a
    // Windows host every program produces an exe and this prints the full count.)
    eprintln!("branch-tail native fuzz: ran {ran}/{PROGRAMS} real exes");
}

// The `alloc` heap-box fuzzers (arena interaction, cross-frame boxes, and the
// `ptr_cast` laundering route) live in their own file: this one is at the ~2500-line
// test-file cap. A submodule rather than a peer so it keeps seeing `Rng`,
// `Outcome`, `run_interpreters`, and `fuzz_native_exit` via `use super::*`.
#[path = "fuzz_alloc.rs"]
mod fuzz_alloc;

// The closure-ABI fuzzers (float captures, positional XMM registers, and >3-parameter
// stack spills) live in their own file for the same reason, and as a submodule for the
// same access to `Rng`/`Outcome`/`run_interpreters`/`fuzz_native_exit`.
#[path = "fuzz_closure.rs"]
mod fuzz_closure;

// The static-buffer arena fuzzers (freestanding tier §5: scoping, overflow, and
// four-tier agreement) live in their own file for the same reason.
mod fuzz_arena;

// The packed narrow array element fuzzers (walking an `array<i32>`/`array<u8>`/…
// through raw pointers, where a wrong stride is silent corruption) live in their
// own file for the same reason.
mod fuzz_narrow;
