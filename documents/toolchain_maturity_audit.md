# Toolchain Maturity Audit — B3 (Stable-grade toolchain)

**Purpose:** honest, code-grounded assessment of how close four toolchain pieces
are to "stable-grade" for **1.0-stable**, per `road_to_1_0_stable.md` item **B3**
("a built-in test runner, debug info on Linux/macOS [DWARF], and LSP +
package-manager maturity"). This is the **B3 backlog**: for each piece, what
exists today, what is missing, and a prioritized split of **1.0-required** vs
**post-1.0 nice-to-have**.

Scope note: this is an audit only — no `*.rs` was changed. Every claim below is
read from the current tree (`crates/lullaby_lsp/`, `crates/lullaby_cli/`,
`crates/lullaby_ir/src/native_object*.rs`).

Overall maturity at a glance:

| Piece | Rating | Verdict for 1.0-stable |
|---|---|---|
| Language Server (LSP) | **Adequate** | Usable core with completion; still needs a shipped editor client |
| Package manager / project system | **Adequate for 1.0** | Local-path project system is complete and honest; registry is correctly post-1.0 |
| Test runner | **Adequate-minus** | Real and correct; needs filtering + multi-backend, both small |
| Debug info | **Adequate-minus** | Function-granularity source lines now on **all three** OSes (CodeView on COFF, DWARF on ELF/Mach-O); per-statement lines, variables/types, and CFI remain |

---

## 1. Language Server (`crates/lullaby_lsp/`)

Four modules: `lib.rs` (protocol + lifecycle), `transport.rs` (JSON-RPC/stdio
framing), `diagnostics.rs` (pipeline → LSP diagnostics), `analysis.rs` (hover +
go-to-definition). Wired to the CLI as `lullaby lsp` (stdio server). Code quality
is genuinely production-grade: pure `handle_message` core, framed transport,
thorough unit tests, no stubs or `todo!()`.

**Maturity: Adequate-minus.** The features that exist are correct and tested; the
missing ones are the ones users notice fastest in a real editor.

### What is implemented (verified)
- **Lifecycle:** `initialize` (advertises capabilities), `initialized`,
  `shutdown`, `exit`.
- **Document sync:** full-text (`textDocumentSync: 1`) `didOpen` / `didChange` /
  `didClose`, held in an in-memory `HashMap<uri, text>`.
- **Diagnostics:** on open/change, runs the real lex → parse → semantic pipeline
  and publishes `publishDiagnostics` with the stable `L####` code, `source:
  "lullaby"`, and a 0-based range widened to cover the offending token. Stops at
  the first failing phase (same as `lullaby check`). This is the strongest
  feature and is stable-grade.
- **Formatting:** `textDocument/formatting` returns a single whole-document
  `TextEdit` from the canonical formatter (comment-preserving), or no edits when
  the document is already canonical or does not parse.
- **Hover:** function signatures, struct/enum declarations, a curated set of
  builtin descriptions, and local/parameter types (read from the checker's
  recorded `expression_types`, not re-inferred).
- **Go-to-definition:** resolves to functions, structs, enums, aliases, local
  `let` bindings, and parameters.
- **Completion (`textDocument/completion`):** offers the Lullaby keyword set
  (mirrored from the lexer and pinned to it by a test), the current file's
  top-level declarations (functions/structs/enums/aliases/traits/constants) and
  the enclosing function's locals/parameters with the correct
  `CompletionItemKind` and a signature/type detail, and — when the file is
  module-aware — the `pub` symbols reachable through its `import`s (via the same
  `lullaby_loader` machinery). Degrades to keyword-only completion on an
  unparseable buffer without panicking. See `documents/lsp_design.md` →
  "Completion".

### What is missing or stubbed (verified by absence + the `-32601` fallback)
- **Member/`.`-completion, signature help, completion ranking/snippets** —
  *absent.* Completion offers keywords, in-scope declarations/locals, and imported
  `pub` symbols, but does not yet complete fields/methods after a `.`, provide
  parameter hints, or rank context-sensitively.
- **Find-references / rename** — *absent* (the `references` test asserts the
  method-not-found path). No workspace-wide symbol rewrite.
- **Document/workspace symbols (`documentSymbol`, `workspace/symbol`)** —
  *absent.* No outline view, no symbol search / breadcrumb.
- **Signature help (`signatureHelp`)** — *absent.* No parameter hints while typing
  a call.
- **Semantic tokens / semantic highlighting** — *absent.* Editors fall back to
  regex TextMate grammars (which also do not ship — see below).
- **Incremental sync** — only full-document sync. Fine for small files; large
  files re-lex/re-parse/re-check the whole document on every keystroke.
- **Multi-file / project awareness** — *addressed.* The server now runs the
  shared module loader (`lullaby_loader`, extracted from the CLI) over a file's
  project when it uses `import` or lives in a `lullaby.json` project, with the
  editor's open buffers supplied as an overlay: diagnostics reflect the merged
  program (imported `pub` symbols resolve; the loader's `L0391`/`L0392`/`L0393`/
  `L0397` for the open file surface) while the open file's own lex/parse/semantic
  errors stay at their real positions, and hover/go-to-definition cross module
  boundaries to an imported declaration in another file. A lone no-import file is
  unchanged (single-document fallback). See `documents/lsp_design.md` → "Module
  And Project Awareness".
- **No shipped editor client / extension** — there is **no** VS Code extension,
  no `package.json` client, no TextMate grammar, no `.vsix` anywhere in the repo.
  A server with no packaged client is not an editor experience a 1.0 user can
  install.
- **No cancellation, no progress, no `workspace/didChangeWatchedFiles`,** no
  config handling.

### 1.0-required (prioritized)
1. **A shipped, installable editor client** (at minimum a VS Code extension with
   a TextMate grammar + LSP glue). Without this, "we have an LSP" is not a user
   experience. *Highest leverage, mostly packaging work.*
2. **Completion** (keywords, in-scope functions/locals/types, imported symbols) —
   *done.* `textDocument/completion` offers the keyword set, the file's top-level
   declarations and enclosing-function locals/parameters (with kinds + detail),
   and imported `pub` symbols, degrading to keywords on an unparseable buffer.
   Member/`.`-completion and signature help remain post-1.0.
3. **Cross-file resolution** — *done.* The server runs the module loader so
   diagnostics, hover, and definition work across `import`ed files (module/
   project-aware, with an open-buffer overlay and single-document fallback).
4. **Document symbols** (outline) — cheap given the parsed AST already in hand;
   large credibility-per-effort.

### Post-1.0 nice-to-have
- Find-references / rename, signature help, semantic tokens, incremental sync,
  code actions / quick-fixes, inlay hints, call hierarchy. All valuable, none
  blocking a credible 1.0.

---

## 2. Package manager / project system (`manifest.rs`, `loader.rs`, `lullaby new`)

**Maturity: Adequate for 1.0.** This is the most complete of the four. The
project system is real, honest, and correctly scoped: it is a *project/build
system with local dependencies*, not a package *manager*, and it does not pretend
otherwise.

### What is implemented (verified)
- **`lullaby.json` manifest** (`ProjectManifest`): `name`, optional `entry`, `src`
  (defaults to `["."]`), and local-path `dependencies` (name → path). Parsed and
  validated with `serde_json`; all failures report `L0343`.
- **Multi-file projects:** file-as-module with `import NAME` + `pub` exports,
  enforced by the loader (`L0391` no-shadowing, `L0392` visibility, `L0393` import
  cycles, `L0397` missing module). Modules merge into one flat `Program` the rest
  of the pipeline consumes unchanged — a clean design.
- **Local path dependencies, transitively resolved:** `load_manifest` walks the
  dependency graph, de-duplicates via canonicalized paths, tolerates cycles, and
  builds an ordered `src` search-directory list (own dirs first, then deps).
- **Library vs executable projects:** `entry`-less projects validate/test every
  module (`load_library_project`); executable commands require an `entry`.
- **`lullaby new <name>`:** scaffolds `lullaby.json`, `src/main.lby`, and a
  `.gitignore`, with a valid-identifier name check and a helpful next-step hint.
  Small, correct, complete.

### What is missing (verified by the module doc-comment "deferred" + absence)
- **No registry / remote dependencies.** Dependencies are local paths only
  (explicitly deferred in the manifest module doc).
- **No versioning / version constraints.** `dependencies` is name → path; there is
  no version field, no semver, no compatibility resolution.
- **No lockfile.** Nothing pins a resolved dependency set for reproducible builds.
- **No transitive *version* resolution.** Path resolution is transitive, but
  since there are no versions there is no diamond/conflict resolution.
- **No build cache / incremental build.** Every build re-lexes/re-parses/re-checks
  every module from source; no artifact caching keyed on content.
- **No `add`/`remove`/`update` dependency commands, no publish flow.**

### 1.0-required (prioritized)
1. **Nothing structurally new is required for 1.0.** The local-path project system
   spans "any program you can write on one machine," which matches the 1.0 goal.
   The honest posture (no fake registry) is the right call.
2. **Manifest `version` field + schema validation with a clear diagnostic**
   — **DONE.** `ProjectManifest` now carries an optional semver-shaped `version`
   (`MAJOR.MINOR.PATCH` + optional `-<prerelease>`), validated in
   `crates/lullaby_loader/src/manifest.rs` (`validate_version`) and rejected via
   `L0343` when malformed. Optional and backward-compatible (manifests without a
   `version` still load); `lullaby new` scaffolds `"0.1.0"`. This future-proofs
   `lullaby.json` so adding a registry later is non-breaking.
3. **Document the manifest surface** (fields, resolution order, diagnostics) as a
   stable, versioned contract — **DONE.** `documents/modules_design.md` now
   documents the full schema (including `version`) and states the manifest is a
   stable, forward-compatible 1.0 contract.

### Post-1.0 nice-to-have
- Registry + remote fetch, semver constraints + resolution, lockfiles, build
  caching / incremental compilation, `add`/`remove`/`update`/`publish`. These are
  a whole workstream and are correctly deferred; a build cache is the first one
  to want (performance), the registry the largest.

---

## 3. Test runner (`lullaby test`, `main.rs::test_file`)

**Maturity: Adequate-minus.** It is a real, working test runner — not a stub —
and it already handles multi-file projects. The gaps are ergonomic (filtering,
backend coverage), each small, none deep.

### What is implemented (verified)
- **Discovery by convention:** every top-level function named `test_*` that takes
  zero parameters, is non-generic, and returns `void`/`i64`/`bool`. Ineligible
  `test_*` functions are **skipped with a printed reason** (params / generic /
  wrong return type) — a nice honesty touch that keeps the surface discoverable.
- **Project-aware:** compiles in `SourceMode::Library` via the shared `compile`
  path, so it runs against a single `.lby` file **or** a whole project directory
  (`resolve_target` → `load_library_project`), no `main` required.
- **Execution + reporting:** runs each test on the **AST interpreter**; a test
  passes if it returns without a runtime error and fails on any runtime error
  (e.g. `assert(false)`). Prints `PASS`/`FAIL` per test, a `N passed, M failed`
  summary, and a non-zero exit on failure. `--verbose` prints a traceback per
  failure.
- Covered by a runnable fixture (`examples/valid/tests_demo/`, "4 passed").

### What is missing
- **No test filtering / selection** (no name filter, no "run just this test").
  The most-missed feature the moment a suite has more than a handful of tests.
- **Single backend only:** tests run on the AST interpreter, so they never
  exercise the IR/bytecode/**native**/WASM backends. For a compiler, "the test
  passes on the tree-walker" is weaker than "the test passes on the backend you
  ship." No `--backend` for `test`.
- **Assertion surface is just `assert(bool)`** — no equality/`assert_eq`-style
  helper, so failure messages are generic ("assert failed") rather than
  "expected X, got Y." This is a stdlib/builtin question but it is what makes a
  test runner pleasant.
- **No structured output** (no TAP/JSON), **no timing per test, no parallelism,
  no setup/teardown, no expected-failure/`should_panic`** form.
- Return-type restriction to `void`/`i64`/`bool` is reasonable but undocumented as
  a contract.

### 1.0-required (prioritized)
1. **Test filtering** (`lullaby test <path> <name-substring>` or `--filter`).
   Small, high value.
2. **Backend selection for tests** (`--backend`, and ideally a "run on the native
   backend too" mode). A compiler's test runner that only runs the interpreter
   undersells the compiler; running the same suite on native is the real
   confidence signal. Medium effort, reuses existing backend plumbing.
3. **A minimal richer assertion** (`assert_eq`-style with a useful failure
   message). Ties into B2 stdlib work.

### Post-1.0 nice-to-have
- Structured/TAP/JSON output, parallel execution, per-test timing, setup/teardown
  fixtures, `should_panic`/expected-failure, coverage. Standard maturity items,
  none blocking.

---

## 4. Debug info (native codegen — `native_object*.rs`)

**Maturity: Incomplete (tracked; native-codegen work, not fixed by this audit).**

### What exists (verified)
- **`lullaby native --debug` / `-g`** adds a CodeView **`.debug$S`** section to the
  emitted object. The flag is native-only and rejected on other commands.
- **Coverage is Windows-only and function-granularity:** one source-line record
  per function **at its entry offset** (`DebugFunctionLine { line:
  function.line }`, the 1-based declaration line from `BytecodeFunction.span`).
  Helper/stub symbols carry no line. Without `--debug` the object bytes are
  byte-for-byte unchanged (snapshot-safe).
- So a Windows debugger can map a function's entry to its source line
  (breakpoint-on-function, function names in stack traces). There is **no**
  per-statement line table, **no** local-variable/type info, **no** frame/CFI
  info beyond what the ABI implies.

### What is missing
- ~~**DWARF for Linux/macOS — entirely absent.**~~ **RESOLVED 2026-07-17.**
  `--debug`/`-g` now emits DWARF `.debug_line` + `.debug_info` (with per-function
  `DW_TAG_subprogram` DIEs) + `.debug_abbrev` on the ELF and Mach-O targets, at
  the same function granularity CodeView gives COFF. `gdb`/`lldb` can break at a
  function and show its `.lby` line.
- **Per-statement line tables** on *all* targets (only function-entry lines
  exist), so you cannot step line-by-line on any OS.
- **Variable/parameter/type debug info** (locals in the debugger) on all targets.
- **Frame/CFI info** (`.eh_frame`/`.debug_frame`) on all targets — the largest
  remaining gap for a real debugging session: a debugger cannot reliably unwind a
  Lullaby stack.

### 1.0-required (prioritized)
1. ~~**DWARF line table + function-level subprogram DIEs for ELF + Mach-O.**~~
   **SHIPPED 2026-07-17** at function granularity — `gdb`/`lldb` now break on a
   function and show its source line on Linux/macOS, at parity with Windows
   CodeView. Verified by decoding the emitted DWARF with `gimli` (an independent
   reader) and once end-to-end through `rust-lld` → linked binary → `gimli`; a
   live debugger session is deferred to the Phase 9 cross-platform CI.
2. **Per-statement line records** on all three targets, so stepping is
   line-by-line rather than function-by-function. Now the top debug-info gap.

### Post-1.0 nice-to-have
- Full local-variable/type DWARF (DWARF DIEs for locals), CFI/unwind tables for
  robust backtraces, and per-statement CodeView line records on Windows to match
  the DWARF line-table granularity. These make debugging *good*; 1.0 needs it
  *present and correct* on all three OSes first.

---

## Consolidated B3 backlog (what actually gates "stable")

**Must-do for 1.0-stable (roughly ordered by leverage):**
1. ~~**DWARF line tables (ELF + Mach-O)**~~ — **DONE 2026-07-17** at function
   granularity (`.debug_line`/`.debug_info`/`.debug_abbrev`, `gimli`-verified).
   Per-statement records, variables/types, and CFI remain (post-1.0 below).
2. **A shipped editor client** (VS Code extension + grammar) fronting the LSP.
3. **LSP completion** — **DONE.** `textDocument/completion` offers keywords,
   in-file declarations + enclosing-function locals/parameters, and imported
   `pub` symbols (cross-file resolution via the module loader). Member/`.`
   completion and signature help remain post-1.0.
4. **Test filtering** + **test backend selection** (run the suite on native, not
   just the AST interpreter).
5. **Manifest `version` field + schema validation**, and document the manifest as
   a stable contract — **DONE** (optional semver `version` validated via `L0343`,
   `lullaby new` default `"0.1.0"`, schema documented as a 1.0 contract in
   `modules_design.md`).
6. **Document symbols** in the LSP (cheap outline).

**Correctly post-1.0 (do not block stable):** package registry / remote deps /
semver / lockfiles / build cache; LSP rename / find-references / signature help /
semantic tokens / incremental sync; structured test output / parallelism /
fixtures; full variable-level DWARF + CFI.

**Bottom line:** the package/project system is the closest to stable-grade and
appropriately scoped. The LSP and test runner are real and correct but need a
short, well-defined list of additions (an editor client, completion, cross-file,
test filtering/backends) to feel 1.0-credible. Debug info was the genuine
engineering gap and its load-bearing half is now closed: source-line debug info
exists on all three OSes (CodeView on Windows, DWARF on Linux/macOS). What
remains there is depth, not presence — per-statement lines, variables/types, and
CFI — and only per-statement lines is a plausible 1.0 ask.
