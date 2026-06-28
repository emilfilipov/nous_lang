# Post-Alpha Roadmap

This document turns the current post-Alpha 1 backlog into repo-owned execution guidance. The ClickUp backlog mirrors this sequence, but this file is the stable source to consult before starting broad implementation work.

## 1. Memory-Aware IR Operations

Goal: make allocation, loads, stores, deallocation, bounds checks, future region operations, copy operations, and cleanup ordering visible to optimizers, bytecode, and later native code generation.

Current increment:

- `crates/lullaby_ir` exposes `analyze_memory_operations` for typed IR modules.
- The analysis reports current Alpha 1 heap-slot operations: `alloc`, `load`, `store`, and `dealloc`.
- The analysis also reports array indexing as a bounds-checked access.
- Each reported operation carries safety metadata for live-resource requirements, bounds-check requirements, memory mutation, cleanup role, and unsafe-boundary handling.
- The optimizer uses the same runtime-checking barrier model for memory calls and bounds-checked indexing, so future passes do not need to rediscover side effects ad hoc.

Remaining work:

- Add first-class IR forms or lowering metadata for `region_create`, `region_resize`, copy operations, and compiler-inserted cleanup.
- Preserve memory operation metadata through bytecode artifact inspection once the bytecode schema is ready for a compatibility bump.
- Add backend snapshot tests that assert memory operation metadata for representative Alpha 1 and planned region/copy fixtures.
- Use this metadata as a prerequisite for native backend lowering and alias analysis.

## 2. Static Offline Documentation Generator

Goal: replace the hand-authored-only offline docs process with a deterministic generator that can build a self-contained HTML bundle from canonical Markdown and repository examples.

Current increment:

- `offline_docs/generate_offline_docs.py` builds a standalone HTML file from canonical Markdown using only the Python standard library.
- The first generated source set includes the project overview, core language rules, Alpha 1 language surface, diagnostic registry, release notes, and this roadmap.
- The default output is `target/offline_docs/index.html`, keeping generated artifacts out of source control.
- The generated output includes fixture-backed examples and is verified with `python offline_docs/verify_offline_docs.py target/offline_docs/index.html --profile generated`.
- `scripts/verify_release.ps1` now builds and verifies generated docs before packaging.

Acceptance path:

- Expand the generator to cover examples, CLI command reference, current limitations, and installation/package content.
- Expand generated profile coverage until it reaches parity with the current hand-authored entry page.
- Switch packaging from copying `offline_docs/index.html` to invoking the generator once generated parity is proven.

## 3. Platform-Agnostic Build Orchestration

Goal: define one release orchestration path that can build and verify the toolchain on Windows, Linux, and macOS without replacing Cargo as the Rust build driver.

Recommended sequence:

- Keep Cargo as the compiler/runtime build engine.
- Use `scripts/package_portable.py` as the initial cross-platform release driver for target tags, output layout, generated docs, examples, checksums, manifests, archives, and host-compatible smoke tests.
- Preserve `scripts/package_windows_portable.ps1` as the Windows Alpha 1 implementation while `scripts/package_portable.py` reaches parity across non-Windows hosts.
- Add CI jobs only after the local release driver is deterministic.

Minimum target matrix:

- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`

## 4. Installer Packaging

Goal: move from a Windows-first portable archive to a packaging plan that can produce predictable user-facing artifacts for all supported platforms.

Recommended sequence:

- Keep portable archives as the first cross-platform package type.
- Put the CLI under `bin/`, generated offline docs under `docs/`, examples under `examples/`, release notes and `MANIFEST.json` at package root, and checksums next to archives on every platform.
- Add platform-specific installers only after portable archive verification is stable.
- Treat PATH setup as optional and user-scoped unless the installer format has an explicit consent step.

Planned package types:

- Windows: portable zip first, MSI or NSIS later.
- Linux: tarball first, Debian/RPM packages later.
- macOS: tarball first, signed package or app bundle later if the distribution model requires it.

## 5. Native Code Generation Roadmap

Goal: reach native code generation only after the typed IR, bytecode contract, memory effects, diagnostics, and release verification are strong enough to make backend failures actionable.

Recommended sequence:

- Freeze the typed IR contract for the Alpha 1 subset plus memory-effect metadata.
- Add backend snapshots for IR and bytecode before introducing machine-code output.
- Define calling convention, stack-frame, value layout, pointer, array, and resource-cleanup rules.
- Prototype object emission for one host target before adding a linker workflow.
- Require native backend diagnostics to use the shared `N####` diagnostic model.

Native backend non-goals for the next checkpoint:

- Do not bypass the existing AST runtime or bytecode VM.
- Do not add speculative optimizations that are not justified by the typed IR contract.
- Do not introduce a new build system unless Cargo plus scripts cannot support the verified release path.
