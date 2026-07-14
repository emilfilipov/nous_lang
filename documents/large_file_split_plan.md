# Large File Split Plan

Standing backlog for breaking up oversized source files so parallel sub-agent work
stops serializing on shared files. This is a **planning document only** — no source
was split to produce it. The coordinator schedules splits one at a time, each when its
target file is free (not being edited by another agent).

Rule under audit (CLAUDE.md, "Keep source files small and modular"): **hard cap ~1500
lines, soft target ~800**.

Measurement date: 2026-07-14. Scope: `crates/**/*.rs` (primary), plus tooling
(`*.py` / `*.ps1`) and `documents/*.md`. Line counts are raw `wc -l`.

---

## 1. Size distribution

### Rust source (`crates/**/*.rs`) — 59 files, 89,597 total LOC (avg 1,518 / file)

| Threshold | Files over |
|-----------|-----------:|
| > 800     | 35 |
| > 1200    | 32 |
| > 1500 (hard cap) | 27 |
| > 2000    | 21 |
| > 3000    | 9  |

The distribution is heavily top-weighted: the average Rust file is already ~1,518
lines, i.e. right at the hard cap. 46% of files (27/59) exceed 1,500 and 36% exceed
2,000. This is genuine bloat concentrated in a handful of subsystems (native x86
backend, semantics checker, parser, runtime), not a long tail of borderline files.

### Tooling and docs

- No `*.py` / `*.ps1` tooling file exceeds 400 lines. Tooling is not a concern.
- Only two `documents/*.md` exceed 800 lines: `freestanding_tier_design.md` (1,077)
  and `wasm_backend_design.md` (811). Both are in-flight/spec docs and out of scope
  for this mechanical-split plan; leave them to their owners.

---

## 2. Is ~1500 the right hard cap?

**Recommendation: keep ~1500 as the hard cap, but drive the backlog by priority tiers
rather than treating every flagged file as equally urgent — and adopt a more lenient
cap for pure test modules.**

Reasoning:

- 1,500 is a sensible *engineering* cap: above it a single file reliably contains
  multiple distinct responsibilities that two agents will want to edit concurrently.
  The data supports the cap's intent — the pain is real (avg file already ~1,518).
- But flagging 46% of files at once is alert fatigue, not an emergency. The true
  collision cost is concentrated in the **9 files over 3,000** (five of them over
  4,000). Those are the ones where multiple agents currently collide.
- **8 of the 27 over-cap files are pure test modules** (`*_tests.rs`, `tests/cli/*`).
  Test files split trivially (move a group of `#[test]` fns to a sibling) and rarely
  cause real merge collisions, because agents append new tests at the end rather than
  editing shared regions. Applying the same 1,500 cap to them inflates the backlog
  without proportional benefit.

Concrete tiering the coordinator should schedule against:

| Tier | Threshold | Count | Meaning |
|------|-----------|------:|---------|
| Urgent | > 3000 | 9 | Split first; these are where agents collide today. |
| Priority | 2000–3000 | 12 | Split as capacity allows. |
| Target | 1500–2000 | 6 | Split opportunistically when the file is touched anyway. |

Suggested refinement to the rule: keep the **1,500 hard cap for non-test source**, and
set a **separate ~2,500 cap for pure test modules**. Under that split rule the flagged
non-test source count drops from 27 to ~19, which is a realistic standing backlog.

---

## 3. Ranked oversized-file list (every file > 1200 lines)

`T` = pure test module. `HOT` = known hot/contended file (native backend, parser,
semantics, VM — actively edited by multiple agents). Splits should be scheduled only
when the file is momentarily free.

