# Nous Lang Alpha 1 Release Notes

Release: `v0.1.0-alpha.2`

Functional baseline commit: `51f85b5`

Package artifact: `dist\nous-lang-alpha1-windows-x64.zip`

Checksum artifact: `dist\nous-lang-alpha1-windows-x64.zip.sha256`

Alpha 1 is the first installable Nous Lang toolchain checkpoint. It is a minimal working language and tooling release, not the full systems language.

## Package Contents

- `bin\nlang.exe`: release CLI binary.
- `docs\index.html`: self-contained offline documentation.
- `examples\`: executable `.nl` examples plus invalid diagnostic examples.
- `install.cmd` / `install.ps1`: optional user PATH setup helper.
- `uninstall.cmd` / `uninstall.ps1`: optional user PATH cleanup helper.
- `README.txt`: package quick start and optional PATH setup.
- `VERSION.txt`: package metadata.
- `RELEASE_NOTES.md`: these release notes.
- A repository license file, if one exists at package time.

## Quick Start

From the unpacked package directory:

```powershell
.\bin\nlang.exe --version
.\bin\nlang.exe docs
.\bin\nlang.exe examples
.\bin\nlang.exe check .\examples\valid\calculator.nl
.\bin\nlang.exe run .\examples\valid\calculator.nl
.\bin\nlang.exe compile --optimize alpha -o .\examples\valid\calculator.nbc .\examples\valid\calculator.nl
.\bin\nlang.exe inspect .\examples\valid\calculator.nbc
.\bin\nlang.exe run .\examples\valid\calculator.nbc
```

Optional user PATH setup from the same unpacked package directory:

```powershell
.\install.cmd
nlang --version
.\uninstall.cmd
```

Verify a downloaded package checksum:

```powershell
$expected = (Get-Content .\nous-lang-alpha1-windows-x64.zip.sha256 -Raw).Split(" ")[0]
$actual = (Get-FileHash .\nous-lang-alpha1-windows-x64.zip -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "checksum mismatch" }
```

## Supported Language Surface

- `.nl` source files.
- Indentation-only blocks; curly braces and semicolon terminators are errors.
- Functions with typed parameters and explicit return types.
- Last-expression returns, explicit `return`, and `-> void`.
- `let` bindings, assignment, and numeric compound assignment.
- `i64`, `bool`, `string`, `void`, `array<T>`, and interim pointer spellings such as `ptr_i64`.
- Arithmetic, equality, ordering, boolean logic, grouped expressions, calls, arrays, and indexing.
- `if`/`elif`/`else`, `while`, inclusive range `for`, `loop`, `break`, and `continue`.
- Memory builtins: `alloc`, `load`, `store`, and `dealloc`.
- Text file builtins: `read_file`, `write_file`, `append_file`, and `file_exists`.
- Safe program-plus-argv command builtins: `sys_status` and `sys_output`.
- Executable source passed to `nlang compile` or source `nlang run` must define zero-argument `main`; helper-only files remain valid for `nlang check`.

See [alpha1_language_surface.md](alpha1_language_surface.md) for the frozen feature surface.

## CLI Surface

- `nlang check [--verbose|--format json] <file.nl>`
- `nlang compile [--optimize none|constant-fold|dead-code|alpha] [-o output.nbc] [--verbose|--format json] <file.nl>`
- `nlang inspect [--verbose|--format json] <file.nbc>`
- `nlang run [--backend ast|ir|bytecode] [--optimize none|constant-fold|dead-code|alpha] [--verbose|--format json] <file.nl>`
- `nlang run [--verbose|--format json] <file.nbc>`
- `nlang docs`
- `nlang examples`
- `nlang --version`

`--diagnostic-format json` is accepted as a JSON diagnostics alias.

## Compiled Artifact Contract

`nlang compile` writes a versioned `.nbc` JSON artifact with:

- `format`: `nous-bytecode`
- `version`: `3`
- deterministic metadata
- entry point
- function table
- instruction-bytecode module with dedicated function `instructions`

`nlang inspect file.nbc` prints artifact metadata and function signatures without executing the program. `nlang run file.nbc` validates format, version, metadata target/payload, entry support, entry presence, duplicate functions, and function-table/module compatibility before execution.

## Diagnostics

Alpha 1 emits stable `N####` diagnostics for source, lexer, parser, semantic, IR, optimizer, bytecode, runtime, and resource failures. Concise, verbose, and JSON modes are covered by CLI tests for representative failures.

Notable codes:

- `N0003`: compiled artifact write failure.
- `N0211`: planned syntax such as imports, modules, structs, or try/catch is not supported in Alpha 1.
- `N0329`: executable entry point is missing or has parameters.
- `N0501`: IR lowering failure.
- `N0502`: optimizer mode requires IR or bytecode backend.
- `N0601`: malformed, unsupported, or incompatible `.nbc` artifact.

See [diagnostic_registry.md](diagnostic_registry.md) for the full registry.

## Verification Evidence

The release gate is:

```powershell
cargo fmt --check
cargo test --all
cargo clippy --all-targets --all-features -- -D warnings
python offline_docs\verify_offline_docs.py
powershell -ExecutionPolicy Bypass -File scripts\verify_release.ps1
```

Additional hygiene checks run for the release work:

```powershell
stale marker search over documents
git diff --check -- .
```

The Markdown local-reference check also passed with the repository's file-like local target filter.

`scripts\verify_release.ps1` builds the portable package and smoke-tests the packaged `nlang.exe` for:

- `--version`
- `docs`
- `examples`
- `check`
- source `run`
- `compile`
- `.nbc` artifact `inspect`
- `.nbc` artifact `run`
- invalid example diagnostics
- dry-run PATH setup and cleanup helpers
- generated zip checksum

GitHub prerelease publication command:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\publish_github_release.ps1
```

## Known Limitations

- No native code generation, linker, or machine-code binary output yet.
- No modules, packages, imports, structs, unions, traits, interfaces, classes, pattern matching, or user-defined generics beyond `array<T>`.
- No region memory, ARC/reference counting, lifetime analysis, or GC hooks yet.
- I/O is limited to text file builtins and direct program-plus-argv command calls.
- Offline docs are hand-authored HTML; a generated docs pipeline is still planned.
- Alpha 1 distribution is a Windows portable archive with optional user PATH helper scripts, not a full MSI/NSIS-style installer.
- Optimizer support is intentionally conservative: constant folding, block-local CSE for repeated pure bindings, conservative loop-invariant motion for safe loop-body bindings, block-local copy propagation for simple aliases, and block-local dead-code elimination are implemented.

## Next Phase

The next phase should keep the working AST runtime and installable package intact while hardening the typed IR and bytecode backend with broader backend snapshots, more bytecode instruction validation, and a fuller instruction-bytecode VM.
