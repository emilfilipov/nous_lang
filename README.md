# Lullaby

Lullaby is an experimental compiled systems programming language focused on concise, indentation-only syntax, strong typing, memory-safety foundations, and source that is easy for LLMs to generate.

The current toolchain is an Alpha 1 language surface with a Windows-first portable package. It is useful for trying the syntax, diagnostics, bytecode artifact path, offline docs, and early runtime subset. It is not yet a native-code compiler or full systems standard library.

## Install The Portable Package

1. Download `lullaby-alpha1-windows-x64.zip` and `lullaby-alpha1-windows-x64.zip.sha256` from the latest GitHub prerelease.
2. Verify the archive checksum:

```powershell
$expected = (Get-Content .\lullaby-alpha1-windows-x64.zip.sha256 -Raw).Split(" ")[0]
$actual = (Get-FileHash .\lullaby-alpha1-windows-x64.zip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
```

3. Unzip the archive.
4. From the package directory, run:

```powershell
.\bin\lullaby.exe --version
.\bin\lullaby.exe docs
.\bin\lullaby.exe examples
.\bin\lullaby.exe run .\examples\valid\calculator.lby
```

Optional user PATH setup:

```powershell
.\install.cmd
lullaby --version
.\uninstall.cmd
```

Open a new PowerShell or cmd window after running `install.cmd`.

## Build From Source

Prerequisites:

- Rust toolchain with Cargo.
- PowerShell on Windows for release packaging scripts.

Common commands:

```powershell
cargo run -p lullaby_cli -- check examples\valid\calculator.lby
cargo run -p lullaby_cli -- run examples\valid\calculator.lby
cargo run -p lullaby_cli -- compile --optimize alpha -o target\calculator.lbc examples\valid\calculator.lby
cargo run -p lullaby_cli -- build --optimize alpha -o target\calculator-build.lbc examples\valid\calculator.lby
cargo run -p lullaby_cli -- inspect target\calculator.lbc
cargo run -p lullaby_cli -- run target\calculator.lbc
```

Release package verification:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\verify_release.ps1
```

## Current CLI

- `lullaby check [--verbose|--format json] <file.lby>`
- `lullaby compile [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>`
- `lullaby build [--optimize none|constant-fold|dead-code|alpha] [-o output.lbc] [--verbose|--format json] <file.lby>`
- `lullaby inspect [--verbose|--format json] <file.lbc>`
- `lullaby run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.lby>`
- `lullaby run [--verbose|--format json] <file.lbc>`
- `lullaby docs`
- `lullaby examples`
- `lullaby help`
- `lullaby --version`

`lullaby check` can validate helper/library-style `.lby` files without `main`. `lullaby compile`, `lullaby build`, and source `lullaby run` require executable source with zero-argument `main`; invalid entry points report `L0329`. `lullaby build` is an artifact-generation alias for `lullaby compile`.

## Documentation

- [Alpha 1 language surface](documents/alpha1_language_surface.md)
- [Language specification](documents/language_specification.md)
- [Implementation plan](documents/implementation_plan.md)
- [Diagnostic registry](documents/diagnostic_registry.md)
- [Contributor guide for language features](documents/contributor_guide.md)
- [Repository map](documents/repository_map.md)

The offline browser documentation is bundled in the package as `docs\index.html` and can be opened directly from disk.
