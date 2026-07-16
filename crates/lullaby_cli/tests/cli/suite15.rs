//! CLI integration tests, part 15 — the freestanding / kernel tier, stage 1:
//! the module-level `no-runtime` directive and its enforcement gate.
//!
//! Stage 1 delivers the tier gate + the allowed/rejected boundary. A module that
//! opens with the `no-runtime` directive is compiled in the freestanding tier;
//! semantic analysis rejects, with `L0441`, any construct that requires the
//! safe-tier runtime — a growable `list`/`map`, string building, an `rc`/`ref`
//! handle, an `actor`/`spawn`/`tell`, a heap closure, or a host-allocator builtin
//! (`alloc`/`dealloc`). What stays allowed is the safe-arena-kernel core: scalars,
//! fixed `array<T>`, structs/enums over scalar fields, control flow, functions,
//! and the raw hardware surface (`unsafe` + `ptr<T>` + the `ptr_*`/`volatile_*`
//! builtins).
//!
//! The enforcement is purely compile-time: a `no-runtime` program that stays in
//! the allowed subset type-checks and runs on every interpreter with normal
//! results, and — when its `main` is a native-eligible scalar function — still
//! compiles under `lullaby native --freestanding` (the directive is the semantic
//! gate; `--freestanding` is the orthogonal no-CRT output contract).
//!
//! The later freestanding stages (static-buffer arenas, inline-asm operand
//! binding, MMIO/port-IO, interrupt/`naked`/`entry` functions, the pluggable
//! panic handler, and direct-ELF/flat-binary output) are out of scope here.

use crate::*;

