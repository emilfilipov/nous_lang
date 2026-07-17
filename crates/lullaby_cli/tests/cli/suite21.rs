//! CLI integration tests, part 21 — three semantics fixes that close frontend
//! holes: model-preserving `ptr_cast`, the `L0350` simple-alias use-after-free
//! case, and void `export fn`.
//!
//! # `ptr_cast` and the two pointer models
//!
//! Lullaby has two non-convertible pointer models — the legacy `ptr_T` heap box
//! that only `alloc` produces (a heap-SLOT INDEX over a one-cell store on the
//! interpreters) and the modern `ptr<T>` address from `addr_of`/`int_to_ptr`.
//! `let`/parameter binding enforced that (`L0303`/`L0313`); `ptr_cast` did not,
//! because it derived its result type purely from the caller's annotation and never
//! from the operand. Both laundering directions are pinned below, plus the
//! negative control that a legitimate `ptr<T>` retarget still works.

use crate::*;

/// Run `source` and return `(exit code, stdout+stderr)`.
fn run_backend(source: &str, backend: &str, tag: &str) -> (i32, String) {
    let dir = std::env::temp_dir();
    let src = dir.join(format!("{tag}_{backend}.lby"));
    std::fs::write(&src, source).expect("write source");
    let out = lullaby()
        .args(["run", "--backend", backend, src.to_str().expect("src path")])
        .output()
        .expect("run backend");
    (
        out.status.code().expect("exit code"),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

/// Assert `source` is REJECTED by every interpreter frontend with `code`. A
/// frontend diagnostic is tier-independent, so all three must agree exactly.
fn assert_all_interpreters_reject(source: &str, tag: &str, code: &str) {
    for backend in ["ast", "ir", "bytecode"] {
        let (exit, output) = run_backend(source, backend, tag);
        assert_ne!(
            exit, 0,
            "{backend} must REJECT this program for {tag}:\n{source}\n{output}"
        );
        assert!(
            output.contains(code),
            "{backend} must reject {tag} with {code}:\n{source}\n{output}"
        );
    }
}

/// Assert every interpreter accepts `source` and prints `expected`.
fn assert_all_interpreters_yield(source: &str, tag: &str, expected: i64) {
    for backend in ["ast", "ir", "bytecode"] {
        let (exit, output) = run_backend(source, backend, tag);
        assert_eq!(
            exit, 0,
            "{backend} must ACCEPT this program for {tag}:\n{source}\n{output}"
        );
        assert_eq!(
            output.trim(),
            expected.to_string(),
            "{backend} must print {expected} for {tag}:\n{source}"
        );
    }
}

/// THE laundering repro: `ptr_cast` used to rewrite an `alloc` box (`ptr_i64`) into
/// a raw address (`ptr<i64>`) purely because the annotation said so. That defeated
/// the `L0303`/`L0313` walls and let `ptr_offset` (below) type-check over a
/// one-cell box. The operand's model must win, so this is now `L0303`.
#[test]
fn ptr_cast_cannot_launder_an_alloc_box_into_a_raw_pointer() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q ptr<i64> = ptr_cast(p)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_ptr_cast_launder_box",
        "L0303",
    );
}

/// The memory-corruption shape the laundering enabled: once the box is spelled
/// `ptr<i64>`, `ptr_offset(q, 1)` type-checks. The interpreters refuse it at RUN
/// time (`L0406`) and the native gate refuses it, but the frontend accepted the
/// program — natively this strides 8 bytes off a one-cell payload into the next
/// heap block's `[size]` header, the word the allocator's free-list scan reads.
/// Now it never gets past the checker.
#[test]
fn laundered_box_pointer_arithmetic_is_rejected_at_the_frontend() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q ptr<i64> = ptr_cast(p)\n",
            "        let r = ptr_offset(q, 1)\n",
            "        ptr_to_int(r)\n",
        ),
        "lullaby_ptr_cast_launder_offset",
        "L0303",
    );
}