| # | Lines | Crate | File | Tier | Notes |
|--:|------:|-------|------|------|-------|
| 1 | 5366 | lullaby_ir | `src/native_object_stmt.rs` | Urgent | HOT |
| 2 | 4396 | lullaby_ir | `src/native_object.rs` | Urgent | HOT (named contended) |
| 3 | 4302 | lullaby_ir | `src/native_program_tests.rs` | Urgent | T |
| 4 | 3358 | lullaby_runtime | `src/lib.rs` | Urgent | HOT |
| 5 | 3343 | lullaby_semantics | `src/semantics_tests.rs` | Urgent | T |
| 6 | 3310 | lullaby_semantics | `src/lib.rs` | Urgent | HOT |
| 7 | 3159 | lullaby_semantics | `src/semantics_checker_calls.rs` | Urgent | HOT |
| 8 | 3148 | lullaby_parser | `src/lib.rs` | Urgent | HOT |
| 9 | 3134 | lullaby_cli | `tests/cli/suite1.rs` | Urgent | T |
| 10 | 2918 | lullaby_ir | `src/native_object_runtime_helpers.rs` | Priority | HOT |
| 11 | 2897 | lullaby_ir | `src/bytecode_vm.rs` | Priority | HOT |
| 12 | 2642 | lullaby_ir | `src/wasm_lowering.rs` | Priority | |
| 13 | 2578 | lullaby_cli | `tests/cli.rs` | Priority | T |
| 14 | 2431 | lullaby_ir | `src/lib.rs` | Priority | HOT |
| 15 | 2392 | lullaby_runtime | `src/runtime_eval.rs` | Priority | HOT |
| 16 | 2232 | lullaby_ir | `src/native_object_lowering.rs` | Priority | HOT |
| 17 | 2187 | lullaby_cli | `tests/cli/suite2.rs` | Priority | T |
| 18 | 2155 | lullaby_ir | `src/ir_interpreter_builtins.rs` | Priority | |
| 19 | 2090 | lullaby_ir | `src/wasm_lowering_mem.rs` | Priority | |
| 20 | 2056 | lullaby_ir | `src/ir_interpreter.rs` | Priority | HOT |
| 21 | 2009 | lullaby_ir | `src/ir_optimizer.rs` | Priority | |
| 22 | 1927 | lullaby_cli | `src/main.rs` | Target | |
| 23 | 1857 | lullaby_ir | `src/wasm.rs` | Target | |
| 24 | 1837 | lullaby_cli | `tests/cli/suite3.rs` | Target | T |
| 25 | 1775 | lullaby_ir | `src/ir_lib_tests.rs` | Target | T |
| 26 | 1652 | lullaby_ir | `src/wasm_tests.rs` | Target | T |
| 27 | 1566 | lullaby_ir | `src/aarch64.rs` | Target | |
| 28 | 1484 | lullaby_parser | `src/format.rs` | (under cap) | |
| 29 | 1425 | lullaby_runtime | `src/runtime_tests.rs` | (under cap) | T |
| 30 | 1368 | lullaby_ir | `src/native_object_writers.rs` | (under cap) | |
| 31 | 1355 | lullaby_ir | `src/native_object_expr.rs` | (under cap) | |
| 32 | 1320 | lullaby_runtime | `src/runtime_builtins.rs` | (under cap) | |

Files 28–32 are under the 1,500 hard cap and are **not** in the split backlog; listed
only for the >1200 completeness the audit asked for.

---

## 4. Per-file split proposals (files over the ~1500 hard cap)

Ordered by ROI: highest first = files most often edited by multiple agents (native
backend, semantics, parser, runtime) where collisions hurt most. Test-module splits
are lower ROI (fewer real collisions) and are grouped at the end.

The `lullaby_ir` native backend already uses a `native_object_*.rs` sibling scheme
(`native_object.rs`, `_stmt`, `_lowering`, `_runtime_helpers`, `_writers`, `_expr`,
`_coff_tests`). **Extend that scheme — do not invent a parallel naming convention.**

Risk key: **Low** = move self-contained fns/impls with no shared private mutable
state; **Medium** = moved code shares a context struct but only via existing
`pub(crate)` items; **High** = tightly coupled to shared private state or requires
breaking up a single giant function/impl (not just moving whole items).

---

### 4.1 `native_object_stmt.rs` — 5366 → several siblings  · ROI #1 · HOT · Risk Medium

Biggest and hottest file in the tree. Cleanly separable responsibilities already sit
in contiguous line ranges, and nearly every helper is already `pub(crate)`, so most
moves are mechanical. Keep statement lowering as the core.

- **Keep in `native_object_stmt.rs`**: statement lowering core — `lower_native_stmts`,
  `lower_native_stmt`, `lower_aggregate_init`, `lower_enum_construction`,
  `lower_value_into`, `lower_native_function`, prologue/epilogue + arena helpers
  (lines ~435–1550).
- **→ `native_object_regalloc.rs`**: `PReg`, promotion analysis
  (`promoted_var_reg`, `expr_reg_promotable`, `instr_reg_promotable`,
  `for_counter_slots`, `plan_register_promotion`, `score_expr_usage`,
  `score_instr_usage`) — lines ~26–434.
- **→ `native_object_place.rs`**: place resolution / address emission (`PathStep`,
  `resolve_place_steps*`, `resolve_read_place*`, `resolve_scalar_place*`,
  `emit_load_place`, `emit_dynamic_addr_into_rcx`) — lines ~1551–1845.
