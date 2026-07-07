# Lullaby

Lullaby is an experimental compiled systems programming language focused on concise, indentation-only syntax, strong typing, memory-safety foundations, and source that is easy for LLMs to generate.

The toolchain runs Lullaby on three parity-checked backends — an AST interpreter, a typed-IR interpreter, and an instruction-bytecode VM (with an optimizer) — plus a versioned `.lbc` artifact path and offline docs, shipped as a Windows-first portable package. Native code generation, linking, and a full systems runtime are still future work.

## Language At A Glance

The implemented surface (all running identically on every backend) includes:

- **Types**: `i64`, `f64`, `bool`, `string`, `char`, `byte`, `void`; `array<T>`, growable `list<T>`, `map<K, V>`; nominal `struct` and `enum` (tagged unions); `option<T>` / `result<T, E>`; function values `fn(T) -> R`; and `rc<T>`/`ref<T>`/`ptr<T>` references.
- **Data**: struct construction (positional and named `Point(x: 3, y: 4)`), field access and mutation, UFCS method calls (`p.dist()`), enum variants with payloads.
- **Control flow**: `if`/`elif`/`else`, `while`, range `for`, `loop`, `break`/`continue`, exhaustive `match` with payload binding, and `throw`/`try`/`catch`.
- **Abstraction**: user-defined generic functions (`fn f<T> ...`) with call-site inference, traits with `impl` and bounded generics (`<T: Show>`), and first-class functions passed and returned by value.
- **Programs**: multi-file `import` with `pub` visibility; a string and math standard library, collections, and text/stream/system I/O — all documented in the [standard library catalog](documents/standard_library.md).
- **Tooling**: strong diagnostics (concise / verbose / JSON), a canonical formatter (`lullaby fmt`), an editor language server (`lullaby lsp`), and a bytecode artifact + inspector.

See the [Alpha 1 language surface](documents/alpha1_language_surface.md) for the exact, authoritative feature list.

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
- `lullaby native [--verbose] [--freestanding|--no-std] [-o out.exe] <file.lby>`
- `lullaby fmt [--write|--check] <file.lby>`
- `lullaby lsp`
- `lullaby docs`
- `lullaby examples`
- `lullaby help`
- `lullaby --version`

`lullaby check` can validate helper/library-style `.lby` files without `main`. `lullaby compile`, `lullaby build`, and source `lullaby run` require executable source with zero-argument `main`; invalid entry points report `L0329`. `lullaby build` is an artifact-generation alias for `lullaby compile`.

`lullaby native` compiles the i64-scalar subset to an x86-64 Windows COFF object and, best-effort, links it into a runnable `.exe`. Adding `--freestanding` (alias `--no-std`) builds a **no-C-runtime** executable: it links `kernel32.lib` only (zero `ucrt`/`vcruntime`/`msvcrt`) and terminates through the minimal OS import `kernel32!ExitProcess`. It is still a Windows PE, not a bare-metal binary. A freestanding build that declares an `extern fn` (which needs the C runtime) is rejected with `L0426`.

## Documentation

- [Alpha 1 language surface](documents/alpha1_language_surface.md)
- [Standard library catalog (the prelude)](documents/standard_library.md)
- [Language specification](documents/language_specification.md)
- [Implementation plan](documents/implementation_plan.md)
- [Diagnostic registry](documents/diagnostic_registry.md)
- [Contributor guide for language features](documents/contributor_guide.md)
- [Repository map](documents/repository_map.md)

The offline browser documentation is bundled in the package as `docs\index.html` and can be opened directly from disk.
