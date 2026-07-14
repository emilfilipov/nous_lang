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
- **Keep source files small and modular.** Hard cap **~1500 lines** per source file; soft target **~800**. Split larger files into cohesive submodules along natural seams. Small files are a hard requirement, not a nicety: they let multiple sub-agents edit disjoint files in parallel without merge collisions, and keep context-loading and review fast. Reasonable exceptions — a single cohesive match/dispatch, a generated table, or a test module — may exceed the cap with a brief comment justifying why. When a change would push a file well past the cap, split the file first (or as part of that change).
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
