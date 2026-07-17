# Lullaby — CLAUDE.md

This repository defines and will implement Lullaby, a compiled systems programming language optimized for concise syntax, strong typing, memory safety, and LLM-friendly source generation.

## First Moves

- Read this file first.
- Read `documents/repository_map.md` before changing code or documentation. Use it to locate the relevant subsystem and update it whenever files, directories, commands, tests, or document responsibilities change.
- For language behavior, start with `documents/language_specification.md` and `documents/core_language_rules.md`.
- For implementation sequencing, start with `documents/implementation_plan.md` and the ClickUp backlog.
- Check the active tool surface before promising MCP, ClickUp, GitHub, Playwright, Context7, or other connector work. Tool availability is session-specific.

## Project Direction

- Implementation language: Rust, unless the owner explicitly changes this decision.
- The language's canonical name is **Lullaby**. The `nous_lang` repository directory and any legacy "Nous"/"Nous Lang" naming are historical only and must never appear as the language's name in code, documents, diagnostics, ClickUp tickets, or user-facing material. "Nous" was an evaluated rename candidate that was rejected in favor of keeping Lullaby — see `documents/name_research.md`.
- Canonical source extension: `.lby` until the language specification is intentionally changed.
- Keep the syntax indentation-only. Curly braces are not block delimiters, and semicolons are not statement terminators.
- The frontend and semantic pipeline came first; native code generation, the WASM backend, and the full 1.0 primitive set are now in active development. Target 1.0 as defined in `documents/roadmap_1_0.md` and the ClickUp `Lullaby` folder: technically capable of expressing any program (a spanning set of primitives) plus an easy-to-install, branded toolchain, with specialized modules layered on after 1.0.
- Documentation is the hosted online website, maintained separately; there is no offline/bundled HTML doc artifact.

## Production Quality Standard

- Everything is written to production quality. No aspect of the language — lexer, parser, semantics, runtime, IR, optimizer, backends (AST/IR/bytecode/native/WASM), CLI, installers, packaging, or documentation — may be committed in a "good enough for now", placeholder, stubbed, mocked, or temporary form.
- No `todo!()`/`unimplemented!()`, no `unreachable!()` used to dodge a real case, no silent partial handling, no hardcoded/fake results standing in for real logic, no "TODO: handle later" left in committed code. If a case can occur, it is handled correctly or rejected with a clear `L####` diagnostic.
- Every feature ships complete: correct edge-case and error handling, full parity across every backend it touches, deterministic tests (including negative/failure cases), and updated Markdown documentation. A feature that is 90% done is not done.
- Placeholder scaffolding is acceptable only transiently inside unmerged work-in-progress and must be replaced with the real implementation before the change is committed. Prefer correctness and completeness over speed of landing.
- When a task is genuinely large, split it into smaller production-complete increments — never land a shallow version of the whole.

## Operating Model: Orchestrate, Don't Implement

This is a hard rule for the primary agent, effective now. The primary agent is the **orchestrator only** — it does not implement, and it does not perform verification/review work by hand. The owner plans overall architecture with it; **sub-agents perform the actual development in parallel, and other (reviewer) sub-agents verify that development**, so the primary stays free to decompose, dispatch, decide, and plan with the owner. The primary's own hands-on work is limited to orchestration: task decomposition, conflict-free partitioning, dispatching agents, merge decisions, and small integration glue.

