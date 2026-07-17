//! Codegen tests for the interim heap-box builtins (`native_object_heapbox.rs`):
//! that `alloc` lowers to a real one-cell allocation through the shared allocator,
//! that the out-of-subset boxes skip cleanly, that `dealloc` skips cleanly (and
//! WHY), that the pointer-identity/arithmetic builtins refuse an `alloc` box, and
//! that an `alloc`-using function is excluded from arena routing.
//!
//! These inspect the emitted `.text` bytes and the arena/skip decisions. The
//! end-to-end "compile a real `.exe` and check its exit code against all three
//! interpreters" proofs live in `crates/lullaby_cli/tests/cli/suite15.rs`, which can
//! actually run the binary.

use super::*;
use crate::{lower, lower_to_bytecode};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;

fn module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

fn read_u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// The emitted `.text` bytes of a program that must compile without skipping.
fn text_of(source: &str) -> Vec<u8> {
    let program = emit_native_program(&module_for(source)).expect("emit native program");
    assert!(
        program.skipped.is_empty(),
        "no function should be skipped: {:?}",
        program.skipped
    );
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32_at(&program.bytes, sec + 20) as usize;
    let text_size = read_u32_at(&program.bytes, sec + 16) as usize;
    program.bytes[text_offset..text_offset + text_size].to_vec()
}

/// Assert `main` does NOT compile natively — it must skip cleanly (`L0339`), never
/// be miscompiled — and that the reason mentions `reason`.
fn assert_main_skips_because(source: &str, reason: &str) {
    match emit_native_program(&module_for(source)) {
        Err(error) => {
            assert_eq!(error.code, "L0339", "a skip must carry L0339: {source}");
            let joined = format!("{:?}", error.skipped);
            assert!(
                joined.contains(reason),
                "skip reason should mention `{reason}`: {joined}"
            );
        }
        Ok(program) => {
            assert!(
                !program.compiled.contains(&"main".to_string()),
                "main must NOT compile for this shape: {source}\ncompiled={:?}",
                program.compiled
            );
            let joined = format!("{:?}", program.skipped);
            assert!(
                joined.contains(reason),
                "skip reason should mention `{reason}`: {joined}"
            );
        }
    }
}

fn contains(text: &[u8], needle: &[u8]) -> bool {
    text.windows(needle.len()).any(|w| w == needle)
}

/// The native signatures of `names`, built exactly as the program driver builds them
/// (`native_object_program.rs`), so an `arena_eligible_functions` query in a test
/// sees the same inputs the real pipeline does.
fn native_signatures_for(
    module: &BytecodeModule,
    names: &[String],
) -> HashMap<String, NativeSignature> {
    let mut signatures = HashMap::new();
    for name in names {
        let Some(function) = module.functions.iter().find(|f| &f.name == name) else {
            continue;
        };
        let lengths = infer_array_lengths(function, module, names).expect("infer array lengths");
        let sig = compute_native_signature(function, &module.structs, &module.enums, &lengths)
            .expect("compute native signature");
        signatures.insert(name.clone(), sig);
    }
    signatures
}