- **→ `native_object_simd.rs`**: SIMD/vector + reduction machinery — bounds/movdqu/
  hfold/cpuid helpers, `MinMaxOp`/`ReduceOp`/`MapOp`/`FloatMapOp`/`MapKind` enums, and
  sum-reduction detection (`SumReductionLoop`, `SumBound`, `detect_sum_reduction`,
  `AddendCheck`, related emit helpers) — lines ~1846–2211 and ~2633–end.
- **→ `native_object_match.rs`**: `lower_native_match`, `lower_match_arm_body`,
  `try_emit_fused_i64_condition_branch`, `lower_native_if`, `emit_cmp_rax_imm`
  — lines ~2212–2632.

Value very high (constantly edited), risk Medium (shared `NativeCtx`, but all touch
points are `pub(crate)`). Must be scheduled when the file is quiescent.

---

### 4.2 `native_object.rs` — 4396 → keep core + 3 siblings  · ROI #2 · HOT · Risk Medium

Explicitly named as a contended file. Three clean regions.

- **Keep in `native_object.rs`**: top-level orchestration — `NativeProgram`,
  `DebugOptions`, `emit_native_program*`, `emit_native_program_for_target`,
  `merge_native_skip`.
- **→ `native_object_coff.rs`**: COFF object emission — `NativeObjectFile/Section/
  Symbol/Snapshot`, `NativeObjectError`, `emit_coff_object`, `snapshot_native_object`,
  `NativeFunctionCodegen` entry-function codegen, `write_x86_64_coff_object` and its
  byte helpers (lines ~170–728).
- **→ `native_object_eligibility.rs`**: signature/eligibility + array-length inference
  — `native_signature_eligibility`, `native_signature_type_is_aggregate`,
  `param_is_read_only` family, `infer_array_lengths`, `infer_return_array_len`,
  `supported_list_element`, `supported_map_kv`, `is_native_collection_element_shape`,
  `native_collection_slot`, `is_scalar_or_string_slot` (lines ~996–2444).
- **→ `native_object_types.rs`**: `NativeType`, `NativeEnumVariant`, `FloatWidth`,
  `resolve_native_type`, `resolve_enum_type`, `native_type_of_init`,
  `resolve_signature_native_type`, `enum_ctor_name`, `is_enum_type_name`
  (lines ~2445–end).

Value very high, risk Medium (regions are contiguous and mostly free functions).

---

### 4.3 `native_object_runtime_helpers.rs` — 2918 → keep core + 3 siblings  · ROI #3 · Risk Low

**Best low-risk / high-value win.** ~47 `emit_*_helper` functions, each returning a
self-contained `HelperFunction` with essentially no shared mutable state. Grouping is
obvious.

- **Keep in `native_object_runtime_helpers.rs`**: allocation/rc/shadow core —
  `emit_heap_alloc_helper`, `emit_rc_free_helper`, `emit_rc_dec_helper`,
  `emit_drop_string_array_helper`, `emit_heap_strlen_helper`, shadow prologue/epilogue,
  `emit_helper_call*`, small shared byte emitters.
- **→ `native_object_list_helpers.rs`**: `emit_list_new/copy/grow_helper`,
  `emit_struct_copy_helper`, `emit_list_word_copy_loop_rsi_rdi_rbx`.
- **→ `native_object_map_helpers.rs`**: `emit_map_new/copy/grow/find_helper`.
- **→ `native_object_string_helpers.rs`**: all `emit_str_*` helpers (the bulk, ~1700
  lines) plus `emit_to_cstr_helper`, `emit_parse_i64_helper`, `emit_char_to_byte_walk`.

Risk Low, value high (native backend, edited). Schedule early.

---

### 4.4 `semantics_checker_calls.rs` — 3159 → break up `check_call`  · ROI #4 · HOT · Risk High

**The standout hazard.** A single method, `check_call`, spans lines ~16–1817 — roughly
1,800 lines in one function. Every agent touching call type-checking edits the same
monster fn, guaranteeing collisions. This is a *High-risk* split because it is not a
file move: `check_call` must first be decomposed into helper methods before anything
can move to a sibling.

Proposed approach (do the extraction and the move as one scheduled unit):

- Inside `check_call`, factor by call category into private helper methods on
  `Checker`: builtin/intrinsic calls, method/UFCS calls, free-function calls, and
  conversion/cast calls.
