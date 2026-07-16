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

/// The whole raw-pointer surface is interpreter-only today: no raw-pointer builtin
/// has native codegen, so a function using `addr_of`/`ptr_offset`/`ptr_cast` is
/// native-ineligible and the native command *cleanly skips* it via the existing
/// `L0339` gate — a clean diagnostic naming the skipped function, never a
/// produced-but-wrong executable. Pinned here rather than left to prose.
#[test]
fn addr_of_cleanly_skips_native() {
    let fixture = workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_addr_of.lby");
    let output = lullaby()
        .args([
            "native",
            "--verbose",
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(
        !output.status.success(),
        "expected the L0339 no-eligible-function gate: {output:?}"
    );
    let errors = stderr(&output);
    assert!(
        errors.contains("L0339"),
        "expected the no-eligible-function skip diagnostic: {errors}"
    );
    assert!(
        errors.contains("skipped main"),
        "expected `main` to be skipped natively: {errors}"
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