/// The REVERSE direction, which the original report did not cover: a legacy `ptr_U`
/// annotation used to capture an `addr_of` address, relabelling a real machine
/// address as an `alloc` box. That falsifies the invariant the native backend's
/// `is_legacy_box_pointer` spelling test rests on — that a `ptr_T`-typed expression
/// is always `alloc`-derived. The model is taken from the operand, so this is
/// `L0303` too.
#[test]
fn ptr_cast_cannot_relabel_a_raw_pointer_as_an_alloc_box() {
    assert_all_interpreters_reject(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let buf array<i64> = [10, 20, 30]\n",
            "        let base = addr_of(buf[0])\n",
            "        let fake ptr_i64 = ptr_cast(base)\n",
            "        ptr_read(fake)\n",
        ),
        "lullaby_ptr_cast_relabel_address",
        "L0303",
    );
}

/// NEGATIVE CONTROL: model-preservation must not break `ptr_cast` on legitimate
/// `ptr<T>` operands. Retargeting the pointee within the modern model — the
/// `addr_of` -> `ptr_cast<u8>` -> back -> read idiom — still works on every tier.
/// If this fails, the fix over-reached.
#[test]
fn ptr_cast_still_retargets_a_genuine_raw_pointer_pointee() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let buf array<i64> = [10, 20, 30]\n",
            "        let base = addr_of(buf[0])\n",
            "        let bp ptr<u8> = ptr_cast(base)\n",
            "        let back ptr<i64> = ptr_cast(bp)\n",
            "        ptr_read(back)\n",
        ),
        "lullaby_ptr_cast_roundtrip",
        10,
    );
}

/// An identity cast of a box stays legal and stays a box: `ptr_cast` of a `ptr_T`
/// yields exactly `ptr_T`, so inference binds it and the box still reads back. This
/// pins that the fix preserves rather than rejects — existing box-cast source that
/// did not launder keeps compiling.
#[test]
fn ptr_cast_of_an_alloc_box_is_an_identity_that_stays_a_box() {
    assert_all_interpreters_yield(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(42)\n",
            "        let q = ptr_cast(p)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_ptr_cast_box_identity",
        42,
    );
}

/// Run `lullaby check` on `source` and return `(exit code, stdout+stderr)`. The
/// `L0350` lifetime check is a frontend check, so `check` is the whole surface.
fn check_source(source: &str, tag: &str) -> (i32, String) {
    let dir = std::env::temp_dir();
    let src = dir.join(format!("{tag}.lby"));
    std::fs::write(&src, source).expect("write source");
    let out = lullaby()
        .args(["check", src.to_str().expect("src path")])
        .output()
        .expect("run check");
    (
        out.status.code().expect("exit code"),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

fn assert_check_rejects(source: &str, tag: &str, code: &str) {
    let (exit, output) = check_source(source, tag);
    assert_ne!(exit, 0, "must be REJECTED for {tag}:\n{source}\n{output}");
    assert!(
        output.contains(code),
        "must be rejected with {code} for {tag}:\n{source}\n{output}"
    );
}

fn assert_check_accepts(source: &str, tag: &str) {
    let (exit, output) = check_source(source, tag);
    assert_eq!(exit, 0, "must be ACCEPTED for {tag}:\n{source}\n{output}");
}

/// THE `L0350` alias repro: a copy of a box escaped the freed-name tracking
/// entirely, so this type-checked and reached the backend, failing only at RUN time
/// (`L0406`). That hole is why native `dealloc` skips instead of lowering to
/// `rc_free` — under `rc_free` this would silently read free-list memory.
#[test]
fn use_after_free_through_a_direct_alias_is_rejected() {
    assert_check_rejects(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        dealloc(p)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_l0350_alias_uaf",
        "L0350",
    );
}

/// Aliasing is transitive AND symmetric over copies: `p`/`q`/`r` denote one box, so
/// freeing `r` — the last copy — kills the ORIGINAL `p`. This is the direction a
/// naive "dest aliases source" rule gets wrong.
#[test]
fn use_after_free_through_a_transitive_alias_is_rejected() {
    assert_check_rejects(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        let r = q\n",
            "        dealloc(r)\n",
            "        ptr_read(p)\n",
        ),
        "lullaby_l0350_alias_transitive",
        "L0350",
    );
}

/// A double free through an alias. Under a native `rc_free` this would push one
/// block onto the free list twice, making it cyclic.
#[test]
fn double_free_through_a_direct_alias_is_rejected() {
    assert_check_rejects(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        dealloc(p)\n",
            "        dealloc(q)\n",
            "        0\n",
        ),
        "lullaby_l0350_alias_double_free",
        "L0350",
    );
}

