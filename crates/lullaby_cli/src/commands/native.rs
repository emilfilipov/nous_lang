//! `lullaby native` — emit a native object for the compilable subset and,
//! best-effort, produce a runnable executable.

use std::{fs, path::PathBuf};

use lullaby_diagnostics::{DiagnosticPhase, DiagnosticReport};
use lullaby_ir::native_contract::{NativeObjectFormat, native_target_for_triple};
use lullaby_ir::{
    DebugOptions, OptimizationConfig, emit_native_program_for_target, lower, lower_to_bytecode,
    optimize,
};

use crate::args::OutputMode;
use crate::commands::native_link::{LinkOutcome, link_native_object};
use crate::compile::{SourceMode, compile};
use crate::diagnostics::{format_reports, ir_report, write_failure_report};

/// Compile the i64-scalar-subset functions of a `.lby` source file to a native
/// x86-64 COFF object and, best-effort, link it into a runnable Windows `.exe`.
///
/// The object is always written (the reliable floor). Linking is attempted with
/// `rust-lld` when it and `kernel32.lib` can be located; if either is missing,
/// the command reports the object it produced and explains that linking was
/// unavailable rather than failing. `--verbose` lists compiled/skipped functions
/// and the linker command.
///
/// When `freestanding` is set (`--freestanding` / `--no-std`), the emitted
/// executable must not depend on the C runtime: only the minimal OS import
/// (`kernel32!ExitProcess`) needed to terminate is allowed. The default native
/// path already links `kernel32.lib` only (the entry stub bypasses the CRT), so
/// freestanding formalizes and guarantees that by rejecting any C-runtime
/// dependency — an `extern fn` that requires `ucrt.lib` is `L0426`.
pub(crate) fn native_file(
    path: PathBuf,
    output: Option<PathBuf>,
    mode: OutputMode,
    freestanding: bool,
    debug: bool,
    target_triple: Option<String>,
    fast_math: bool,
) -> Result<(), String> {
    // Resolve the object-file target. `None` is the default Windows COFF target;
    // an explicit `--target` selects ELF (Linux) or Mach-O (macOS). An unknown or
    // non-x86-64 triple is rejected up front with `L0347`.
    let target = match target_triple.as_deref() {
        None => native_target_for_triple("x86_64-pc-windows-msvc")
            .expect("default target is always known"),
        Some(triple) => match native_target_for_triple(triple) {
            Some(target) => target,
            None => {
                let report = DiagnosticReport::new(
                    "L0347",
                    DiagnosticPhase::Ir,
                    format!("unknown or unsupported native target triple `{triple}`"),
                )
                .with_note(
                    "supported targets: x86_64-pc-windows-msvc (COFF), x86_64-unknown-linux-gnu (ELF), x86_64-apple-darwin (Mach-O), aarch64-unknown-linux-gnu (aarch64 ELF)",
                );
                return Err(format_reports(&[report], mode, None));
            }
        },
    };

    // Library mode: a `main` is not required. A program with `main` still emits a
    // runnable executable; an export-only program (only `export fn` functions)
    // emits a C-callable library object. `emit_native_program` requires at
    // least a `main` or an eligible `export fn`, reporting `L0339` otherwise.
    let compiled = match compile(&path, SourceMode::Library) {
        Ok(compiled) => compiled,
        Err(failure) => {
            return Err(format_reports(
                &failure.reports,
                mode,
                failure.source.as_deref(),
            ));
        }
    };

    // A program that declares actors is native-ineligible — actors run on the
    // interpreter. Skip cleanly with `L0339` (no eligible function) rather than
    // attempting to lower `spawn`/`tell`, so a native build never miscompiles an
    // actor program.
    if !compiled.checked.program.actors.is_empty() {
        let report = DiagnosticReport::new(
            "L0339",
            DiagnosticPhase::Ir,
            "no functions were eligible for native compilation: this program uses actors, which run on the interpreter",
        )
        .with_source_path(compiled.path.display().to_string())
        .with_note(
            "actors (`actor`/`spawn`/`tell`) are not compiled to native code; run the program with `lullaby run` (the default backend)",
        );
        return Err(format_reports(&[report], mode, Some(&compiled.source)));
    }

    let module = lower(&compiled.checked).map_err(|error| {
        format_reports(
            &[ir_report(error, &compiled.path)],
            mode,
            Some(&compiled.source),
        )
    })?;
    // Inline small leaf helpers (e.g. `rem`, `is_even`) before native codegen so a
    // helper called in a hot loop becomes inline arithmetic instead of a `call`.
    // Only the inlining pass runs here: it produces pure scalar expressions the
    // backend already compiles, without the other optimization passes' rewrites.
    let (module, _report) = optimize(&module, &OptimizationConfig::inlining());
    let bytecode = lower_to_bytecode(&module);

    // With `--debug`, emit source-line debug info mapping each compiled function's
    // entry offset to its `.lby` source declaration line — a CodeView `.debug$S`
    // section on COFF, DWARF on ELF/Mach-O (the emitter picks by container).
    // Without it, the object bytes are byte-for-byte identical to the default
    // native path on every target.
    let debug_options = debug.then(|| DebugOptions {
        source_file: compiled.path.display().to_string(),
    });
    let program =
        match emit_native_program_for_target(&bytecode, &target, debug_options.as_ref(), fast_math)
        {
            Ok(program) => program,
            Err(error) => {
                let mut report =
                    DiagnosticReport::new(error.code, DiagnosticPhase::Ir, error.message)
                        .with_source_path(compiled.path.display().to_string());
                report = report.with_note(
                    "the native backend compiles scalar (i64/fixed-width/bool/char/byte/f64/f32), \
                 string, list, and map values; scalar-field and one-level heap-field \
                 (string-field) structs/enums; and control flow, calls, and FFI over these — \
                 a function outside that subset runs on the interpreters instead",
                );
                let mut rendered = format_reports(&[report], mode, Some(&compiled.source));
                if mode == OutputMode::Verbose {
                    for skip in &error.skipped {
                        rendered.push_str(&format!("\nskipped {}: {}", skip.name, skip.reason));
                    }
                }
                return Err(rendered);
            }
        };

    // Freestanding / no-std mode guarantees a no-C-runtime executable. The emitted
    // program links only the minimal OS import (`kernel32!ExitProcess`); any
    // additional import library (e.g. `ucrt.lib`, pulled in by an `extern fn` C
    // call) is a C-runtime dependency and is rejected here with `L0426`. When the
    // program has no such dependency, `import_libs` is empty and the guarantee
    // already holds — the default native path links `kernel32.lib` only.
    if freestanding && !program.import_libs.is_empty() {
        let libs = program.import_libs.join(", ");
        let report = DiagnosticReport::new(
            "L0426",
            DiagnosticPhase::Ir,
            format!(
                "freestanding (`--freestanding`) native build cannot depend on the C runtime, but this program requires the C runtime import library `{libs}` (via an `extern fn`)"
            ),
        )
        .with_source_path(compiled.path.display().to_string())
        .with_note(
            "remove the `extern fn` (and its calls) for a freestanding build, or drop `--freestanding` to link the C runtime",
        );
        return Err(format_reports(&[report], mode, Some(&compiled.source)));
    }

    // The object-file extension and the link step are target-specific. Windows
    // COFF objects (`.obj`) can be linked into a runnable `.exe` on this host;
    // the ELF (`.o`) and Mach-O (`.o`) objects cannot be linked or run here (no
    // cross-linker on a Windows host), so their link+run verification is deferred
    // to the Phase 9 cross-platform CI. The relocatable object is always written.
    let is_coff = matches!(target.object_format, NativeObjectFormat::Coff);
    let (exe_output, obj_output) = if is_coff {
        let exe = output.unwrap_or_else(|| compiled.path.with_extension("exe"));
        let obj = exe.with_extension("obj");
        (Some(exe), obj)
    } else {
        let obj = output.unwrap_or_else(|| compiled.path.with_extension("o"));
        (None, obj)
    };

    // Direct PE fast path — the DEFAULT for eligible native executable builds.
    // Whenever the backend produced a complete runnable PE32+ image (a COFF target
    // with a `main` and no C-runtime import — see how `pe_image` is set in
    // `native_object.rs`), writing it directly skips both the intermediate object
    // file and the external linker (`rust-lld`) — the single biggest compile-speed
    // lever. This applies with OR without `--freestanding`: `--freestanding` is a
    // stricter *guarantee* (it rejects any C-runtime dependency above with L0426),
    // not the trigger for this path. The linker path is kept only when the direct
    // image is unusable, which the guard already encodes:
    //   * `pe_image` is `None` — a library object (no `main`), a program needing
    //     the C runtime (`extern fn`), or a non-COFF target. For those the exe is
    //     produced by the linker, or the deliverable is an object, not an exe; so
    //     they fall through to the object + link path below. AND
    //   * `--debug` (`-g`) is NOT set — a debug build must keep the object +
    //     linker path so its CodeView `.debug$S`/PDB source-line info is produced;
    //     the direct PE image carries no debug info.
    // Non-exe deliverables (library objects, ELF/Mach-O objects) never reach here
    // because `pe_image` is `None` for them. See `native_backend_contract.md`.
    if is_coff
        && !debug
        && let Some(image) = program.pe_image.as_ref()
    {
        let exe = exe_output.as_ref().expect("COFF target has an exe path");
        if let Err(error) = fs::write(exe, image) {
            return Err(format_reports(
                &[write_failure_report(exe, error)],
                mode,
                None,
            ));
        }
        if mode == OutputMode::Json {
            println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
            return Ok(());
        }
        println!(
            "target: {} ({})",
            target.triple,
            object_format_label(&target)
        );
        if freestanding {
            println!(
                "freestanding (no-std): no C runtime linked; only kernel32!ExitProcess for process exit"
            );
        } else {
            println!("no C runtime linked; only kernel32!ExitProcess for process exit");
        }
        println!("native exe: {} (direct PE, no linker)", exe.display());
        if mode == OutputMode::Verbose {
            for name in &program.compiled {
                println!("compiled {name}");
            }
            for skip in &program.skipped {
                println!("skipped {}: {}", skip.name, skip.reason);
            }
        }
        return Ok(());
    }

    if let Err(error) = fs::write(&obj_output, &program.bytes) {
        return Err(format_reports(
            &[write_failure_report(&obj_output, error)],
            mode,
            None,
        ));
    }

    // Best-effort link (COFF only). Extra C import libraries (e.g. `ucrt.lib`)
    // are required when the program calls `extern fn` C functions. A program with
    // no `main` (only `export fn` functions) is a *library object* with no entry
    // point: there is nothing to link into a standalone `.exe`, so the CLI stops
    // at the object and reports that it is C-callable. Non-COFF targets are never
    // linked on this host.
    let is_library = program.entry_symbol.is_empty();
    let link = if !is_coff || is_library {
        None
    } else {
        Some(link_native_object(
            &obj_output,
            exe_output.as_ref().expect("COFF target has an exe path"),
            &program.entry_symbol,
            &program.import_libs,
        ))
    };

    if mode == OutputMode::Json {
        println!("{{\"status\":\"ok\",\"diagnostics\":[]}}");
        return Ok(());
    }

    println!("native object: {}", obj_output.display());
    println!(
        "target: {} ({})",
        target.triple,
        object_format_label(&target)
    );
    if freestanding {
        println!(
            "freestanding (no-std): no C runtime linked; only kernel32!ExitProcess for process exit"
        );
    }
    if debug {
        // The debug format follows the object container: CodeView is what the
        // Windows link+debug toolchain folds into a PDB, DWARF is what Linux/macOS
        // debuggers read. Both carry the same per-function `.lby` line mapping.
        let format = match target.object_format {
            NativeObjectFormat::Coff => "CodeView `.debug$S`",
            NativeObjectFormat::Elf | NativeObjectFormat::MachO => {
                "DWARF `.debug_line`/`.debug_info`/`.debug_abbrev`"
            }
        };
        println!(
            "debug info: {format} source-line table emitted (per-function `.lby` line mapping)"
        );
    }
    if !is_coff {
        use lullaby_ir::native_contract::NativeArchitecture;
        if matches!(target.architecture, NativeArchitecture::Aarch64) {
            // The aarch64 ELF is a real, freestanding relocatable object. It links
            // with a cross-linker (`rust-lld`/`ld.lld -m aarch64linux`) and runs
            // under arm64 emulation (Docker/QEMU); this is link+run verified in CI.
            println!(
                "aarch64 ELF object emitted; link with `rust-lld -flavor gnu -m aarch64linux` (or `ld.lld -m aarch64linux`) and run under arm64 (Docker/QEMU)"
            );
        } else {
            // x86-64 ELF/Mach-O: emitted and structurally well-formed, but this
            // Windows host has no cross-linker, so it is not linked or run here.
            println!(
                "cross-target object emitted; linking and running are deferred to the native platform / Phase 9 CI (structurally verified, x86-64 only)"
            );
        }
    }
    match &link {
        None if is_coff => {
            println!(
                "C-callable library object (no `main`): link it against a C program that calls the exported function(s)"
            );
        }
        None => {}
        Some(LinkOutcome::Linked) => {
            println!(
                "native exe: {}",
                exe_output
                    .as_ref()
                    .expect("linked COFF has exe path")
                    .display()
            );
        }
        Some(LinkOutcome::Unavailable(reason)) => {
            println!("linking unavailable: {reason}");
            println!("emitted object only (link manually with rust-lld + kernel32.lib)");
        }
        Some(LinkOutcome::Failed(reason)) => {
            println!("linking failed: {reason}");
        }
    }
    if mode == OutputMode::Verbose {
        for name in &program.compiled {
            println!("compiled {name}");
        }
        for skip in &program.skipped {
            println!("skipped {}: {}", skip.name, skip.reason);
        }
    }
    Ok(())
}

/// A short human-readable label for a native target's object-file container.
fn object_format_label(target: &lullaby_ir::native_contract::NativeTarget) -> &'static str {
    match target.object_format {
        NativeObjectFormat::Coff => "COFF",
        NativeObjectFormat::Elf => "ELF64",
        NativeObjectFormat::MachO => "Mach-O",
    }
}
