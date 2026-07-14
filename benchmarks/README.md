# Lullaby benchmarks

Performance baseline and regression harness. Measures a fixed workload across
every execution tier plus a pure-C reference, normalized to **nanoseconds per
operation** so results at different problem sizes are comparable.

## Run

```powershell
powershell -ExecutionPolicy Bypass -File benchmarks/run_bench.ps1 -Label mylabel
```

Requires a release build (`cargo build --release -p lullaby_cli`) and MSVC Build
Tools (the harness imports `vcvars64` so both `cl` and `lullaby native` link).
Results are written to `benchmarks/results_<label>.csv` for before/after diffs.

## Workload

`fib.lby` / `fib.c` — identical i64 naive recursive Fibonacci. Recursion depth
`n` makes `2*fib(n+1)-1` calls; the harness divides wall time by that to report
**ns per fib-call**. Compiled tiers (C, `lullaby native`) run at N=40;
interpreter tiers (ast/ir/bytecode) run at N=30. Every result is correctness-checked.

## Baseline (2026-07-11, best-of-5)

| Tier | ns/call | vs C |
|---|---|---|
| C (`cl /O2`) | 1.26 | 1.0x |
| lullaby native | 2.17 | 1.72x |
| lullaby bytecode | 436 | 346x |
| lullaby ir | 430 | 342x |
| lullaby ast | 439 | 349x |

### Current standings (after the optimization campaign)

| Tier | baseline ns/call | now | vs C |
|---|---|---|---|
| C (`cl /O2`) | 1.26 | ~1.25 | 1.0× |
| **lullaby native** | 2.17 (1.72×) | **1.56 (1.21× C)** | 28% faster |
| lullaby bytecode | 436 | **229** | 47% faster |
| lullaby ir | 430 | **233** | 46% faster |
| lullaby ast | 439 | **228** | 48% faster |

What changed:
- `[profile.release]` (fat LTO + `codegen-units=1` + `panic=abort`) — interpreters
  −8…15%; native/C unchanged (native is emitted machine code, not Rust).
- Native codegen: fuse i64 comparisons into their branch (`cmp; jcc` vs
  `setcc;movzx;test;jz`), fold constant operands into immediates, memory-destination
  self-assign, and pass a single scalar call arg straight into `rcx` — **native fib
  2.17 → 1.56 (1.72× → 1.21× C)**.
- Per-call environment pooling in both the AST and IR/bytecode interpreters — the
  per-call `Env` allocation dominated the call path: **~435 → ~230 ns/call (≈47%)**.

## Compute-bound loop (`run_loop.ps1`, sum 0..N)

`loopsum.lby` / `loopsum.c` — a tight `while` loop (no calls). C + native at
N=1e9, interpreters at N=1e7; ns per iteration.

| Tier | ns/iter | vs C |
|---|---|---|
| C (`cl /O2`) | 0.13 | 1.0× |
| lullaby native | 0.78 | 6.0× |
| bytecode / ir / ast | ~160 | ~1230× |

After **register promotion** (keeping a purely-scalar function's hot `i64` locals
in callee-saved `rbx`/`rsi` instead of the stack), the native loop dropped from
**0.70 → 0.285 ns/iter (5.6× → 2.1× C)** — the loop body is now
`add rbx, rax; add rsi, 1` with `acc`/`i` in registers. fib is ~neutral (it's
call-bound, so `n` in a register offsets the register save/restore). Interpreters
run ~160 ns/iter here vs ~230 ns/call on fib, so per-call setup remains a cost.

## Aggregate-shape workloads (array / string / realistic transform)

fib and loop only exercise scalar `i64` recursion and a scalar loop. These three
add coverage for the program shapes the arena-first memory and native-aggregate
work will target — arrays, strings, and a multi-feature transform — so those
changes are measured, not guessed. Each has a matching `cl /O2` C reference with
byte-identical data, is correctness-checked on every tier, and reports ns per
inner operation. All three are structured so the measured loop **allocates
nothing per iteration** (a fixed stack array / one immutable string literal),
which is what keeps them native-eligible: the native bump heap has no
reclamation, so any per-iteration heap growth exhausts it (see the string note).

```powershell
powershell -ExecutionPolicy Bypass -File benchmarks/run_arraysum.ps1 -Label baseline
powershell -ExecutionPolicy Bypass -File benchmarks/run_strscan.ps1  -Label baseline
powershell -ExecutionPolicy Bypass -File benchmarks/run_csvsum.ps1   -Label baseline
```

Native results carry the accumulated `i64` in the process exit code, which
overflows 32 bits at the timed reps, so native correctness is spot-checked at the
largest reps whose result still fits a signed 32-bit exit code (and full native
aggregate correctness is covered by `cargo test --all`).

### `run_arraysum.ps1` — reduction over `array<i64>`

A read-only 64-element `array<i64>` is passed by fat pointer to a `scan` helper
that sums it with `for i: s += a[i]`, repeated `reps` times. Baseline
(2026-07-14, best-of-5; native/C @ reps=10M, interpreters @ reps=200k):

| Tier | ns/elem | vs C |
|---|---|---|
| C (`cl /O2`) | 0.079 | 1.0× |
| lullaby native | 0.684 | 8.7× |
| lullaby bytecode | 68.2 | ~870× |
| lullaby ir | 93.9 | ~1195× |
| lullaby ast | 99.3 | ~1263× |

### `run_strscan.ps1` — char-code checksum over a string

Each rep walks a fixed 66-char ASCII string with `for c in s: sum += char_code(c)`
(the **linear** O(n) idiom — `s[i]` indexing is O(i) on UTF-8 and would be
quadratic). Baseline (native/C @ reps=300k, interpreters @ reps=100k):

| Tier | ns/char | vs C |
|---|---|---|
| C (`cl /O2`) | 0.60 | 1.0× |
| lullaby native | 29.6 | ~49× |
| lullaby bytecode | 158 | ~262× |
| lullaby ir | 304 | ~503× |
| lullaby ast | 168 | ~278× |

Native is ~49× C here (vs ~9× on the array and ~1.2× on fib): Lullaby chars are
Unicode scalar values, so each step decodes UTF-8 through a per-character helper,
whereas the C reference walks raw bytes. On ASCII the checksums are identical;
the gap is real per-character decode work and is the headline optimization target
this workload exists to track. The **build** idiom (`s = s + "a"` in a loop) is
intentionally *not* the measured shape — immutable strings plus the no-free bump
heap make it O(n²) and exhaust the native heap past ~1000 chars, so it does not
scale on native today (an honest limitation, noted in the script).

### `run_csvsum.ps1` — CSV-integer parse + aggregate (realistic transform)

A small realistic transform: parse a fixed 62-char string of comma-separated
integers with a digit accumulator (`cur = cur*10 + (c-48)`, flush on non-digit)
and sum them — exercising `for c in s`, per-character branching, multiply/add, and
accumulation at once. Baseline (native/C @ reps=500k, interpreters @ reps=100k):

| Tier | ns/char | vs C |
|---|---|---|
| C (`cl /O2`) | 1.05 | 1.0× |
| lullaby native | 30.6 | ~29× |
| lullaby bytecode | 254 | ~242× |
| lullaby ir | 499 | ~476× |
| lullaby ast | 393 | ~375× |

The optimization backlog lives in ClickUp: Lullaby → **18 Performance
Optimization**.
