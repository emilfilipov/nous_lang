# Language Rename Research

Date researched: 2026-06-28.

The language's canonical name is **Lullaby**. This document records the research into renaming it (with "Nous"/"Nous Lang" as the leading candidate) and the final decision to **keep Lullaby** — "Nous" was evaluated and rejected. It is not a legal clearance report. Before any future public rename, run a proper trademark/domain/package-name clearance pass.

## Naming Constraints

The project is a compiled systems programming language optimized for:

- concise indentation-only syntax;
- strong typing and memory-safety foundations;
- OS, system-service, and user-space application development;
- LLM-friendly source generation, learning, diagnostics, and token cost.

The name should be short, easy to pronounce, easy to search, plausible as a command name, and not already strongly associated with another programming language or developer tool.

## Existing Nous Collision

`Nous` / `Nous Lang` is already occupied in the language/tooling space:

- PyPI has `nous-lang`, described as "NOUS -- The Living Language" and a programming language for agentic AI systems: https://pypi.org/project/nous-lang/5.1.0/
- The public website `nous-lang.org` presents NOUS as a language/compliance infrastructure for agentic AI systems: https://nous-lang.org/
- The GitHub project metadata points to `contrario/nous` and describes "NOUS -- The Living Language for Agentic AI Systems": https://github.com/contrario/nous/blob/main/pyproject.toml

Conclusion: the old name should be retired.

## Earlier Candidate Assessment

| Candidate | Fit | Collision Risk | Assessment |
| :--- | :--- | :--- | :--- |
| **Seren** | Short, calm, memorable; can imply clarity/star/navigation. Works as `seren`, `serenc`, or `seren-lang`. | Medium-low. No obvious programming-language collision in web search. `cargo search seren` has a small placeholder-looking crate; PyPI has `seren 0.0.1`; npm package name appears available. There are non-language products such as Seren case-management software and SerenDB. | Earlier leading fallback before Lullaby was selected. Strong enough brand, relatively clean in developer-language space, not too mystical. |
| **Syra** | Very short and distinctive. Works as `syra`, `syrac`, or `syra-lang`. | Medium. No obvious "Syra Lang" programming-language collision in web search. `cargo search syra` has a cryptography crate; npm has `syra`; PyPI appears free. The word is less meaningful and has unrelated uses. | Good fallback if we want a coined, compact name. Needs stronger story. |
| **Sorin** | Human-name feel, concise, pronounceable. Works as `sorin` or `sorin-lang`. | Medium. No obvious language collision; npm appears free. PyPI has `sorin`; common personal name and PL professor search noise. | Acceptable, but weaker brand semantics for an AI-optimized systems language. |
| **Vireo** | Distinctive, lively, pronounceable; a bird name. | Medium-high. No exact programming language found, but Twitter/X has `vireo` video-processing library, PyPI has many `vireo` releases, npm has `vireo`, and there are older technical products with the name. | Usable only if we accept package-name friction. |
| **Neris** | Short, crisp, can feel systems-oriented. | Medium. No obvious developer-language collision, but NERIS is a major emergency-reporting/government system acronym. | Possible but less ideal; acronym collision is strong outside dev. |
| **Seer** / **Seer Lang** | Excellent semantic fit: insight, prediction, AI readability. | High. Sentry has Seer as an AI debugging agent; `seer` exists as an npm devtool package; crates include `seer`, `seer-cli`, `seer-core`, and `seer-z3`; SeeR is an older bytecode script compiler/interpreter; SEER is also an MLIR/HLS compiler-research tool; Seer is a GDB GUI frontend. | **Do not choose unless we deliberately accept heavy developer-tool collision.** |
| **Orison** | Good meaning and tone; memorable. | High. An active GitHub project already describes Orison as an agent-native programming language with a Rust compiler/toolchain, and recent posts discuss lowering Orison AST to LLVM IR. | Avoid. Too close to our domain. |
| **Vela** | Short, strong, astronomical. | Very high. `vela-lang.org` is already a memory-safe systems programming language. | Avoid. |
| **Halcyon** | Polished, positive. | Very high. `halcyon-lang.dev` is already a programming language. | Avoid. |
| **Mimir** | Strong knowledge/mythic fit. | High. MimIR is an active compiler/intermediate-representation project; Mimir also has IDE/classroom/tooling uses. | Avoid. |
| **Aster** | Short, star-related. | High. ASTER is already used for scripting/testing and LLM unit-test-generation tooling; Code_Aster also has its own command language. | Avoid. |
| **Oriel** | Elegant and distinctive. | High. Oriel is an existing 1991 graphics-based scripting language. | Avoid. |
| **Augur** | Similar semantic field to Seer. | High. Augur is a probabilistic programming language/compiler and other developer/security tooling. | Avoid. |
| **Scry** | Strong semantic fit. | High. Scry is a Crystal language server and there are current "Scry programming language" references. | Avoid. |

## Current Decision

Use **Lullaby** as the project name.

Suggested public spelling:

- Language: **Lullaby**
- Formal: **Lullaby**
- Compiler/CLI: `lullaby`
- Repository/package possibilities: `lullaby` or `lullaby-lang`
- File extension: `.lby` (canonical and only accepted extension).

The long `.lullaby` extension was used during early development because it is clear in
examples and searchable in docs. It has since been shortened to the canonical
`.lby` (following the follow-up decision below and the convention of short
source extensions such as `.rs`, `.py`, and `.rb`). `.lby` is distinctive,
clearly maps to Lullaby, and has no meaningful collision in the source-language
space. The `.lullaby` extension has been fully retired; the lexer no longer
accepts it.

## Why Not Seer

Seer is semantically appealing, but the search surface is already crowded in exactly the wrong places:

- Sentry Seer is an AI debugger for production issues: https://sentry.io/product/seer/
- SeeR is listed as a script compiler/interpreter that compiles C/C++-style scripts to bytecode: https://directory.fsf.org/wiki/SeeR
- SEER is an HLS/MLIR compiler-research tool: https://arxiv.org/abs/2308.07654
- Seer is a GUI frontend to GDB: https://github.com/epasveer/seer

That makes "Seer Lang" harder to search for and easier to confuse with existing developer tools.

## Follow-Up Clearance

Before publishing broadly:

1. Check USPTO/EUIPO/WIPO trademark records for Lullaby and likely package names.
2. Check domain options such as `lullaby-lang.org`, `lullabylang.org`, and `lullaby.dev`.
3. Reserve or verify package names in crates.io, npm, PyPI, GitHub, and VS Code extension marketplace.
4. Resolved: the canonical extension is `.lby`; the original `.lullaby` extension has been fully retired.
5. Rename external project surfaces that live outside this repository, including the GitHub repository and ClickUp folder/list references.
