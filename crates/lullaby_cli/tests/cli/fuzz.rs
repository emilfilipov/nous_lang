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
        let h = format!(
            "fn h a i64 -> i64\n    let total i64 = 0\n    for j from 0 to {k}\n        \
             let s string = {scratch}\n        {acc}\n    total\n"
        );
        return format!("{h}{main}");
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
    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native array differential fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 80;
    let base_seed = 0x7C6A_2F13_88BE_4D05u64;
    let dir = std::env::temp_dir().join("lullaby_fuzz_native_array");
    let _ = std::fs::create_dir_all(&dir);

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

        let src_path = dir.join(format!("fuzz_arr_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_arr_{i}.exe"));
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
            "native emit failed on array-fuzz #{i} (seed {seed:#x}) — the helper's \
             fat-pointer array parameter must compile:\n{source}\n{}",
            stderr(&emit)
        );
        assert!(
            exe_path.is_file(),
            "no linked exe on array-fuzz #{i} (seed {seed:#x}):\n{source}\n{}",
            stdout(&emit)
        );

        let run = Command::new(&exe_path).output().expect("run fuzz exe");
        let exit = run.status.code().expect("native exit code");
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (fat array) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
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
    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native string-loop fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x6EA3_55C1_9B02_7DF1u64;
    let dir = std::env::temp_dir().join("lullaby_fuzz_native_string_loop");
    let _ = std::fs::create_dir_all(&dir);

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

        let src_path = dir.join(format!("fuzz_sl_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_sl_{i}.exe"));
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
            "native emit failed on string-loop-fuzz #{i} (seed {seed:#x}):\n{source}\n{}",
            stderr(&emit)
        );
        assert!(
            exe_path.is_file(),
            "no linked exe on string-loop-fuzz #{i} (seed {seed:#x}) — main must compile \
             natively:\n{source}\n{}",
            stdout(&emit)
        );

        let run = Command::new(&exe_path).output().expect("run fuzz exe");
        let exit = run.status.code().expect("native exit code");
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
    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native arena fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x9D2B_C4E7_16A0_38F5u64;
    let dir = std::env::temp_dir().join("lullaby_fuzz_native_arena");
    let _ = std::fs::create_dir_all(&dir);

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

        let src_path = dir.join(format!("fuzz_arena_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_arena_{i}.exe"));
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
            "native emit failed on arena-fuzz #{i} (seed {seed:#x}) — the arena-eligible \
             helper must compile:\n{source}\n{}",
            stderr(&emit)
        );
        assert!(
            exe_path.is_file(),
            "no linked exe on arena-fuzz #{i} (seed {seed:#x}):\n{source}\n{}",
            stdout(&emit)
        );

        let run = Command::new(&exe_path).output().expect("run fuzz exe");
        let exit = run.status.code().expect("native exit code");
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (arena) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
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
    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32.lib unavailable; skipping native arena struct-string fuzz");
        return;
    }
    ensure_msvc_env();

    const PROGRAMS: u64 = 100;
    let base_seed = 0x2F81_66DA_9C40_7B15u64;
    let dir = std::env::temp_dir().join("lullaby_fuzz_native_arena_struct_string");
    let _ = std::fs::create_dir_all(&dir);

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

        let src_path = dir.join(format!("fuzz_arena_ss_{i}.lby"));
        let exe_path = dir.join(format!("fuzz_arena_ss_{i}.exe"));
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
            "native emit failed on arena-struct-string-fuzz #{i} (seed {seed:#x}) — the \
             arena-eligible struct-string helper must compile:\n{source}\n{}",
            stderr(&emit)
        );
        assert!(
            exe_path.is_file(),
            "no linked exe on arena-struct-string-fuzz #{i} (seed {seed:#x}):\n{source}\n{}",
            stdout(&emit)
        );

        let run = Command::new(&exe_path).output().expect("run fuzz exe");
        let exit = run.status.code().expect("native exit code");
        assert_eq!(
            exit, expected as i32,
            "NATIVE MISCOMPILE (arena struct-string) on #{i} (seed {seed:#x}):\n{source}\n\
             interpreter={expected}, native exit={exit}"
        );
    }
}
