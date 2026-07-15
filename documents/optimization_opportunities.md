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
| Corpus tokens | 20,321 | < 19,120 (Python) | **+6.3% over Python** |
| `count_primes_below` native | 29.3 ms | ≤ 28.3 ms (C) | **1.04× C** |
| `fib(40)` native | 1.53 ns/call | ≤ 1.28 ns (C) | **1.20× C** |

Lullaby is now **terser than every language except Python**: ~22% ahead of C,
~24% ahead of C++, ~18% ahead of Rust, ~8% ahead of JavaScript. Python is the
only language still ahead on tokens (by +6.3%, down from +16.9% after return-type
inference and the four inline-syntax features shipped and were adopted). On
speed, only C/Rust beat it on native — both by a hair.

**The last token lever to go under Python is parameter-type inference** (~1,841
tokens, the only remaining structural cost now that return types infer) — a
genuine design decision, not a desugar. Two other candidates were **measured and
ruled out**: string interpolation saves ~0 *counted* tokens (the tokenizer strips
the `main` driver, and there are zero `to_string` calls in counted functions),
and dropping redundant array-length params (`n`→`len(a)`) is net-neutral to worse
(`len(a)` is more tokens than `n`). See the artifact's Open-gaps tab.

### Shipped: the four token gaps (2026-07-12)

All four language gaps below are **implemented end-to-end** (parser → semantics →
IR desugar → AST interpreter, with the IR/bytecode/native/WASM backends covered
by the desugar) and adopted in the corpus:

- **Inline conditional** `A if C else B` — the broad win, replacing 1/0 (and
  small-value) block `if`/`else` across most categories.
- **`string + char` / `+= char`** — drops the `to_string(char_from(...))` wrapper.
- **Membership `x in collection`** — `c in "aeiou"`, `sub in s`, list membership.
- **String slicing `s[i:j]`** (and `s[i:]`, `s[:j]`, `s[:]`).

Adopting them (plus `array_fill(n, 0)` for the DP-buffer literals) moved the
corpus from 22,356 → 21,535 tokens (+16.9% → +12.6% over Python) with every
function's output byte-identical.

The remaining Python gap is now **structural, not ergonomic**: the corpus is
mostly array/numeric algorithms, so the string features have limited reach. What
still separates Lullaby from Python is (a) **mandatory type annotations** on
every parameter and return, and (b) **explicit array-length parameters** (`n`,
`la`, `lb`) carried for cross-language algorithm parity. Closing the rest means
optional **return-type inference** and dropping redundant length params in favor
of `len(a)` — see below.

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
workload — `count_primes_below` is at **1.01× C** after adopting the native `%`
operator. The remaining margin is per-call overhead in deep recursion:

Two levers guessed here earlier were checked against the emitted machine code and
**ruled out**:

