# Lullaby 1.0 Dogfood Gap Audit

**Status:** Empirical audit, 2026-07-14. Method: build the real toolchain
(`cargo build -p lullaby_cli`) and actually write + run six representative
programs through the shipping compiler (`lullaby run`, `--backend ir|bytecode`,
`native`, `native --freestanding`). This document records what could be
expressed and run, the exact blockers hit (with `L####` diagnostics), and a
prioritized 1.0 gap list cross-referenced to the two-tier model in
[execution_tiers_and_1_0_scope.md](execution_tiers_and_1_0_scope.md).

This is an audit only — no compiler or language change was made. Sample programs
are kept as evidence in [`examples/dogfood/`](../examples/dogfood).

## Scope reminder (what 1.0 must span)

Per the owner decision, Lullaby 1.0 is a memory-safe-by-default systems language
you can write **apps, services, AND a kernel** in, across two tiers:

- **Safe tier** (default) — apps & services; arena-first memory, structs/enums/
  collections, closures, actors.
- **Freestanding tier** (`--freestanding` / `no-runtime`) — kernels, boot,
  embedded; raw pointers + pointer arithmetic, static-buffer arenas, inline asm,
  MMIO/volatile, `repr(C)`/packed, interrupt calling conventions, pluggable
  panic handler, direct-object output.

## Result at a glance

| # | Program | AST / IR / bytecode | `native` | `native --freestanding` |
|---|---------|:---:|:---:|:---:|
| 1 | Expression parser + evaluator | ran fully | ineligible (L0339) | ineligible |
| 2 | Request-handler / service core | ran fully | ineligible (L0339) | ineligible |
| 3 | CLI word-frequency tool | ran fully | ineligible (L0339) | ineligible |
| 4 | Data-processing pipeline (HOFs/closures) | ran fully | ineligible (L0339) | ineligible |
| 5 | State machine / game loop | ran fully | ineligible (L0339) | ineligible |
| 6 | Freestanding bit-manip probe (scalar) | ran fully | **ran (exit 56)** | **ran (exit 56, no linker)** |
| 6b | Low-level unsafe/volatile/layout probe | ran fully | ineligible | ineligible |

**Five of six programs ran fully — on the interpreters only.** Programs 1–5 were
rejected by `native` with `L0339`. **CORRECTION (verified by the coordinator
after this audit):** the `L0339` diagnostic's note text — "the native backend
compiles only i64-scalar functions" — is **stale and inaccurate**, and this audit
initially trusted it. In reality native compiles a large surface to PE
executables today: scalars, `string`, `list`, `map`, one-level structs/enums, and
`result`/`option` over scalar/string payloads (e.g. `native_aggregate_enum.lby`,
`native_list_struct.lby`, `native_map_string.lby`, `native_string_build.lby` all
build direct-PE exes). Programs 1–5 hit `L0339` for **specific deferred shapes**,
not a blanket lack of types — see the corrected highest-impact section below. The
`L0339` note text is itself a diagnostics bug to fix.

## Single highest-impact missing capability (CORRECTED)

Native already compiles structs, enums, `list`, `map`, `string`, and one-level
aggregates to PE. The real, verified native gaps that blocked Programs 1–5 are
**three specific deferred shapes**, confirmed from the detailed per-function skip
reasons (`native --verbose`):

1. **Nested aggregate payloads** — an aggregate *inside* an enum/`result`/`option`
   payload, e.g. `result<list<Tok>, string>` ("deeper payloads are deferred";
   one-level payloads compile).
2. **Heap-typed struct/enum fields** — a struct/enum whose *field* is a
   `string`/`list`/`map` (e.g. `struct PR` with an `err string` field): "heap-value
   struct fields are not in the native subset". Scalar-field aggregates compile.
3. **Void-returning `main` as native entry** — the native entry must return `i64`
   (its exit code); a `void main` (println-demo style) leaves no eligible entry, so
   the whole program reports `L0339`. Several audit programs used `void main`.

Closing #1 and #2 (native support for nested/heap-typed aggregate payloads and
fields) is the highest-impact native-expansion work — it's what a real parser
(`result<list<Tok>, string>`) or service struct (a record with `string` fields)
needs. This is a **targeted expansion of an already-broad native backend**, not
the wholesale "native only does i64" rewrite this audit first reported. Also fix
the misleading `L0339` note text so the next reader (or user) isn't misled the
same way.