/// The canonical repro: a heap-box program that used to compile to NOTHING
/// (`skipped main: call to non-i64-scalar or unknown function 'alloc'`). It must now
/// compile, and its `alloc` must be a real call to the shared allocator asking for
/// exactly one 8-byte cell.
#[test]
fn alloc_lowers_to_a_one_cell_shared_allocation() {
    let source = concat!(
        "fn main -> i64\n",
        "    unsafe\n",
        "        let p = alloc(8)\n",
        "        ptr_write(p, 42)\n",
        "        ptr_read(p)\n",
    );
    let program = emit_native_program(&module_for(source)).expect("emit");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "the alloc repro must compile natively: skipped={:?}",
        program.skipped
    );
    // The allocator is asked for exactly 8 bytes (one cell) — `mov rcx, 8`.
    let text = text_of(source);
    assert!(
        contains(&text, &[0x48, 0xC7, 0xC1, 0x08, 0x00, 0x00, 0x00])
            || contains(
                &text,
                &[0x48, 0xB9, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            ),
        "`alloc` must request exactly one 8-byte cell from the shared allocator"
    );
    // The initializing store `mov [rax], rcx` writes the boxed value into the cell.
    assert!(
        contains(&text, &[0x48, 0x89, 0x08]),
        "`alloc` must store the boxed value into the fresh cell"
    );
    // The allocation goes through the SHARED bump/RC helper, not a second allocator:
    // the symbol is longer than 8 bytes, so it is interned in the COFF string table
    // and appears verbatim in the object bytes.
    assert!(
        contains(&program.bytes, HEAP_ALLOC_SYMBOL.as_bytes()),
        "`alloc` must call the existing `{HEAP_ALLOC_SYMBOL}` helper, not a new allocator"
    );
}

/// `alloc(8)` is a BOX holding the value 8 — not 8 bytes of storage. Reading it back
/// without writing must yield `8`, which is what the interpreters do
/// (`heap.push(Some(value))`). This pins the semantic that the builtin's name
/// misleads about.
#[test]
fn alloc_boxes_its_argument_rather_than_reserving_bytes() {
    let source = concat!(
        "fn main -> i64\n",
        "    unsafe\n",
        "        let p = alloc(8)\n",
        "        ptr_read(p)\n",
    );
    let program = emit_native_program(&module_for(source)).expect("emit");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "an initialized-box read must compile: skipped={:?}",
        program.skipped
    );
    // The boxed value reaches the cell: `mov [rax], rcx` after the allocator call.
    assert!(
        contains(&text_of(source), &[0x48, 0x89, 0x08]),
        "the box's initializer must be stored into the cell"
    );
}

/// Only an 8-byte cell is boxed natively. A `string`/`bool`/float/narrow box has no
/// width-exact native representation on the raw-pointer read path, so it skips
/// cleanly rather than guessing.
#[test]
fn out_of_subset_boxes_skip_gracefully() {
    for (init, pointee) in [
        ("\"hi\"", "string"),
        ("true", "bool"),
        ("1.5", "f64"),
        ("to_i32(5)", "i32"),
    ] {
        assert_main_skips_because(
            &format!("fn main -> i64\n    unsafe\n        let p = alloc({init})\n        7\n"),
            &format!("`alloc` of a `{pointee}` value is not lowered natively"),
        );
    }
}

/// `dealloc` has NO native lowering and must skip cleanly. The interpreters
/// invalidate the freed cell and DETECT a later use / double free (`L0406`); the
/// native bump/RC heap cannot reproduce that, and every available lowering turns a
/// detected error into a silent wrong answer or silent heap corruption. See the
/// module docs of `native_object_heapbox.rs`.
#[test]
fn dealloc_skips_gracefully() {
    assert_main_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(5)\n",
            "        dealloc(p)\n",
            "        7\n",
        ),
        "`dealloc` is not lowered natively",
    );
}

/// The interpreters model an `alloc` box as a heap-SLOT INDEX, not an address, so
/// `ptr_to_int` of one is a slot number (`0`, `1`, …) — a DEFINED program that a
/// real machine address would answer differently. It must skip, not diverge.
#[test]
fn ptr_to_int_of_an_alloc_box_skips_gracefully() {
    assert_main_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(7)\n",
            "        ptr_to_int(p)\n",
        ),
        "`ptr_to_int` over the `ptr_i64` produced by `alloc`",
    );
}

/// An `alloc` box is ONE cell; the interpreters refuse to stride over it at all
/// (`L0406` "ptr_offset requires a pointer produced by addr_of"), and natively a
/// stride would walk into the allocator's own RC header. It must skip.
#[test]
fn ptr_offset_over_an_alloc_box_skips_gracefully() {
    assert_main_skips_because(
        concat!(
            "fn main -> i64\n",
            "    unsafe\n",
            "        let p = alloc(7)\n",
            "        ptr_read(ptr_offset(p, 1))\n",
        ),
        "`ptr_offset` over the `ptr_i64` produced by `alloc`",
    );
}