- *Overflow-check elision* is a non-issue: the native backend already emits plain
  `add`/`imul` for `i64` arithmetic (bit-for-bit with the interpreters by the
  two's-complement semantics, not a runtime check) — there is nothing to elide.
- *Leaf-frame omission* already happens: the prologue self-omits the frame and
  shadow space when a function makes no calls and needs no stack (`stack_size == 0`).

The real remaining levers are:

1. **Recursive-call codegen.** `fib(35)` runs 1.26× C. Disassembly narrows the
   residual precisely: the argument is *already* register-promoted to callee-saved
   `rbx` and survives both recursive calls (the earlier "C keeps the arg in a
   register, we don't" guess was wrong). What remains is finer — computing an
   argument emits `mov rax,rbx; sub rax,1; mov rcx,rax` (3 insns) where C emits one
   `lea rcx,[rbx-1]`, and `fib(n-1)` is spilled across the second call via
   `push`/`pop` where C uses a second callee-saved register. Both are near-free on
   an out-of-order core (register-renamed movs, L1-hot stack), and this is a
   *call-bound* workload, so the benchmark's own lesson (immediate-folds measure as
   noise on call-bound code) predicts a small payoff. Closing it well needs a real
   register allocator (a second promoted slot for the intermediate result + `lea`
   address-arithmetic for arguments) — a substantial change to a stack-machine
   emitter with no instruction IR, scoped as a deliberate follow-up rather than
   rushed into the correctness-critical backend.
2. **Broaden SIMD auto-vectorization.** Three phases are shipped, all emitting an
   SSE2 packed loop (two `i64` lanes per iteration, scalar tail, verified bit-for-bit
   identical to every interpreter): (1) `i64` sum reduction (3.3× the scalar loop),
   (2) element-wise map `c[i] = a[i] ± b[i]` (3.36×), (3) bitwise reductions
   `acc = acc <op> a[i]` and bitwise maps `c[i] = a[i] <op> b[i]` for `& | ^`
   (2.89×), via `pand`/`por`/`pxor` seeded with the operator identity. Native
   scalar `i64` bitwise operators (`& | ^ << >>`) shipped alongside, which the
   bitwise reductions build on.

   The remaining requested patterns are **blocked by the x86-64 baseline ISA, not
   by effort** — shipping an emulation would be a measured regression:
   - *Dot product / product reduction* need a 64-bit packed multiply. SSE2 has
     none; the `pmuludq` schoolbook emulation is ~6 SSE ops/element versus one
     scalar `imul`, so it measures slower than the scalar loop.
   - *Min/max reductions* need a 64-bit packed compare (`pcmpgtq`), which is SSE4.2,
     not part of the guaranteed x86-64 baseline.
   - *`f64` accumulation* would break bit-exact interpreter parity: floating-point
     addition is not associative, so a two-lane packed sum rounds differently from
     the scalar left-to-right fold.

   Unlocking these needs runtime CPU-feature detection (CPUID) selecting a widened
   SSE4.2/AVX2 code path, or an opt-in fast-math mode that relaxes the `f64`
   parity contract. Both are separate multi-session epics.

## How to refresh this analysis

```
pwsh benchmarks/crosslang/run_benchmark.ps1        # tokens + reassemble
pwsh benchmarks/crosslang/run_benchmark.ps1 -Perf  # also re-time the workloads
```

Then update the standing table above, republish the artifact
(`benchmarks/crosslang/report.html`), **and refresh the Benchmarks table in the
repository [`README.md`](../README.md)** — the README's token totals and native
performance figures are the project's headline sell and must move with every
re-benchmark, not drift behind the report.

---

## 2026-07-14 — full four-tier measurement + next-lever analysis (measurement-only pass)

Measured against current `main` (release build of `lullaby_cli`, `cl /O2` C
reference, MSVC, best-of-N wall time, every tier correctness-checked `ok`). This
was a **measurement + analysis pass** — no compiler source was changed; the
native codegen and interpreter-VM files were in-flight under other work, so the
levers below are queued with evidence rather than applied.

### Current four-tier standing (ns per inner op; vs-C compared within one run)

| Workload | C | native | native vs C | bytecode | ir | ast |
|---|---:|---:|---:|---:|---:|---:|
| `fib(40)` recursion (ns/call) | 1.45 | 1.43 | **0.99×** | 247 | 268 | 266 |
| `sum 0..N` loop (ns/iter) | 0.143 | 0.006 | **~0.04× (≈24× faster)** | 78.5 | 157 | 156 |
| `gcd(i,1071)` loop (ns/gcd) | 11.08 | 11.06 | **1.00×** | 195 | 300 | 303 |
| `array<i64>` reduction (ns/elem) | 0.079 | 0.723 | **9.1×** | 70.5 | 97.7 | 105.6 |
| string char-scan (ns/char) | 0.649 | 25.27 | **39×** | 163 | 315 | 166 |
| CSV parse+sum (ns/char) | 1.045 | 31.79 | **30×** | 246 | 524 | 408 |

Native is at/under C on all three **scalar** shapes (fib, gcd, and the `sum`
loop, which the backend recognizes and emits as an O(1) closed form — no loop, so
it beats C's O(N) by ~24×). The open native gaps are the **aggregate** shapes:
array reduction (~9× C) and the two string shapes (~30–39× C).

### Prioritized next-optimization levers (highest ROI first)

**1. Native `for c in s` was O(n²) per scan — SHIPPED (2026-07-15), now O(N).**
*Status.* **DONE.** Lowered `for c in s` to a forward byte cursor:
`detect_string_char_foreach` recognizes the `for idx from 0 to len(s)-1 { let c =
s[idx]; … }` desugar and `lower_native_for_string_chars`
(`crates/lullaby_ir/src/native_object_loops.rs`) emits a loop that decodes one code
point at `data + p` and advances `p += width` per step, via the new inline
`emit_utf8_decode_advance` (`native_object_runtime_helpers.rs`) — byte-for-byte the
same decode as `__lullaby_str_char_at`, so char values and order are identical (a
pure performance change). Default-deny: any deviation from the exact desugar (the
counter used elsewhere, the string reassigned mid-loop, a promoted slot) falls
through to the previous O(n²) lowering, which stays correct.
*Measured (this box, best-of-5).* strscan native **30.6 → 1.66 ns/char** (**42.7× →
2.43× C**); csvsum native **24.8 → 2.88 ns/char** (**24.5× → 2.61× C**). The
scaling signature is gone: at a fixed 19.8M total chars, native wall time is now
**flat with string length** (66/264/528 chars → 33/30/31 ms, was 616/2124/5334 ms)
— i.e. O(N), not O(N²). Correctness: full `cargo test --all` green (incl. the
`gen_string_loop_program` differential fuzzer) plus a new multi-byte fixture
`tests/fixtures/valid/native_string_utf8_foreach.lby` (1/2/3/4-byte code points:
`é`, `☕`, `日本語`, `🎉`) asserting native exit == interpreter.

*Original evidence (kept for the record).* Disassembly of the native string-scan `checksum` (`strscan_native`)
shows the `for c in s` body calling a per-character helper that, for character
index `i`, **walks the UTF-8 from the string start counting code points to locate
the i-th one** (the loop at the helper's entry: `xor rax,rax; xor rcx,rcx;
cmp rcx,index; inc rax; test continuation-byte; …`), then decodes it. That is
O(i) per character, so the whole scan is O(n²). Confirmed behaviorally: holding
total character work fixed at 19.8M chars, native wall time grows with string
*length* — 66 chars 513 ms, 264 chars 1530 ms, 528 chars 2870 ms (a linear scan
would be flat). The bytecode interpreter over the identical program is flat
(3292 / 3240 / 3360 ms) — i.e. **the interpreters iterate `for c in s` linearly;
only the native backend re-indexes**. This also contradicts the harness's own
documentation, which states `for c in s` is the linear O(n) idiom (true on the
interpreters, false on native today).
*Fix.* Lower a string `for c in s` to a single forward byte cursor that decodes
one code point and advances by its UTF-8 width per step (what the interpreters
already do), instead of desugaring to indexed `char_at(s, i)`.
*Files.* Native string/char for-loop lowering in
`crates/lullaby_ir/src/native_object_stmt.rs` + the char helper in
`crates/lullaby_ir/src/native_object_runtime_helpers.rs` (and the shared IR
for-over-string desugar feeding it).
*ROI.* Very high — turns strscan/csvsum from O(n²) to O(n); on longer strings the
win compounds. Even at 66 chars it should recover a large fraction of the ~39×/30×
gap. *Risk.* Moderate: touches correctness-critical char decoding; must stay
bit-identical to the interpreters (gate with `cargo test --all`).
*(The lowering shipped in `native_object_loops.rs`, not `native_object_stmt.rs` —
`for` lowering moved there in the split; see the SHIPPED note above.)*

**2. Native array-argument pass-by-value copies the whole array per call.**
*Evidence.* Disassembly of `arraysum_native` shows the timed rep loop, before
each `scan(a)` call, copying all 64 elements from the source stack slots into a
fresh temp region (64 `mov`-load + 64 `mov`-store = 128 memory ops) — the
by-value argument copy — for 64 useful `add`s. C passes a pointer and copies
nothing. No packed/SSE instructions are emitted (0 `paddq`/`movdqa`): the
reduction does **not** vectorize across the fat-pointer function boundary, unlike
the inline SIMD reductions elsewhere. The per-call copy dominates the ~9× C gap.
*Fix.* Pass a read-only (unmutated) array argument by reference/borrow instead of
copying it — an escape/mutation analysis on the callee, defaulting to the current
copy when unsure. Secondarily, extend SIMD reduction recognition through the
fat-pointer parameter.
*Files.* Native call-argument lowering / value-semantics in
`crates/lullaby_ir/src/native_object_stmt.rs` and `native_object_expr.rs`; ties
into the arena-first / value-semantics memory-model work.
*ROI.* High on array-passing code. *Risk.* Moderate–high: aliasing correctness
must be exact. *Status.* **Blocked** — native codegen + memory-model in-flight.

**3. Native call-argument marshalling does a redundant stack round-trip.**
*Evidence.* In both the strscan loop and the arraysum call site, a single scalar
argument is moved into `rcx` via `push rax; mov rcx,[rsp]; add rsp,8` (or
`push rax; … pop rcx`) where a plain `mov rcx, rax` suffices; the char-loop bound
check also emits the generic `test rax,rax; js; setle/setge; test al; je`
(~7 insns) instead of a fused `cmp; jl`. These are the same class of localized,
emit-time wins the skill documents (fuse compare-into-branch, drop stack
round-trips).
*Fix.* Peephole the single-scalar-arg-into-`rcx` path and fuse the `for`-bound
comparison into its branch.
*Files.* `crates/lullaby_ir/src/native_object_expr.rs` (call arg emit),
`native_object_stmt.rs` (for-bound compare).
*ROI.* Low–moderate and, per the benchmark's own lesson (immediate-folds measure
as noise on call-bound code), likely small until #1/#2 remove the dominant costs;
worth folding in alongside them. *Status.* **Blocked** — native codegen in-flight.

**4. Interpreter: per-call name resolution (`dispatch_named_call`).**
*Evidence.* `crates/lullaby_ir/src/ir_interpreter.rs:368` — every call runs a
name-keyed chain before invoking: `extern_functions.contains(name)`,
`async_functions.contains(name)`, then `call_function` does
`trait_method_names.contains(name)`, `variants.get(name)`, `structs.get(name)`,
a large `match name { …builtins… }`, and finally `functions.get(name)`
(a `HashMap<String, usize>`). All of it is driven by the call's function-name
*string* at run time, though the target is statically known at compile time. fib
and gcd are call-bound and all three interpreters cluster ~250–300 ns/call, so
this chain is a real (est. ~10–20%) slice of per-call cost.
*Fix.* Resolve each `Call(name, argc)` to a numeric function index (or a
distinct op for constructor/variant/builtin) once at compile/emit time —
mirroring the slot-based local resolution already shipped — so dispatch is a
direct index, not a string-hash chain.
*Files.* `crates/lullaby_ir/src/ir_interpreter.rs`, `bytecode_vm.rs`.
*ROI.* Moderate for the interpreters, but **interpreter perf is secondary to
native** (dev/REPL tiers, ~200–1200× C by nature). *Status.* **Blocked** —
interpreter-VM files in-flight.

**5. Interpreter: struct field access (`field_of` linear name scan).**
*Evidence.* `crates/lullaby_ir/src/bytecode_vm.rs:79` — `field_of` does
`s.fields.iter().find(|(name,_)| name == field)`, an O(fields) string-compare
scan per field read. None of the current benchmark workloads (fib/gcd/array/
string/CSV) use structs, so this does **not** show in the present suite — it is a
real but here-unmeasured lever that would matter on struct-heavy code.
*Fix.* Store struct fields in declared order and resolve field access to a fixed
slot index at compile time (same slot approach as locals).
*Files.* `crates/lullaby_ir/src/bytecode_vm.rs` (+ the AST interpreter's
equivalent). *ROI.* Low against today's corpus (no struct benchmark);
add a struct-heavy workload before investing. *Status.* **Blocked** —
interpreter-VM files in-flight; also **needs a struct benchmark** to measure.

### Honest notes

- Nothing here is a claimed win: this pass changed no compiler code. Items 1–3
  are evidence-backed native levers; 4–5 are secondary interpreter levers, one of
  which (5) is not even exercised by the current suite.
- `bytecode` is consistently the fastest interpreter tier on loop/scan shapes
  (e.g. 78 vs ~157 ns/iter on the sum loop; 195 vs ~300 ns/gcd), confirming the
  flat VM is earning its tier — but on the string scan it is ~163 vs the AST's
  ~166 (no lead), so string iteration cost is shared, not VM-specific.
- Reproduce: `cargo build --release -p lullaby_cli`, then
  `benchmarks/run_bench.ps1`, `run_loop.ps1`, `run_gcd.ps1`, `run_arraysum.ps1`,
  `run_strscan.ps1`, `run_csvsum.ps1` (all `-Reps 5`). Disassemble the emitted
  hot function with `dumpbin /disasm:nobytes <exe>` to re-confirm the O(n²) char
  walk and the per-call array copy.
