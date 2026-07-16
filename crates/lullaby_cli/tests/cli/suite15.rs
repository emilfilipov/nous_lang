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

/// A **genuinely dangling** `addr_of` pointer is diagnosed, never guessed: the place
/// it named no longer exists, so `L0459` is raised rather than reading storage the
/// program no longer owns.
///
/// All three shapes are undefined behaviour in C (C11 6.2.4p6 ties an automatic
/// object's lifetime to its *block*), so refusing them forbids no defined program:
///
/// - `addr_of_outlives_frame.lby` returns a pointer to a dead local — the frame
///   returned, so `RawPointerMemory::exit_frame` dropped its regions.
/// - `addr_of_inner_block_dies.lby` uses a pointer to an inner-block local after that
///   block ended — the region survives (its frame is alive) but the *scope* is gone,
///   so `Env::at` misses.
/// - `addr_of_loop_body_dies.lby` is the same, for a loop-body local after the loop.
///
/// This is what `L0459` means after the env shelf, and essentially all it means:
/// passing an address *into a callee* is valid C and now resolves for real on every
/// tier (`cross_frame_addr_of_reaches_the_callers_place`). What is left refusing is
/// code whose place has actually ceased to exist.
///
/// The refusal must never soften into a stale read. Each of these would report a
/// plausible-looking value (`7`, `42`, `9`) out of storage the program has released —
/// a wrong answer dressed as a right one, which is the exact failure mode the
/// place-backed model exists to prevent.
#[test]
fn a_dangling_addr_of_pointer_is_refused_not_read() {
    for fixture in [
        "tests/fixtures/invalid/raw_ptr/addr_of_outlives_frame.lby",
        "tests/fixtures/invalid/raw_ptr/addr_of_inner_block_dies.lby",
        "tests/fixtures/invalid/raw_ptr/addr_of_loop_body_dies.lby",
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
                "[{backend}] {fixture}: a dangling addr_of pointer must not silently \
                 resolve. stderr: {errors}"
            );
            assert!(
                errors.contains("L0459"),
                "[{backend}] {fixture}: expected the L0459 dangling refusal. \
                 stderr: {errors}"
            );
            // The refusal must name the dead place, not an anonymous bad address —
            // that is what the tombstone/scope bookkeeping buys.
            assert!(
                errors.contains("already ended"),
                "[{backend}] {fixture}: the L0459 text must say the place's block or \
                 function has ended. stderr: {errors}"
            );
        }
    }
}

/// **Cross-frame `addr_of` resolves for real.** A pointer passed into a callee names
/// the caller's live place, so the callee reads and writes the caller's actual
/// storage — the out-parameter idiom (`scanf("%d", &x)`, `strtol(s, &end, 10)`).
///
/// This is well-defined C, not undefined behaviour: C11 6.2.4p6 ties an automatic
/// object's lifetime to its *block*, and calling a function does not end the caller's
/// block. Native has always compiled it (`addr_of` is a real `lea`); the interpreters
/// reach the caller's environment through the **env shelf** (see
/// `lullaby_runtime::raw_pointer`), which retired the acceptance divergence that used
/// to make these programs `L0459` on the interpreters while native ran them.
///
/// The two fixtures split by native eligibility:
///
/// - `raw_ptr_cross_frame.lby` (432) covers the surface broadly — out-parameter
///   write-through, a callee read *after a later independent write* (the case a
///   snapshot would get wrong), two frames deep, a buffer walked and filled in a
///   callee, whole-array decay, the size law at two element widths, negative offsets,
///   and region dedup in a loop. Its `i32` buffer puts it outside the native
///   i64-scalar subset, so native skips it cleanly with `L0339`.
/// - `freestanding_cross_frame.lby` (327) stays inside that subset, so **all four
///   tiers** run it and must agree bit-for-bit — pinning the interpreters' shelf
///   against native's real machine addresses.
#[test]
fn cross_frame_addr_of_reaches_the_callers_place() {
    for (fixture, expected) in [
        ("tests/fixtures/valid/raw_ptr_cross_frame.lby", "432"),
        (
            "tests/fixtures/valid/no_runtime/freestanding_cross_frame.lby",
            "327",
        ),
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
                output.status.success(),
                "[{backend}] {fixture}: passing an addr_of pointer into a callee is \
                 valid C and must run. stderr: {errors}"
            );
            assert_eq!(
                stdout(&output).trim(),
                expected,
                "[{backend}] {fixture}: a callee must read and write the caller's real \
                 place. stderr: {errors}"
            );
        }
    }
}