/// Run a valid fixture on `backend` and return the captured output.
fn run_backend(fixture: &str, backend: &str) -> std::process::Output {
    let path = workspace_root().join(fixture);
    lullaby()
        .args([
            "run",
            "--backend",
            backend,
            path.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli")
}

/// Assert that `fixture` is rejected by `lullaby check` with the freestanding-tier
/// violation diagnostic `L0441`.
fn assert_no_runtime_rejected(fixture: &str) {
    let path = workspace_root().join(fixture);
    let output = lullaby()
        .args(["check", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    let stderr = stderr(&output);
    assert!(
        !output.status.success(),
        "{fixture} should be rejected but exited 0. stderr: {stderr}"
    );
    assert!(
        stderr.contains("L0441"),
        "{fixture} should report the `no-runtime` violation L0441. stderr: {stderr}"
    );
}

/// Assert that a rejected `no-runtime` fixture ALSO fails to run on every
/// interpreter (the gate is enforced on the run path, not only `check`).
fn assert_no_runtime_rejected_on_interpreters(fixture: &str) {
    let path = workspace_root().join(fixture);
    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                path.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        let stderr = stderr(&output);
        assert!(
            !output.status.success(),
            "[{backend}] {fixture} should be rejected but ran. stderr: {stderr}"
        );
        assert!(
            stderr.contains("L0441"),
            "[{backend}] {fixture} should report L0441. stderr: {stderr}"
        );
    }
}

#[test]
fn no_runtime_scalar_core_runs_on_all_interpreters() {
    // A `no-runtime` module using only the allowed subset (scalars, a fixed
    // `array<i64>`, a scalar struct, control flow) type-checks and runs with the
    // same result on all three interpreters: manhattan((3,4)) + sum[1,4,9,16] = 37.
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(
            "tests/fixtures/valid/no_runtime/freestanding_scalar_core.lby",
            backend,
        );
        assert!(
            output.status.success(),
            "[{backend}] scalar-core no-runtime program should run: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "37",
            "[{backend}] scalar-core should compute 37"
        );
    }
}

#[test]
fn no_runtime_raw_pointer_surface_is_allowed() {
    // `unsafe` blocks and the raw `ptr<T>` / `ptr_read` / `ptr_write` builtins are
    // part of the freestanding core, NOT rejected by the gate. The module
    // type-checks and runs on every interpreter (main = 52).
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(
            "tests/fixtures/valid/no_runtime/freestanding_raw_pointer.lby",
            backend,
        );
        assert!(
            output.status.success(),
            "[{backend}] raw-pointer no-runtime program should run: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "52",
            "[{backend}] raw-pointer program should compute 52"
        );
    }
}

#[test]
fn no_runtime_scalar_core_compiles_freestanding_native() {
    // The scalar-core `main` is native-eligible, so `lullaby native --freestanding`
    // compiles it (the `no-runtime` directive composes with the existing
    // `--freestanding` no-CRT output path). When the linker + kernel32 are
    // available, the exit code equals the interpreter result (37).
    let fixture =
        workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_scalar_core.lby");
    let out = std::env::temp_dir().join("lullaby_no_runtime_scalar_core.exe");

    let emit = lullaby()
        .args([
            "native",
            "--freestanding",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        emit.status.success(),
        "freestanding native compile of a no-runtime scalar module should succeed: {}",
        stderr(&emit)
    );
    let listing = stdout(&emit);
    assert!(
        listing.contains("freestanding (no-std)"),
        "expected the freestanding no-CRT notice: {listing}"
    );
    assert!(
        listing.contains("compiled main"),
        "expected `main` compiled: {listing}"
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!(
            "rust-lld and/or kernel32.lib not available; skipping freestanding \
             link+run parity (compile check already ran)"
        );
        return;
    }
    assert!(out.is_file(), "expected linked exe at {}", out.display());
    let exe = std::process::Command::new(&out)
        .output()
        .expect("run native exe");
    let exit = exe.status.code().expect("native exit code");
    assert_eq!(
        exit, 37,
        "freestanding native exit code must equal the interpreter result"
    );
}

#[test]
fn no_runtime_growable_list_is_rejected() {
    // A growable `list<T>` push calls the host allocator — hidden allocation.
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/list_growth.lby");
    assert_no_runtime_rejected_on_interpreters("tests/fixtures/invalid/no_runtime/list_growth.lby");
}

#[test]
fn no_runtime_growable_map_is_rejected() {
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/map_growth.lby");
    assert_no_runtime_rejected_on_interpreters("tests/fixtures/invalid/no_runtime/map_growth.lby");
}

#[test]
fn no_runtime_string_building_is_rejected() {
    // Building a `string` at runtime (`to_string`) allocates a growable heap string.
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/string_build.lby");
    assert_no_runtime_rejected_on_interpreters(
        "tests/fixtures/invalid/no_runtime/string_build.lby",
    );
}

#[test]
fn no_runtime_rc_handle_is_rejected() {
    // Reference counting is a safe-tier-only tool (never present in `no-runtime`).
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/rc_handle.lby");
    assert_no_runtime_rejected_on_interpreters("tests/fixtures/invalid/no_runtime/rc_handle.lby");
}

#[test]
fn no_runtime_ref_handle_is_rejected() {
    // A borrowed `ref<T>` handle is part of the RC surface the tier drops.
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/ref_handle.lby");
    assert_no_runtime_rejected_on_interpreters("tests/fixtures/invalid/no_runtime/ref_handle.lby");
}

#[test]
fn no_runtime_actor_and_spawn_are_rejected() {
    // Actors need the runtime scheduler; the declaration, `spawn`, and `tell` are
    // each rejected.
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/actor_spawn.lby");
    assert_no_runtime_rejected_on_interpreters("tests/fixtures/invalid/no_runtime/actor_spawn.lby");
}

#[test]
fn no_runtime_heap_closure_is_rejected() {
    // A closure literal needs a heap-allocated capture environment.
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/heap_closure.lby");
    assert_no_runtime_rejected_on_interpreters(
        "tests/fixtures/invalid/no_runtime/heap_closure.lby",
    );
}

#[test]
fn no_runtime_host_allocator_builtin_is_rejected() {
    // `alloc` requests memory from the host allocator.
    assert_no_runtime_rejected("tests/fixtures/invalid/no_runtime/host_alloc.lby");
    assert_no_runtime_rejected_on_interpreters("tests/fixtures/invalid/no_runtime/host_alloc.lby");
}

#[test]
fn misplaced_no_runtime_directive_is_rejected() {
    // The directive is only valid as the first non-comment line of the module; a
    // later occurrence is a parse-time misplacement (`L0201`), not a declaration.
    let temp = std::env::temp_dir().join("lullaby_no_runtime_misplaced.lby");
    std::fs::write(&temp, "fn main -> i64\n    0\nno-runtime\n").expect("write temp");
    let output = lullaby()
        .args(["check", temp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    let stderr = stderr(&output);
    assert!(
        !output.status.success(),
        "a misplaced `no-runtime` directive should be rejected. stderr: {stderr}"
    );
    assert!(
        stderr.contains("L0201") && stderr.contains("no-runtime"),
        "expected an L0201 misplacement error naming the directive. stderr: {stderr}"
    );
}

#[test]
fn no_runtime_directive_survives_fmt_round_trip() {
    // `lullaby fmt` re-emits the `no-runtime` directive at the top of the module,
    // and re-formatting the output is a fixed point (idempotent).
    let path =
        workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_scalar_core.lby");
    let first = lullaby()
        .args(["fmt", path.to_str().expect("fixture path")])
        .output()
        .expect("run fmt");
    assert!(first.status.success(), "fmt failed: {}", stderr(&first));
    let formatted = stdout(&first);
    assert!(
        formatted.starts_with("no-runtime\n"),
        "fmt must re-emit the `no-runtime` directive first: {formatted}"
    );

    let temp = std::env::temp_dir().join("lullaby_no_runtime_fmt_idempotent.lby");
    std::fs::write(&temp, &formatted).expect("write temp");
    let second = lullaby()
        .args(["fmt", temp.to_str().expect("temp path")])
        .output()
        .expect("run fmt again");
    assert!(
        second.status.success(),
        "second fmt failed: {}",
        stderr(&second)
    );
    assert_eq!(stdout(&second), formatted, "fmt must be idempotent");
}

#[test]
fn ordinary_program_without_directive_is_unaffected() {
    // A program WITHOUT the `no-runtime` directive is completely unaffected by the
    // gate: it may freely use a growable `list`, and it runs normally.
    let temp = std::env::temp_dir().join("lullaby_no_runtime_absent.lby");
    std::fs::write(
        &temp,
        "fn main -> i64\n    let xs list<i64> = list_new()\n    xs = push(xs, 7)\n    len(xs)\n",
    )
    .expect("write temp");
    let output = lullaby()
        .args(["run", temp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    assert!(
        output.status.success(),
        "a non-`no-runtime` program using `list` should run: {}",
        stderr(&output)
    );
    assert_eq!(stdout(&output).trim(), "1", "one push -> len 1");
}

// --- Freestanding tier, stage 2: the raw-pointer addressing surface
// (`addr_of` / `ptr_offset` / `ptr_cast`). Interpreter-only, exactly like the
// delivered `ptr_read`/`int_to_ptr`/`volatile_*` builtins (a function using them
// is native/WASM-ineligible and cleanly skips). See
// `documents/freestanding_tier_design.md` §2.2. ---

#[test]
fn no_runtime_addr_of_surface_is_allowed_and_runs() {
    // A `no-runtime` module using `addr_of`/`ptr_offset`/`ptr_cast` over a local
    // array + struct is NOT rejected by the gate (they yield an allowed `ptr<T>`)
    // and runs identically on every interpreter. main = 100 (array sum) + 8 (size
    // law) + 10 (cast round-trip) + 9 (struct field) = 127.
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(
            "tests/fixtures/valid/no_runtime/freestanding_addr_of.lby",
            backend,
        );
        assert!(
            output.status.success(),
            "[{backend}] no-runtime addr_of program should run: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "127",
            "[{backend}] addr_of/ptr_offset/ptr_cast program should compute 127"
        );
    }
}

#[test]
fn raw_ptr_addressing_runs_on_all_interpreters() {
    // The safe-tier fixture: the addressing surface is available under `unsafe` in
    // both tiers. Covers a non-8-byte stride (`i32`) so the size law and a negative
    // offset are exercised. main = 11 + 4 + 3 = 18 on ast/ir/bytecode.
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend("tests/fixtures/valid/raw_ptr_addressing.lby", backend);
        assert!(
            output.status.success(),
            "[{backend}] raw-pointer addressing should run: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "18",
            "[{backend}] i32-stride walk + negative offset should compute 18"
        );
    }
}

/// Assert a fixture is rejected by `lullaby check` with a specific diagnostic code.
fn assert_check_rejected_with(fixture: &str, code: &str) {
    let path = workspace_root().join(fixture);
    let output = lullaby()
        .args(["check", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    let stderr = stderr(&output);
    assert!(
        !output.status.success(),
        "{fixture} should be rejected but exited 0. stderr: {stderr}"
    );
    assert!(
        stderr.contains(code),
        "{fixture} should report {code}. stderr: {stderr}"
    );
}

#[test]
fn addr_of_outside_unsafe_is_rejected() {
    // Every raw-pointer operation requires `unsafe`; `addr_of` outside one is the
    // existing unsafe-required diagnostic L0330.
    assert_check_rejected_with(
        "tests/fixtures/invalid/raw_ptr/addr_of_outside_unsafe.lby",
        "L0330",
    );
}

#[test]
fn addr_of_temporary_is_rejected() {
    // `addr_of` requires an addressable place; the address of a temporary is L0458.
    assert_check_rejected_with(
        "tests/fixtures/invalid/raw_ptr/addr_of_temporary.lby",
        "L0458",
    );
}

#[test]
fn ptr_offset_unsized_pointee_is_rejected() {
    // `ptr_offset` scales by size_of<T>, so an unsized pointee `T` is L0431.
    assert_check_rejected_with(
        "tests/fixtures/invalid/raw_ptr/ptr_offset_unsized.lby",
        "L0431",
    );
}

/// `addr_of` is place-backed, so a pointer genuinely **aliases** the place: writes
/// through it mutate the original, and reads through it observe independent writes to
/// the original. This is the property stage 2 could not provide (its region was a
/// by-value snapshot, so it refused stores with `L0459`), and retiring that refusal
/// is exactly what this fixture pins — on all three interpreters, byte-identically.
#[test]
fn addr_of_aliases_the_place_it_addresses() {
    let path = workspace_root().join("tests/fixtures/valid/raw_ptr_aliasing.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                path.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        let errors = stderr(&output);
        assert!(
            output.status.success(),
            "[{backend}] a write through an addr_of pointer must succeed and alias. \
             stderr: {errors}"
        );
        // 5 (write-through) + 99 (read-after-independent-write) + 300 (offset write)
        // + 60 (struct field) + 40 (whole-array decay) + 21 (nested s.arr[i]).
        assert_eq!(
            stdout(&output).trim(),
            "525",
            "[{backend}] addr_of must alias in both directions. stderr: {errors}"
        );
    }
}

/// A pointer that leaves the frame that took the address is **diagnosed, never
/// guessed**: the interpreters keep each frame's locals in that frame's own
/// environment, so such a pointer cannot be resolved, and `L0459` is raised rather
/// than reading or writing the wrong storage.
///
/// The two shapes pinned here are **not** the same kind of thing, and the diagnostics
/// say so:
///
/// - `addr_of_escapes_frame.lby` passes a pointer into a callee. That is **valid C**
///   (the out-parameter idiom) and the **native backend supports it** for the places
///   it lowers (an 8-byte scalar or a struct-field path); the
///   refusal is a limitation of the interpreter model, not a program error. This is a
///   known acceptance divergence between the tiers — loud on the interpreters, never
///   silent.
/// - `addr_of_outlives_frame.lby` returns a pointer to a dead local. That is a
///   genuine program error (undefined behaviour in C), correctly refused everywhere.
#[test]
fn an_addr_of_pointer_that_escapes_its_frame_is_refused() {
    for fixture in [
        "tests/fixtures/invalid/raw_ptr/addr_of_escapes_frame.lby",
        "tests/fixtures/invalid/raw_ptr/addr_of_outlives_frame.lby",
    ] {
        let path = workspace_root().join(fixture);
        for backend in ["ast", "ir", "bytecode"] {
            let output = lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    path.to_str().expect("fixture path"),
                ])
                .output()
                .expect("run cli");
            let errors = stderr(&output);
            assert!(
                !output.status.success(),
                "[{backend}] {fixture}: an escaped addr_of pointer must not silently \
                 resolve. stderr: {errors}"
            );
            assert!(
                errors.contains("L0459"),
                "[{backend}] {fixture}: expected the L0459 escaped/dangling refusal. \
                 stderr: {errors}"
            );
        }
    }
}

/// An `int_to_ptr` address that merely lands in the raw-pointer space (an MMIO
/// register, a fixed physical address) is **not** an `addr_of` pointer, and the
/// diagnostic must not blame one. It is unmapped: `L0406`, not `L0459`.
#[test]
fn an_mmio_address_is_diagnosed_as_unmapped_not_as_addr_of() {
    let path = workspace_root().join("tests/fixtures/invalid/raw_ptr/mmio_volatile_store.lby");
    for backend in ["ast", "ir", "bytecode"] {
        let output = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                path.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        let errors = stderr(&output);
        assert!(
            !output.status.success(),
            "[{backend}] an unmapped raw address must not be dereferenceable. \
             stderr: {errors}"
        );
        assert!(
            errors.contains("L0406"),
            "[{backend}] expected L0406 for an unmapped int_to_ptr address. \
             stderr: {errors}"
        );
        // The message may *mention* `addr_of` to explain what this address is not;
        // what it must never do is assert that an `addr_of` pointer is involved, as
        // the stage-2 L0459 refusal wrongly did for this exact program.
        assert!(
            !errors.contains("L0459") && !errors.contains("this `addr_of` pointer"),
            "[{backend}] the diagnostic must not blame an `addr_of` pointer that was \
             never taken. stderr: {errors}"
        );
    }
}

/// The full stage-2 raw-pointer ADDRESSING surface now compiles natively and
/// agrees with all three interpreters: `addr_of(a[0])` walked with a RUNTIME
/// `ptr_offset(base, i)` in a `while` loop, the size law, a `ptr<i64>` ->
/// `ptr<byte>` -> `ptr<i64>` cast round-trip, and an `addr_of` of a struct field.
///
/// This shape used to be *refused* (`L0339`) because the native frame laid an
/// aggregate's words out at DESCENDING addresses, so the pointer would have
/// walked backwards — disagreeing with C, with `size_of`/`offset_of`, and with
/// the interpreters, on a program the interpreters define. The layout now ASCENDS
/// (C-compatible), so the refusal is lifted and the program is simply correct.
#[test]
fn addr_of_addressing_surface_matches_the_interpreters() {
    let fixture = "tests/fixtures/valid/no_runtime/freestanding_addr_of.lby";
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(fixture, backend);
        assert!(
            output.status.success(),
            "[{backend}] the addr_of addressing surface must run: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "127",
            "[{backend}] expected 100 + 8 + 10 + 9 = 127"
        );
    }
    let Some(exit) = native_exit_code(fixture, "lullaby_addr_of_surface.exe", &[]) else {
        return;
    };
    assert_eq!(
        exit, 127,
        "native must agree with all three interpreters on the addr_of \
         addressing surface (array walk, size law, ptr_cast, struct field)"
    );
}

/// The WASM backend likewise cleanly skips the raw-pointer addressing surface with
/// the existing `L0338` gate (the freestanding tier targets bare-metal native, not
/// WASM), never miscompiling it.
#[test]
fn addr_of_cleanly_skips_wasm() {
    let fixture = workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_addr_of.lby");
    let output = lullaby()
        .args(["wasm", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(
        !output.status.success(),
        "expected the L0338 no-eligible-function gate: {output:?}"
    );
    assert!(
        stderr(&output).contains("L0338"),
        "expected the WASM no-eligible-function skip diagnostic: {}",
        stderr(&output)
    );
}

// ---------------------------------------------------------------------------
// Freestanding stage 3 — NATIVE raw-pointer codegen.
//
// The whole raw-pointer surface now has native x86-64 codegen, so a function
// using it is native-ELIGIBLE instead of skipping. These tests compile real
// `.exe`s (the direct-PE writer needs no linker) and check their exit codes.
//
// The headline: `ptr_write(addr_of(x), 5)` genuinely mutates `x` on native,
// which the interpreters cannot model — they refuse the store with a runtime
// `L0459` (see `ptr_write_through_addr_of_is_refused_at_runtime` above). That
// asymmetry is intentional and honest: a loud refusal on the tiers that cannot
// alias, real aliasing on the tier that can. Retiring `L0459` (making the
// interpreter model place-backed) is separate, parallel work.
// ---------------------------------------------------------------------------

/// Compile `fixture` to a native `.exe` at `out_name` and return its exit code.
/// The direct-PE writer produces a runnable image with no external linker and no
/// CRT, so this needs no toolchain gate — only a Windows host to execute on.
/// Returns `None` (with a printed note) when the host cannot run a PE image.
fn native_exit_code(fixture: &str, out_name: &str, extra_args: &[&str]) -> Option<i32> {
    let path = workspace_root().join(fixture);
    let out = std::env::temp_dir().join(out_name);
    let _ = std::fs::remove_file(&out);

    let mut args: Vec<&str> = vec!["native", "--verbose", "-o"];
    let out_str = out.to_str().expect("out path").to_string();
    args.push(&out_str);
    args.extend_from_slice(extra_args);
    let fixture_str = path.to_str().expect("fixture path").to_string();
    args.push(&fixture_str);

    let emit = lullaby().args(&args).output().expect("run native");
    assert!(
        emit.status.success(),
        "{fixture} must compile natively (no L0339 skip): {}",
        stderr(&emit)
    );
    assert!(
        stdout(&emit).contains("compiled main"),
        "{fixture}: `main` must be native-eligible, not skipped: {}{}",
        stdout(&emit),
        stderr(&emit)
    );

    if !cfg!(windows) {
        eprintln!("not a Windows host; skipping the run of {out_name}");
        return None;
    }
    assert!(
        out.is_file(),
        "expected a direct-PE exe at {}",
        out.display()
    );
    let run = std::process::Command::new(&out)
        .output()
        .expect("run native exe");
    Some(run.status.code().expect("native exit code"))
}

/// THE headline proof: native `addr_of` produces a REAL address, so a
/// `ptr_write` through it genuinely mutates the local. `x` starts at 1 and the
/// process must exit 5.
///
/// This is precisely the semantics the interpreters cannot model — they refuse
/// this exact program with `L0459` rather than return 1. Native is the tier that
/// aliases correctly.
#[test]
fn native_addr_of_write_through_mutates_the_local() {
    let Some(exit) = native_exit_code(
        "tests/fixtures/native_only/raw_ptr_addr_of_alias.lby",
        "lullaby_rawptr_alias.exe",
        &[],
    ) else {
        return;
    };
    assert_eq!(
        exit, 5,
        "ptr_write(addr_of(x), 5) must genuinely mutate `x` on native (1 would mean the \
         store did not alias)"
    );
}

/// The register-promotion / address-taken hazard, proven behaviourally. `acc` is
/// an otherwise-promotable hot loop accumulator whose address is taken and
/// written through. Exit 145 means the store landed; 45 would mean `acc` was
/// promoted into `rbx`/`rsi` and the `lea`'d frame slot was dead — a silent
/// miscompile.
#[test]
fn native_addr_of_defeats_register_promotion() {
    let Some(exit) = native_exit_code(
        "tests/fixtures/native_only/raw_ptr_promotion_hazard.lby",
        "lullaby_rawptr_promotion.exe",
        &[],
    ) else {
        return;
    };
    assert_eq!(
        exit, 145,
        "an address-taken local must live in its frame slot; 45 would mean the promoted-local \
         miscompile (the store through addr_of was lost)"
    );
}

/// `volatile_load`/`volatile_store` are genuinely non-eliding. The fixture is
/// built so that any folding, hoisting, CSE, or removal changes the answer: a
/// CSE'd second read or a hoisted loop read gives far less than 71.
#[test]
fn native_volatile_accesses_are_not_elided() {
    let Some(exit) = native_exit_code(
        "tests/fixtures/native_only/raw_ptr_volatile_no_elision.lby",
        "lullaby_rawptr_volatile.exe",
        &[],
    ) else {
        return;
    };
    assert_eq!(
        exit, 71,
        "every volatile access must be a real machine load/store: 1 + 2 + (2+12+22+32) = 71"
    );
}

/// The pointer size law `ptr_to_int(ptr_offset(p, 1)) - ptr_to_int(p) ==
/// size_of(T)` holds natively for several `T`, and a negative `n` walks back.
#[test]
fn native_ptr_offset_obeys_the_size_law() {
    let Some(exit) = native_exit_code(
        "tests/fixtures/native_only/raw_ptr_size_law.lby",
        "lullaby_rawptr_sizelaw.exe",
        &[],
    ) else {
        return;
    };
    assert_eq!(
        exit, 47,
        "size law: 8 (i64) + 4 (i32) + 2 (i16) + 1 (byte) + 8 (ptr<i64>) + 24 (a -3 walk) = 47"
    );
}

/// `addr_of(s.f)` write-through and a `ptr_cast` round-trip both land in the
/// struct itself: hi = 33, lo = 7 + 2 = 9 -> 42.
#[test]
fn native_addr_of_struct_field_writes_through() {
    let Some(exit) = native_exit_code(
        "tests/fixtures/native_only/raw_ptr_struct_field_write.lby",
        "lullaby_rawptr_field.exe",
        &[],
    ) else {
        return;
    };
    assert_eq!(
        exit, 42,
        "addr_of(s.f) stores must land in the struct: 33 + 9"
    );
}

/// A `no-runtime` module using the raw-pointer surface compiles under
/// `lullaby native --freestanding` (no CRT linked) and runs. `ptr<T>` values
/// cross the call ABI in a GPR both as a parameter and as a return value.
#[test]
fn native_freestanding_module_uses_the_raw_pointer_surface() {
    let Some(exit) = native_exit_code(
        "tests/fixtures/valid/no_runtime/freestanding_native_rawptr.lby",
        "lullaby_rawptr_freestanding.exe",
        &["--freestanding"],
    ) else {
        return;
    };
    assert_eq!(
        exit, 42,
        "the freestanding raw-pointer module must run: 7 * 6"
    );
}

/// Where the interpreters CAN run a shape — reads and `ptr_offset`/`ptr_cast`
/// walks, but not `addr_of` stores — native must agree with them exactly.
#[test]
fn native_addr_of_reads_match_the_interpreters() {
    let fixture = "tests/fixtures/valid/no_runtime/freestanding_addr_of_reads.lby";
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(fixture, backend);
        assert!(
            output.status.success(),
            "[{backend}] addr_of reads must run on the interpreters: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "44",
            "[{backend}] expected 7 + 9 + 20 + 8 = 44"
        );
    }
    let Some(exit) = native_exit_code(fixture, "lullaby_rawptr_reads.exe", &[]) else {
        return;
    };
    assert_eq!(
        exit, 44,
        "native must agree with all three interpreters on addr_of reads + the size law"
    );
}

/// THE KERNEL IDIOM, end to end: `addr_of(buf[0])` + `ptr_offset` walks a fixed
/// buffer forward, and the produced `.exe` agrees with all three interpreters.
///
/// This is the payoff of the ASCENDING (C-compatible) native aggregate layout:
/// element `k` sits at `base + 8*k`, so `ptr_offset(p, +1)` steps FORWARD to the
/// next element. Under the previous DESCENDING layout this shape was refused
/// outright (`L0339`) rather than miscompiled, because the pointer would have
/// walked backwards — disagreeing with C, with `size_of`/`offset_of`, and with
/// the interpreters, on a program the interpreters define.
///
/// Reads and `ptr_offset` walks through an `addr_of` pointer ARE modelled by the
/// interpreters, so this is a genuine parity assertion, not a native-only claim.
#[test]
fn native_buffer_walk_matches_the_interpreters() {
    let fixture = "tests/fixtures/valid/no_runtime/freestanding_buffer_walk.lby";
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(fixture, backend);
        assert!(
            output.status.success(),
            "[{backend}] the buffer walk must run on the interpreters: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "124",
            "[{backend}] expected 100 + 3 + 13 + 8 = 124"
        );
    }
    let Some(exit) = native_exit_code(fixture, "lullaby_buffer_walk.exe", &[]) else {
        return;
    };
    assert_eq!(
        exit, 124,
        "native must agree with all three interpreters on the buffer walk \
         (a mismatch means the aggregate layout no longer ascends)"
    );
}

/// The same buffer walk under `--freestanding`: a `no-runtime` module walking a
/// buffer compiles to a direct-PE `.exe` with no C runtime and no external
/// linker, and still exits 124. This is the kernel-tier delivery shape.
#[test]
fn native_freestanding_buffer_walk_runs() {
    let fixture = "tests/fixtures/valid/no_runtime/freestanding_buffer_walk.lby";
    let Some(exit) = native_exit_code(
        fixture,
        "lullaby_buffer_walk_freestanding.exe",
        &["--freestanding"],
    ) else {
        return;
    };
    assert_eq!(
        exit, 124,
        "a freestanding no-runtime buffer walk must exit 124"
    );
}

/// A WRITE-THROUGH buffer walk: `ptr_write` each cell through a forward-walked
/// pointer, then read the buffer back by index. Asserted on ALL FOUR tiers: the
/// stores genuinely alias the buffer, and they land on the RIGHT cells (a
/// descending native layout would write backwards off the front of the buffer
/// instead of filling it, and a snapshot-backed `addr_of` would drop the writes).
///
/// The interpreters answer this too: `addr_of` is place-backed, so an in-frame
/// store aliases for real. (An earlier revision of this test pinned an `L0459`
/// refusal here — correct when the interpreters' `addr_of` was a by-value
/// snapshot, stale once it became place-backed. The refusal now applies only to
/// a pointer used outside the frame owning its place, which this program never
/// does.)
#[test]
fn native_buffer_write_through_walk_aliases_the_buffer() {
    let fixture = "tests/fixtures/valid/no_runtime/freestanding_buffer_write.lby";
    // Every interpreter aliases the buffer and agrees with native.
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(fixture, backend);
        assert!(
            output.status.success(),
            "[{backend}] an in-frame addr_of store must alias, not fail: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "105",
            "[{backend}] the write-through walk must fill the buffer forward (15) and poke index 1 (90)"
        );
    }
    let Some(exit) = native_exit_code(fixture, "lullaby_buffer_write.exe", &["--freestanding"])
    else {
        return;
    };
    assert_eq!(
        exit, 105,
        "the write-through walk must fill the buffer forward (15) and poke index 1 (90)"
    );
}

// ---------------------------------------------------------------------------
// Freestanding stage 3: MMIO (delivered by composition) and port-mapped I/O.
// ---------------------------------------------------------------------------

/// Compile `fixture` under `lullaby native --freestanding` and return the
/// `--verbose` listing. **The emitted executable is deliberately NOT run** —
/// every caller here compiles code that touches hardware (an unmapped physical
/// address, or a privileged `in`/`out`), which faults in a user-mode test
/// process. Compilation plus the emitted bytes are the evidence; see
/// `crates/lullaby_ir/src/native_object_portio_tests.rs` for the byte assertions.
fn compile_freestanding_only(fixture: &str, exe_name: &str) -> String {
    let path = workspace_root().join(fixture);
    let out = std::env::temp_dir().join(exe_name);
    let emit = lullaby()
        .args([
            "native",
            "--freestanding",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            path.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        emit.status.success(),
        "{fixture} must compile under `native --freestanding`: {}",
        stderr(&emit)
    );
    let listing = stdout(&emit);
    assert!(
        listing.contains("freestanding (no-std)"),
        "{fixture}: expected the freestanding no-CRT notice: {listing}"
    );
    assert!(
        out.is_file(),
        "{fixture}: expected a direct-PE exe at {}",
        out.display()
    );
    listing
}

/// **MMIO works, and needs no intrinsics of its own.** The VGA text-buffer poke —
/// `int_to_ptr(0xB8000)` + `ptr_offset` + `volatile_store` in a void function —
/// compiles natively in a `no-runtime` module today, purely by composition of the
/// delivered raw-pointer surface. A device register mapped into the address space
/// IS memory, so pointers reach it.
///
/// This pins that shape so it cannot regress silently. It was working but
/// untested, which is how a "delivered" capability quietly rots.
///
/// **Not executed:** 0xB8000 is not mapped in a user-mode process; writing it
/// faults. Compilation is the whole claim.
#[test]
fn mmio_vga_poke_compiles_freestanding_native() {
    let listing = compile_freestanding_only(
        "tests/fixtures/valid/no_runtime/freestanding_mmio_vga.lby",
        "lullaby_mmio_vga.exe",
    );
    for symbol in ["compiled vga_put", "compiled vga_get", "compiled main"] {
        assert!(
            listing.contains(symbol),
            "MMIO composes from int_to_ptr + ptr_offset + volatile_store + void fns, \
             so `{symbol}` is expected: {listing}"
        );
    }
}

/// Every port builtin — both port forms, all three widths, read and write —
/// compiles in a `no-runtime` module under `--freestanding`.
///
/// **Not executed, and that is not a gap in the testing.** `in`/`out` are
/// privileged: they raise a general-protection fault at CPL 3 unless IOPL or the
/// TSS I/O bitmap grants access, and no device sits behind these ports in a test
/// harness regardless. Running the exe would crash the harness, not verify it.
/// The correctness evidence for the emitted instructions is the byte-level
/// assertions in `native_object_portio_tests.rs`.
#[test]
fn port_io_compiles_freestanding_native() {
    let listing = compile_freestanding_only(
        "tests/fixtures/valid/no_runtime/freestanding_port_io.lby",
        "lullaby_port_io.exe",
    );
    for symbol in [
        "compiled pic_eoi",
        "compiled pic_read_mask",
        "compiled read_word_imm",
        "compiled write_dword_imm",
        "compiled serial_write",
        "compiled serial_read",
        "compiled serial_ready",
        "compiled read_word",
        "compiled write_word",
        "compiled read_dword",
        "compiled write_dword",
        "compiled main",
    ] {
        assert!(
            listing.contains(symbol),
            "every port-I/O shape must be native-eligible; `{symbol}` is expected: {listing}"
        );
    }
}

/// **The interpreters refuse port I/O — they do not fake it.**
///
/// `in`/`out` address the CPU's I/O port space, which the AST/IR/bytecode tiers
/// do not model. There is no honest value to return, so each refuses with
/// `L0444` rather than fabricate one: a plausible-but-wrong device read (an
/// invented `0`, say) would silently mis-drive a PIC/PIT/UART, which is far worse
/// than a loud refusal.
///
/// This is an honest **acceptance divergence**, not a parity claim — native
/// compiles this program, the interpreters decline to define it — framed exactly
/// like the cross-frame `addr_of` divergence (`L0459`).
#[test]
fn port_io_is_refused_on_every_interpreter() {
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(
            "tests/fixtures/valid/no_runtime/freestanding_port_io.lby",
            backend,
        );
        let errors = stderr(&output);
        assert!(
            !output.status.success(),
            "[{backend}] port I/O must not appear to succeed on an interpreter. \
             stderr: {errors}"
        );
        assert!(
            errors.contains("L0444"),
            "[{backend}] expected the L0444 port-I/O native-only refusal. stderr: {errors}"
        );
        assert!(
            errors.contains("port_out8"),
            "[{backend}] the refusal must name the offending builtin. stderr: {errors}"
        );
    }
}

/// Port I/O is a raw hardware operation and needs an `unsafe` block, reusing the
/// delivered raw-operation gate `L0330` — no new code, because that is precisely
/// what `L0330` already means.
#[test]
fn port_io_outside_unsafe_is_rejected() {
    let path = workspace_root().join("tests/fixtures/invalid/port_io/port_io_outside_unsafe.lby");
    let output = lullaby()
        .args(["check", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    let errors = stderr(&output);
    assert!(
        !output.status.success(),
        "port I/O outside `unsafe` must be rejected. stderr: {errors}"
    );
    assert!(
        errors.contains("L0330"),
        "expected the L0330 unsafe gate. stderr: {errors}"
    );
}

/// A wrong port width or data width is `L0442`.
///
/// Both halves matter. A **port** is `u16` — the architectural port space is
/// exactly `0..=0xFFFF` — and Lullaby has no implicit coercion, so an unsuffixed
/// `i64` literal is rejected rather than silently truncated. A **data** width is
/// fixed by the builtin's name, so a `u32` value passed to `port_out8` is a
/// definite error, not an inference puzzle.
#[test]
fn port_io_width_errors_are_rejected() {
    for (fixture, expected) in [
        (
            "tests/fixtures/invalid/port_io/port_number_wrong_width.lby",
            "u16",
        ),
        (
            "tests/fixtures/invalid/port_io/port_data_wrong_width.lby",
            "u8",
        ),
    ] {
        let path = workspace_root().join(fixture);
        let output = lullaby()
            .args(["check", path.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        let errors = stderr(&output);
        assert!(
            !output.status.success(),
            "{fixture} must be rejected. stderr: {errors}"
        );
        assert!(
            errors.contains("L0442"),
            "{fixture}: expected the L0442 port-width diagnostic. stderr: {errors}"
        );
        assert!(
            errors.contains(expected),
            "{fixture}: the diagnostic must name the required width `{expected}`. \
             stderr: {errors}"
        );
    }
}

/// **`L0441` allows port I/O in a `no-runtime` module.** Port builtins are kernel
/// core, exactly like `ptr_read`/`volatile_*`: they name only `u8`/`u16`/`u32`,
/// allocate nothing, and add no hidden control flow, so the freestanding gate
/// (which rejects heap/runtime *types* and the host-allocator builtins) must not
/// touch them.
///
/// Pinned by observing that the port fixture type-checks clean — no `L0441` — and
/// that its only complaint at run time is the `L0444` native-only refusal, which
/// proves it got all the way through the gate to execution.
#[test]
fn port_io_is_available_in_a_no_runtime_module() {
    let path = workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_port_io.lby");
    let output = lullaby()
        .args(["check", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    let errors = stderr(&output);
    assert!(
        output.status.success(),
        "a `no-runtime` module using port I/O must type-check: {errors}"
    );
    assert!(
        !errors.contains("L0441"),
        "the freestanding gate must NOT reject port I/O — it is kernel core, like \
         `ptr_read`/`volatile_*`. stderr: {errors}"
    );
}
