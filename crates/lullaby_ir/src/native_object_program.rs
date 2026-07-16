//! Native backend: the **program-level driver**. Owns the public entry points
//! (`emit_native_program` / `_with_debug` / `_for_target`), the result and error
//! types they return (`NativeProgram`, `NativeSkippedFunction`,
//! `NativeProgramError`, `DebugOptions`), and the eligibility/lowering fixpoint
//! that decides which functions compile and which skip to the interpreters.
//!
//! This is the layer *above* per-function lowering: it expands method instances,
//! resolves closure layouts, infers array lengths and native signatures, drives
//! `lower_native_function` to a fixpoint (a demoted function drops from the
//! callable set and the loop re-runs), and hands the lowered set to the object
//! writer selected by the target's `NativeObjectFormat` (COFF / ELF / Mach-O),
//! plus the direct PE32+ image for freestanding-eligible programs.
//!
//! Split out of `native_object.rs`, which keeps the shared per-function codegen
//! state (`NativeCtx` and friends); sees the parent's items via `use super::*`.
//! Its tests live in `native_program_tests.rs`.

use super::*;

// ===========================================================================
// Extended native program emitter (multi-function, linkable, i64-scalar subset)
// ===========================================================================
//
// The prototype `emit_coff_object` above lowers a single literal-return
// `main`. The emitter below extends the same COFF machinery to the full
// i64-scalar subset the WASM backend targets: every function whose parameters
// and return type are all `i64` (up to four parameters, Win64 register args) is
// compiled to x86-64 machine code, with control flow (`if`/`while`/`loop`/`for`)
// lowered structurally and inter-function calls resolved through COFF
// relocations. An entry stub (`_lullaby_start`) calls `main`, moves its result
// into `ecx`, and calls `ExitProcess` (imported from kernel32) so the process
// exit code is `main`'s result mod 256. Functions using anything outside the
// subset are SKIPPED (they still run on the interpreters).

/// The result of emitting a linkable native program: the COFF object bytes plus
/// the record of which functions compiled and which were skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProgram {
    /// Target triple (`x86_64-pc-windows-msvc`).
    pub target: NativeTarget,
    /// The COFF object bytes (a real linkable `.obj`).
    pub bytes: Vec<u8>,
    /// The entry-point symbol name the linker should use (`/entry:`).
    pub entry_symbol: String,
    /// Names of functions compiled to native code, in module order.
    pub compiled: Vec<String>,
    /// Functions skipped for the native subset, each with a reason.
    pub skipped: Vec<NativeSkippedFunction>,
    /// C import libraries the linker must also resolve, beyond `kernel32.lib`.
    /// Populated when the program calls `extern fn` C functions (e.g. `ucrt.lib`
    /// for the C runtime). Empty for a program with no extern calls.
    pub import_libs: Vec<String>,
    /// A directly-emitted, runnable PE32+ executable, present only when the
    /// program is **freestanding-eligible** (a COFF/Windows target with a `main`
    /// and no C-runtime import): Lullaby lays the `.exe` image around the `.text`
    /// bytes itself, so the CLI can write a runnable executable without invoking
    /// the external linker (`rust-lld`). `None` for non-freestanding programs,
    /// library objects, and the ELF/Mach-O/AArch64 targets, which keep the
    /// object-file + linker path. See [`pe_image::write_pe_executable`].
    pub pe_image: Option<Vec<u8>>,
}

/// The C runtime import library that provides the standard C library symbols
/// (e.g. `llabs`) an `extern fn` may name. Discovered like `kernel32.lib` via
/// the MSVC `LIB` environment variable.
pub const C_RUNTIME_IMPORT_LIB: &str = "ucrt.lib";

/// A function that was not eligible for the native i64-scalar subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSkippedFunction {
    pub name: String,
    pub reason: String,
}

/// A hard failure while emitting the native program. The only hard error is "no
/// i64-scalar function was eligible", surfaced by the CLI as diagnostic `L0339`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProgramError {
    pub code: &'static str,
    pub message: String,
    /// Functions skipped, so the CLI can still report why nothing compiled.
    pub skipped: Vec<NativeSkippedFunction>,
}

/// Diagnostic code for "no i64-scalar functions eligible for native codegen".
/// Kept inline (like the WASM backend's `L0338`) rather than in the shared
/// diagnostic registry, which only carries frontend/semantic codes.
pub const NATIVE_NO_ELIGIBLE_CODE: &str = "L0339";

/// The entry-stub symbol name. The linker is invoked with `/entry:` set to this.
pub const NATIVE_ENTRY_SYMBOL: &str = "_lullaby_start";