- **→ `semantics_checker_call_builtins.rs`**: the builtin/intrinsic-call helpers.
- **→ `semantics_checker_call_dispatch.rs`**: method/trait/free-function dispatch
  helpers (`check_trait_method_call`, `check_generic_call`, `type_param_has_bound`
  already exist at ~1817+ and move here too).
- **Keep in `semantics_checker_calls.rs`**: the thin `check_call` dispatcher plus
  construction/match helpers (`check_struct_construction`, `check_enum_construction`,
  `check_match`, `check_struct_literal`, `expect_*` argument validators).

Value very high, risk **High** — needs careful behavior-preserving extraction with the
full semantics suite green after each step, not a mechanical move.

---

### 4.5 `semantics/lib.rs` — 3310 → keep Checker + 2 helper siblings + checker-method split  · ROI #5 · HOT · Risk High

Dominated by one `impl Checker` block spanning ~711–3175 (~2,460 lines). The crate
already proves the pattern with `mod checker_calls` / `mod tests`.

- **→ `semantics_aliases.rs`**: `resolve_program_aliases`, `chain_is_cyclic`,
  `resolve_alias_type` / `_depth`, `rewrite_stmt_types` (lines ~104–676). **Low risk**
  — free functions.
- **→ `semantics_generics.rs`**: `GenericInferenceError`, `decompose_generic`,
  `unify_param`, `substitute_type`, `first_unresolved_type_var`, `infer_generic_return`,
  `type_contains_var`, `substitute_self` (lines ~311–520). **Low risk**.
- **→ `checker_exprs.rs` / `checker_stmts.rs`**: partition the `impl Checker` methods
  into expression-checking vs statement/decl-checking, mirroring the existing
  `checker_calls` split. **High risk** (shared private state on `Checker`; needs a
  method-level read to partition cleanly).
- **Keep in `lib.rs`**: `Checker` struct def, `validate` / `validate_executable` /
  `validate_entrypoint`, `Signature`, `Scope`, and the small type-predicate helpers.

Start with the two Low-risk helper extractions (immediate ~200-line + ~200-line
reduction with near-zero risk), then schedule the High-risk method partition
separately.

---

### 4.6 `parser/lib.rs` — 3148 → AST + expr parser + literals split  · ROI #6 · HOT · Risk Medium

`impl Parser` (~986–2438) and `impl ExprParser` (~2455–3133) are two large, separable
parsing surfaces; the AST node definitions (~8–975) are pure data. `mod format` and
`mod tests` already sit as siblings.

- **→ `ast.rs`**: all public AST node types — `Program`, `TraitDecl`, `ImplDecl`,
  `StructDecl`, `EnumDecl`, `AliasDecl`, `Function`, `Param`, `TypeParam`, `TypeRef`,
  `Stmt`, `Place`, `Expr`, `ExprKind`, `UnaryOp`, `BinaryOp`, `MatchArm`, etc., plus the
  small pure helpers on them (`function_type`, `generic_type`, `is_builtin_type_name`,
  `split_generic_args`). Lines ~8–975. **Low–Medium risk** (pure data; wide re-export
  surface to keep intact).
- **→ `number_literal.rs`**: `normalize_number_literal`, `parse_radix_literal`,
  `is_radix_prefixed`, `int_suffix_range`, `literal_base_to_i128`, `conversion_call`,
  `parse_number_literal` (lines ~771–975). **Low risk**.
- **→ `expr_parser.rs`**: `ExprParser` impl + `TokenKindRef` / `BlockEnd` helpers
  (lines ~2455–3147). **Medium risk**.
- **Keep in `lib.rs`**: `parse` entrypoint and the `Parser` (statement/declaration)
  impl.

Value high (parser is contended), risk Medium.

---

### 4.7 `runtime/lib.rs` — 3358 → keep Value + 4 siblings  · ROI #7 · HOT · Risk Medium

The runtime value model. `Value`/`StructValue`/`EnumValue` are central and stay; the
surrounding machinery separates cleanly.

- **Keep in `lib.rs`**: `Value`, `StructValue`, `EnumValue`, `Closure`, `Display for
  Value`, `value_type_name`, and small scalar helpers.
- **→ `runtime_int.rs`**: `IntKind` (+ impl), `ArithOp`, `OverflowMode`, `int_div`,
  `int_rem`, `int_shl`, `int_shr`, `int_cmp`, `shift_left`, `overflow_arith`,
  `expect_fixed_int` (lines ~151–421, ~971+). **Low risk**.
