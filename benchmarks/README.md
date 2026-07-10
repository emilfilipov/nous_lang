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

After adding `[profile.release]` (fat LTO + `codegen-units=1` + `panic=abort`):
bytecode 381 (-13%), ir 365 (-15%), ast 403 (-8%); native and C unchanged
(native is emitted machine code, not Rust).

The optimization backlog lives in ClickUp: Lullaby → **18 Performance
Optimization**.