- **Delegate implementation by default.** Decompose work into small, well-scoped tasks with explicit acceptance criteria and dispatch each to a sub-agent (use an isolated `worktree` for any task that edits code). Do not write substantial feature or implementation code directly. Reserve direct edits for orchestration, merge decisions, small integration glue, and trivial fixes not worth spinning up an agent for. Do not do verification/review by hand — delegate it (below).
- **Run many agents in parallel whenever possible, without collisions.** Keep as many agents working concurrently as there is disjoint work for (target ~5–10 when it exists), partitioned so no two concurrent agents edit the same file. Sequence tasks that would touch the same files — queue the later one until the earlier merges. Append-only shared docs are acceptable low-risk overlap. Be mindful of local build contention; ~4–6 heavy Rust-building agents at once is a practical ceiling. Default to parallelizing; only serialize when files genuinely collide.
- **Delegate review to a reviewer sub-agent — never verify by hand.** A sub-agent's "done" is a claim to verify, not a fact, but the primary does NOT run the build/test grind itself. For each completed branch, dispatch a dedicated **reviewer/verifier sub-agent** that checks out the branch, builds it, runs `cargo test --all` + `cargo clippy --all-targets --all-features -- -D warnings` + the change's specific fixtures/fuzzer, and checks it against the Production Quality Standard and Definition of Done (correct-or-unchanged codegen, deterministic tests including negative cases, docs updated, no placeholders/stubs), returning a concise **PASS/FAIL verdict with specifics**. The primary merges on PASS and, on FAIL, hands the reviewer's feedback to an implementation sub-agent to redo. Never merge on the implementer's unverified claim; never spend the primary's own cycles building or testing.
- **Documentation is part of delegated review.** The reviewer must confirm the change includes the doc updates it requires (relevant Markdown under `documents/` and `documents/repository_map.md`). If docs were skipped or under-done, do NOT hand-fix inline — **dispatch a dedicated documentation sub-agent for the doc work AND keep the original agent moving on its next task**, so documentation runs in parallel and never serializes feature work. Only trivial one-line doc touch-ups are worth doing yourself.
- **Drive the pipeline autonomously.** As branches pass review, independently dispatch the next queued or derived tasks without waiting for owner sign-off. Escalate to the owner only for genuinely architectural decisions or real forks — not for per-task approval.
- **Own orchestration, delegate execution and verification.** The primary remains responsible for task decomposition, dependency ordering, conflict-free partitioning, the merge decision, and keeping docs current — but it **commissions verification through reviewer sub-agents rather than performing it**, and delegates doc updates to doc sub-agents. Merging is a lightweight orchestration act and stays with the primary as the integration/correctness gate; the heavy verification that precedes each merge is delegated.
- **Exceptions (kept minimal).** A quick investigation/measurement needed to make an orchestration decision (e.g. verifying a claim before trusting it, sizing a task, checking for collisions), answering the owner's questions, and one-line trivial fixes may be done directly. Verifying a sub-agent's deliverable is NOT an exception — it goes to a reviewer sub-agent. When work is too small or too tightly coupled to hand off cleanly, use judgment — but the default is always to delegate, both development and review.

## Core Documentation Map

- `documents/core_language_rules.md`: canonical source extension, indentation-only scope, forbidden block delimiters, canonical block examples, and global language rules.
- `documents/language_specification.md`: full language overview, philosophy, core language components, syntax reference, examples, and roadmap.
- `documents/implementation_plan.md`: compiler and installer implementation epics, dependency order, and delivery plan.
- `documents/lullaby_syntax_design.md`: syntax philosophy, declarations, control forms, functions, data structures, operators, naming, comments, examples, and token-efficiency goals.
- `documents/lullaby_type_system.md`: primitive/composite/reference/function types, inference rules, type safety, generics, aliases, and OS-development type needs.
- `documents/lullaby_memory_management.md`: regions, stack/heap model, lifetime tracking, GC hooks, memory safety, runtime API, and kernel memory examples.
- `documents/lullaby_control_structures.md`: conditionals, loops, switch, error-control forms, coroutine syntax, operators, and control-flow examples.
- `documents/lullaby_input_output.md`: file I/O, streams, memory-mapped files, threads, processes, async operations, IPC, sockets, and I/O performance strategy.
- `documents/lullaby_error_handling.md`: error token model, compile-time/runtime/resource/type error families, throw/catch/recovery behavior, and diagnostic integration.
- `documents/lullaby_compilation_architecture.md`: tokenizer, semantic analysis, IR, optimization, code generation, linking, binary output, and compiler performance.
- `documents/repository_map.md`: living map of the repository. Update it with every material source, docs, test, command, or layout change.

## Development Workflow

- Convert non-trivial implementation plans into ClickUp tasks before large changes. Use the existing `Lullaby` ClickUp folder/backlog when the connector is available.
- Keep ClickUp current as implementation work progresses. When tasks are started, completed, materially changed, or blocked, update the relevant ClickUp ticket status when the tool supports it; otherwise add a concise task comment with the commit, verification, and remaining work.
- Work in small, reviewable increments. Each commit should describe one coherent change.
- **Keep source files small and modular.** Hard cap **~1500 lines** per non-test source file; soft target **~800**. Pure test modules get a looser **~2500** cap. Split larger files into cohesive submodules along natural seams. Small files are a hard requirement, not a nicety: they let multiple sub-agents edit disjoint files in parallel without merge collisions, and keep context-loading and review fast. This is **forward-looking**: do not create a new file over the cap, and do not grow a file further past it — when a change would push a file past the cap, split it first (or as part of that change). The **existing** oversized files are a prioritized backlog, not an emergency — worked worst-first (the 3000+ line files first) per `documents/large_file_split_plan.md`, one split at a time, only when the target file has no agent editing it. Splits are **behavior-preserving code moves gated by the full test suite** (`cargo test --all` green), reviewed like any change. Reasonable exceptions (a single cohesive match/dispatch, a generated table) may exceed the cap with a brief justifying comment.
- Keep source, tests, and docs moving together. If implementation changes behavior, update the relevant core document and `documents/repository_map.md` in the same commit.
- Use sub agents for parallel development, documentation cleanup, and maintenance tasks when work can be split into clear, non-overlapping ownership areas. The primary agent is allowed to use multiple sub-agents, and to use multiple rounds of sub-agents at any moment, whenever that helps move the work forward safely. Keep ownership clear: the primary agent remains responsible for integrating sub-agent output, checking accuracy, keeping the Markdown documentation current, and running the required verification.
- Avoid broad rewrites unless they remove real duplication, resolve contradictions, or unblock implementation.
- Preserve user work. If the tree is dirty, inspect changes before editing and do not revert unrelated files.
- Prefer local repo patterns once code exists. Do not introduce new frameworks or build systems without recording the decision in docs.