/// The `ptr<T>` surface is UNAFFECTED by the box gate: `addr_of` + `ptr_offset` (the
/// buffer-walking kernel idiom) and `ptr_to_int` still lower for an `addr_of`-derived
/// pointer. This is the negative control for the two tests above — the gate keys on
/// the legacy `ptr_T` spelling only, so it must not have caught the typed surface.
#[test]
fn the_typed_pointer_surface_is_unaffected_by_the_box_gate() {
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let buf array<i64> = [1, 2, 3]\n",
        "    unsafe\n",
        "        let p ptr<i64> = addr_of(buf[0])\n",
        "        let q ptr<i64> = ptr_offset(p, 1)\n",
        "        ptr_read(q) + ptr_to_int(q) - ptr_to_int(p)\n",
    )))
    .expect("emit");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "`addr_of` + `ptr_offset` + `ptr_to_int` must still compile: skipped={:?}",
        program.skipped
    );
}

// -- The arena interaction ----------------------------------------------------

/// An `alloc`'d cell is manually managed and INVISIBLE to the arena escape analysis
/// (`ptr_T` is not a heap type, and `expr_touches_heap` on an `alloc` call only
/// inspects its arguments). Without the gate, this exact shape is a use-after-free:
/// the loop looks heap-touching (the `string`) and confined (the only store is a
/// `ptr_i64`), so it gets a per-iteration sub-region whose bump rewind reclaims the
/// cell `q` still names — and the post-loop string then reuses those bytes. Verified
/// end-to-end as a REAL miscompile (native `92` vs the interpreters' `2116`) with the
/// gate disabled; this test pins the gate that prevents it.
#[test]
fn an_alloc_using_function_is_excluded_from_arena_routing() {
    let module = module_for(concat!(
        "fn h a i64 -> i64\n",
        "    unsafe\n",
        "        let q = alloc(0)\n",
        "        for j from 0 to 5\n",
        "            q = alloc(j * 100 + 7)\n",
        "            let s string = to_string(a + j)\n",
        "        let z string = to_string(a) + \"clobberclobberclobber\"\n",
        "        ptr_read(q) + len(z)\n",
        "\n",
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 3\n",
        "        total = total + h(i)\n",
        "    total\n",
    ));
    let h = module
        .functions
        .iter()
        .find(|f| f.name == "h")
        .expect("h exists");
    // The body-scan gate sees the `alloc`s (one in a `Let`, one in an `Assign`
    // nested inside a `for`).
    assert!(
        alloc_defeats_arena(&h.instructions, &module.closures),
        "the gate must see an `alloc` in a `Let` and in a loop-nested `Assign`"
    );

    // And the module-level decision must actually exclude `h`, which is otherwise
    // arena-eligible (scalar return, touches the heap via the strings, a leaf, and
    // its heap loop looks "confined").
    let program = emit_native_program(&module).expect("emit");
    assert!(
        program.compiled.contains(&"h".to_string()),
        "h must still compile — the gate denies the ARENA, not the function: {:?}",
        program.skipped
    );
    let eligible: Vec<String> = program.compiled.clone();
    let signatures = native_signatures_for(&module, &eligible);
    let arena = arena_eligible_functions(&module, &eligible, &signatures);
    assert!(
        !arena.contains("h"),
        "an `alloc`-using function must NOT be arena-routed (use-after-free): {arena:?}"
    );
}

// -- The `ptr_cast` laundering route -----------------------------------------