/// Options for emitting native source-line debug info (`lullaby native --debug`).
///
/// When present, the emitter adds a CodeView `.debug$S` section carrying a
/// per-function line-number table that maps each compiled function's entry code
/// offset to its `.lby` source declaration line, plus the source file name. A
/// debugger (or `llvm-pdbutil`) can then place a breakpoint at a function and
/// show the corresponding source line. Without these options the object bytes are
/// byte-for-byte unchanged (no `.debug$S` section), so existing snapshot and
/// structural tests are unaffected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugOptions {
    /// The `.lby` source file path recorded in the CodeView file table. Shown by a
    /// debugger as the source file for the compiled functions.
    pub source_file: String,
}

/// Emit a linkable COFF object for the i64-scalar-subset functions of `module`.
///
/// Eligible functions (all params + return are `i64`, at most four params, and a
/// body built from the supported subset) are lowered to x86-64. An entry stub
/// calls `main` and forwards its result to `ExitProcess`. Ineligible functions
/// are recorded in `skipped`. If no function is eligible, returns an error with
/// code `L0339`.
///
/// This is the default (no debug info) entry point; see
/// [`emit_native_program_with_debug`] to additionally emit CodeView
/// source-line debug info.
pub fn emit_native_program(module: &BytecodeModule) -> Result<NativeProgram, NativeProgramError> {
    emit_native_program_with_debug(module, None)
}

/// Like [`emit_native_program`], but when `debug` is `Some`, additionally
/// emits a CodeView `.debug$S` section with per-function source-line info (see
/// [`DebugOptions`]). When `debug` is `None` the emitted object bytes are exactly
/// those of [`emit_native_program`].
pub fn emit_native_program_with_debug(
    module: &BytecodeModule,
    debug: Option<&DebugOptions>,
) -> Result<NativeProgram, NativeProgramError> {
    emit_native_program_for_target(module, &x86_64_windows_target(), debug, false)
}

