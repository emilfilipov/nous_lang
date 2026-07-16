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

#[test]
fn ptr_write_through_addr_of_is_refused_at_runtime() {
    // A store through an `addr_of` pointer would only mutate the interpreters'
    // by-value snapshot — `x` would stay 1 where real native addressing gives 5. It
    // is refused with L0459 on every interpreter rather than silently returning the
    // wrong answer. (Temporary: the native raw-pointer codegen increment makes
    // `addr_of` place-backed and lifts this.) Reads/walks stay supported, and a
    // store through an `alloc`/`int_to_ptr` heap-slot pointer is unaffected.
    let path =
        workspace_root().join("tests/fixtures/invalid/raw_ptr/ptr_write_through_addr_of.lby");
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
            "[{backend}] a store through an addr_of pointer must not silently succeed. \
             stderr: {errors}"
        );
        assert!(
            errors.contains("L0459"),
            "[{backend}] expected the L0459 unmodelled-store refusal. stderr: {errors}"
        );
    }
}

/// The raw-pointer surface HAS native codegen (see the stage-3 tests at the end
/// of this file), but `addr_of` of an **array element** is deliberately outside
/// it: the native frame lays an aggregate's words out at DESCENDING addresses, so
/// a pointer into an array would walk backwards under `ptr_offset` — disagreeing
/// with C, with `size_of`/`offset_of`, and with the interpreters' ascending
/// snapshot model, on a program the interpreters define. This fixture walks
/// `addr_of(a[0])`, so the native command *cleanly skips* it via the `L0339` gate
/// — a diagnostic naming the skipped function, never a produced-but-wrong
/// executable. Pinned here rather than left to prose.
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