## Per-program detail

### 1. Expression parser + evaluator — `examples/dogfood/01_expr_parser.lby`

Recursive-descent tokenizer + parser + evaluator for `+ - * /` and parentheses,
with `result`/`?`/`err` error handling. **Ran fully** on AST, IR, and bytecode
(`2 + 3 * 4 = 14`, `(2 + 3) * 4 = 20`, and correct errors for `7 + `, `1 / 0`,
`2 @ 3`).

Blockers / friction hit:

- **`match` is not an expression, and a match arm body cannot be an assignment.**
  The natural token-dispatch (`match op` with arms that set a flag, or
  `let k = match op ...`) is rejected at parse time with `L0207` ("unexpected
  token in expression"). Worked around by writing five one-line predicate
  helpers (`is_plus`, `is_star`, …) that each *return* a `bool` from a `match`.
  This is verbose and un-idiomatic for a switch-heavy parser. **Severity: High
  (ergonomics + LLM-friendliness).**
- Recursive `enum` (an AST node type) works on the interpreters, but native
  rejects the whole program (`L0339`). A recursive-enum probe compiled with
  `native` fails identically.
- Parser state is threaded through a `struct PR { val, pos, err }` returned by
  value — fine, but there is no `option`-of-tuple or multiple-return, so a
  sentinel `err string` field stands in.

### 2. Request-handler / service core — `examples/dogfood/02_service_core.lby`

`Request`/`Response` structs, a `map<string, string>` store, method+path routing,
string-built responses. **Ran fully.** Output routes GET/PUT/POST correctly
(`GET /hello -> HTTP 200 world`, `POST /y -> HTTP 405 …`).

Notes:

- No real socket was opened here so the core stays interpreter-portable, but the
  socket surface to make it a *real* server **does** exist and is exercised by
  `examples/valid/http_server/` (`tcp_listen`/`tcp_accept`/`tcp_read`/`tcp_write`/
  `tcp_shutdown`/`tcp_close`). So a service is expressible today — **on the
  interpreters only** (sockets are interpreter/runtime builtins, absent from the
  native and WASM subsets).
- Functional `map` updates mean a mutating route must thread the returned map
  back out; there is no in-place map handle. Minor.

### 3. CLI word-frequency tool — `examples/dogfood/03_cli_wordfreq.lby`

Reads a file path from `args()`, `words(lower(text))`, tallies a
`map<string,i64>`, rebuilds sortable `Entry` structs, `sort_by` with a comparator
`fn` value, prints the top 5. **Ran fully** against a sample file.

Blockers / friction hit:

- **No stdin.** The I/O surface is `args()` / `read_file` / `read_lines` /
  `read_bytes` — there is no `read_stdin`/line-reader, so a Unix-style
  `cat file | tool` filter cannot be written; a CLI tool must be handed a path.
  **Severity: Medium (CLI ergonomics).**
- Same **`match`-arm-assignment** gap as Program 1: `match map_get(...) { some(c)
  -> counts = map_set(...) }` is a parse error (`L0207`). Worked around with a
  `bump(counts, w) -> map` helper that returns the updated map.
- **`map` has no entry iteration** — no `map_entries`/`for (k,v) in m`. Rebuilt a
  parallel `list<Entry>` from `map_keys` + `map_get`. Works; mildly awkward.

### 4. Data-processing pipeline — `examples/dogfood/04_pipeline.lby`

CSV-ish parse → `list_map` (parse) → `list_filter` (closure capturing a `dept`
name) → `list_reduce` / `list_map` + `list_sum` aggregate → `list_max`.
**Ran fully** (`engineers: 3`, `sum of eng ages: 105`, `avg 35`, `oldest 52`).

Findings:

- **Closures work well.** Closure literals `fn r Rec -> r.dept == dept` capture
  enclosing locals by value and compose with `list_map`/`list_filter`/
  `list_reduce` over `list<struct>`. This was the smoothest program.
- Higher-order collection helpers are **interpreter-level only** (per
  [standard_library.md](standard_library.md)); a native build of this program is
  `L0339`-ineligible.
- Same `match`-arm-assignment gap pushed the `parse_i64` handling into a
  `parse_int_or(s, default)` helper.

### 5. State machine / game loop — `examples/dogfood/05_game_loop.lby`

A fixed-step 1D bouncing-ball simulation: `enum Phase { Playing, Over }`, a
`World` struct, wall bounces, a bounce budget that flips the phase. **Ran fully**
(11 ticks to `phase=over`, `bounces=3`).

Findings:

- **Structs are value types with no in-place field mutation** (no `&mut`
  receiver). Each tick returns a fresh `World` the loop reassigns (`w = step(w)`).
  Correct and clear, but every update copies the whole struct — a real
  consideration for a hot game/sim loop, and exactly the case the decided
  arena/loop-sub-region model is meant to reclaim (see Gap G2). **Severity:
  Low–Medium.**
- Enums + `match` for phase logic are ergonomic. No blocker.

### 6 / 6b. Freestanding & low-level probe — `06_freestanding_probe.lby`, `06b_unsafe_probe.lby`

**6 (scalar bit-manipulation): the only program that reaches compiled code.**
Hand-rolled `popcount` + `set_bit`/`clear_bit`/`test_bit` over `i64`, reporting
312. Runs on interpreters **and** `native` **and** `native --freestanding`
(direct PE, no linker) — exit code 56 = 312 mod 256 on all three. This confirms
the freestanding *output path* is real for scalar code.

**6b (unsafe / volatile / layout): interpreter-only.** `size_of`/`align_of`/
`offset_of` fold correctly (struct `Regs` → 24, `offset_of(r,"c")` → 16); an
`unsafe` block round-trips a heap slot with `ptr_to_int`/`int_to_ptr`/
`volatile_store`/`volatile_load`/`ptr_read` (→ 111). All correct.

Freestanding-tier blockers hit (these map directly onto the kernel checklist):

- **Native/freestanding is scalar-only.** Even the `unsafe` heap round-trip of
  6b is `L0339`-ineligible for `native`; `--freestanding` cannot compile any
  program that allocates. The kernel tier therefore cannot yet compile a struct,
  an enum, a buffer write, or an `unsafe` pointer op. **Severity: Critical.**
- **No real pointer arithmetic.** Interpreter pointers are opaque heap-slot
  handles, not addresses: `int_to_ptr(ptr_to_int(p) + 8)` fails at runtime with
  `L0406` ("invalid pointer `8`"). Kernel checklist item #1 (pointer *arithmetic*)
  is unmet — you can round-trip a pointer through an integer but cannot compute a
  neighbouring address. **Severity: High (freestanding tier).**
- **Inline assembly is raw machine-code bytes only.** `asm 72, 199, 192, …`
  emits verbatim bytes; there is no textual assembly, operand/register
  constraints, or clobber list. It also cannot run on an interpreter (`L0425`),
  only via `native`. Kernel checklist #3 is only partially met. **Severity:
  Medium–High.**
- **No `repr(C)` / packed / explicit-alignment attributes.** Layout is
  *queryable* (`size_of`/`align_of`/`offset_of`, C-natural layout) but not
  *controllable* — there is no `repr(C)`/`packed`/`align(N)` on a struct.
  Checklist #7 unmet. **Severity: High (kernel/FFI).**
- **No MMIO at real addresses, port I/O, control-register, interrupt/naked, or
  panic-handler surface.** `volatile_load`/`volatile_store` exist but operate on
  heap-slot handles on the interpreters (the native volatility guarantee has no
  way to name a physical MMIO address without pointer arithmetic). Checklist
  items #4 (MMIO/port I/O), #5 (control registers), #6 (interrupt conventions),
  #8 (pluggable panic handler) have no source surface today. **Severity:
  Critical for the kernel claim.**

## Cross-cutting findings

- **The arena/region memory model — the decided headline 1.0 differentiator — is
  not implemented.** `region NAME: size=N, align=N` parses only as an inert
  one-line *declaration*; it does **not** open an indented block. Attempting the
  documented explicit-`region` block form
  (`region scratch: size=1024`\<newline\>\<indented body\>) is a parse error
  (`L0207` "expected expression"). At runtime the declaration is a no-op
  (`Stmt::Region(_) => {}` in the runtime; returns `Void`). So neither implicit
  function/loop sub-regions nor explicit region blocks reclaim anything today —
  the memory model that distinguishes Lullaby from "terser C" and makes
  LLM-generated code safe-by-default is still a syntax stub. **Severity: Critical
  for 1.0 identity.**
- **`lullaby fmt` deletes comments.** Running `fmt --write` on any dogfood file
  strips every `#` comment and blank line. Since human/LLM-readable commented
  source is a stated project goal, a formatter that discards comments is a
  correctness gap for the toolchain, not a style choice. (The dogfood examples
  are kept in their hand-commented form for this reason; they are not
  fmt-clean.) **Severity: Medium.**
- **No generic user types.** `struct Stack<T>` is a parse error (`L0205`
  "expected newline after struct name"), matching the roadmap. Generic
  *functions* and built-in generics (`list`/`map`/`option`/`result`) work, and
  nested instantiations (`list<list<i64>>`, `map<string, list<i64>>`) run fine —
  but you cannot define your own generic container. **Severity: Medium
  (post-1.0 per exec-tiers doc, but limits a std-collections story).**
- **`match`-arm-assignment / match-as-expression** is the most frequently hit
  ergonomic gap — it forced a helper function in three of six programs. For a
  language whose thesis is terse, LLM-friendly source, the inability to write
  `x = match e { … }` or a side-effecting arm is a disproportionately large tax.

## Prioritized 1.0 gap list

### Must-have for the spanning set (block "apps, services, AND a kernel")

| ID | Gap | Tier | Evidence |
|----|-----|------|----------|
| **G1** | **Native defers three aggregate shapes** (native already compiles scalars/`string`/`list`/`map`/one-level structs+enums): (a) nested aggregate payloads (`result<list<T>,…>`), (b) heap-typed struct/enum *fields* (a `string`/`list` field), (c) `void main` isn't a native entry. Plus the stale `L0339` note text to fix. | Both | `native --verbose` skip reasons on Programs 1–5 |
| **G2** | **Arena/region memory model is a no-op stub** — no working `region` block, no implicit function/loop sub-regions, no reclamation. The decided default memory model does not run. | Safe | `L0207` on region-block; `Stmt::Region(_) => {}` |
| **G3** | **Freestanding hardware surface absent** — no real pointer arithmetic (`L0406`), no `repr(C)`/packed/align, no MMIO-at-address / port I/O / control-register / interrupt-convention / panic-handler primitives; inline asm is raw bytes only. | Freestanding | Program 6b; checklist items #1,#3,#4,#5,#6,#7,#8 |
| **G4** | **Static-buffer arenas for `no-runtime`** — depends on G2 + G1; no allocation discipline without a host allocator exists yet. | Freestanding | (not reachable to test; blocked by G1/G2) |

### Should-have for a credible 1.0 (ergonomics / completeness the audit repeatedly hit)

| ID | Gap | Evidence |
|----|-----|----------|
| **G5** | **`match` as an expression + non-trivial arm bodies** (`let x = match …`, side-effecting/block arms). Hit in 3/6 programs. | `L0207` |
| **G6** | **`lullaby fmt` preserves comments** (currently strips them). | fmt --write diff |
| **G7** | **stdin / line-reader I/O** so CLI filters (`cat | tool`) are expressible. | Program 3 |
| **G8** | **`map` entry iteration** (`map_entries` / `for (k,v) in m`). | Program 3 |

### Nice-to-have / post-1.0 (consistent with the exec-tiers doc deferrals)

| ID | Gap | Notes |
|----|-----|-------|
| G9 | Generic **user** types (`struct Stack<T>`) | `L0205`; already roadmapped post-1.0 |
| G10 | In-place struct mutation / `&mut`-style receivers | value-copy works; perf/ergonomics only |
| G11 | Bit-intrinsic builtins (`count_ones` etc.) in the native subset | hand-rollable today |
| G12 | Trait objects / `dyn`, associated types, default trait bodies | already roadmapped |

## Bottom line

The **interpreters are broadly capable today** — five of six representative
programs (parser, service core, CLI tool, data pipeline, state machine) were
written and ran correctly with only ergonomic friction (chiefly G5). Native is
also **broadly capable** (structs, enums, `list`, `map`, `string`, one-level
aggregates all compile to PE) — corrected from this audit's first draft. The gap
to 1.0 is: **(G1)** native must cover the three deferred aggregate shapes (nested
payloads, heap-typed fields, void entry) so real parsers/services compile;
**(G2)** the arena memory model that defines the safety story is still an inert
stub (being implemented now); and **(G3)** the freestanding hardware surface. G2
and the native-aggregate expansion (G1) are the load-bearing work for the
"spanning set" claim; G5 (match-as-expression) is the top ergonomic fix.
