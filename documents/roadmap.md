# Roadmap

This document turns the current development backlog into repo-owned execution guidance. The ClickUp backlog mirrors this sequence, but this file is the stable source to consult before starting broad implementation work.

## 1. Memory-Aware IR Operations

Goal: make allocation, loads, stores, deallocation, bounds checks, future region operations, copy operations, and cleanup ordering visible to optimizers, bytecode, and later native code generation.

Current increment:

- `crates/lullaby_ir` exposes `analyze_memory_operations` for typed IR modules.
- The analysis reports current heap-slot operations: `alloc`, `load`, `store`, and `dealloc`.
- The analysis also reports array indexing as a bounds-checked access.
- Each reported operation carries artifact-order sequence metadata plus safety metadata for live-resource requirements, bounds-check requirements, memory mutation, cleanup role, and unsafe-boundary handling.
- The optimizer uses the same runtime-checking barrier model for memory calls and bounds-checked indexing, so future passes do not need to rediscover side effects ad hoc.
- Version 5 `.lbc` artifacts preserve bytecode-level `memory_operations` with stable sequence numbers, artifact decoding validates those operations against module instructions, and `lullaby inspect` reports memory operation counts/details.
- Backend snapshot tests under `crates/lullaby_ir/tests/memory_snapshots.rs` pin the current bytecode memory metadata for heap-slot operations and bounds-checked array indexing.

Delivered since:

- `region NAME: size=N[, ...]` declarations lower to `region_create` and classify as `RegionCreate` memory operations with region-name metadata.
- `rc_clone` lowers to a `Copy` memory operation and `rc_release` to a `Cleanup` memory operation, activating the previously reserved copy/cleanup safety metadata end to end (see `memory_analysis_covers_region_copy_and_cleanup_end_to_end`).
- `frame_layout` produces deterministic per-scope cleanup plans that describe compiler-inserted cleanup ordering.

Remaining work:

- Emit `region_resize` once dynamic-region growth has a source form (the kind and its safety metadata are already reserved).
- Use this metadata as a prerequisite for native backend lowering and alias analysis.

## 2. Platform-Agnostic Build Orchestration

Goal: define one release orchestration path that can build and verify the toolchain on Windows, Linux, and macOS without replacing Cargo as the Rust build driver.

Recommended sequence:

- Keep Cargo as the compiler/runtime build engine.
- Use `scripts/package_portable.py` as the initial cross-platform release driver for target tags, output layout, examples, checksums, manifests, archives, and host-compatible smoke tests.
- Preserve `scripts/package_windows_portable.ps1` as the Windows implementation while `scripts/package_portable.py` reaches parity across non-Windows hosts.
- `documents/portable_package_ci_workflow.yml` provides the GitHub Actions workflow to run formatting, tests, clippy, and explicit target-triple `scripts/package_portable.py --target <triple> --target-tag <os-arch> --verify` package jobs on Windows, Linux, and macOS hosts. It must be copied to `.github/workflows/portable-package.yml` from a session or token with GitHub `workflow` scope.

Minimum target matrix:

- `x86_64-pc-windows-msvc`
- `x86_64-unknown-linux-gnu`
- `aarch64-apple-darwin`
- `x86_64-apple-darwin`

Remaining work:

- Activate the GitHub Actions workflow under `.github/workflows/portable-package.yml` once the GitHub token/session has `workflow` scope.
- Promote the portable package archives to release assets after target-triple package verification is proven on Windows, Linux, and macOS.

## 3. Installer Packaging

Goal: move from a Windows-first portable archive to a packaging plan that can produce predictable user-facing artifacts for all supported platforms.

Recommended sequence:

- Keep portable archives as the first cross-platform package type.
- Put the CLI under `bin/`, examples under `examples/`, release notes and `MANIFEST.json` at package root, optional user PATH helpers at package root, and checksums next to archives on every platform.
- Ship reversible, user-scoped PATH helpers in portable archives: `install.cmd`/`install.ps1` plus `uninstall.cmd`/`uninstall.ps1` on Windows, and `install.sh` plus `uninstall.sh` on Linux/macOS.
- Add platform-specific installers only after portable archive verification is stable.
- Treat PATH setup as optional and user-scoped unless the installer format has an explicit consent step.

Planned package types:

- Windows: portable zip first, MSI or NSIS later.
- Linux: tarball first, Debian/RPM packages later.
- macOS: tarball first, signed package or app bundle later if the distribution model requires it.

## 4. Native Code Generation Roadmap

Goal: reach native code generation only after the typed IR, bytecode contract, memory effects, diagnostics, and release verification are strong enough to make backend failures actionable.

Recommended sequence:

- Freeze the typed IR contract for the current subset plus memory-effect metadata. Current status: bytecode memory metadata is versioned and ordered, and `crates/lullaby_ir::native_contract` records the first native backend contract.
- Add backend snapshots for IR and bytecode before introducing machine-code output. Current status: bytecode memory metadata snapshots exist, the native backend contract has a checked-in JSON snapshot, and COFF object-emission snapshots cover literal return, stack-backed `i64` local addition, and straight-line `i64` assignments.
- Define calling convention, stack-frame, value layout, pointer, array, and resource-cleanup rules. Current status: see [native_backend_contract.md](native_backend_contract.md) and `native_backend_contract()`.
- Prototype object emission for one host target before adding a linker workflow. Current status: `lullaby_ir::native_object` emits a minimal `x86_64-pc-windows-msvc` COFF object for zero-argument `main` with literal return, `void`, literal `bool`, stack-backed `i64` local arithmetic, and straight-line `i64` assignment arithmetic; broader lowering and linker work remain pending.
- Require native backend diagnostics to use the shared `L####` diagnostic model.

Native backend non-goals for the next checkpoint:

- Do not bypass the existing AST runtime or bytecode VM.
- Do not add speculative optimizations that are not justified by the typed IR contract.
- Do not introduce a new build system unless Cargo plus scripts cannot support the verified release path.