/// An `addr_of` pointer sent to **another thread** is refused — and refused as
/// `L0406`, not `L0459`.
///
/// The env shelf reaches a *caller's* frame, which is same-thread by construction. A
/// `spawn`/`async fn` builds its own interpreter with its own `RawPointerMemory`, so
/// the address names no region there at all: it is unmapped, not dangling. This is
/// pinned because the distinction is easy to get wrong in the docs (an earlier draft of
/// this very change claimed `L0459` here), and because the property that actually
/// matters — **a stack address never silently resolves on the wrong thread** — must not
/// regress into a read of unrelated storage.
#[test]
fn an_addr_of_pointer_cannot_cross_a_thread_boundary() {
    let source = "async fn worker p ptr<i64> -> i64\n\
                  \x20   unsafe\n\
                  \x20       ptr_read(p)\n\
                  \n\
                  fn main -> i64\n\
                  \x20   let x i64 = 7\n\
                  \x20   unsafe\n\
                  \x20       let p ptr<i64> = addr_of(x)\n\
                  \x20       let f = worker(p)\n\
                  \x20       await f\n";
    let scratch = ScratchDir::new("cross_thread_addr_of");
    let path = scratch.join("cross_thread_addr_of.lby");
    std::fs::write(&path, source).expect("write fixture");
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
            "[{backend}] a stack address must not resolve on another thread. \
             stderr: {errors}"
        );
        assert!(
            errors.contains("L0406"),
            "[{backend}] a cross-thread addr_of pointer names no region in the child \
             interpreter's address space, so it is unmapped (L0406), not dangling \
             (L0459). stderr: {errors}"
        );
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