/// FALSE-POSITIVE CONTROL: re-binding an alias detaches it from the group and
/// revives it. `q` gets a fresh box after `p` is freed, so reading it is fine. If
/// this fails, the alias tracking is too eager and breaks working programs.
#[test]
fn rebinding_an_alias_after_a_free_is_accepted() {
    assert_check_accepts(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        dealloc(p)\n",
            "        q = alloc(5)\n",
            "        ptr_read(q)\n",
        ),
        "lullaby_l0350_alias_rebound",
    );
}

/// FALSE-POSITIVE CONTROL: two independent boxes are not aliases. Freeing one must
/// not implicate the other — a whole-type-based or too-coarse rule would fail here.
#[test]
fn freeing_one_box_does_not_implicate_an_independent_box() {
    assert_check_accepts(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let a = alloc(10)\n",
            "        let b = alloc(20)\n",
            "        dealloc(a)\n",
            "        ptr_read(b)\n",
        ),
        "lullaby_l0350_independent_boxes",
    );
}

/// FALSE-POSITIVE CONTROL: using an alias BEFORE the free is legal, and must stay
/// legal — the check is straight-line and order-sensitive, not name-based.
#[test]
fn using_an_alias_before_the_free_is_accepted() {
    assert_check_accepts(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(8)\n",
            "        let q = p\n",
            "        let v = ptr_read(q)\n",
            "        dealloc(p)\n",
            "        v\n",
        ),
        "lullaby_l0350_alias_use_before_free",
    );
}

/// Locate `llvm-nm.exe` in the rustc toolchain bin dir, mirroring
/// `llvm_readobj_path`. `None` when the toolchain or tool cannot be found.
fn llvm_nm_path() -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let tool = std::path::PathBuf::from(sysroot)
        .join("lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-nm.exe");
    tool.is_file().then_some(tool)
}

/// A void `export fn` is the natural C-ABI shape for a driver/callback entry point
/// (`void NAME(...)`), but `is_exportable_scalar` admitted only `i64`/`f64`/`f32`
/// for BOTH parameters and the return, so it was rejected with `L0424` — even
/// though void functions compile natively. It must now check, compile, and emit a
/// real external symbol.
///
/// The symbol assertion is unconditional (the name is in the COFF symbol table
/// bytes); the `llvm-nm` decode below is the stronger, gated check.
#[test]
fn void_export_fn_compiles_and_emits_a_c_callable_symbol() {
    let dir = std::env::temp_dir();
    let src = dir.join("lullaby_void_export.lby");
    let obj = dir.join("lullaby_void_export.obj");
    // No `main`: an export-only program is a C-callable LIBRARY object, which is
    // exactly the driver/callback shape a void export exists for.
    let source = concat!(
        "export fn tick x i64 -> void\n",
        "    let y = x + 1\n",
        "\n",
        "export fn compute a i64 -> i64\n",
        "    a * 2\n",
    );
    std::fs::write(&src, source).expect("write source");
    let _ = std::fs::remove_file(&obj);

    let check = lullaby()
        .args(["check", src.to_str().expect("src path")])
        .output()
        .expect("run check");
    assert!(
        check.status.success(),
        "a void `export fn` must type-check:\n{}",
        stderr(&check)
    );

    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            obj.to_str().expect("obj path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    assert!(
        emit.status.success(),
        "a void `export fn` must emit natively:\n{}",
        stderr(&emit)
    );
    let listing = format!("{}{}", stdout(&emit), stderr(&emit));
    assert!(
        listing.contains("compiled tick"),
        "the void export must COMPILE, not skip:\n{listing}"
    );
    assert!(obj.is_file(), "expected a native object:\n{listing}");

    // Unconditional: the exported name must be in the object's symbol table.
    let bytes = std::fs::read(&obj).expect("read object");
    assert!(
        contains_subslice(&bytes, b"tick"),
        "the exported symbol `tick` must appear in the object's symbol table"
    );

    // Stronger, toolchain-gated: decode the symbol table and assert `tick` is an
    // external DEFINED text symbol (`T`) — i.e. genuinely C-callable, not a local.
    match llvm_nm_path() {
        Some(tool) => {
            let dump = std::process::Command::new(tool)
                .arg(obj.to_str().expect("obj path"))
                .output()
                .expect("run llvm-nm");
            let symbols = String::from_utf8_lossy(&dump.stdout).to_string();
            assert!(
                symbols.lines().any(|line| line.ends_with(" T tick")),
                "`tick` must be an external defined text symbol (T):\n{symbols}"
            );
            assert!(
                symbols.lines().any(|line| line.ends_with(" T compute")),
                "the i64 export must still be external too:\n{symbols}"
            );
            eprintln!("llvm-nm decode ran: verified `T tick` and `T compute`");
        }
        None => eprintln!(
            "llvm-nm not found; ran the unconditional symbol-table byte check only \
             (the `T tick` linkage decode was NOT executed)"
        ),
    }
}