/// Emit a native program for an explicit `target`, selecting the object-file
/// container by the target's [`NativeObjectFormat`]:
///
/// - `x86_64-pc-windows-msvc` → COFF (the default; byte-for-byte unchanged),
/// - `x86_64-unknown-linux-gnu` → ELF64 (System V AMD64), and
/// - `x86_64-apple-darwin` → Mach-O x86-64.
///
/// The x86-64 machine code and the internal calling convention are identical
/// across all three; only the object wrapper and the entry/exit stub differ (a
/// freestanding `exit` syscall on Linux/macOS instead of `kernel32!ExitProcess`).
/// The ELF and Mach-O objects are relocatable objects verified structurally on
/// this host; link-and-run verification is deferred to the Phase 9 cross-platform
/// CI. See `documents/native_backend_contract.md`.
pub fn emit_native_program_for_target(
    module: &BytecodeModule,
    target: &NativeTarget,
    debug: Option<&DebugOptions>,
    fast_math: bool,
) -> Result<NativeProgram, NativeProgramError> {
    let target = target.clone();

    // AArch64 is a distinct instruction set with its own code generator: it
    // consumes the same `BytecodeModule` but emits AArch64 machine code and an
    // aarch64 ELF object (see `crate::aarch64`). The `--debug` CodeView line
    // table is an x86-64/COFF feature and does not apply to the AArch64 core.
    if matches!(target.architecture, NativeArchitecture::Aarch64) {
        return crate::aarch64::emit_aarch64_program(module, &target);
    }

    // Inherent-method dispatch (x86-64): a source `recv.method(args)` reaches the
    // backend as an ordinary `Call { name: "method", args: [recv, ...] }` (UFCS),
    // but the method bodies live in `module.impls`, not `module.functions`. Expand
    // every native-resolvable method call into a direct call to a synthesized,
    // monomorphized instance function appended to `functions`, so the whole
    // existing pipeline (eligibility, signatures, lowering, emission) applies to a
    // method exactly as to any function. Default-deny: an unresolvable/out-of-subset
    // method call is left untouched and skips through the fixpoint. Produces a
    // structural clone with byte-identical bodies when there are no methods.
    let expanded_module = expand_method_instances(module);
    let module = &expanded_module;

    // Native closure layouts (Stage 1): for each closure definition that appears as
    // a `fn(...)` literal in the module, resolve its native layout (captures, param
    // count) from the literal's static function type. A closure outside the Stage-1
    // subset (a non-scalar capture/param/return, or more than three parameters) gets
    // no layout, so any function binding it skips cleanly. Computed once — it does
    // not depend on the eligible set.
    let closure_layouts = compute_module_closure_layouts(module);

    // First pass: decide signature eligibility. Calls resolve against the set of
    // names we intend to compile.
    let mut skipped: Vec<NativeSkippedFunction> = Vec::new();
    let mut eligible_names: Vec<String> = Vec::new();
    for function in &module.functions {
        match native_signature_eligibility(function, &module.structs, &module.enums) {
            Ok(()) => eligible_names.push(function.name.clone()),
            Err(reason) => skipped.push(NativeSkippedFunction {
                name: function.name.clone(),
                reason,
            }),
        }
    }

    // Second pass: lower each eligible body. A lowering failure demotes the
    // function to skipped and drops it from the callable set, then re-runs (a
    // call to a demoted function must also fail). Converges quickly.
    loop {
        // Compiled functions plus every declared `extern fn` are callable. An
        // extern name resolves to an undefined external symbol (bound by the
        // linker) rather than a compiled `.text` function.
        let mut callable: std::collections::HashSet<&str> =
            eligible_names.iter().map(String::as_str).collect();
        for name in &module.extern_functions {
            callable.insert(name.as_str());
        }
        // C-ABI signatures for the declared externs, keyed by name, so an extern
        // call marshals its arguments/return to the correct C scalar widths.
        let extern_sigs: HashMap<&str, &crate::IrExternSignature> = module
            .extern_signatures
            .iter()
            .map(|sig| (sig.name.as_str(), sig))
            .collect();
        let mut lowered: Vec<LoweredNativeFunction> = Vec::new();
        let mut demoted: Option<NativeSkippedFunction> = None;
        // String constants are interned fresh each attempt so a demotion that
        // drops a function also drops any strings only it referenced.
        let mut strings = StringPool::default();

        // Infer array lengths for every eligible function's array-typed signature
        // slots (fixed arrays carry no length in their `array<T>` type), then
        // compute the native signatures (parameter + return layouts). A function
        // whose array slot cannot be sized consistently — or that would call a
        // function whose signature failed — is demoted and the loop retries.
        let mut array_lengths_by_fn: HashMap<String, ArrayLengths> = HashMap::new();
        let mut signatures: HashMap<String, NativeSignature> = HashMap::new();
        for name in &eligible_names {
            let function = module
                .functions
                .iter()
                .find(|f| &f.name == name)
                .expect("eligible name exists");
            let inference =
                infer_array_lengths(function, module, &eligible_names).and_then(|lengths| {
                    let sig = compute_native_signature(
                        function,
                        &module.structs,
                        &module.enums,
                        &lengths,
                    )?;
                    Ok((lengths, sig))
                });
            match inference {
                Ok((lengths, sig)) => {
                    array_lengths_by_fn.insert(name.clone(), lengths);
                    signatures.insert(name.clone(), sig);
                }
                Err(reason) => {
                    demoted = Some(NativeSkippedFunction {
                        name: name.clone(),
                        reason,
                    });
                    break;
                }
            }
        }
        if let Some(demoted) = demoted {
            eligible_names.retain(|n| n != &demoted.name);
            merge_native_skip(&mut skipped, demoted);
            continue;
        }

        // Arena-first memory (stage 1): the set of functions whose heap allocations
        // provably stay local, so they route through a function-scoped arena
        // (reclaimed by rewinding the bump pointer on every return edge). Default-
        // deny; every other function keeps its unchanged RC / free-list codegen.
        let arena_names = arena_eligible_functions(module, &eligible_names, &signatures);

        for name in &eligible_names {
            let function = module
                .functions
                .iter()
                .find(|f| &f.name == name)
                .expect("eligible name exists");
            let array_lengths = &array_lengths_by_fn[name];
            match lower_native_function(
                function,
                &callable,
                &extern_sigs,
                &module.structs,
                &module.enums,
                &mut strings,
                &signatures,
                array_lengths,
                fast_math,
                arena_names.contains(name.as_str()),
                &closure_layouts,
            ) {
                Ok(l) => lowered.push(l),
                Err(reason) => {
                    demoted = Some(NativeSkippedFunction {
                        name: name.clone(),
                        reason,
                    });
                    break;
                }
            }
        }

        if let Some(demoted) = demoted {
            eligible_names.retain(|n| n != &demoted.name);
            merge_native_skip(&mut skipped, demoted);
            continue;
        }

        // Synthesize a native `.text` body (`__closure_{id}`) for each closure the
        // compiled functions reference. A synthesis failure (a body outside the
        // Stage-1 subset — heap touch, user call, or otherwise non-lowerable) demotes
        // the referencing function and re-runs the fixpoint, exactly like a top-level
        // lowering failure: the enclosing function then skips to the interpreters
        // rather than emitting a dangling `lea __closure_{id}` relocation. On success
        // the bodies are appended to `lowered` so they are emitted as ordinary `.text`
        // symbols and the enclosing `lea`/`call` resolves.
        match synthesize_referenced_closure_bodies(
            &eligible_names,
            module,
            &callable,
            &extern_sigs,
            &mut strings,
            &signatures,
            &closure_layouts,
        ) {
            Ok(closure_bodies) => lowered.extend(closure_bodies),
            Err(demoted) => {
                eligible_names.retain(|n| n != &demoted.name);
                merge_native_skip(&mut skipped, demoted);
                continue;
            }
        }

        // The entry classification carries `main`'s RETURN SHAPE, not just its
        // presence: a void `main` leaves `rax` undefined, so its stub must not read
        // it as the exit code (see `EntryStub`).
        let entry_stub = EntryStub::classify(&lowered, module);
        let has_main = entry_stub.emits();
        // Whether any compiled function is a C-callable export. An export-only
        // program (no `main`) is a *library object*: it has no entry stub, so a C
        // (or other) `main` can link against it and call the exported symbols.
        let has_export = lowered
            .iter()
            .any(|f| module.export_functions.contains(&f.name));

        if lowered.is_empty() || (!has_main && !has_export) {
            // Nothing runnable and nothing exported: there is nothing to emit.
            let reason = if lowered.is_empty() {
                "no functions were eligible for the native i64-scalar subset".to_string()
            } else {
                "neither `main` nor an `export fn` is eligible for the native i64-scalar subset"
                    .to_string()
            };
            return Err(NativeProgramError {
                code: NATIVE_NO_ELIGIBLE_CODE,
                message: reason,
                skipped,
            });
        }

        let compiled: Vec<String> = lowered.iter().map(|f| f.name.clone()).collect();
        // Emit the entry stub only when a `main` is present. A pure export library
        // (no `main`) omits the stub entirely, so it carries no exit dependency
        // and does not collide with a C `main` at link time.
        //
        // The object container is selected by the target format: COFF keeps its
        // own byte-for-byte writer (and `kernel32!ExitProcess` entry stub); ELF
        // and Mach-O flow through the shared neutral object model with a
        // freestanding `exit`-syscall entry stub.
        let (bytes, entry_symbol) = match target.object_format {
            NativeObjectFormat::Coff => {
                let bytes = write_native_program_object(&lowered, &strings, entry_stub, debug);
                let entry = if has_main {
                    NATIVE_ENTRY_SYMBOL.to_string()
                } else {
                    String::new()
                };
                (bytes, entry)
            }
            NativeObjectFormat::Elf => {
                let model = build_object_model(&lowered, &strings, entry_stub, PlatformAbi::Linux);
                let entry = model.entry_symbol.clone().unwrap_or_default();
                (elf_object::write_elf64(&model), entry)
            }
            NativeObjectFormat::MachO => {
                let model = build_object_model(&lowered, &strings, entry_stub, PlatformAbi::MacOs);
                let entry = model.entry_symbol.clone().unwrap_or_default();
                (macho_object::write_macho64(&model), entry)
            }
        };
        // When the program declares any `extern fn`, the C runtime import library
        // must be linked so the external C symbols resolve.
        let import_libs = if module.extern_functions.is_empty() {
            Vec::new()
        } else {
            vec![C_RUNTIME_IMPORT_LIB.to_string()]
        };
        // Freestanding-eligible programs (a Windows/COFF target with a `main` and
        // no C-runtime import) also get a directly-emitted PE32+ executable, so the
        // CLI can write a runnable `.exe` without the external linker. A program
        // that needs the C runtime, a library object (no `main`), or a non-COFF
        // target keeps the object-file + linker path (`pe_image` stays `None`).
        let pe_image = if matches!(target.object_format, NativeObjectFormat::Coff)
            && has_main
            && import_libs.is_empty()
        {
            write_pe_executable(&lowered, &strings, entry_stub)
        } else {
            None
        };
        return Ok(NativeProgram {
            target,
            bytes,
            entry_symbol,
            compiled,
            skipped,
            import_libs,
            pe_image,
        });
    }
}

fn merge_native_skip(skips: &mut Vec<NativeSkippedFunction>, skip: NativeSkippedFunction) {
    if !skips.iter().any(|s| s.name == skip.name) {
        skips.push(skip);
    }
}
