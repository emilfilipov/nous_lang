# Optimization Opportunities — Tokens and Performance

Canonical language rules: see [core_language_rules.md](core_language_rules.md).

This document is the standing analysis of where Lullaby can close its two
marketed gaps against the cross-language benchmark
([benchmarks/crosslang](../benchmarks/crosslang/README.md)): **fewer tokens than
Python** and **native performance at or below C**. Rerun the benchmark and
refresh this analysis whenever the corpus, the language, or the optimizer
changes.

## Current standing (o200k_base tokens; same-box native timings)

| Metric | Lullaby | Target | Gap |
|---|---|---|---|
| Corpus tokens | 22,356 | < 19,120 (Python) | **+16.9% over Python** |
| `count_primes_below` native | 29.3 ms | ≤ 28.3 ms (C) | **1.04× C** |
| `fib(40)` native | 1.53 ns/call | ≤ 1.28 ns (C) | **1.20× C** |

Lullaby is already terser than C (1.16×), C++ (1.20×), Rust (1.11×) and ties
JavaScript (0.99×). Python is the only language it loses to on tokens, and only
C/Rust beat it on native speed — both by a hair.

## Token gap: where it lives

Per-category `Lullaby − Python` deltas. The **top 7 string/text categories are
67% (+2,166) of the entire +3,236 deficit**:

| Category | Lby | Py | Δ | ratio |
|---|---|---|---|---|
| string_algos | 1996 | 1488 | +508 | 1.34× |
| text_processing | 703 | 333 | +370 | 2.11× |
| parsing | 1267 | 950 | +317 | 1.33× |
| validation | 1035 | 765 | +270 | 1.35× |
| combinatorics | 1151 | 921 | +230 | 1.25× |
| services | 708 | 520 | +188 | 1.36× |
| collections | 1417 | 1234 | +183 | 1.15× |
| …18 more, each < +170 | | | | |
| statistics | 678 | 724 | −46 | 0.94× |
| state_machines | 649 | 697 | −48 | 0.93× |

Lullaby already **beats** Python on `statistics` and `state_machines` — the
computation-heavy categories — confirming the deficit is almost entirely a
**string/text-ergonomics** problem, not a fundamental verbosity problem.

## Root causes (empirically verified against the current compiler)

1. **No inline conditional (ternary) expression.** `let x = 1 if c else 0`
   → `L0207`. The block form `if C` / `X` / `else` / `Y` costs ~4 lines where
   Python spends one. This idiom recurs in nearly every `validation`,
   `services`, `text_processing`, and `games` function that returns `1/0`.
   *Highest-ROI single feature.* Proposed surface (indentation-friendly, mirrors
   Python and the corpus): `value if cond else other` as an expression.
2. **No string slicing.** `s[0:3]` → `L0207`, no reverse. Python leans on
   `s[i:j]`, `s[:n]`, `s[::-1]`. Lullaby only has verbose `substring(s, i, j)`
   and hand-rolled reverse loops. Proposed: `s[i:j]` slice (and array slices),
   optionally negative indices.
3. **No `in` membership operator.** `c in "aeiou"` / `sub in s` → `L0207`.
   Lullaby writes `contains(s, sub)` or `c == 'a' or c == 'e' or …`. Proposed:
   `x in collection` for `string`/`array`/`list`/`map`.
4. **`string += char` is rejected.** `out += char_from(65)` → `L0315`
   ("requires a string operand"), forcing `out += to_string(char_from(…))`.
   Every ASCII case-fold / char-building loop pays the `to_string(…)` wrapper.
   Proposed: implicit `char → string` in `+`/`+=` (and `string + char`).
5. **Mandatory type annotations on every parameter and return.** Python spends
   zero tokens on types. This is a deliberate Lullaby value proposition and
   should stay for parameters, but **return types are inferable** from the body
   in the common case and could become optional — ~2–3 tokens saved per
   function across 200+ functions.
6. **Corpus does not always use Lullaby's own builtins.** `repeat(s, n)` and
   `split(s, sep)` **exist and pass `check`**, yet `text_processing` hand-rolls
   `repeat_str`/`word_count` with loops. This is a *corpus-hygiene* deficit that
   inflates Lullaby's count and understates the language. Fixing it is a pure,
   immediate win that also makes the benchmark honest.
7. **`graph_algos` is incomplete.** Only `lullaby.lby` exists (no C/C++/Rust/
   Python/JS siblings), so the category scores 0/0 and contributes nothing. It
   should be completed to all six languages (adds coverage; likely a Lullaby win
   given `statistics`/`state_machines`).

## Recommended sequencing (fastest path to "under Python")

- **Phase A — corpus hygiene (low risk, no compiler change).** Sweep every
  category for un-idiomatic verbosity: use `repeat`/`split`/`join`, real bitwise
  operators in `bitwise` (they ship now — the corpus still models bits with
  arithmetic), drop redundant temporaries. Complete `graph_algos`. Expected to
  remove several hundred tokens immediately.
- **Phase B — inline conditional + char→string coercion (root causes 1, 4).**
  The two highest structural wins; each touches parser + semantics + all five
  backends but is well-scoped. Together these should erase most of the top-7
  string/text deficit.
- **Phase C — slicing + `in` (root causes 2, 3).** Further string wins and a
  large ergonomics jump; slicing also benefits `parsing`/`string_algos`.
- **Phase D — optional return-type inference (root cause 5).** Broad, small
  per-function savings; ergonomics improvement.

After Phases A–C, Lullaby should cross under Python's 19,120.

## Performance gap: closing 1.04× / 1.20× to ≤ 1.0× C

The native backend is already essentially at C on the arithmetic-and-loops
workload. The remaining margin is call/arithmetic overhead:

1. **Overflow-check elision via range analysis.** Native integer arithmetic is
   checked to stay bit-for-bit with the interpreters; that costs cycles in hot
   loops. Elide the check where the operand range provably cannot overflow (or
   offer a release/wrapping mode). Primary lever for `count_primes_below`.
2. **Leaf-frame omission.** Small all-`i64` leaf functions (e.g. `fib` base
   cases, helpers) still emit a full Win64 prologue/epilogue and reserve shadow
   space. Omit the frame when a function makes no calls and fits in registers —
   the main lever for the `fib` 1.20×.
3. **Broaden SIMD auto-vectorization.** Phase-1 covers `i64` sum reductions;
   extend to `min`/`max`/count/product reductions and `f64` accumulation, and to
   strided array scans.
4. **Tail-call / self-recursion optimization** and tighter register allocation
   (fewer spills) for the recursive and loop-carried cases.

## How to refresh this analysis

```
pwsh benchmarks/crosslang/run_benchmark.ps1        # tokens + reassemble
pwsh benchmarks/crosslang/run_benchmark.ps1 -Perf  # also re-time the workloads
```

Then update the standing table above and republish the artifact
(`benchmarks/crosslang/report.html`).