/// `ptr_cast` over an `alloc` box must not be lowered — the backend's defense in
/// depth behind the frontend's model-preservation rule.
///
/// **History, and why these shapes changed.** `check_ptr_cast` used to derive its
/// result type from the caller's ANNOTATION (defaulting to `ptr<i64>`) and never
/// from the operand, so `let q ptr<i64> = ptr_cast(p)` rewrote `ptr_i64` into the
/// very spelling `refuse_legacy_box_pointer` keys on — laundering the box past the
/// gate. Left open, `ptr_offset(q, 1)` then strode 8 bytes past the one-cell payload
/// into the NEXT block's `[size]` header (the word `__lullaby_alloc`'s free-list scan
/// reads), so a write through it corrupted allocator metadata. Measured then: the
/// `ptr_offset` read compiled and exited 0 where the interpreters raise `L0406`;
/// `ptr_to_int` gave a real address where the interpreters give the slot index `0`;
/// the write compiled and executed.
///
/// `check_ptr_cast` now takes the result's pointer MODEL from the operand, so that
/// annotation is `L0303` and the laundered source cannot be written at all (pinned in
/// `lullaby_cli`'s `suite21.rs`). The gate below is therefore no longer the only
/// thing standing between that source and corruption — but it is still live and still
/// reachable, via the IDENTITY cast the frontend does allow: `let q = ptr_cast(p)`
/// keeps the `ptr_i64` model, so the operand reaching the backend is still a box.
/// This asserts the gate fires on exactly that.
#[test]
fn ptr_cast_over_an_alloc_box_is_not_lowered() {
    for tail in ["ptr_read(q)", "ptr_to_int(q)"] {
        let source = format!(
            "fn main -> i64\n    unsafe\n        let p ptr_i64 = alloc(7)\n        \
             let q = ptr_cast(p)\n        {tail}\n"
        );
        assert_main_skips_because(&source, "`ptr_cast` over the `ptr_i64` produced by `alloc`");
    }
}

/// The CROSS-FUNCTION route. A helper that laundered the model outright
/// (`fn launder p ptr_i64 -> ptr<i64>` returning `ptr_cast(p)`) is now rejected by
/// the frontend — its body's `ptr_cast` yields `ptr_i64`, which is not the declared
/// `ptr<i64>` (`L0301`), and passing a box to a `ptr<i64>` parameter is `L0313`. So
/// this pins what remains expressible: a model-preserving helper
/// (`-> ptr_i64`) whose own `ptr_cast` site carries the `ptr_T` operand. The gate
/// refuses there, the helper skips, and the demotion fixpoint skips every caller —
/// which is what closes the cross-function route in the backend for free.
#[test]
fn ptr_cast_over_an_alloc_box_is_not_lowered_across_a_function() {
    let source = concat!(
        "fn rebox p ptr_i64 -> ptr_i64\n",
        "    unsafe\n",
        "        ptr_cast(p)\n",
        "\n",
        "fn main -> i64\n",
        "    unsafe\n",
        "        let p ptr_i64 = alloc(7)\n",
        "        let q = rebox(p)\n",
        "        ptr_read(q)\n",
    );
    match emit_native_program(&module_for(source)) {
        Err(error) => assert_eq!(error.code, "L0339", "a skip must carry L0339"),
        Ok(program) => {
            assert!(
                !program.compiled.contains(&"launder".to_string()),
                "the laundering helper must NOT compile: {:?}",
                program.compiled
            );
            assert!(
                !program.compiled.contains(&"main".to_string()),
                "main must demote once its laundering callee skips: {:?}",
                program.compiled
            );
        }
    }
}