- **→ `runtime_map.rs`**: `MapKey`, `OrderedMap` and all its impls (lines ~434–590).
  **Low risk**.
- **→ `runtime_error.rs`**: `RuntimeError` (+ impl), `ErrorCategory`,
  `extern_call_error`, `asm_interpreter_error`, `expect_string/i64/bool` (lines
  ~764–958). **Low risk**.
- **→ `runtime_concurrency.rs`**: `Chan`, `Task`, `Future`, `SharedMutex`,
  `SharedAtomic` and their `PartialEq` impls (lines ~54–150). **Low risk**.
- **→ `runtime_os.rs`**: `monotonic_now_nanos`, `wall_now_millis`, `sleep_millis`,
  `os_random_bytes` (lines ~882–932). **Low risk**.

Value high (runtime edited often), overall risk Medium only because `Value` re-exports
must stay stable; each individual extraction is Low.

---

### 4.8 `bytecode_vm.rs` — 2897 → break up Lowerer  · ROI #8 · HOT · Risk High

`impl Lowerer` spans ~790–2839 (~2,050 lines) — a single large impl like the semantics
Checker. `VmCompiler` (~119–543) and the free type helpers (`option_type`,
`list_element_type`, `map_kv_types`, `reference_inner`, `substitute_self_type`) at the
tail separate easily; the Lowerer impl needs a method-level partition.

- **→ `bytecode_vm_types.rs`**: `option_type`, `list_element_type`, `map_kv_types`,
  `reference_inner`, `substitute_self_type` and `statement_span`, `expr_mentions_var`
  (Low risk).
- **→ `bytecode_lowerer_expr.rs` / `bytecode_lowerer_stmt.rs`**: partition the
  `Lowerer` methods (High risk — shared private state).
- **Keep in `bytecode_vm.rs`**: `VmOp`, `VmProgram`, `VmStep`, `VmLoop`, `VmCompiler`,
  `Control`, `Env`, and eval helpers.

Value high, risk High for the Lowerer partition.

---

### 4.9 `ir_optimizer.rs` — 2009 → one file per pass  · ROI #9 · Risk Low

Textbook clean split: four independent optimization passes, each its own impl.

- **→ `ir_optimizer_inline.rs`**: `impl Inliner` (~24–314).
- **→ `ir_optimizer_constfold.rs`**: `impl ConstantFolder`, `fold_binary` (~315–686).
- **→ `ir_optimizer_cse.rs`**: `impl CommonSubexpressionEliminator`,
  `invalidate_available_exprs`, `pure_expr_signature`, `combine_signatures` (~687–1085).
- **→ `ir_optimizer_licm.rs`**: `impl LoopInvariantMover` and the collect/analyze
  helpers (~1086–end).
- **Keep in `ir_optimizer.rs`**: shared types + the pass-orchestration entrypoint.

Risk Low (passes are decoupled), moderate value. A good early confidence-builder.

---

### 4.10 `native_object_lowering.rs` — 2232 → float + collections siblings  · ROI #10 · HOT · Risk Low–Medium

Two coherent clusters plus shared byte-emit primitives.

- **→ `native_object_float.rs`**: float expr/compare lowering + all XMM/`movsd`/`movq`
  load/store/arith helpers and `FloatWidth`-parameterized emitters (lines ~26–553,
  ~2177–end). **Low risk**.
- **→ `native_object_collections.rs`**: list/map/string runtime lowering
  (`lower_list_new/push/set/pop/get`, `lower_map_*`, `lower_string_*`, `lower_to_string`,
  `lower_int_to_string`, deep-copy/fixup emitters, `lower_parse_i64_into`) — lines
  ~1119–2176. **Medium risk** (touches `NativeCtx`).
- **Keep in `native_object_lowering.rs`**: the generic integer/overflow ALU emit
  primitives (`emit_*_reg_*`, `emit_overflow_core`, `emit_i64_binop_from_stack`,
  `emit_signed_idiv/irem`, `lower_native_saturating/checked_into`, `normalize_bool`).

---

### Remaining Priority/Target source files (over 1500)

- **`ir/lib.rs` (2431)** → **`ir_ast.rs`**: move IR node type defs `IrModule` …
  `IrCleanupRole` (~56–403); keep `lower` entrypoint, `OptimizationConfig`,
  `OptimizationPass`, `OptimizationReport` in `lib.rs`; consider **`ir_lowering.rs`**
  for the large `lower` body. Risk Medium (wide re-export surface).