/// **Native agrees with the env shelf on cross-frame `addr_of`.** The fourth tier for
/// `cross_frame_addr_of_reaches_the_callers_place`: `freestanding_cross_frame.lby`
/// stays inside the native i64-scalar subset, so native compiles every one of its
/// out-parameter / callee-read / two-frames-deep / buffer-walk shapes to real `lea`
/// addressing and must land on exactly the interpreters' 327.
///
/// This is the assertion that keeps the shelf honest. The interpreters model memory
/// abstractly; native does not. If the shelf ever resolved to the wrong storage — a
/// copy, a stale scope, the wrong frame's binding — the two would part company here.
#[test]
fn native_cross_frame_addr_of_matches_the_interpreters() {
    let fixture = "tests/fixtures/valid/no_runtime/freestanding_cross_frame.lby";
    let Some(exit) = native_exit_code(
        fixture,
        "lullaby_cross_frame_freestanding.exe",
        &["--freestanding"],
    ) else {
        return;
    };
    assert_eq!(
        exit, 327,
        "native `addr_of` is a real `lea`, so it must agree with the interpreters' env \
         shelf on every cross-frame shape"
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

// ---------------------------------------------------------------------------
// Freestanding tier §5 — static-buffer arenas.
//
// The feature that gives a `no-runtime` module somewhere bounded to put data:
// `alloc`/`list`/`map`/`string`/`rc` are all `L0441`-rejected in this tier by
// design, so without an arena a driver can talk to hardware but has nowhere to
// keep what it reads. See `documents/freestanding_tier_design.md` §5.
// ---------------------------------------------------------------------------

/// A static-buffer arena is **available in a `no-runtime` module** — the whole
/// point of the feature, and the thing most at risk of regressing.
///
/// `L0441` rejects every construct that needs the host allocator. An arena needs
/// none: its memory is a fixed `array<i64>` the caller already declared, and its
/// bump cursor is a frame word. If a future change to the tier gate started
/// rejecting this, the freestanding tier would lose its only bounded storage — so
/// this asserts `check` accepts it rather than merely that some diagnostic differs.
#[test]
fn arena_is_available_in_a_no_runtime_module() {
    let path =
        workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_arena_alloc.lby");
    let output = lullaby()
        .args(["check", path.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(
        output.status.success(),
        "a static-buffer arena allocates from a caller-owned buffer, never the host \
         allocator, so the `no-runtime` gate (`L0441`) must not reject it: {}",
        stderr(&output)
    );
}

/// The arena **runs natively**: bumping cells out of a caller-owned buffer,
/// writing and reading through the returned pointers, and composing with a loop.
///
/// Native is the tier that matters for §5 — a kernel targets bare metal. The exit
/// code is the real evidence: `two_cells` (42) proves two allocations do not alias,
/// `loop_sum` (60) proves the arena composes with a loop, and `block` (7) proves a
/// multi-cell request is walkable with `ptr_offset`.
#[test]
fn arena_alloc_runs_freestanding_native() {
    let fixture =
        workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_arena_alloc.lby");
    let out = std::env::temp_dir().join("lullaby_arena_alloc.exe");
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
        "a no-runtime arena module must compile under `native --freestanding`: {}",
        stderr(&emit)
    );
    let listing = stdout(&emit);
    for symbol in ["compiled two_cells", "compiled loop_sum", "compiled block"] {
        assert!(
            listing.contains(symbol),
            "the arena must be natively lowered, not skipped — expected `{symbol}`: {listing}"
        );
    }

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32 unavailable; skipping arena run (compile check ran)");
        return;
    }
    let exe = std::process::Command::new(&out)
        .output()
        .expect("run native exe");
    assert_eq!(
        exe.status.code().expect("native exit code"),
        109,
        "arena result: two_cells (42, distinct allocations do not alias) + loop_sum (60, \
         the arena composes with a loop) + block (7, a multi-cell request walked with \
         ptr_offset)"
    );
}

/// **The overflow edge.** An arena never grows and never calls an allocator, so a
/// bump past the buffer's end must go somewhere defined. As delivered that is
/// `ud2` — an invalid-opcode trap, the same edge the native bounds check uses.
///
/// This asserts **what is actually observable**: the process dies on the trap
/// rather than exiting. It deliberately does NOT assert a clean exit code (there
/// is none — the program never reaches its return), and it cannot hang: `ud2`
/// faults immediately. The key assertion is that the run does **not** produce 6,
/// the value `exhaust` would return if the third allocation had wrongly succeeded —
/// that is what distinguishes a real trap from a silently-handed-out bad pointer.
///
/// §8 replaces the trap with a call to the program's own `panic fn`; the range
/// check and the bump are already final.
#[test]
fn arena_overflow_hits_the_panic_edge_natively() {
    let fixture =
        workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_arena_overflow.lby");
    let out = std::env::temp_dir().join("lullaby_arena_overflow.exe");
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
        "the overflow fixture must compile: {}",
        stderr(&emit)
    );
    assert!(
        stdout(&emit).contains("compiled exhaust"),
        "the overflow path must be natively lowered, not skipped: {}",
        stdout(&emit)
    );

    if rust_lld_path().is_none() || !kernel32_available() {
        eprintln!("rust-lld/kernel32 unavailable; skipping overflow run (compile check ran)");
        return;
    }
    let exe = std::process::Command::new(&out)
        .output()
        .expect("run native exe");
    let code = exe.status.code().expect("native exit status");
    assert_ne!(
        code, 0,
        "an arena overflow must not exit cleanly — it is a safety failure"
    );
    assert_ne!(
        code, 6,
        "6 is what `exhaust` would return if the overflowing third allocation had \
         wrongly SUCCEEDED. Seeing it would mean the arena handed out a pointer past \
         the buffer's end — the exact silent-wrong-answer this edge exists to prevent"
    );
}

/// **Full four-tier parity**: the arena produces `109` on the AST, IR, and bytecode
/// interpreters too, matching native exactly.
///
/// An earlier version of this increment *refused* the arena on all three
/// interpreters (`L0460`), arguing their typed-cell pointer model could not
/// reinterpret a buffer's storage. That argument did not survive its own design:
/// because the arena bumps in whole 8-byte cells of an `array<i64>`,
/// `arena_alloc(r, n)` is exactly `addr_of(buf[cursor])` plus an integer cursor —
/// and the interpreters define both halves. There was nothing to reinterpret, so
/// the refusal was work not done rather than an honest limitation. This test is the
/// standing proof that it was modellable.
#[test]
fn arena_runs_identically_on_every_interpreter() {
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(
            "tests/fixtures/valid/no_runtime/freestanding_arena_alloc.lby",
            backend,
        );
        assert!(
            output.status.success(),
            "the {backend} interpreter must RUN a static-buffer arena, not refuse it — an \
             arena cell is an ordinary `array<i64>` element, which this tier addresses \
             natively via the same place-backed `addr_of` machinery: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output).trim(),
            "109",
            "{backend} must agree with native (109) on the arena's result"
        );
    }
}

