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

The native loop is 6× C because loop locals (`acc`, `i`) are spilled to the
stack and reloaded each iteration — **register allocation is the largest
remaining native lever**. Interpreters run ~160 ns/iter here vs ~380 ns/call on
fib, so per-call setup (env/frame/args) is a major interpreter cost.

The optimization backlog lives in ClickUp: Lullaby → **18 Performance
Optimization**.