- **`runtime_eval.rs` (2392)** → **`runtime_eval_builtins.rs`**: move the `builtin_*`
  interpreter methods (`builtin_alloc/load/store/dealloc/size_of/…/read_file/read_line`,
  ~603+) to a sibling `impl` block; keep `eval_expr`/`eval_binary`/`eval_match`/
  `eval_scoped_block`/traceback in place. Risk Low–Medium.
- **`ir_interpreter_builtins.rs` (2155)** → split the `builtin_*` methods by group
  (char/string predicates → `_char_builtins.rs`; conversions `to_int/i64/f32/f64` and
  numeric → `_convert_builtins.rs`). Risk Low.
- **`wasm_lowering.rs` (2642)** → **`wasm_lowering_string.rs`** (all `lower_*`/`emit_*`
  string ops: substring/find/contains/starts_with/ends_with/to_string/itoa/byte-search,
  ~644–2133) and **`wasm_lowering_math.rs`** (scalar math + overflow: `try_lower_scalar_math`,
  `emit_i64_abs/min_max/gcd/sign/clamp`, `OverflowOp`, `lower_wasm_overflow`,
  `emit_fixed_binop`); keep `lower_expr` dispatcher + struct/enum/array/binary lowering
  in place. Risk Low–Medium.
- **`wasm_lowering_mem.rs` (2090)** → **`wasm_lowering_deepcopy.rs`** (`emit_deep_copy*`
  family) vs keep list ops (`lower_list_new/push`, `emit_list_*`). Risk Low–Medium.
- **`ir_interpreter.rs` (2056)** → move the `eval_statement`/`eval_block`/`eval_match`/
  `resolve_places` cluster or the move-analysis helpers (`try_move_*`) to a sibling impl
  file (`ir_interpreter_move.rs`). Risk Medium (shared interpreter state).
- **`wasm.rs` (1857)** → **`wasm_layout.rs`** (`EnumLayout`, `enum_layout`,
  `build_layout`, `WasmValType`, `scalar_val_type`, `collection_slot_type`, pointer/
  slot predicates) vs keep module-emission core. Risk Medium.
- **`cli/src/main.rs` (1927)** → move subcommand handlers (`fmt_file`, `examples`,
  `new_project`, `lsp`, `docs`, project scaffolding) into a `commands/` submodule; keep
  `main`/`run` dispatch. Risk Low–Medium.
- **`aarch64.rs` (1566)** → only 66 lines over cap and low-contention; split
  instruction encoders vs lowering only if touched. Risk Low, **low ROI — defer**.

---

### Test-module splits (lower ROI — schedule after source)

Pure test files split trivially: move a cohesive group of `#[test]` fns to a sibling
and add it to the test harness. They rarely cause real collisions (agents append), so
these are backlog cleanup, not collision relief.

- **`native_program_tests.rs` (4302, 134 tests)** → group by feature into
  `native_program_scalar_tests.rs`, `_string_tests.rs`, `_collection_tests.rs`,
  `_float_tests.rs`, `_control_tests.rs`.
- **`semantics_tests.rs` (3343, 247 tests)** → split by checked area (types/generics,
  calls, patterns/match, diagnostics).
- **`cli tests suite1.rs (3134, 131 tests)`, `cli.rs (2578)`, `suite2.rs (2187)`,
  `suite3.rs (1837)`** → the suite scheme already exists; rebalance into additional
  `suiteN.rs` files so none exceeds the test cap.
- **`ir_lib_tests.rs` (1775, 68 tests)** and **`wasm_tests.rs` (1652)** → split by
  feature group.

---

## 5. Scheduling notes for the coordinator

1. Each split touches exactly one target file (plus `mod` declarations in its parent).
   Never schedule two splits that touch the same parent module concurrently.
2. Schedule a split only when its target file is **not** being edited by another agent.
   The native backend files (`native_object*`), parser, semantics, and VM are actively
   edited — coordinate windows.
3. Prefer the **Low-risk** extractions first (native_object_runtime_helpers,
   ir_optimizer, the semantics_aliases/generics helpers, runtime sub-structs): high
   line reduction, near-zero behavior risk, immediate parallelism relief.
4. The two **High-risk** items — `check_call` decomposition and the `impl Checker` /
   `impl Lowerer` method partitions — are behavior-preserving refactors, not moves.
   Give each its own scoped task with the full suite green after every step.
5. Definition of done per split: `cargo test --all` and
   `cargo clippy --all-targets --all-features -- -D warnings` green, no behavior change,
   `documents/repository_map.md` updated with the new files, and the parent `mod`
   wiring correct.