/// The **overflow edge** on the interpreters: a bump past the buffer aborts with
/// `L0460` on all three, mirroring native's `ud2` trap.
///
/// Both terminate without producing a value, which is exactly the relationship the
/// delivered array-bounds failure already has (`L0413` on the interpreters, `ud2`
/// natively) and what decision **A5** requires — abort with a diagnostic, no
/// unwinding. §8 will route both edges to the program's own `panic fn`.
///
/// `L0460` used to mean "an arena cannot run on an interpreter at all". It was
/// retargeted to the failure that genuinely exists once the arena was implemented.
#[test]
fn arena_overflow_aborts_on_every_interpreter() {
    for backend in ["ast", "ir", "bytecode"] {
        let output = run_backend(
            "tests/fixtures/valid/no_runtime/freestanding_arena_overflow.lby",
            backend,
        );
        let errors = stderr(&output);
        assert!(
            !output.status.success(),
            "an arena overflow must abort on the {backend} interpreter, not return a value"
        );
        assert!(
            errors.contains("L0460"),
            "the {backend} interpreter must abort an arena overflow with `L0460`: {errors}"
        );
        assert!(
            !stdout(&output).contains('6'),
            "the {backend} interpreter must not produce 6 — the value `exhaust` would return \
             if the overflowing third allocation had wrongly succeeded"
        );
    }
}

/// A backing name that resolves to nothing has no memory to bump into.
#[test]
fn arena_backing_not_in_scope_is_rejected() {
    assert_check_rejected_with(
        "tests/fixtures/invalid/no_runtime/arena_backing_not_in_scope.lby",
        "L0445",
    );
}

/// The backing buffer must be a fixed `array<i64>`: the arena bumps in 8-byte
/// cells, and a scalar is not a buffer at all.
#[test]
fn arena_backing_wrong_type_is_rejected() {
    assert_check_rejected_with(
        "tests/fixtures/invalid/no_runtime/arena_backing_wrong_type.lby",
        "L0445",
    );
}

/// `arena_alloc`'s region operand is a compile-time region name, so an undeclared
/// one cannot resolve to a buffer.
#[test]
fn arena_alloc_unknown_region_is_rejected() {
    assert_check_rejected_with(
        "tests/fixtures/invalid/no_runtime/arena_alloc_unknown_region.lby",
        "L0445",
    );
}

/// `arena_alloc` hands back an unchecked raw pointer into caller memory, so it is
/// `unsafe`-gated like every other raw-pointer producer.
#[test]
fn arena_alloc_outside_unsafe_is_rejected() {
    assert_check_rejected_with(
        "tests/fixtures/invalid/no_runtime/arena_alloc_outside_unsafe.lby",
        "L0330",
    );
}

/// `region <name> in <buffer>` survives a `lullaby fmt` round trip, and fmt is a
/// fixed point on it.
///
/// The formatter must render the arena form *distinctly*: it carries no
/// `size=`/`kind=`/`mutable=` metadata, because its extent is the backing buffer's.
/// Rendering it through the delivered metadata-region path would emit
/// `region scratch: size=0, kind=static, mutable=true` — which does not even
/// re-parse as the same declaration.
///
/// This asserts the fmt *output* rather than `fmt --check` on the fixture: every
/// `no_runtime` fixture is non-canonical under `--check` because fmt hoists the
/// `no-runtime` directive above a leading comment block — a pre-existing property
/// of the directive that has nothing to do with arenas.
#[test]
fn arena_region_survives_fmt_round_trip() {
    let path =
        workspace_root().join("tests/fixtures/valid/no_runtime/freestanding_arena_alloc.lby");
    let first = lullaby()
        .args(["fmt", path.to_str().expect("fixture path")])
        .output()
        .expect("run fmt");
    assert!(first.status.success(), "fmt failed: {}", stderr(&first));
    let formatted = stdout(&first);
    for region in ["region scratch in buf", "region pool in buf"] {
        assert!(
            formatted.contains(region),
            "fmt must re-emit the static-buffer arena form verbatim as `{region}`, with no \
             `size=`/`kind=` metadata: {formatted}"
        );
    }
    assert!(
        !formatted.contains("region scratch: size="),
        "fmt must not render a static-buffer arena through the metadata-region path: \
         {formatted}"
    );

    let temp = std::env::temp_dir().join("lullaby_arena_fmt_idempotent.lby");
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

/// **Two arenas over ONE buffer must be rejected** — they would silently alias.
///
/// Each region bumps from its own cursor starting at zero, so `region a in buf` and
/// `region b in buf` both hand out `&buf[0]`: two logically distinct arenas
/// returning overlapping cells, each write clobbering the other. Before this was
/// caught, the program compiled and returned 40 instead of 30 — a silent wrong
/// answer, and one that directly contradicts `freestanding_arena_alloc.lby`'s
/// `two_cells`, which exists to assert distinct allocations do not alias.
///
/// This is the shape §5's per-CPU-pool motivation actively invites: an author who
/// wants two bounded pools reaches for two regions. Separate pools need separate
/// buffers.
#[test]
fn arena_two_regions_over_one_buffer_are_rejected() {
    assert_check_rejected_with(
        "tests/fixtures/invalid/no_runtime/arena_two_regions_one_buffer.lby",
        "L0445",
    );
}