## MCP And Connector Usage

- ClickUp: use for implementation planning and granular task tracking. Confirm hierarchy first with `clickup_get_workspace_hierarchy`; the connector may require `max_depth` as string values `"0"`, `"1"`, or `"2"` even when a schema reports numbers.
- ClickUp updates are part of delivery, not optional bookkeeping. If the connector only exposes comments and not status updates, add progress comments to the affected tickets and say that status changes could not be made through the available tool surface.
- GitHub: use for repository creation, pushing, issue/PR inspection, and remote verification when available. If using `gh`, verify authentication with `gh auth status` without printing tokens.
- Sequential thinking: use for broad architecture plans when available.
- Mem0 (memory MCP): use it every session. At the start of non-trivial work, search Mem0 for relevant project/user context (`search_memories`); when you learn a durable fact about the project, the language design, Emil's preferences, or a decision/rationale, store it (`add_memory`, scoped to user_id `emillfilipov@gmail.com`). Keep it current — update or supersede stale memories rather than duplicating. Treat recalled memories as background context, and verify any file/flag/code they name still exists before acting on it.
- Memory/retrieval: use before broad, ambiguous, or workflow-sensitive changes.
- Context7 or official docs: use for current library/framework/API documentation when implementation depends on external tools.
- Playwright: use for browser-based validation only if a frontend or web tool is introduced.
- If an MCP server is configured but not callable in the current session, say so clearly and fall back to local commands or direct APIs.

## Testing Expectations

Until implementation code exists:

- Run documentation checks after doc/layout changes:
  - `rg -n "DELETED|Clean start|compiled_programming_languages_overview|programming_paradigms|top_programming_languages|language_comparison_guide" documents`
  - Verify all Markdown references point to real files.
  - Run `git diff --check`.

Once Rust code exists:

- Use `cargo fmt --check` for formatting.
- Use `cargo clippy --all-targets --all-features -- -D warnings` for linting unless a narrower documented command replaces it.
- Use `cargo test --all` for unit and integration tests.
- **Gate rustdoc after any change that touches doc comments — `cargo clippy` does NOT check intra-doc links:** `$env:RUSTDOCFLAGS="-D warnings"; cargo doc --no-deps --document-private-items`. The rustdoc lints (`broken_intra_doc_links`, `private_intra_doc_links`, `redundant_explicit_links`, `invalid_html_tags`) are **not** clippy lints, so `cargo clippy -- -D warnings` passes with a broken link sitting in the tree — measured: a deliberately broken `[`crate::resolve_module_slots`]` link left clippy at exit **0** and failed only this command (exit **101**). `--document-private-items` is **required**, not a nicety: without it rustdoc never resolves links on private items, and plain `cargo doc --no-deps` also passed on that same broken link (exit **0**). This blindness is exactly what let a stale link to a nonexistent `array_element_native_type` (renamed to `narrow_array_element`) survive. Fix the link rather than silencing it — never add `#[allow(rustdoc::…)]` or `--cap-lints` to quiet this gate. Two recurring fixes: link a renamed/moved item by its **real path** (items behind `#[path]` modules are named through their `mod` alias and any `pub(crate) use …::*` re-export, e.g. `crate::native_object::expand_method_instances`), and wrap generic/placeholder spellings in backticks (`` `array<string>` ``, `` `<src>` ``) so rustdoc stops parsing the angle brackets as HTML. A link from **public** docs to a `pub(crate)`/private item is an error regardless — backtick it instead.
- **Also lint the Linux target when a change touches `cfg(unix)`/`cfg(windows)` code:** `cargo clippy --target x86_64-unknown-linux-gnu --all-targets --all-features -- -D warnings` (~50s; `rustup target add x86_64-unknown-linux-gnu` once). There is **no CI**, so every gate runs on a Windows host — which means a `cfg(unix)` branch is compiled out and *structurally invisible* to the normal gates. This has already hidden a real defect (an `unused doc comment` that fails `-D warnings` on Linux only). It is a **compile** check, not an execution check: POSIX runtime behavior still ships verified by inspection alone until a Linux job exists.
- **Read cargo's real exit code.** Do NOT pipe cargo and check `$?` in bash — that reports the *pipe's* status, and bash masks exit codes to 8 bits (a genuine exe exit of 1234 reads as 210). Use PowerShell `$LASTEXITCODE`, or capture cargo's status directly. This has produced false "green" readings for several agents. The same trap has a PowerShell form: `Select-Object -First N` closes the pipeline early, which **terminates cargo**, so `$LASTEXITCODE` reports the broken pipe (a `-1` alongside every test printing `ok`). Capture the output to a variable first, then read the exit code.
- **Beware stale binaries when verifying.** `Copy-Item` preserves timestamps and `git checkout --` restores committed content, so stashing/restoring a file mid-verification can make cargo silently skip the rebuild ("Finished in 0.09s") and leave you measuring the *old* binary. Force the rebuild and assert your edit actually landed before trusting any result — this has caused both false negatives ("the attack doesn't reproduce") and false positives.
- **A test that can pass without running is worse than no test.** When adding a generator, oracle, or harness, prove it has teeth: inject the bug it is meant to catch, watch it fail, revert. Toolchain-gated tests that `return` early on a missing dependency must report what they executed (a differential fuzzer here once passed having run nothing). **A test can also stop testing what it claims while still passing**: tightening one layer can make a lower layer's guard unreachable, so the test now proves only that the frontend rejects its programs — green throughout, teeth gone (this happened to `gen_alloc_cast_launder_program` when `ptr_cast` was fixed). After changing a rule, re-prove the teeth of every test that guards a *different* layer against it; do not assume a passing test still means what it meant before your change.
- **Do not misread parallel-load flakiness as a defect — or a defect as flakiness.** CLI tests still write generated `.exe`s to **fixed `%TEMP%` paths** (`lullaby_*.exe`), so concurrent agents produce `os error 32 ... used by another process`, and the TCP fixtures fail on timing under many cargo processes. Re-run the affected suite **in isolation** before calling a failure real — and never dismiss one as "flaky" without proving it in isolation. The `ScratchDir` work fixed the native fuzz tests only; the remaining fixed-path writers are a known open bug, not an accepted cost.
- Keep fixture-based tests for lexer/parser/type-checker/diagnostics deterministic.
- Add integration tests for end-to-end `.lby` source through parse, semantic validation, runtime/backend execution, stdout/stderr capture, and exit code.
- Do not call work complete until relevant tests and documentation checks have run or the reason they could not run is documented.

## Git And GitHub Rules

- Commit regularly at meaningful checkpoints.
- Use clear commit messages:
  - `docs: organize language specification`
  - `compiler: add indentation token scanner`
  - `tests: add parser fixtures for invalid blocks`
- Include the why in commit bodies when behavior, architecture, or workflow changes.
- Push regularly to GitHub after coherent commits, especially after creating or updating major docs, implementation milestones, or test infrastructure.
- Do not commit secrets, tokens, local caches, build outputs, editor state, or generated artifacts unless explicitly intended.

## Documentation Rules

- Documentation is part of the product. Keep it current with every implementation update, concept change, command change, test change, or layout change.
- If code changes syntax, typing, memory behavior, diagnostics, CLI behavior, runtime semantics, or build/test commands, update the matching document under `documents/`.
- User-facing documentation is the hosted online website, maintained separately from this repository; there is no offline/bundled HTML doc artifact to update or verify here.
- Treat docs examples as executable fixtures when practical. If an example is not yet supported by the current compiler/runtime, mark it clearly as planned or future syntax.
- Keep `documents/core_language_rules.md` as the single source for repeated canonical rules. Do not copy that block into every subsystem document.
- Keep `documents/repository_map.md` current and use it as the first navigation aid.
- If documents duplicate substantial content, consolidate it into one canonical file and replace duplicates with references.
- Prefer concise, specific documentation over generic prose. Examples should be valid for the current language design or explicitly marked as unresolved.

## Definition Of Done

- The requested change is implemented or the blocker is explicit.
- Relevant docs are updated.
- `documents/repository_map.md` is still accurate.
- Duplicate Markdown content has not been reintroduced.
- Tests/checks relevant to the change have run.
- Changes are committed with a useful message and pushed when requested or when reaching a coherent milestone.
- Relevant ClickUp tasks are updated with status changes or comments describing progress, verification, blockers, and follow-up work.
