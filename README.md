# Nous Lang

Nous Lang is an experimental compiled systems programming language focused on concise, indentation-only syntax, strong typing, memory-safety foundations, and source that is easy for LLMs to generate.

The current toolchain is an Alpha 1 language surface with a Windows-first portable package. It is useful for trying the syntax, diagnostics, bytecode artifact path, offline docs, and early runtime subset. It is not yet a native-code compiler or full systems standard library.

## Install The Portable Package

1. Download `nous-lang-alpha1-windows-x64.zip` and `nous-lang-alpha1-windows-x64.zip.sha256` from the latest GitHub prerelease.
2. Verify the archive checksum:

```powershell
$expected = (Get-Content .\nous-lang-alpha1-windows-x64.zip.sha256 -Raw).Split(" ")[0]
$actual = (Get-FileHash .\nous-lang-alpha1-windows-x64.zip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
```

3. Unzip the archive.
4. From the package directory, run:

```powershell
.\bin\nlang.exe --version
.\bin\nlang.exe docs
.\bin\nlang.exe examples
.\bin\nlang.exe run .\examples\valid\calculator.nl
```

Optional user PATH setup:

```powershell
.\install.cmd
nlang --version
.\uninstall.cmd
```

Open a new PowerShell or cmd window after running `install.cmd`.

## Build From Source

Prerequisites:

- Rust toolchain with Cargo.
- PowerShell on Windows for release packaging scripts.

Common commands:

```powershell
cargo run -p nous_cli -- check examples\valid\calculator.nl
cargo run -p nous_cli -- run examples\valid\calculator.nl
cargo run -p nous_cli -- compile --optimize alpha -o target\calculator.nbc examples\valid\calculator.nl
cargo run -p nous_cli -- build --optimize alpha -o target\calculator-build.nbc examples\valid\calculator.nl
cargo run -p nous_cli -- inspect target\calculator.nbc
cargo run -p nous_cli -- run target\calculator.nbc
```

Release package verification:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\verify_release.ps1
```

## Current CLI

- `nlang check [--verbose|--format json] <file.nl>`
- `nlang compile [--optimize none|constant-fold|dead-code|alpha] [-o output.nbc] [--verbose|--format json] <file.nl>`
- `nlang build [--optimize none|constant-fold|dead-code|alpha] [-o output.nbc] [--verbose|--format json] <file.nl>`
- `nlang inspect [--verbose|--format json] <file.nbc>`
- `nlang run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.nl>`
- `nlang run [--verbose|--format json] <file.nbc>`
- `nlang docs`
- `nlang examples`
- `nlang help`
- `nlang --version`

`nlang check` can validate helper/library-style `.nl` files without `main`. `nlang compile`, `nlang build`, and source `nlang run` require executable source with zero-argument `main`; invalid entry points report `N0329`. `nlang build` is an artifact-generation alias for `nlang compile`.

## Documentation

- [Alpha 1 language surface](documents/alpha1_language_surface.md)
- [Language specification](documents/language_specification.md)
- [Implementation plan](documents/implementation_plan.md)
- [Diagnostic registry](documents/diagnostic_registry.md)
- [Contributor guide for language features](documents/contributor_guide.md)
- [Repository map](documents/repository_map.md)

The offline browser documentation is bundled in the package as `docs\index.html` and can be opened directly from disk.