/// The C ABI of a void export: it must take its argument in the Win64 integer
/// register and return WITHOUT publishing a return value — no C caller of a `void`
/// function may read `rax`. Pinned by disassembling the emitted symbol.
///
/// This is the mirror of the entry-stub defect where a void `main` leaked `rax` as
/// the process exit code: a void function has no value position, so nothing may
/// treat its `rax` as meaningful.
#[test]
fn void_export_fn_uses_the_c_abi_and_publishes_no_return_value() {
    let Some(nm) = llvm_nm_path() else {
        eprintln!("llvm-nm not found; the void-export ABI disassembly did NOT run");
        return;
    };
    let objdump = nm.with_file_name("llvm-objdump.exe");
    if !objdump.is_file() {
        eprintln!("llvm-objdump not found; the void-export ABI disassembly did NOT run");
        return;
    }
    let dir = std::env::temp_dir();
    let src = dir.join("lullaby_void_export_abi.lby");
    let obj = dir.join("lullaby_void_export_abi.obj");
    std::fs::write(&src, "export fn tick x i64 -> void\n    let y = x + 1\n")
        .expect("write source");
    let _ = std::fs::remove_file(&obj);

    let emit = lullaby()
        .args([
            "native",
            "-o",
            obj.to_str().expect("obj path"),
            src.to_str().expect("src path"),
        ])
        .output()
        .expect("run native");
    assert!(
        emit.status.success(),
        "void export must emit:\n{}",
        stderr(&emit)
    );

    let dump = std::process::Command::new(&objdump)
        .args(["-d", obj.to_str().expect("obj path")])
        .output()
        .expect("run llvm-objdump");
    let text = String::from_utf8_lossy(&dump.stdout).to_string();
    let body: String = text
        .lines()
        .skip_while(|line| !line.contains("<tick>:"))
        .take_while(|line| !line.contains("<compute>:"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!body.is_empty(), "could not find `tick` in:\n{text}");
    // Win64: the first integer argument arrives in `rcx`.
    assert!(
        body.contains("%rcx"),
        "a void export must read its i64 argument from the Win64 register `rcx`:\n{body}"
    );
    // It must return normally...
    assert!(
        body.contains("retq"),
        "a void export must return to its C caller:\n{body}"
    );
    // ...and must not publish a computed value: the void path zeroes `rax` rather
    // than leaving the body's last value in it, so nothing can be mistaken for a
    // return value (and nothing leaks).
    assert!(
        body.contains("xorq\t%rax, %rax") || body.contains("xorq %rax, %rax"),
        "a void export must not publish a return value in `rax`:\n{body}"
    );
    eprintln!("llvm-objdump decode ran: verified rcx arg, retq, and a zeroed rax");
}

/// A void export must still be CALLABLE and return control correctly on every tier.
/// The exported symbol is ordinary code, so calling it from Lullaby's own `main`
/// exercises the same lowering a C caller would reach — and `main`'s exit code
/// proves the void call returned cleanly without disturbing the caller's value.
///
/// Note what this does NOT do: it cannot observe a side effect, because an export's
/// parameters are limited to the `i64`/`f64`/`f32` scalar set — a `ptr<i64>`
/// out-parameter is itself still `L0424` (a separate, pre-existing limit that this
/// change deliberately does not touch). So a void export currently has no way to
/// communicate anything back to a caller. That makes the feature real but narrow:
/// useful for a callback invoked purely for its effect on external state, not yet
/// for the `poke(addr_of(cell), v)` driver spelling. Widening export parameters to
/// pointers is the follow-up that makes void exports genuinely useful.
#[test]
fn void_export_fn_is_callable_and_returns_cleanly_on_every_tier() {
    assert_all_interpreters_yield(
        concat!(
            "export fn tick x i64 -> void\n",
            "    let y = x + 1\n",
            "\n",
            "fn main -> i64\n",
            "    let v i64 = 42\n",
            "    tick(7)\n",
            "    return v\n",
        ),
        "lullaby_void_export_call",
        42,
    );
}

/// NEGATIVE CONTROL: admitting `void` must not open the export gate generally. A
/// genuinely non-exportable return (`string`) is still `L0424`.
#[test]
fn a_non_exportable_return_type_is_still_rejected() {
    assert_check_rejects(
        concat!(
            "export fn name -> string\n",
            "    \"hi\"\n",
            "\n",
            "fn main -> i64\n",
            "    0\n",
        ),
        "lullaby_export_string_return",
        "L0424",
    );
}

/// NEGATIVE CONTROL: `void` is a RETURN-only concession. There is no `void` value
/// to pass, so a `void` PARAMETER stays rejected — the asymmetry is deliberate.
#[test]
fn a_void_parameter_is_still_rejected() {
    assert_check_rejects(
        concat!(
            "export fn sink x void -> void\n",
            "    let y = 1\n",
            "\n",
            "fn main -> i64\n",
            "    0\n",
        ),
        "lullaby_export_void_param",
        "L0424",
    );
}

/// HONESTY PIN — this documents a hole that is still OPEN, not a fix.
///
/// An alias laundered through a **call** is not tracked: `identity(p)` returns the
/// same box, but the checker sees an opaque call, so this compiles and dies at RUN
/// time with `L0406`. Closing it needs interprocedural alias analysis, which is out
/// of scope. If a later change closes it, this test will start failing — that is the
/// intent: it must be rewritten, not deleted, so the frontier stays documented.
#[test]
fn alias_through_a_call_is_not_tracked_and_still_fails_only_at_runtime() {
    let source = concat!(
        "fn identity p ptr_i64 -> ptr_i64\n",
        "    p\n",
        "\n",
        "fn main -> i64\n",
        "    unsafe\n",
        "        let p = alloc(8)\n",
        "        let q = identity(p)\n",
        "        dealloc(p)\n",
        "        ptr_read(q)\n",
    );
    let (exit, output) = check_source(source, "lullaby_l0350_alias_via_call");
    assert_eq!(
        exit, 0,
        "an alias through a call is NOT statically tracked today; if this now fails, \
         interprocedural aliasing was closed and this pin needs rewriting:\n{output}"
    );
    // It is still caught, but only at run time, by the interpreters.
    let (run_exit, run_output) = run_backend(source, "ast", "lullaby_l0350_alias_via_call_run");
    assert_ne!(
        run_exit, 0,
        "the runtime must still catch it:\n{run_output}"
    );
    assert!(
        run_output.contains("L0406"),
        "the runtime diagnostic should be L0406:\n{run_output}"
    );
}
