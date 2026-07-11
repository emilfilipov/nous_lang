# Native optimizer roadmap

The plan for closing the native backend's gap to C/Rust. Current standing
(`cl /O2` = 1.0×): native is **~1.2× C** on call-bound code and **~2.1× C** on
compute-bound loops. Rust/C++/Zig sit at ~parity (1.0×). The remaining gap is
optimizer engineering, added as independent, measured, test-gated passes — each
following `.claude/skills/optimize-lullaby-performance`.

The native backend (`crates/lullaby_ir/src/native_object.rs`) is a direct x86-64
byte emitter with a stack-machine model and no instruction IR. Some passes below
are localized emit-time transforms (low risk); the deepest ones (scheduling, SIMD)
eventually want a small instruction-level IR, called out where relevant.

## Discipline (applies to every pass)

- One pass at a time. `cargo test --all` green is the gate (it runs native
  fixtures end-to-end). Re-benchmark with `benchmarks/run_bench.ps1` +
  `run_loop.ps1` and, once it exists, the cross-language suite.
- Conservative by construction: a pass fires correctly or falls back to the exact
  prior codegen. Any unhandled shape → unchanged.
- Commit each pass with its measured before→after (vs C).

## Passes, in priority order

### 1. Redundant-load / move elimination (peephole) — LOW risk, quick
The register-promoted loop still reloads the same value twice
(`mov rax, rsi; … ; mov rax, rsi`) and emits `mov`s that are provably dead. A
small trailing peephole over the just-emitted bytes (track "rax already holds
local X / immediate K" within a straight-line run, invalidate on any write/call/
branch target) removes them. Expected: a few % on tight loops; cleans up every
tier. Foundation for later peepholes.

### 2. Extended register allocation — MEDIUM risk
Generalize the scalar promotion already landed:
- Promote **more than two** locals (add `r12`–`r15`, all callee-saved and unused
  by the main-body lowering — same safety proof as `rbx`/`rsi`).
- Promote induction variables of **`for`** loops and widen the eligibility whitelist
  where provably scalar.
- Track liveness well enough to reuse a register once a local is dead.
Expected: extends the 5.6×→2.1× loop win to functions with >2 hot locals and to
`for`-loops. Still no full instruction IR; a lightweight per-function liveness
scan suffices.

### 3. Function inlining — MEDIUM/HIGH risk, high payoff for call-bound code
Inline small, non-recursive callees at their call sites (substitute the callee's
body with parameters bound to the caller's argument expressions, renaming locals
to fresh frame slots). This is the biggest lever for call-heavy straight-line code
(not recursion — `fib` stays a call). Guard: size threshold, non-recursive,
scalar-only first, no address-taken params. Expected: call-bound microbenchmarks
toward ~1.0–1.1× C; compounds with promotion (inlined locals become promotable).

### 4. Frame-pointer omission for leaf functions — LOW/MEDIUM risk
A function that makes no calls and needs no dynamic stack can skip
`push rbp; mov rbp, rsp` and address locals off `rsp`. Trims prologue/epilogue
on hot leaves. Expected: small but broad; helps the many small functions in the
cross-language suite.

### 5. Strength reduction + `lea` address arithmetic — LOW risk
Fold `x * 2^k` → shifts, `x * const` → `lea`/shift-add, constant division by
powers of two → shifts; use `lea` for `base + index*scale + disp` instead of
mul+add chains. Expected: measurable on arithmetic-heavy loops.

### 6. Instruction scheduling — MEDIUM risk, wants a small IR
Reorder independent instructions to reduce false dependencies and fill load-use
latency (e.g. hoist the loop-condition load, separate dependent `add`s). Best done
once a per-basic-block instruction IR exists (a prerequisite refactor: model
instructions as an enum before encoding, so passes 1/5/6 operate on structured
ops rather than raw bytes). Expected: single-digit % broadly; larger on
latency-bound loops.

### 7. Auto-vectorization (SIMD) — HIGH risk, HIGH payoff, largest effort
Recognize simple counted loops with no cross-iteration dependence except a
reduction/map (sum, dot product, elementwise add) and emit SSE2/AVX2
(`paddq`/`vpaddq`, horizontal reduce). This is where LLVM wins big on numeric
loops. Requires the instruction IR (pass 6) plus a dependence/idiom recognizer.
Deliver last, in its own staged effort; gate hard (a vectorization bug is
subtle). Expected: numeric loops from ~2× C toward ~1× C or better.

## Sequencing

1 → 2 → 4 → 5 (all localized, low/medium risk, ship quickly and measure) →
then the IR refactor → 3 (inlining) and 6 (scheduling) on top of it → 7 (SIMD)
as a dedicated final effort. Re-run the cross-language suite after each to track
the marketability number (native vs C/C++/Rust). The ClickUp list
`18 Performance Optimization` tracks each as a task.