/// The negative control for the `ptr_cast` gate: a genuine `ptr<T>` cast chain —
/// `addr_of` -> `ptr_cast` to `ptr<u8>` -> `ptr_cast` back -> `ptr_offset` ->
/// `ptr_to_int` — must still compile. The gate keys on the legacy `ptr_T` spelling
/// only, so the documented `let bp ptr<byte> = ptr_cast(base)` idiom is untouched.
/// (Verified end-to-end at 10 on all four tiers.)
#[test]
fn ptr_cast_over_a_typed_pointer_still_compiles() {
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let buf array<i64> = [1, 2, 3]\n",
        "    unsafe\n",
        "        let p ptr<i64> = addr_of(buf[0])\n",
        "        let bp ptr<u8> = ptr_cast(p)\n",
        "        let q ptr<i64> = ptr_cast(bp)\n",
        "        let r ptr<i64> = ptr_offset(q, 1)\n",
        "        ptr_read(r) + ptr_to_int(r) - ptr_to_int(q)\n",
    )))
    .expect("emit");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "a genuine `ptr<T>` cast chain must still compile: skipped={:?}",
        program.skipped
    );
}

/// An `alloc` inside a CLOSURE body must be visible to the arena gate. A closure
/// literal carries only its parse-order `id` — the body lives in the module's closure
/// table — so a scan that stops at `Closure { .. }` would miss it entirely.
///
/// The shape is reachable (`fn x i64 -> ptr_read(alloc(x * 2))` type-checks and runs),
/// and `h` here is otherwise arena-eligible: scalar return, touches the heap via the
/// `string`, and is a leaf w.r.t. USER code (a closure call is not a module function,
/// so `body_calls_user` does not see it). This is defense-in-depth — the box is read
/// before the rewind today, so no miscompile is observable — but the gate guards a
/// demonstrated miscompile and must not depend on that coincidence, nor on the
/// Stage-1 closure rules staying as they are.
#[test]
fn an_alloc_inside_a_closure_body_defeats_arena() {
    let module = module_for(concat!(
        "fn h a i64 -> i64\n",
        "    unsafe\n",
        "        let f fn(i64) -> i64 = fn x i64 -> ptr_read(alloc(x * 2))\n",
        "        let s string = to_string(a)\n",
        "        f(a) + len(s)\n",
        "\n",
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 5\n",
        "        total = total + h(i)\n",
        "    total\n",
    ));
    let h = module
        .functions
        .iter()
        .find(|f| f.name == "h")
        .expect("h exists");
    assert!(
        !module.closures.is_empty(),
        "the fixture must actually produce a closure body to scan"
    );
    // The scan must resolve the closure id and find the `alloc` in its body...
    assert!(
        alloc_defeats_arena(&h.instructions, &module.closures),
        "an `alloc` inside a closure body must defeat the arena"
    );
    // ...and it must be the CLOSURE body that supplies it: with an empty closure
    // table the same instructions are `alloc`-free, proving the scan is not merely
    // seeing an `alloc` elsewhere in `h`.
    assert!(
        !alloc_defeats_arena(&h.instructions, &[]),
        "the fixture's only `alloc` must be inside the closure body, or this test \
         proves nothing about closure resolution"
    );
}

/// The gate is precise: the SAME function without the `alloc` (the plain
/// per-iteration string scratch) still gets its arena, so the exclusion costs only
/// `alloc`-using functions and does not regress the existing arena reach.
#[test]
fn an_alloc_free_function_keeps_its_arena() {
    let module = module_for(concat!(
        "fn h a i64 -> i64\n",
        "    let total i64 = 0\n",
        "    for j from 0 to 5\n",
        "        let s string = to_string(a + j)\n",
        "        total = total + len(s)\n",
        "    total\n",
        "\n",
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 3\n",
        "        total = total + h(i)\n",
        "    total\n",
    ));
    let h = module
        .functions
        .iter()
        .find(|f| f.name == "h")
        .expect("h exists");
    assert!(
        !alloc_defeats_arena(&h.instructions, &module.closures),
        "a function with no `alloc` must not trip the gate"
    );
    let program = emit_native_program(&module).expect("emit");
    let signatures = native_signatures_for(&module, &program.compiled);
    let arena = arena_eligible_functions(&module, &program.compiled, &signatures);
    assert!(
        arena.contains("h"),
        "an alloc-free confined-loop function must keep its arena: {arena:?}"
    );
}
