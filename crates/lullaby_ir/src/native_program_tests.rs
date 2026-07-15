use super::*;
use crate::{lower, lower_to_bytecode};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;

/// Compile source through the full frontend into a `BytecodeModule`.
fn module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

/// Compile source as a *library* (no `main` required) into a `BytecodeModule`.
/// Used for the C-callable-export path where the program may have only
/// `export fn` functions.
fn library_module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = lullaby_semantics::validate(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

/// Walk the COFF symbol table, returning `(section_number, storage_class)` for
/// the first symbol whose name matches `name`. The COFF header stores the
/// symbol-table pointer at byte 8 and the symbol count at byte 12; each record
/// is 18 bytes (8-byte name, u32 value, i16 section, u16 type, u8 storage
/// class, u8 aux count). A name <= 8 bytes is stored inline; a longer name is
/// stored in the string table (which follows the symbol records) and its
/// record's name field is four zero bytes then a u32 offset into that table.
fn coff_symbol(bytes: &[u8], name: &str) -> Option<(i16, u8)> {
    let sym_table = read_u32(bytes, 8) as usize;
    let count = read_u32(bytes, 12) as usize;
    let string_table = sym_table + count * 18;
    for i in 0..count {
        let rec = sym_table + i * 18;
        let matches = if name.len() <= 8 {
            let mut padded = [0u8; 8];
            padded[..name.len()].copy_from_slice(name.as_bytes());
            bytes[rec..rec + 8] == padded
        } else if bytes[rec..rec + 4] == [0, 0, 0, 0] {
            let str_offset = read_u32(bytes, rec + 4) as usize;
            let start = string_table + str_offset;
            let end = start + name.len();
            end <= bytes.len()
                && bytes[start..end] == *name.as_bytes()
                && bytes.get(end) == Some(&0)
        } else {
            false
        };
        if matches {
            let section = i16::from_le_bytes(bytes[rec + 12..rec + 14].try_into().unwrap());
            let storage = bytes[rec + 16];
            return Some((section, storage));
        }
    }
    None
}

/// Parse the little-endian u32 at `offset` in `bytes`.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// Parse the little-endian u16 at `offset` in `bytes`.
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

#[test]
fn emits_object_for_add_and_main() {
    let program = emit_native_program(&module_for(
        "fn add a i64 b i64 -> i64\n    a + b\n\nfn main -> i64\n    return add(20, 22)\n",
    ))
    .expect("emit native program");

    assert_eq!(program.target.triple, "x86_64-pc-windows-msvc");
    assert_eq!(program.entry_symbol, NATIVE_ENTRY_SYMBOL);
    assert_eq!(
        program.compiled,
        vec!["add".to_string(), "main".to_string()]
    );
    assert!(program.skipped.is_empty());

    // COFF header: AMD64 machine, one section.
    assert_eq!(read_u16(&program.bytes, 0), AMD64_MACHINE);
    assert_eq!(read_u16(&program.bytes, 2), 1, "one section");

    // `.text` section header begins right after the COFF header.
    let sec = COFF_HEADER_SIZE as usize;
    assert_eq!(&program.bytes[sec..sec + 5], b".text");
    let num_relocs = read_u16(&program.bytes, sec + 32);
    // Three relocations: stub->main, stub->ExitProcess, main->add.
    assert_eq!(num_relocs, 3, "expected three relocations");

    // The entry stub is the first bytes of `.text`: `sub rsp, 40` then a call.
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    assert_eq!(
        &program.bytes[text_offset..text_offset + 5],
        &[0x48, 0x83, 0xEC, 0x28, 0xE8]
    );
}

#[test]
fn emits_object_for_if_based_function() {
    // A recursive `if`-based `fib` plus a `main` calling it. Every function is
    // i64-scalar, so all compile.
    let program = emit_native_program(&module_for(
            "fn fib n i64 -> i64\n    if n < 2\n        return n\n    return fib(n - 1) + fib(n - 2)\n\nfn main -> i64\n    return fib(6)\n",
        ))
        .expect("emit native program");
    assert_eq!(
        program.compiled,
        vec!["fib".to_string(), "main".to_string()]
    );

    // The `if n < 2` condition lowers as a fused compare-and-branch against
    // the immediate, then `jge rel32` (0F 8D ..) — the inverse of `<`, taken
    // when the condition is false. This replaces the old `setl al; movzx;
    // test rax,rax; jz` sequence, so the boolean is never materialized.
    // `fib` is a purely-i64-scalar function, so its parameter `n` is promoted
    // into the first callee-saved register (rbx): the compare reads the
    // register directly (`cmp rbx, 2` = 48 81 FB 02 00 00 00) rather than
    // round-tripping through `mov rax, rbx; cmp rax, 2` — the promoted-operand
    // fold firing on the fused-comparison path.
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(7)
            .any(|w| w == [0x48, 0x81, 0xFB, 0x02, 0x00, 0x00, 0x00]),
        "expected a fused `cmp rbx, 2` reading the promoted register directly"
    );
    assert!(
        !text
            .windows(6)
            .any(|w| w == [0x48, 0x3D, 0x02, 0x00, 0x00, 0x00]),
        "the promoted operand should not be reloaded into rax before the compare"
    );
    assert!(
        text.windows(2).any(|w| w == [0x0F, 0x8D]),
        "expected a fused `jge` branch (inverted `<`)"
    );
    assert!(
        !text.windows(3).any(|w| w == [0x0F, 0x9C, 0xC0]),
        "the `<` should be fused into the branch, not materialized as `setl al`"
    );
    // The recursive arguments `fib(n - 1)` / `fib(n - 2)` form the value with a
    // single `lea rcx, [rbx - k]` (rbx is the promoted `n`), as C does, rather
    // than `mov rax,rbx; sub rax,k; mov rcx,rax`. `lea rcx, [rbx - 1]` is
    // 48 8D 8B FF FF FF FF (disp = -1).
    assert!(
        text.windows(7)
            .any(|w| w == [0x48, 0x8D, 0x8B, 0xFF, 0xFF, 0xFF, 0xFF]),
        "expected `lea rcx, [rbx - 1]` for the `fib(n - 1)` argument"
    );
}

#[test]
fn emits_object_for_while_loop() {
    let program = emit_native_program(&module_for(
            "fn main -> i64\n    let n i64 = 0\n    let sum i64 = 0\n    while n < 5\n        n += 1\n        sum += n\n    return sum\n",
        ))
        .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);

    // Only the entry stub references external symbols (main, ExitProcess); the
    // loop is self-contained, so exactly two relocations exist.
    let sec = COFF_HEADER_SIZE as usize;
    assert_eq!(read_u16(&program.bytes, sec + 32), 2);

    // A `while` loop closes with a backward `jmp` (E9) whose rel32 is negative
    // (it jumps back to the loop top). Confirm at least one such backward jump
    // appears in the compiled `.text`.
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        has_backward_jmp(text),
        "expected a backward `jmp` closing the while loop"
    );
}

#[test]
fn counting_sum_lowers_to_closed_form() {
    // `while i < N { acc = acc + i; i = i + 1 }` with `acc`/`i` promoted i64 is
    // recognized as a counting sum and lowered to the O(1) closed form
    // `acc += (i0+N-1)*(N-i0)/2` — NO loop. The exact-halving `imul rax, rdx`
    // (48 0F AF C2) and the parity `test dl, 1` (F6 C2 01) must appear, and there
    // must be no backward `jmp` in `main` (the loop is gone).
    let program = emit_native_program(&module_for(
            "fn main -> i64\n    let acc i64 = 0\n    let i i64 = 0\n    while i < 1000\n        acc = acc + i\n        i = i + 1\n    return acc\n",
        ))
        .expect("emit native program");
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(4).any(|w| w == [0x48, 0x0F, 0xAF, 0xC2]),
        "expected the closed-form exact-halving `imul rax, rdx`"
    );
    assert!(
        text.windows(3).any(|w| w == [0xF6, 0xC2, 0x01]),
        "expected the closed-form parity `test dl, 1`"
    );
    assert!(
        !text
            .windows(5)
            .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0),
        "the counting sum is closed-form (O(1)); no backward loop jump should remain"
    );
}

#[test]
fn div_and_mod_by_power_of_two_strength_reduce_to_shifts() {
    // `x / 8` and `x % 8` on plain i64 lower to the signed shift idiom (sign
    // bias + `sar`), not `idiv`. `div_qword` (F7 /7) must be absent; the
    // rounding `sar rax, 3` (48 C1 F8 03) present.
    for (body, shift_k) in [("x / 8\n", 3u8), ("x % 8\n", 3u8)] {
        let src = format!("fn f x i64 -> i64\n    {body}\nfn main -> i64\n    f(100)\n");
        let program = emit_native_program(&module_for(&src)).expect("emit native program");
        let sec = COFF_HEADER_SIZE as usize;
        let text_offset = read_u32(&program.bytes, sec + 20) as usize;
        let text_size = read_u32(&program.bytes, sec + 16) as usize;
        let text = &program.bytes[text_offset..text_offset + text_size];
        assert!(
            text.windows(4).any(|w| w == [0x48, 0xC1, 0xF8, shift_k]),
            "expected `sar rax, {shift_k}` strength-reduced shift for `{body}`"
        );
        // `idiv r/m64` is `48 F7 F8..FF`; ModRM reg field = 7 (F8-FF). Assert none.
        assert!(
            !text
                .windows(3)
                .any(|w| w[0] == 0x48 && w[1] == 0xF7 && (0xF8..=0xFF).contains(&w[2])),
            "power-of-two `{body}` must not emit idiv"
        );
    }
}

#[test]
fn affine_reduction_lowers_to_closed_form() {
    // `acc += 3*i + 5` is affine, so the whole loop closed-forms to
    // `acc += a*S + b*count` (no loop): `imul rax, rax, 3` (48 69 C0 03..) scales
    // S by a, `imul rdx, r8, 5` (49 69 D0 05..) forms b*count, then `add rax, rdx`
    // — and no backward loop jump remains.
    let program = emit_native_program(&module_for(concat!(
        "fn red n i64 -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    while i < n\n",
        "        acc = acc + ((3 * i) + 5)\n",
        "        i = i + 1\n",
        "    return acc\n\n",
        "fn main -> i64\n",
        "    red(1000)\n",
    )))
    .expect("emit native program");
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(7)
            .any(|w| w[0..3] == [0x48, 0x69, 0xC0] && w[3..7] == [0x03, 0x00, 0x00, 0x00]),
        "expected `imul rax, rax, 3` (a*S) in the affine closed form"
    );
    assert!(
        text.windows(7)
            .any(|w| w[0..3] == [0x49, 0x69, 0xD0] && w[3..7] == [0x05, 0x00, 0x00, 0x00]),
        "expected `imul rdx, r8, 5` (b*count) in the affine closed form"
    );
    assert!(
        !text
            .windows(5)
            .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0),
        "the affine reduction is closed-form (O(1)); no backward loop jump should remain"
    );
}

#[test]
fn for_loop_affine_reduction_lowers_to_closed_form() {
    // `for i from a to b { acc += 3*i + 5 }` closes to O(1) — no loop — even
    // though the `for` counter is stack-resident and could not be ILP-unrolled.
    // `count = b-a+1` in r10, `S1 = (a+b)*count/2`, then `imul rax, rax, 3` (a*S1)
    // and `imul rdx, r10, 5` (b*count) with no backward loop jump.
    let program = emit_native_program(&module_for(concat!(
        "fn f n i64 -> i64\n",
        "    let acc i64 = 0\n",
        "    for i from 0 to n\n",
        "        acc = acc + ((3 * i) + 5)\n",
        "    acc\n\n",
        "fn main -> i64\n",
        "    f(1000)\n",
    )))
    .expect("emit native program");
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(7)
            .any(|w| w[0..3] == [0x48, 0x69, 0xC0] && w[3..7] == [0x03, 0x00, 0x00, 0x00]),
        "expected `imul rax, rax, 3` (a*S1) in the for-loop closed form"
    );
    assert!(
        text.windows(7)
            .any(|w| w[0..3] == [0x49, 0x69, 0xD2] && w[3..7] == [0x05, 0x00, 0x00, 0x00]),
        "expected `imul rdx, r10, 5` (b*count) in the for-loop closed form"
    );
    assert!(
        !text
            .windows(5)
            .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0),
        "the for-loop affine reduction is closed-form (O(1)); no backward jump"
    );
}

#[test]
fn quadratic_reduction_lowers_to_closed_form() {
    // `acc += i*i` is degree 2, so it closes to `acc += c2*S2 + c1*S1 + c0*count`
    // with `S2 = sum(i²)` — the Faulhaber `g(m)` uses the modular inverse of 3
    // (`mov rdx, 0xAAAA_AAAA_AAAA_AAAB` = 48 BA AB AA…AA) for the exact `/3`.
    // No loop remains.
    let program = emit_native_program(&module_for(concat!(
        "fn red n i64 -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    while i < n\n",
        "        acc = acc + (i * i)\n",
        "        i = i + 1\n",
        "    return acc\n\n",
        "fn main -> i64\n",
        "    red(1000)\n",
    )))
    .expect("emit native program");
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(10)
            .any(|w| w[0..2] == [0x48, 0xBA] && w[2..10] == 0xAAAA_AAAA_AAAA_AAABu64.to_le_bytes()),
        "expected `mov rdx, inv3` (the exact /3 for the sum-of-squares closed form)"
    );
    assert!(
        !text
            .windows(5)
            .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0),
        "the quadratic reduction is closed-form (O(1)); no backward loop jump"
    );
}

#[test]
fn cubic_reduction_uses_multi_accumulator() {
    // `acc += i*i*i` is degree 3 (no simple closed form), so it uses the
    // multi-accumulator unroll: the three extra accumulators are zeroed
    // (`xor r8,r8` = 4D 31 C0) and folded via `add r8, rax` (49 01 C0).
    let program = emit_native_program(&module_for(concat!(
        "fn red n i64 -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    while i < n\n",
        "        acc = acc + (i * i * i)\n",
        "        i = i + 1\n",
        "    return acc\n\n",
        "fn main -> i64\n",
        "    red(1000)\n",
    )))
    .expect("emit native program");
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(3).any(|w| w == [0x4D, 0x31, 0xC0]),
        "expected the multi-accumulator zeroing `xor r8, r8`"
    );
    assert!(
        text.windows(3).any(|w| w == [0x49, 0x01, 0xC0]),
        "expected an accumulator lane add `add r8, rax`"
    );
}

#[test]
fn runtime_bound_counting_sum_lowers_to_closed_form() {
    // `while i < n` (a *runtime* i64 bound) also lowers to the O(1) closed form:
    // `n` is materialized into rcx (`mov rcx, rax` after loading the parameter),
    // `count = n - i0` in rdx, and the exact-halving `imul rax, rdx` computes the
    // sum with no loop at all.
    let program = emit_native_program(&module_for(concat!(
        "fn sum_to n i64 -> i64\n",
        "    let acc i64 = 0\n",
        "    let i i64 = 0\n",
        "    while i < n\n",
        "        acc = acc + i\n",
        "        i = i + 1\n",
        "    return acc\n\n",
        "fn main -> i64\n",
        "    sum_to(1000)\n",
    )))
    .expect("emit native program");
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(4).any(|w| w == [0x48, 0x0F, 0xAF, 0xC2]),
        "expected the closed-form exact-halving `imul rax, rdx` on a runtime bound"
    );
    assert!(
        text.windows(3).any(|w| w == [0xF6, 0xC2, 0x01]),
        "expected the closed-form parity `test dl, 1`"
    );
}

/// Whether `code` contains a near `jmp rel32` (opcode `0xE9`) whose signed
/// 32-bit displacement is negative — i.e. a backward branch, as a loop's
/// closing jump must be. Scans every `0xE9` and decodes the following four
/// bytes as a little-endian `i32`.
fn has_backward_jmp(code: &[u8]) -> bool {
    code.windows(5)
        .any(|w| w[0] == 0xE9 && i32::from_le_bytes([w[1], w[2], w[3], w[4]]) < 0)
}

#[test]
fn emits_for_loops_and_inter_function_calls() {
    // A `for`-sum, a `for`-product, and a `combine` that calls all three
    // helpers plus itself feeds `main`. Every function is i64-scalar, so all
    // compile — none is skipped — and the emitter must produce real `call`
    // relocations (inter-function calls) and backward `jmp`s (the loops).
    let program = emit_native_program(&module_for(concat!(
        "fn for_sum n i64 -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 1 to n\n",
        "        total += i\n",
        "    return total\n\n",
        "fn for_product n i64 -> i64\n",
        "    let product i64 = 1\n",
        "    for i from 1 to n\n",
        "        product *= i\n",
        "    return product\n\n",
        "fn combine a i64 b i64 -> i64\n",
        "    return for_sum(a) + for_product(b)\n\n",
        "fn main -> i64\n",
        "    return combine(4, 3)\n",
    )))
    .expect("emit native program");
    assert_eq!(
        program.compiled,
        vec![
            "for_sum".to_string(),
            "for_product".to_string(),
            "combine".to_string(),
            "main".to_string(),
        ],
        "every i64-scalar function compiles"
    );
    assert!(
        program.skipped.is_empty(),
        "no function should be skipped: {:?}",
        program.skipped
    );

    // The `.text` holds the entry stub plus every function. The two intra-body
    // `call` relocations (combine->for_sum, combine->for_product) join the
    // stub's two (stub->main, stub->ExitProcess) and main->combine, so at least
    // three `call` relocations to compiled functions are present.
    let sec = COFF_HEADER_SIZE as usize;
    let num_relocs = read_u16(&program.bytes, sec + 32) as usize;
    assert!(
        num_relocs >= 5,
        "expected the inter-function call relocations, got {num_relocs}"
    );

    // The compiled `.text` must contain a backward `jmp` (each `for` loop
    // closes with one) — direct evidence the loops were lowered natively.
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        has_backward_jmp(text),
        "expected a backward `jmp` closing a for loop"
    );
    // And an `imul rax, rcx` (48 0F AF C1), the `for`-product multiply.
    assert!(
        text.windows(4).any(|w| w == [0x48, 0x0F, 0xAF, 0xC1]),
        "expected an `imul rax, rcx` for the product loop"
    );
}

#[test]
fn vectorizes_minmax_reduction_with_cpuid_dispatch() {
    // `acc = max(acc, a[i])` over an `array<i64>` vectorizes via SSE4.2 behind a
    // runtime CPUID gate: the `.text` must contain `cpuid` (0F A2), the SSE4.2
    // `pcmpgtq` (66 0F 38 37), and a `cmov` (the branchless scalar fallback /
    // fold) — direct evidence both the feature probe and the packed path emitted.
    let program = emit_native_program(&module_for(concat!(
        "fn arr_max a array<i64> n i64 -> i64\n",
        "    let m = a[0]\n",
        "    for i from 1 to n - 1\n",
        "        m = max(m, a[i])\n",
        "    m\n\n",
        "fn main -> i64\n",
        "    let a array<i64> = [3, 7, 1, 9, 2]\n",
        "    arr_max(a, 5)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"arr_max".to_string()),
        "the min/max reduction function should compile natively: {:?}",
        program.skipped
    );
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(2).any(|w| w == [0x0F, 0xA2]),
        "expected a `cpuid` runtime feature probe"
    );
    assert!(
        text.windows(4).any(|w| w == [0x66, 0x0F, 0x38, 0x37]),
        "expected an SSE4.2 `pcmpgtq` in the packed min/max path"
    );
    // `cmovl rax, rcx` (48 0F 4C C1) — the branchless scalar max fold.
    assert!(
        text.windows(4).any(|w| w == [0x48, 0x0F, 0x4C, 0xC1]),
        "expected a `cmovl` scalar max fold"
    );
}

#[test]
fn compiles_string_char_index_and_char_builtins() {
    // `s[i]` (string char access), `char_code`, and `is_digit` now compile
    // natively — a parse loop over a string goes native end to end. The `.text`
    // must call the char-at helper (a relocation to STR_CHAR_AT_SYMBOL).
    let program = emit_native_program(&module_for(concat!(
        "fn parse_uint s string -> i64\n",
        "    let val = 0\n",
        "    let n = len(s)\n",
        "    for i from 0 to n - 1\n",
        "        if is_digit(s[i])\n",
        "            val = val * 10 + char_code(s[i]) - 48\n",
        "    val\n\n",
        "fn main -> i64\n",
        "    parse_uint(\"31337\")\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"parse_uint".to_string()),
        "the string-indexing parse loop should compile natively: {:?}",
        program.skipped
    );
    // The char-at helper symbol is present in the emitted object (it backs `s[i]`).
    assert!(
        program
            .bytes
            .windows(STR_CHAR_AT_SYMBOL.len())
            .any(|w| w == STR_CHAR_AT_SYMBOL.as_bytes()),
        "expected the char-at helper `{STR_CHAR_AT_SYMBOL}` in the object"
    );
}

#[test]
fn f64_reduction_vectorizes_only_under_fast_math() {
    // An f64 sum reduction `acc += a[i]` stays SCALAR by default (bit-exact
    // parity) and vectorizes to a packed `addpd` (66 0F 58) only under
    // --fast-math (which reorders the additions).
    let src = concat!(
        "fn compute -> i64\n",
        "    let a array<f64> = [1.0, 2.0, 3.0, 4.0]\n",
        "    let s f64 = 0.0\n",
        "    for i from 0 to 3\n",
        "        s += a[i]\n",
        "    let ok i64 = 0\n",
        "    if s > 9.5\n",
        "        ok = 1\n",
        "    ok\n\n",
        "fn main -> i64\n",
        "    compute()\n",
    );
    let has_addpd = |program: &NativeProgram| {
        let sec = COFF_HEADER_SIZE as usize;
        let off = read_u32(&program.bytes, sec + 20) as usize;
        let size = read_u32(&program.bytes, sec + 16) as usize;
        program.bytes[off..off + size]
            .windows(4)
            .any(|w| w == [0x66, 0x0F, 0x58, 0xC1])
    };
    // Default: scalar (no addpd).
    let default = emit_native_program(&module_for(src)).expect("default emit");
    assert!(
        !has_addpd(&default),
        "an f64 reduction must NOT vectorize without --fast-math (parity)"
    );
    // --fast-math: vectorized (addpd present).
    let fast = emit_native_program_for_target(
        &module_for(src),
        &crate::native_contract::x86_64_windows_target(),
        None,
        true,
    )
    .expect("fast-math emit");
    assert!(
        has_addpd(&fast),
        "an f64 reduction must vectorize (addpd) under --fast-math"
    );
}

#[test]
fn compiles_f64_struct_fields() {
    // An f64 struct field (init, read, arithmetic, by-value copy) compiles
    // natively — a full 8-byte word is bit-lossless through the GPR copy path,
    // and reads/stores route through the float lowerer. (f32 fields stay out.)
    let program = emit_native_program(&module_for(concat!(
        "struct Point\n",
        "    x f64\n",
        "    y f64\n\n",
        "fn norm2 p Point -> i64\n",
        "    let q Point = p\n",
        "    q.x = 6.0\n",
        "    let d f64 = p.x * p.x + q.y * q.y\n",
        "    let ok i64 = 0\n",
        "    if d > 24.5\n",
        "        ok = 1\n",
        "    ok\n\n",
        "fn main -> i64\n",
        "    norm2(Point(3.0, 4.0))\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"norm2".to_string()),
        "f64 struct fields should compile natively: {:?}",
        program.skipped
    );
}

#[test]
fn vectorizes_f64_elementwise_map() {
    // `for i: c[i] = a[i] + b[i]` over `array<f64>` vectorizes to a packed
    // `addpd` (66 0F 58) loop with an `addsd` (F2 0F 58) scalar tail — bit-exact
    // (per-lane, no reordering). The i64 map path is unchanged.
    let program = emit_native_program(&module_for(concat!(
        "fn vadd -> i64\n",
        "    let a array<f64> = [1.0, 2.0, 3.0]\n",
        "    let b array<f64> = [4.0, 5.0, 6.0]\n",
        "    let c array<f64> = [0.0, 0.0, 0.0]\n",
        "    for i from 0 to 2\n",
        "        c[i] = a[i] + b[i]\n",
        "    let ok i64 = 0\n",
        "    if c[0] > 4.5\n",
        "        ok = 1\n",
        "    ok\n\n",
        "fn main -> i64\n",
        "    vadd()\n",
    )))
    .expect("emit native program");
    assert!(program.compiled.contains(&"vadd".to_string()));
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(4).any(|w| w == [0x66, 0x0F, 0x58, 0xC1]),
        "expected a packed `addpd xmm0, xmm1`"
    );
    assert!(
        text.windows(4).any(|w| w == [0xF2, 0x0F, 0x58, 0xC1]),
        "expected an `addsd xmm0, xmm1` scalar tail"
    );
}

#[test]
fn compiles_f64_array_read_and_write() {
    // `array<f64>` literal init, const- and dynamic-index reads, and element
    // stores all compile natively (via the float xmm path). A movsd store to a
    // computed address (F2 0F 11 01) confirms the dynamic float store emitted.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a array<f64> = [1.5, 2.5, 3.5]\n",
        "    let i i64 = 1\n",
        "    a[i] = a[0] + a[2]\n",
        "    let r i64 = 0\n",
        "    if a[i] > 4.0\n",
        "        r = 1\n",
        "    return r\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "an array<f64> read+write function should compile natively: {:?}",
        program.skipped
    );
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(4).any(|w| w == [0xF2, 0x0F, 0x11, 0x01]),
        "expected a `movsd [rcx], xmm0` dynamic float-element store"
    );
}

#[test]
fn compiles_string_repeat_and_trim_builtins() {
    // `repeat(text, count)` and `trim(text)` compile natively (fresh records).
    let program = emit_native_program(&module_for(concat!(
        "fn f -> i64\n",
        "    let a string = repeat(\"ab\", 3)\n",
        "    let b string = trim(\"  x  \")\n",
        "    len(a) + len(b)\n\n",
        "fn main -> i64\n",
        "    f()\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"f".to_string()),
        "repeat/trim should compile natively: {:?}",
        program.skipped
    );
    for sym in [STR_REPEAT_SYMBOL, STR_TRIM_SYMBOL] {
        assert!(
            program
                .bytes
                .windows(sym.len())
                .any(|w| w == sym.as_bytes()),
            "expected helper `{sym}` in the object"
        );
    }
}

#[test]
fn compiles_string_count_builtin() {
    // `count(text, sub)` compiles natively (non-overlapping occurrence count).
    let program = emit_native_program(&module_for(concat!(
        "fn cc s string t string -> i64\n",
        "    count(s, t)\n\n",
        "fn main -> i64\n",
        "    cc(\"a,b,c\", \",\")\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"cc".to_string()),
        "string count should compile natively: {:?}",
        program.skipped
    );
    assert!(
        program
            .bytes
            .windows(STR_COUNT_SYMBOL.len())
            .any(|w| w == STR_COUNT_SYMBOL.as_bytes()),
        "expected the count helper `{STR_COUNT_SYMBOL}` in the object"
    );
}

#[test]
fn vectorized_reduction_has_a_hoisted_bounds_guard() {
    // An auto-vectorized `acc += a[i]` loop addresses the array inline, so it
    // carries a one-time hoisted bounds guard at loop entry: a `ud2` (0F 0B)
    // reachable when the (non-empty) index range escapes the array. Confirm the
    // guard's `ud2` is present (the scalar per-access path is tested separately).
    let program = emit_native_program(&module_for(concat!(
        "fn arr_sum a array<i64> n i64 -> i64\n",
        "    let acc = 0\n",
        "    for i from 0 to n - 1\n",
        "        acc += a[i]\n",
        "    acc\n\n",
        "fn main -> i64\n",
        "    let a array<i64> = [1, 2, 3, 4, 5]\n",
        "    arr_sum(a, 5)\n",
    )))
    .expect("emit native program");
    assert!(program.compiled.contains(&"arr_sum".to_string()));
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(2).any(|w| w == [0x0F, 0x0B]),
        "expected a `ud2` bounds-guard trap in the vectorized reduction"
    );
}

#[test]
fn dynamic_array_index_is_bounds_checked() {
    // A runtime array index `a[idx]` emits a bounds check: `cmp rax, len`
    // (48 3D <len>) then `jb +2` (72 02) over a `ud2` (0F 0B), so an
    // out-of-range index faults deterministically instead of reading adjacent
    // memory — mirroring the interpreters' L0413. A constant in-range index
    // stays a static offset (no check).
    let program = emit_native_program(&module_for(concat!(
        "fn at a array<i64> idx i64 -> i64\n",
        "    a[idx]\n\n",
        "fn main -> i64\n",
        "    let a array<i64> = [10, 20, 30]\n",
        "    at(a, 2)\n",
    )))
    .expect("emit native program");
    assert!(program.compiled.contains(&"at".to_string()));
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    // `cmp rax, 3` (len) then `jb +2` then `ud2` — 10 bytes.
    assert!(
        text.windows(10)
            .any(|w| w == [0x48, 0x3D, 0x03, 0x00, 0x00, 0x00, 0x72, 0x02, 0x0F, 0x0B]),
        "expected a `cmp rax,3; jb +2; ud2` bounds check on the dynamic index"
    );
}

#[test]
fn constant_out_of_bounds_index_is_rejected_at_compile_time() {
    // A literal index past the end can't fault at runtime — it's rejected up
    // front so the function skips gracefully rather than emitting an OOB read.
    // `main` is the only function and it is demoted, so the emit reports
    // L0339 — but the error carries the skip reason, which is what we assert.
    let err = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a array<i64> = [10, 20, 30]\n",
        "    a[7]\n",
    )))
    .expect_err("a program whose only function is demoted should not emit");
    assert!(
        err.skipped
            .iter()
            .any(|s| s.name == "main" && s.reason.contains("out of bounds")),
        "the constant out-of-bounds index should demote `main` with a bounds reason: {:?}",
        err.skipped
    );
}

#[test]
fn compiles_match_over_enum_parameter() {
    // `match` over an enum passed as a *parameter* now compiles: a scalar-
    // payload enum crosses the boundary by pointer (copied into the callee's
    // frame), and the callee matches the local copy. `double`/`main` still
    // compile too.
    let program = emit_native_program(&module_for(concat!(
        "fn classify o option<i64> -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> 0\n\n",
        "fn double x i64 -> i64\n",
        "    x + x\n\n",
        "fn main -> i64\n",
        "    return double(21) + classify(some(1))\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"classify".to_string())
            && program.compiled.contains(&"double".to_string())
            && program.compiled.contains(&"main".to_string()),
        "enum-parameter match must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn skips_non_i64_functions_but_compiles_the_rest() {
    // `stringify` uses `to_string(f64)` (dtoa, deferred) so it is skipped;
    // `main` and `add` are i64 (compiled). (Plain string values are now in the
    // native subset, so the skipped example uses the still-deferred float
    // `to_string` rather than an identity string function.)
    let program = emit_native_program(&module_for(
            "fn stringify -> string\n    to_string(1.5)\n\nfn add a i64 b i64 -> i64\n    a + b\n\nfn main -> i64\n    return add(1, 2)\n",
        ))
        .expect("emit native program");
    assert_eq!(
        program.compiled,
        vec!["add".to_string(), "main".to_string()]
    );
    assert_eq!(program.skipped.len(), 1);
    assert_eq!(program.skipped[0].name, "stringify");
}

#[test]
fn compiles_fixed_width_integer_function_natively() {
    // A `main` whose signature is `-> i64` but which uses the fixed-width
    // integer types internally (u32 wrapping subtraction, an unsigned
    // comparison, and the `to_u32`/`to_i64` conversions) now compiles
    // natively instead of being skipped.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a u32 = to_u32(0)\n",
        "    let b u32 = to_u32(1)\n",
        "    let wrapped i64 = to_i64(a - b)\n",
        "    let flag i64 = 0\n",
        "    if a > b\n",
        "        flag = 1\n",
        "    wrapped + flag\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no function should be skipped: {:?}",
        program.skipped
    );

    // The compiled body must contain a `mov eax, eax` (89 C0) — the u32
    // zero-extend that re-normalizes each width-producing op — and a `setb
    // al` (0F 92 C0), the unsigned `>` (a > b) condition code.
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    assert!(
        text.windows(2).any(|w| w == [0x89, 0xC0]),
        "expected a `mov eax, eax` u32 normalization"
    );
    assert!(
        text.windows(3).any(|w| w == [0x0F, 0x97, 0xC0]),
        "expected a `seta al` for the unsigned `>`"
    );
}

#[test]
fn compiles_fixed_width_bitwise_and_shifts_natively() {
    // Bitwise and shift operators on fixed-width kinds compile natively: a u8
    // AND, a signed i32 arithmetic right shift, and one's-complement `~`.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a u8 = to_u8(200)\n",
        "    let b u8 = to_u8(15)\n",
        "    let band u8 = a & b\n",
        "    let notv u8 = ~a\n",
        "    let s i32 = to_i32(0 - 8)\n",
        "    let sar i32 = s >> to_i32(1)\n",
        "    to_i64(band) + to_i64(notv) + to_i64(sar)\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    // `sar rax, cl` (48 D3 F8) for the signed right shift, and `not rax`
    // (48 F7 D0) for `~`.
    assert!(
        text.windows(3).any(|w| w == [0x48, 0xD3, 0xF8]),
        "expected a `sar rax, cl` for the signed i32 `>>`"
    );
    assert!(
        text.windows(3).any(|w| w == [0x48, 0xF7, 0xD0]),
        "expected a `not rax` for `~`"
    );
}

#[test]
fn signed_division_guards_min_over_neg_one_overflow() {
    // `idiv` raises a hardware #DE on `i64::MIN / -1`, but the interpreters
    // wrap it to `i64::MIN` (`wrapping_div`). Both the plain-`i64` and the
    // fixed-width signed division paths must emit the wrapping guard —
    // `cmp r8, -1` (49 83 F8 FF) followed by `neg rax` (48 F7 D8) — so the
    // native backend matches the interpreters instead of trapping.
    //
    // `a / b` is plain i64; `to_isize(a) / to_isize(b)` is the fixed-width
    // signed (isize) path. Both go through `emit_signed_idiv_r8`.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a i64 = 0 - 9223372036854775807 - 1\n",
        "    let b i64 = 0 - 1\n",
        "    let q64 i64 = a / b\n",
        "    let qsz isize = to_isize(a) / to_isize(b)\n",
        "    to_i64(qsz) - q64\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    let text = &program.bytes[text_offset..text_offset + text_size];
    // Two signed divisions -> two guards. Count `cmp r8, -1` occurrences.
    let guards = text
        .windows(4)
        .filter(|w| *w == [0x49, 0x83, 0xF8, 0xFF])
        .count();
    assert_eq!(
        guards, 2,
        "expected a `cmp r8, -1` guard before each of the two signed divisions"
    );
    assert!(
        text.windows(3).any(|w| w == [0x48, 0xF7, 0xD8]),
        "expected a `neg rax` implementing the wrapping `x / -1`"
    );
}

#[test]
fn skips_float_math_builtin_gracefully() {
    // f64/f32 arithmetic, comparison, `to_f32`/`to_f64`, and `sqrt` are now native,
    // but the remaining transcendental/rounding math builtins (`sin`, `floor`, …)
    // remain deferred. A `-> i64` function that calls one must skip gracefully and
    // report why, leaving nothing eligible.
    let err = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let r f64 = floor(16.7)\n",
        "    let flag i64 = 0\n",
        "    if r > 3.0\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect_err("float math builtin is deferred");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn compiles_sqrt_as_sqrtsd() {
    // `sqrt(x f64)` lowers to a single SSE2 `sqrtsd xmm0, xmm0` (F2 0F 51 C0), so a
    // function using it (with f64 locals but an i64 result) compiles natively.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let r f64 = sqrt(2.0)\n",
        "    let flag i64 = 0\n",
        "    if r > 1.41\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect("emit sqrt program");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "a sqrt-using function must compile natively: {:?}",
        program.skipped
    );
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0xF2, 0x0F, 0x51, 0xC0]),
        "sqrt must emit `sqrtsd xmm0, xmm0`"
    );
}

#[test]
fn compiles_abs_f64_as_sse2_sign_clear() {
    // `abs(x f64)` lowers to an SSE2 in-register sign-bit clear with no memory
    // constant: pcmpeqd xmm1,xmm1 / psrlq xmm1,1 / andpd xmm0,xmm1, so a function
    // using it (f64 locals, i64 result) compiles natively.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let r f64 = abs(0.0 - 7.5)\n",
        "    let flag i64 = 0\n",
        "    if r > 7.4\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect("emit abs program");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "an abs(f64)-using function must compile natively: {:?}",
        program.skipped
    );
    assert!(
        program.bytes.windows(13).any(|w| w
            == [
                0x66, 0x0F, 0x76, 0xC9, // pcmpeqd xmm1, xmm1
                0x66, 0x0F, 0x73, 0xD1, 0x01, // psrlq xmm1, 1
                0x66, 0x0F, 0x54, 0xC1, // andpd xmm0, xmm1
            ]),
        "abs(f64) must emit the SSE2 sign-bit-clear sequence"
    );
}

#[test]
fn compiles_abs_i64_as_twos_complement_idiom() {
    // `abs(i64)` lowers to the branchless two's-complement abs idiom
    // (`sar rdx, 63` = 48 C1 FA 3F, then `xor`/`sub rax, rdx`), matching release
    // `i64::abs` (wraps `abs(i64::MIN)` to i64::MIN like the native contract).
    let program = emit_native_program(&module_for(
        concat!("fn main -> i64\n", "    abs(0 - 5)\n",),
    ))
    .expect("abs(i64) compiles natively");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "an abs(i64)-using function must compile natively: {:?}",
        program.skipped
    );
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0x48, 0xC1, 0xFA, 0x3F])
            && program.bytes.windows(3).any(|w| w == [0x48, 0x29, 0xD0]),
        "abs(i64) must emit `sar rdx, 63` + `sub rax, rdx`"
    );
}

#[test]
fn compiles_min_max_i64_as_cmp_cmov() {
    // `min`/`max` on plain `i64` lower to a branchless `cmp rcx, rax` (48 39 C1)
    // plus `cmovl`/`cmovg rax, rcx` (48 0F 4C C1 / 48 0F 4F C1) — signed, matching
    // `i64::min`/`i64::max`. A function using both compiles natively.
    let program = emit_native_program(&module_for(concat!(
        "fn pick a i64 b i64 -> i64\n",
        "    min(a, b) + max(a, b)\n\n",
        "fn main -> i64\n",
        "    pick(7, 3)\n",
    )))
    .expect("emit min/max program");
    assert!(
        program.compiled.contains(&"pick".to_string()),
        "a min/max-using function must compile natively: {:?}",
        program.skipped
    );
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0x48, 0x0F, 0x4C, 0xC1]),
        "min must emit `cmovl rax, rcx`"
    );
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0x48, 0x0F, 0x4F, 0xC1]),
        "max must emit `cmovg rax, rcx`"
    );
}

#[test]
fn compiles_gcd_i64_inline() {
    // `gcd(a, b)` on i64 lowers inline: two's-complement `abs` (sar rdx,63 =
    // 48 C1 FA 3F) of each operand into an unsigned magnitude, then an unsigned
    // Euclid loop whose remainder step is `div r8` (49 F7 F0). No helper, no heap.
    let program = emit_native_program(&module_for(concat!(
        "fn g a i64 b i64 -> i64\n",
        "    gcd(a, b)\n\n",
        "fn main -> i64\n",
        "    g(48, 36)\n",
    )))
    .expect("emit gcd program");
    assert!(
        program.compiled.contains(&"g".to_string()),
        "a gcd-using function must compile natively: {:?}",
        program.skipped
    );
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0x48, 0xC1, 0xFA, 0x3F]),
        "gcd must emit the two's-complement abs (`sar rdx, 63`)"
    );
    assert!(
        program.bytes.windows(3).any(|w| w == [0x49, 0xF7, 0xF0]),
        "gcd must emit the unsigned Euclid remainder (`div r8`)"
    );
}

#[test]
fn compiles_sign_and_clamp_i64() {
    // `sign(x)` on i64 lowers to `test` + two signed cmovs (cmovg/cmovl rax, rdx
    // = 48 0F 4F C2 / 48 0F 4C C2). `clamp(x, lo, hi)` on i64 applies an upper
    // then a lower clamp via cmovl/cmovg rax, r8 (49 0F 4C C0 / 49 0F 4F C0).
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a i64 = sign(0 - 3)\n",
        "    let b i64 = clamp(150, 0, 100)\n",
        "    a + b\n",
    )))
    .expect("emit sign/clamp program");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "a sign/clamp-using function must compile natively: {:?}",
        program.skipped
    );
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0x48, 0x0F, 0x4F, 0xC2])
            && program
                .bytes
                .windows(4)
                .any(|w| w == [0x48, 0x0F, 0x4C, 0xC2]),
        "sign must emit `cmovg`/`cmovl rax, rdx`"
    );
    assert!(
        program
            .bytes
            .windows(4)
            .any(|w| w == [0x49, 0x0F, 0x4C, 0xC0])
            && program
                .bytes
                .windows(4)
                .any(|w| w == [0x49, 0x0F, 0x4F, 0xC0]),
        "clamp must emit `cmovl`/`cmovg rax, r8`"
    );
}

#[test]
fn sign_clamp_f64_defer_gracefully() {
    // Only the i64 case of `sign`/`clamp` is lowered natively; the f64 cases
    // (which need float comparisons) skip the whole function to the interpreters.
    for src in [
        concat!("fn main -> i64\n", "    let s i64 = sign(1.5)\n", "    s\n",),
        concat!(
            "fn main -> i64\n",
            "    let c f64 = clamp(1.5, 0.0, 1.0)\n",
            "    let flag i64 = 0\n",
            "    if c < 1.1\n",
            "        flag = 1\n",
            "    flag\n",
        ),
    ] {
        let err = emit_native_program(&module_for(src)).expect_err("f64 sign/clamp is deferred");
        assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
        assert!(err.skipped.iter().any(|s| s.name == "main"));
    }
}

#[test]
fn min_max_f64_defers_gracefully() {
    // Only the `i64` case of `min`/`max` is lowered natively; an `f64` `min`/`max`
    // (whose SSE `minsd`/`maxsd` NaN/±0.0 rules diverge from `f64::min`) skips the
    // whole function to the interpreters rather than miscompiling.
    let err = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let m f64 = min(1.5, 2.5)\n",
        "    let flag i64 = 0\n",
        "    if m < 2.0\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect_err("f64 min is deferred");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn compiles_overflow_builtins() {
    // The overflow-aware builtins are now emitted natively (not deferred):
    // `wrapping_*`/`saturating_*` produce a fixed-width scalar and `checked_*`
    // an `option<T>` matched in place. A `main` exercising all three compiles.
    let program = emit_native_program(&module_for(concat!(
        "fn chk m i8 n i8 -> i64\n",
        "    match checked_add(m, n)\n",
        "        some(v) -> to_i64(v)\n",
        "        none -> 0 - 1\n\n",
        "fn main -> i64\n",
        "    let s u8 = saturating_add(to_u8(200), to_u8(100))\n",
        "    let w u8 = wrapping_mul(to_u8(16), to_u8(16))\n",
        "    to_i64(s) + to_i64(w) + chk(to_i8(127), to_i8(1))\n",
    )))
    .expect("overflow builtins compile natively");
    assert_eq!(
        program.compiled,
        vec!["chk".to_string(), "main".to_string()]
    );
    assert!(program.skipped.is_empty(), "nothing should skip");
}

#[test]
fn emits_extern_call_as_undefined_external_symbol() {
    // An `extern fn` C function is called from `main`. The call lowers to a
    // REL32 relocation against an undefined external symbol named after the C
    // function, and the C runtime import library is requested for linking.
    let program = emit_native_program(&module_for(
        "extern fn llabs x i64 -> i64\n\nfn main -> i64\n    llabs(-7)\n",
    ))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert_eq!(
        program.import_libs,
        vec![C_RUNTIME_IMPORT_LIB.to_string()],
        "extern calls require the C runtime import library"
    );

    // Three relocations: stub->main, stub->ExitProcess, main->llabs.
    let sec = COFF_HEADER_SIZE as usize;
    assert_eq!(read_u16(&program.bytes, sec + 32), 3, "three relocations");

    // The undefined external symbol `llabs` (<= 8 bytes) is stored inline in a
    // symbol record's name field; scan the object bytes for it.
    let needle = b"llabs\0\0\0";
    assert!(
        program.bytes.windows(8).any(|w| w == needle),
        "expected an `llabs` external symbol record"
    );
}

#[test]
fn emits_i32_extern_call_with_import_and_return_normalization() {
    // An `extern fn` with an `i32` C signature (e.g. `toupper(int) -> int`)
    // now compiles: the call lowers to a REL32 relocation against an undefined
    // external symbol, requests the C runtime import library, and — because
    // Win64 leaves the upper bits of a narrow integer return undefined — the
    // emitter normalizes the `i32` return with `movsxd rax, eax` (48 63 C0).
    let program = emit_native_program(&module_for(
        "extern fn toupper c i32 -> i32\n\nfn main -> i64\n    to_i64(toupper(to_i32(97)))\n",
    ))
    .expect("emit native program for an i32 extern");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert_eq!(
        program.import_libs,
        vec![C_RUNTIME_IMPORT_LIB.to_string()],
        "an i32 extern call still requires the C runtime import library"
    );

    // The undefined external symbol `toupper` (<= 8 bytes) is stored inline.
    assert!(
        program.bytes.windows(8).any(|w| w == b"toupper\0"),
        "expected a `toupper` external symbol record"
    );

    // `main`'s text contains `movsxd rax, eax` (48 63 C0) — the i32 C return
    // normalization emitted after the `call`.
    let text = text_bytes(&program);
    assert!(
        text.windows(3).any(|w| w == [0x48, 0x63, 0xC0]),
        "expected an i32 return normalization (`movsxd rax, eax`) after the extern call"
    );
}

#[test]
fn emits_u8_extern_call_with_zero_extend_return_normalization() {
    // A `u8`/`byte`-class C return is zero-extended (`movzx rax, al` = 48 0F
    // B6 C0). This also exercises the `bool`/`byte` -> u8 marshalling class.
    let program = emit_native_program(&module_for(
        "extern fn tolower c u8 -> u8\n\nfn main -> i64\n    to_i64(tolower(to_u8(65)))\n",
    ))
    .expect("emit native program for a u8 extern");
    let text = text_bytes(&program);
    assert!(
        text.windows(4).any(|w| w == [0x48, 0x0F, 0xB6, 0xC0]),
        "expected a u8 return normalization (`movzx rax, al`) after the extern call"
    );
}

#[test]
fn float_extern_arg_and_return_route_through_xmm0() {
    // A `f64`-taking/returning C extern now marshals across the Win64 C ABI:
    // its single float argument is loaded into `xmm0` (the position-0 SSE
    // register) — `movsd xmm0, [rsp+0]` = `F2 0F 10 44 24 00` — and the `f64`
    // return is consumed from `xmm0`. The caller compiles natively instead of
    // being demoted.
    let program = emit_native_program(&module_for(concat!(
        "extern fn cfloor x f64 -> f64\n\n",
        "fn main -> i64\n",
        "    let r f64 = cfloor(3.7)\n",
        "    let flag i64 = 0\n",
        "    if r > 3.0\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect("float extern now marshals natively");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    let text = text_bytes(&program);
    assert!(
        text.windows(6)
            .any(|w| w == [0xF2, 0x0F, 0x10, 0x44, 0x24, 0x00]),
        "expected the float argument loaded into xmm0 (`movsd xmm0, [rsp]`)"
    );
}

#[test]
fn mixed_float_then_int_extern_routes_xmm0_and_rdx() {
    // Win64 positional routing: for `f(double a, int b)` the float at position
    // 0 goes to `xmm0` and the integer at position 1 goes to integer register 1
    // (`rdx`), never both sequences for one argument. `ldexp(double, int)`
    // exercises exactly this mixed signature.
    let program = emit_native_program(&module_for(concat!(
        "extern fn ldexp x f64 e i32 -> f64\n\n",
        "fn main -> i64\n",
        "    let r f64 = ldexp(1.5, to_i32(3))\n",
        "    let flag i64 = 0\n",
        "    if r > 11.0\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect("mixed float/int extern marshals natively");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    let text = text_bytes(&program);
    // Position 0 float -> xmm0: `movsd xmm0, [rsp + disp]` (`F2 0F 10 /r` with
    // an rsp SIB and reg 0). Match the fixed opcode+SIB prefix, any disp8.
    assert!(
        text.windows(5)
            .any(|w| w[..3] == [0xF2, 0x0F, 0x10] && w[3] == 0x44 && w[4] == 0x24),
        "expected the position-0 float argument loaded into xmm0"
    );
    // Position 1 integer -> rdx: `mov rdx, [rsp + disp]` (`48 8B /r`, reg=rdx=2,
    // rsp SIB). ModRM for disp8 with reg=2 = 0x54, SIB 0x24.
    assert!(
        text.windows(5)
            .any(|w| w[..2] == [0x48, 0x8B] && w[2] == 0x54 && w[3] == 0x24),
        "expected the position-1 integer argument loaded into rdx"
    );
}

#[test]
fn int_then_float_extern_routes_rcx_and_xmm1() {
    // The mirror case: `f(int a, double b)` puts the integer at position 0 in
    // `rcx` and the float at position 1 in `xmm1` — each position consumes its
    // slot in exactly one register sequence.
    let program = emit_native_program(&module_for(concat!(
        "extern fn scalbn_like a i32 b f64 -> f64\n\n",
        "fn main -> i64\n",
        "    let r f64 = scalbn_like(to_i32(2), 4.0)\n",
        "    let flag i64 = 0\n",
        "    if r > 3.0\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect("int/float extern marshals natively");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    let text = text_bytes(&program);
    // Position 0 integer -> rcx: `mov rcx, [rsp + disp]` (48 8B, reg=rcx=1,
    // ModRM disp8 = 0x4C, SIB 0x24).
    assert!(
        text.windows(4)
            .any(|w| w[..2] == [0x48, 0x8B] && w[2] == 0x4C && w[3] == 0x24),
        "expected the position-0 integer argument loaded into rcx"
    );
    // Position 1 float -> xmm1: `movsd xmm1, [rsp + disp]` (F2 0F 10, reg=1,
    // ModRM disp8 = 0x4C, SIB 0x24).
    assert!(
        text.windows(5)
            .any(|w| w[..3] == [0xF2, 0x0F, 0x10] && w[3] == 0x4C && w[4] == 0x24),
        "expected the position-1 float argument loaded into xmm1"
    );
}

#[test]
fn cstr_extern_materializes_string_and_emits_helper() {
    // An `extern fn` with a `cstr` parameter accepts a Lullaby `string`: the
    // caller evaluates the string to a record pointer, then calls
    // `__lullaby_to_cstr` to materialize a NUL-terminated buffer before the C
    // `call`. The distinctive tail of that helper is the NUL-terminator write
    // `mov byte [rdi], 0` (`C6 07 00`); its presence proves the helper is
    // emitted into `.text`.
    let program = emit_native_program(&module_for(
        "extern fn strlen s cstr -> usize\n\nfn main -> i64\n    to_i64(strlen(\"hi\"))\n",
    ))
    .expect("cstr extern compiles natively");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    assert_eq!(
        program.import_libs,
        vec![C_RUNTIME_IMPORT_LIB.to_string()],
        "a cstr extern call requires the C runtime import library"
    );
    // The `__lullaby_to_cstr` helper symbol is a defined external in the object.
    assert!(
        program
            .bytes
            .windows(TO_CSTR_SYMBOL.len())
            .any(|w| w == TO_CSTR_SYMBOL.as_bytes()),
        "expected the `__lullaby_to_cstr` helper symbol in the object"
    );
    let text = text_bytes(&program);
    // The helper's NUL-terminator write (`mov byte [rdi], 0`).
    assert!(
        text.windows(3).any(|w| w == [0xC6, 0x07, 0x00]),
        "expected the cstr NUL-terminator write in `__lullaby_to_cstr`"
    );
}

#[test]
fn pointer_extern_param_and_return_compile() {
    // Raw pointers cross the FFI boundary as machine-address words: `malloc`
    // returns a `ptr<byte>` bound to a native local, and `free` takes a
    // `ptr<byte>` parameter. Both compile natively (a pointer is an `i64`-class
    // word), so the caller is not demoted.
    let program = emit_native_program(&module_for(concat!(
        "extern fn malloc n usize -> ptr<byte>\n\n",
        "extern fn free p ptr<byte> -> void\n\n",
        "fn main -> i64\n",
        "    let p ptr<byte> = malloc(to_usize(8))\n",
        "    free(p)\n",
        "    0\n",
    )))
    .expect("pointer extern params/returns compile natively");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    // Both C symbols are undefined externals in the object.
    assert!(
        program.bytes.windows(7).any(|w| w == b"malloc\0"),
        "expected a `malloc` external symbol record"
    );
    assert!(
        program.bytes.windows(5).any(|w| w == b"free\0"),
        "expected a `free` external symbol record"
    );
}

#[test]
fn six_arg_extern_call_spills_fifth_and_sixth_to_stack() {
    // An `extern fn` with six arguments spills its 5th and 6th (0-indexed
    // positions 4 and 5) into the outgoing stack-argument area above the 32-byte
    // shadow space, exactly like an internal call. During staging (six pushed
    // words), position 4's outgoing slot is `[rsp + 8*6 + 32 + 0] = [rsp+0x50]`;
    // the write is `mov [rsp+0x50], rax` (`48 89 84 24 50 00 00 00`).
    let program = emit_native_program(&module_for(concat!(
        "extern fn take6 a i64 b i64 c i64 d i64 e i64 f i64 -> i64\n\n",
        "fn main -> i64\n",
        "    take6(1, 2, 3, 4, 5, 6)\n",
    )))
    .expect("a >4-arg extern call compiles natively");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    let text = text_bytes(&program);
    assert!(
        text.windows(8)
            .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x50, 0x00, 0x00, 0x00]),
        "expected the 5th argument written to the outgoing stack slot `[rsp+0x50]`"
    );
    assert!(
        text.windows(8)
            .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x58, 0x00, 0x00, 0x00]),
        "expected the 6th argument written to the outgoing stack slot `[rsp+0x58]`"
    );
}

#[test]
fn export_fn_with_float_params_spills_from_xmm() {
    // An `export fn` with a float parameter receives it in the positional SSE
    // register and spills it into the parameter slot with `movsd [rbp-slot],
    // xmm0` (`F2 0F 11 85 <disp32>` for xmm0 at position 0). The exported
    // symbol compiles natively as a library object.
    let program = emit_native_program(&library_module_for(
        "export fn scale x f64 -> f64\n    x * x\n",
    ))
    .expect("float export compiles natively");
    assert!(
        program.compiled.contains(&"scale".to_string()),
        "expected `scale` compiled: {:?}",
        program.compiled
    );
    let text = text_bytes(&program);
    // Prologue spill of the xmm0 float parameter: `movsd [rbp+disp32], xmm0`.
    assert!(
        text.windows(4).any(|w| w == [0xF2, 0x0F, 0x11, 0x85]),
        "expected the float parameter spilled from xmm0 (`movsd [rbp-slot], xmm0`)"
    );
}

#[test]
fn errors_when_no_i64_scalar_function_is_eligible() {
    // `main` uses `to_string(f64)` (dtoa, deferred), so nothing is eligible for
    // native. (Plain string values are now in the subset, so the not-eligible
    // example uses the still-deferred float `to_string`.)
    let err = emit_native_program(&module_for("fn main -> i64\n    len(to_string(1.5))\n"))
        .expect_err("no eligible");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn string_table_holds_long_symbol_names() {
    // A function name longer than eight bytes must live in the COFF string
    // table; the emitter must still produce a valid object.
    let program = emit_native_program(&module_for(
            "fn accumulate_total n i64 -> i64\n    n + 1\n\nfn main -> i64\n    return accumulate_total(41)\n",
        ))
        .expect("emit native program");
    assert!(
        program.compiled.contains(&"accumulate_total".to_string()),
        "long-named function compiled"
    );
    // The long name appears verbatim in the string table at the tail.
    assert!(
        program
            .bytes
            .windows("accumulate_total".len())
            .any(|w| w == b"accumulate_total"),
        "long symbol name stored"
    );
}

#[test]
fn compiles_all_i64_struct_locals() {
    // A `main` that builds a struct positionally and by name, mutates a
    // field, and reads fields is eligible: it compiles with no skips.
    let program = emit_native_program(&module_for(
            "struct Point\n    x i64\n    y i64\n\nfn main -> i64\n    let p Point = Point(3, 4)\n    let q Point = Point(y: 10, x: 20)\n    p.x = p.x + 5\n    return p.x + q.y\n",
        ))
        .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn compiles_fixed_i64_array_with_const_and_dynamic_index() {
    // Fixed array: constant-index write, compound-index write, `len`, and a
    // dynamic-index read inside a `for` loop. All in the native subset.
    let program = emit_native_program(&module_for(
            "fn main -> i64\n    let xs array<i64> = [1, 2, 3, 4]\n    xs[0] = 10\n    xs[3] += 6\n    let total i64 = 0\n    for i from 0 to len(xs) - 1\n        total += xs[i]\n    return total\n",
        ))
        .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn compiles_struct_with_string_field() {
    // A struct with an immutable `string` field IS in the native subset: it is
    // stack-flattened with the string field held as one pointer word, constructed
    // by evaluating the string expression into that word, read via a field access,
    // and shared (never deep-copied) on the value-semantic copy since strings are
    // immutable. `main` builds it, reads the string field, and derives a scalar.
    let program = emit_native_program(&module_for(
            "struct Tagged\n    id i64\n    name string\n\nfn main -> i64\n    let t Tagged = Tagged(1, \"hello\")\n    return t.id + len(t.name)\n",
        ))
        .expect("string-field struct is native");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn skips_struct_with_mutable_heap_field() {
    // A struct with a MUTABLE heap field (`list`/`map`) is NOT in the native subset:
    // the flat word copy the aggregate paths emit would SHARE the mutable heap block
    // instead of deep-copying it, breaking value semantics. So a `main` constructing
    // it is demoted to skipped, and with nothing else eligible the emitter reports
    // `L0339` — a clean skip, never a miscompile.
    let err = emit_native_program(&module_for(
            "struct Bag\n    id i64\n    items list<i64>\n\nfn main -> i64\n    let xs list<i64> = list_new()\n    let b Bag = Bag(1, xs)\n    return b.id\n",
        ))
        .expect_err("mutable-heap-field struct is not native");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn skips_struct_with_f32_field() {
    // An `f32` field is still rejected inside an aggregate: the flat 8-byte word
    // copy would not keep a 4-byte f32 rounded. (An `f64` field is fine — a full
    // 8-byte word — and a `string` field is fine — immutable pointer word.)
    let err = emit_native_program(&module_for(
            "struct Mixed\n    id i64\n    ratio f32\n\nfn main -> i64\n    let m Mixed = Mixed(1, to_f32(2.0))\n    return m.id\n",
        ))
        .expect_err("f32-field struct is not native");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn compiles_generic_struct_with_scalar_type_argument() {
    // A user generic `struct Box<T>` instantiated with a SCALAR type argument
    // (`Box<i64>`) is MONOMORPHIZED: `T` is substituted to `i64`, and the layout is
    // byte-identical to a hand-written `struct BoxI64 { value i64 }`. Construction,
    // field read, value-semantic copy, and passing/returning the generic value across
    // a boundary all compile natively with no skips.
    let program = emit_native_program(&module_for(
            "struct Box<T>\n    value T\n\nfn rewrap b Box<i64> -> Box<i64>\n    Box(b.value + 1)\n\nfn main -> i64\n    let a Box<i64> = Box(5)\n    let c Box<i64> = a\n    let d Box<i64> = rewrap(a)\n    return a.value + c.value + d.value\n",
        ))
        .expect("scalar generic struct is native");
    assert_eq!(
        program.compiled,
        vec!["rewrap".to_string(), "main".to_string()]
    );
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn compiles_multi_param_generic_struct_with_scalar_arguments() {
    // A two-parameter `struct Pair<K, V>` with two scalar arguments
    // (`Pair<i64, bool>`) monomorphizes each parameter independently: `K -> i64`,
    // `V -> bool` (a normalized `0`/`1` cell), a flat two-word aggregate.
    let program = emit_native_program(&module_for(
            "struct Pair<K, V>\n    first K\n    second V\n\nfn main -> i64\n    let p Pair<i64, bool> = Pair(first: 10, second: true)\n    let base i64 = p.first\n    if p.second\n        return base + 1\n    return base\n",
        ))
        .expect("scalar multi-param generic struct is native");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn compiles_generic_enum_with_scalar_type_argument() {
    // A user generic `enum Opt<T>` instantiated with a scalar argument (`Opt<i64>`)
    // is monomorphized: the `present` variant payload substitutes to `i64`, and the
    // native tag+payload layout matches the interpreters. Construction of both a
    // payload and a unit variant plus an exhaustive `match` compile with no skips.
    let program = emit_native_program(&module_for(
            "enum Opt<T>\n    present T\n    absent\n\nfn opt_or o Opt<i64> fallback i64 -> i64\n    match o\n        present(x) -> x\n        absent -> fallback\n\nfn main -> i64\n    let o Opt<i64> = present(30)\n    let m Opt<i64> = absent\n    return opt_or(o, 0) + opt_or(m, 100)\n",
        ))
        .expect("scalar generic enum is native");
    assert_eq!(
        program.compiled,
        vec!["opt_or".to_string(), "main".to_string()]
    );
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn skips_generic_struct_with_heap_string_type_argument() {
    // DEFAULT-DENY: a scalar-`T` increment. A generic struct instantiated with a
    // HEAP type argument (`Box<string>` -> a `string` field after substitution) is
    // DEFERRED to the interpreters as a clean follow-up (heap-`T` needs
    // per-instantiation drop-glue/reclamation). Even though native already supports
    // a hand-written string-field struct, the monomorphization scalar-only gate
    // rejects the instantiation, so `main` is demoted and — nothing else eligible —
    // the emitter reports `L0339`: a clean skip, never a miscompile.
    let err = emit_native_program(&module_for(
            "struct Box<T>\n    value T\n\nfn main -> i64\n    let b Box<string> = Box(\"hi\")\n    return len(b.value)\n",
        ))
        .expect_err("heap-T generic struct is deferred");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn skips_generic_struct_whose_field_becomes_heap_after_substitution() {
    // DEFAULT-DENY even for a SCALAR type argument: `struct Stack<T> { items list<T> }`
    // with `Stack<i64>` substitutes to a `list<i64>` FIELD — a mutable heap value —
    // which is outside the scalar-`T` scope, so the whole function skips cleanly.
    let err = emit_native_program(&module_for(
            "struct Stack<T>\n    items list<T>\n    count i64\n\nfn main -> i64\n    let xs list<i64> = list_new()\n    let s Stack<i64> = Stack(xs, 0)\n    return s.count\n",
        ))
        .expect_err("generic struct with a heap-typed field is deferred");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn skips_generic_enum_with_heap_string_payload() {
    // DEFAULT-DENY: a generic enum instantiated so a payload becomes heap-typed
    // (`Either<i64, string>` -> a `string` payload) is deferred to the interpreters,
    // exactly like the generic-struct heap-`T` case.
    let err = emit_native_program(&module_for(
            "enum Either<L, R>\n    left L\n    right R\n\nfn fold e Either<i64, string> -> i64\n    match e\n        left(x) -> x\n        right(s) -> len(s)\n\nfn main -> i64\n    let e Either<i64, string> = left(3)\n    return fold(e)\n",
        ))
        .expect_err("heap-payload generic enum is deferred");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn skips_list_of_struct_string_field_when_pushing_a_variable() {
    // A `list<struct-with-string-field>` classifies as a native collection type, but
    // storing a struct *variable* into an element slot has no sound lowering: a struct
    // value is stack-flattened everywhere except in a collection slot, so a struct
    // local is a run of frame words, NOT the `[nwords][field words]` heap block that
    // `__lullaby_struct_copy` deep-copies (it reads the count at `[ptr - 8]` and walks
    // off into a neighbouring frame word → a corrupt scalar + bad string pointer,
    // i.e. SIGSEGV). The store path rejects a non-constructor `HeapStruct` value, so
    // `main` is demoted and — nothing else eligible — the emitter reports `L0339`: a
    // clean skip, exactly as the backend behaved before a struct-string was a native
    // type. (This is the reviewer's `min_list` repro.)
    let err = emit_native_program(&module_for(
            "struct Rec\n    name string\n    id i64\n\nfn main -> i64\n    let bucket list<Rec> = list_new()\n    let r Rec = Rec(\"hello\", 3)\n    bucket = push(bucket, r)\n    let got Rec = get(bucket, 0)\n    return len(got.name) + got.id\n",
        ))
        .expect_err("pushing a struct-string variable into a list is not native");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(err.skipped.iter().any(|s| s.name == "main"));
}

#[test]
fn compiles_list_of_struct_string_field_with_inline_constructor() {
    // The companion positive case: pushing an INLINE constructor into the same
    // `list<struct-with-string-field>` stays native-eligible — the fresh struct block
    // is built directly on the heap by `lower_heap_struct_construct` (already an
    // independent snapshot), so no stack->heap bridge is needed. `main` builds the
    // list, `get`s the element (the heap->stack bridge), and reads its fields.
    let program = emit_native_program(&module_for(
            "struct Rec\n    name string\n    id i64\n\nfn main -> i64\n    let bucket list<Rec> = list_new()\n    bucket = push(bucket, Rec(\"hello\", 3))\n    let got Rec = get(bucket, 0)\n    return len(got.name) + got.id\n",
        ))
        .expect("inline struct-string push is native");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn emits_rdata_and_bss_for_string_len() {
    // A `main` deriving an i64 from `len` over string literals is eligible for
    // native codegen and gains `.rdata` (the constants) + `.bss` (the heap).
    let program = emit_native_program(&module_for(
            "fn main -> i64\n    let a i64 = len(\"hello\")\n    let b i64 = len(\"native\")\n    return a + b\n",
        ))
        .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );

    // Three sections now: .text, .rdata, .bss.
    assert_eq!(read_u16(&program.bytes, 2), 3, "three sections");
    let sec1 = COFF_HEADER_SIZE as usize;
    let sec2 = sec1 + SECTION_HEADER_SIZE as usize;
    let sec3 = sec2 + SECTION_HEADER_SIZE as usize;
    assert_eq!(&program.bytes[sec1..sec1 + 5], b".text");
    assert_eq!(&program.bytes[sec2..sec2 + 6], b".rdata");
    assert_eq!(&program.bytes[sec3..sec3 + 4], b".bss");

    // `.bss` is uninitialized: SizeOfRawData is the reserved size, no raw
    // pointer.
    let bss_size = read_u32(&program.bytes, sec3 + 16);
    assert_eq!(
        bss_size, HEAP_BSS_SIZE,
        "bss reserves heap + bump pointer + free-list head + arena-mode flag"
    );
    assert_eq!(
        read_u32(&program.bytes, sec3 + 20),
        0,
        "bss has no raw data"
    );

    // The interned string bytes appear verbatim in `.rdata`.
    assert!(
        program.bytes.windows(5).any(|w| w == b"hello"),
        "hello constant stored"
    );
    assert!(
        program.bytes.windows(6).any(|w| w == b"native"),
        "native constant stored"
    );

    // Identical string literals are interned once (dedup): a symbol `__str1`
    // exists (two distinct strings) but not `__str2`.
    assert!(
        program.bytes.windows(6).any(|w| w == b"__str1"),
        "second string symbol present"
    );
}

#[test]
fn dedups_repeated_string_literals() {
    // The same literal used twice interns to a single `.rdata` constant, so
    // only `__str0` exists.
    let program = emit_native_program(&module_for(
        "fn main -> i64\n    return len(\"hi\") + len(\"hi\")\n",
    ))
    .expect("emit native program");
    assert!(
        program.bytes.windows(6).any(|w| w == b"__str0"),
        "first string symbol present"
    );
    assert!(
        !program.bytes.windows(6).any(|w| w == b"__str1"),
        "no second symbol for a repeated literal"
    );
}

#[test]
fn native_type_words_flatten_nested_aggregates() {
    // Layout sizing: a nested all-i64 struct and a fixed array flatten to the
    // expected word counts.
    let structs = vec![
        IrStructDef {
            name: "Pair".to_string(),
            type_params: vec![],
            fields: vec![
                ("a".to_string(), TypeRef::new("i64")),
                ("b".to_string(), TypeRef::new("i64")),
            ],
        },
        IrStructDef {
            name: "Line".to_string(),
            type_params: vec![],
            fields: vec![
                ("start".to_string(), TypeRef::new("Pair")),
                ("end".to_string(), TypeRef::new("Pair")),
            ],
        },
    ];
    let line = resolve_native_type(&TypeRef::new("Line"), &structs, &[]).expect("resolve Line");
    assert_eq!(line.words(), 4, "Line flattens to four i64 words");

    let array = NativeType::Array {
        elem: Box::new(NativeType::I64),
        len: 5,
    };
    assert_eq!(array.words(), 5);
}

#[test]
fn exports_function_as_external_defined_text_symbol() {
    // An `export fn` with a body, no `main`. It compiles as a library object:
    // no entry stub, and the exported function appears in the COFF symbol
    // table as an EXTERNAL (storage class 2) symbol DEFINED in `.text`
    // (section number 1) under its plain C name, so a C caller declaring
    // `extern long long add_seven(long long);` links against it.
    let program = emit_native_program(&library_module_for(
        "export fn add_seven x i64 -> i64\n    x + 7\n",
    ))
    .expect("emit native program");

    assert_eq!(program.compiled, vec!["add_seven".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
    // A library object has no entry point.
    assert!(
        program.entry_symbol.is_empty(),
        "an export-only object is a library with no entry symbol"
    );

    // The exported symbol is external + defined in `.text`.
    let (section, storage) =
        coff_symbol(&program.bytes, "add_seven").expect("add_seven symbol present");
    assert_eq!(section, 1, "add_seven is defined in `.text` (section 1)");
    assert_eq!(storage, 2, "add_seven has EXTERNAL storage class");

    // No entry stub / ExitProcess symbol exists in a library object.
    assert!(
        coff_symbol(&program.bytes, NATIVE_ENTRY_SYMBOL).is_none(),
        "a library object omits the `_lullaby_start` entry stub"
    );
    assert!(
        coff_symbol(&program.bytes, EXIT_PROCESS_SYMBOL).is_none(),
        "a library object has no `ExitProcess` dependency"
    );
}

#[test]
fn export_alongside_main_keeps_the_entry_stub() {
    // When a program has both `main` and an `export fn`, the entry stub is
    // still emitted (a runnable program) and the export is additionally
    // external + defined in `.text`.
    let program = emit_native_program(&module_for(
        "export fn doubled x i64 -> i64\n    x * 2\n\nfn main -> i64\n    doubled(21)\n",
    ))
    .expect("emit native program");

    assert_eq!(program.entry_symbol, NATIVE_ENTRY_SYMBOL);
    let (section, storage) =
        coff_symbol(&program.bytes, "doubled").expect("doubled symbol present");
    assert_eq!(section, 1, "export defined in `.text`");
    assert_eq!(storage, 2, "export is EXTERNAL");
    // The stub is present because `main` exists.
    assert!(
        coff_symbol(&program.bytes, NATIVE_ENTRY_SYMBOL).is_some(),
        "entry stub present when `main` exists"
    );
}

#[test]
fn asm_bytes_are_emitted_verbatim_into_text() {
    // A `main` whose `unsafe` `asm` block emits the seven bytes of
    // `mov rax, 42`. The emitter must copy those bytes verbatim into `.text`.
    let program = emit_native_program(&module_for(
        "fn main -> i64\n    unsafe\n        asm 72, 199, 192, 42, 0, 0, 0\n",
    ))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty());

    // The exact `mov rax, 42` byte pattern must appear in the object bytes
    // (inside `.text`), proving the raw bytes were emitted verbatim.
    let needle = [0x48u8, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00];
    let found = program
        .bytes
        .windows(needle.len())
        .any(|window| window == needle);
    assert!(
        found,
        "expected the raw `mov rax, 42` asm bytes verbatim in the emitted object"
    );
}

/// The `.text` bytes of an emitted program (the single `.text` section's
/// raw data range).
fn text_bytes(program: &NativeProgram) -> &[u8] {
    let sec = COFF_HEADER_SIZE as usize;
    let text_offset = read_u32(&program.bytes, sec + 20) as usize;
    let text_size = read_u32(&program.bytes, sec + 16) as usize;
    &program.bytes[text_offset..text_offset + text_size]
}

#[test]
fn compiles_float_arithmetic_function_natively() {
    // A `main` whose signature is `-> i64` but which computes with `f64`/`f32`
    // internals (arithmetic, comparison, and the `to_f32`/`to_f64`
    // conversions) now compiles natively instead of being skipped.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a f64 = 1.5\n",
        "    let b f64 = 2.0\n",
        "    let s f64 = a + b\n",
        "    let half f32 = to_f32(1.0) / to_f32(2.0)\n",
        "    let flag i64 = 0\n",
        "    if s > 3.0\n",
        "        flag = 1\n",
        "    if to_f64(half) < 1.0\n",
        "        flag = flag + 1\n",
        "    flag\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.is_empty(),
        "no function should be skipped: {:?}",
        program.skipped
    );

    let text = text_bytes(&program);
    // `addsd xmm0, xmm1` (F2 0F 58 C1) — the f64 addition.
    assert!(
        text.windows(4).any(|w| w == [0xF2, 0x0F, 0x58, 0xC1]),
        "expected an `addsd xmm0, xmm1`"
    );
    // `divss xmm0, xmm1` (F3 0F 5E C1) — the f32 division (single precision).
    assert!(
        text.windows(4).any(|w| w == [0xF3, 0x0F, 0x5E, 0xC1]),
        "expected a single-precision `divss xmm0, xmm1`"
    );
    // `ucomisd xmm0, xmm1` (66 0F 2E C1) — the f64 `>` compare.
    assert!(
        text.windows(4).any(|w| w == [0x66, 0x0F, 0x2E, 0xC1]),
        "expected a `ucomisd` for the f64 comparison"
    );
    // `cvtsd2ss xmm0, xmm0` (F2 0F 5A C0) — the `to_f32` rounding.
    assert!(
        text.windows(4).any(|w| w == [0xF2, 0x0F, 0x5A, 0xC0]),
        "expected a `cvtsd2ss` for `to_f32`"
    );
    // `cvtss2sd xmm0, xmm0` (F3 0F 5A C0) — the `to_f64` widening.
    assert!(
        text.windows(4).any(|w| w == [0xF3, 0x0F, 0x5A, 0xC0]),
        "expected a `cvtss2sd` for `to_f64`"
    );
}

#[test]
fn f32_operations_round_to_single_precision() {
    // An f32 add must use `addss` (single precision), not `addsd`. This is the
    // rounding guarantee that keeps native f32 bit-identical to the
    // interpreter's real `f32` storage.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let a f32 = to_f32(1.0)\n",
        "    let b f32 = to_f32(2.0)\n",
        "    let s f32 = a + b\n",
        "    let flag i64 = 0\n",
        "    if s > to_f32(2.0)\n",
        "        flag = 1\n",
        "    flag\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let text = text_bytes(&program);
    // `addss xmm0, xmm1` (F3 0F 58 C1) — single-precision add.
    assert!(
        text.windows(4).any(|w| w == [0xF3, 0x0F, 0x58, 0xC1]),
        "f32 add must be single-precision `addss`"
    );
    // No `addsd` (F2 0F 58 C1) — the f32 path must never widen to double.
    assert!(
        !text.windows(4).any(|w| w == [0xF2, 0x0F, 0x58, 0xC1]),
        "f32 add must not use the double-precision `addsd`"
    );
    // `ucomiss xmm0, xmm1` (0F 2E C1) — single-precision compare for `>`.
    assert!(
        text.windows(3).any(|w| w == [0x0F, 0x2E, 0xC1]),
        "f32 comparison must use `ucomiss`"
    );
}

#[test]
fn function_with_float_signature_compiles_natively() {
    // A function with a float parameter and float return is now a register
    // scalar routed through the SSE registers: it compiles natively (its float
    // parameter is spilled from `xmm0` and its float return is left in `xmm0`)
    // rather than being demoted to the interpreters.
    let program = emit_native_program(&module_for(concat!(
        "fn scale x f64 -> f64\n",
        "    x * 2.0\n\n",
        "fn main -> i64\n",
        "    let n i64 = 7\n",
        "    n\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"scale".to_string()),
        "expected `scale` to compile natively: {:?}",
        program.compiled
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    let text = text_bytes(&program);
    // `scale`'s prologue spills its `f64` parameter from xmm0: `movsd
    // [rbp+disp32], xmm0` = F2 0F 11 85 ...
    assert!(
        text.windows(4).any(|w| w == [0xF2, 0x0F, 0x11, 0x85]),
        "expected the float parameter spilled from xmm0"
    );
}

#[test]
fn function_with_six_i64_params_compiles_with_stack_args() {
    // A six-parameter i64 function is no longer demoted: its 5th and 6th
    // arguments pass on the stack (Win64 stack-argument ABI). Both the callee
    // (`six`) and the caller (`main`) must compile natively.
    let program = emit_native_program(&module_for(concat!(
        "fn six a i64 b i64 c i64 d i64 e i64 f i64 -> i64\n",
        "    a + b + c + d + e + f\n\n",
        "fn main -> i64\n",
        "    six(1, 2, 3, 4, 5, 6)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"six".to_string())
            && program.compiled.contains(&"main".to_string()),
        "expected `six` and `main` compiled: {:?} / skipped {:?}",
        program.compiled,
        program.skipped,
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let text = text_bytes(&program);
    // Callee prologue: the 5th parameter (0-indexed stack slot 0) is loaded
    // from `[rbp + 48]` (16 for saved rbp + return address, then 32 shadow) =
    // `48 8B 85 30 00 00 00` (mov rax, [rbp+0x30]).
    assert!(
        text.windows(7)
            .any(|w| w == [0x48, 0x8B, 0x85, 0x30, 0x00, 0x00, 0x00]),
        "expected the 5th parameter loaded from [rbp+48] in the callee prologue"
    );
    // The 6th parameter is loaded from `[rbp + 56]` = `48 8B 85 38 00 00 00`.
    assert!(
        text.windows(7)
            .any(|w| w == [0x48, 0x8B, 0x85, 0x38, 0x00, 0x00, 0x00]),
        "expected the 6th parameter loaded from [rbp+56] in the callee prologue"
    );
    // Caller call site: the 5th argument is written into the outgoing area at
    // `[rsp + 32 + ...]` after staging six words (n=6): disp = 8*6 + 32 = 80 =
    // 0x50 => `48 89 84 24 50 00 00 00` (mov [rsp+0x50], rax).
    assert!(
        text.windows(8)
            .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x50, 0x00, 0x00, 0x00]),
        "expected the 5th argument stored to the outgoing stack area [rsp+0x50]"
    );
    // The 6th argument is written to `[rsp + 88]` = `[rsp + 0x58]`.
    assert!(
        text.windows(8)
            .any(|w| w == [0x48, 0x89, 0x84, 0x24, 0x58, 0x00, 0x00, 0x00]),
        "expected the 6th argument stored to the outgoing stack area [rsp+0x58]"
    );
}

#[test]
fn function_with_eight_i64_params_compiles_with_stack_args() {
    // Eight i64 parameters: arguments 5..=8 spill to the stack. Verifies the
    // arity is not capped and the callee reads its four stack parameters from
    // ascending `[rbp + 16 + 8*k]` offsets.
    let program = emit_native_program(&module_for(concat!(
        "fn eight a i64 b i64 c i64 d i64 e i64 f i64 g i64 h i64 -> i64\n",
        "    a + b + c + d + e + f + g + h\n\n",
        "fn main -> i64\n",
        "    eight(1, 2, 3, 4, 5, 6, 7, 8)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"eight".to_string())
            && program.compiled.contains(&"main".to_string()),
        "expected `eight` and `main` compiled: {:?} / skipped {:?}",
        program.compiled,
        program.skipped,
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let text = text_bytes(&program);
    // The 8th (last) parameter is at `[rbp + 48 + 8*3]` = `[rbp + 72]` = 0x48.
    assert!(
        text.windows(7)
            .any(|w| w == [0x48, 0x8B, 0x85, 0x48, 0x00, 0x00, 0x00]),
        "expected the 8th parameter loaded from [rbp+72] in the callee prologue"
    );
}

#[test]
fn function_with_mixed_int_float_params_beyond_four_compiles() {
    // A six-parameter signature mixing i64 and f64: the integer and float
    // registers are consumed positionally (`rcx`/`rdx`, `xmm2`, `r8`; then
    // stack), and the 5th/6th arguments spill onto the stack. It must compile,
    // proving float and integer stack arguments coexist.
    let program = emit_native_program(&module_for(concat!(
        "fn scale a i64 b i64 x f64 c i64 d i64 y f64 -> i64\n",
        "    let base i64 = a + b + c + d\n",
        "    if x < y\n",
        "        return base + 1\n",
        "    return base\n\n",
        "fn main -> i64\n",
        "    scale(10, 20, 1.5, 5, 5, 2.5)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"scale".to_string())
            && program.compiled.contains(&"main".to_string()),
        "expected `scale` and `main` compiled: {:?} / skipped {:?}",
        program.compiled,
        program.skipped,
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn f32_precision_loss_matches_interpreter_semantics() {
    // The `run_f32.lby` scenario in miniature: 2^24 + 1 rounds back to 2^24 in
    // f32 (single precision cannot represent the extra bit), so the equality
    // holds and the function compiles natively. This is the exact case that
    // would fail if the f32 add were done in double precision.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let big f32 = to_f32(16777216.0)\n",
        "    let bumped f32 = big + to_f32(1.0)\n",
        "    let same i64 = 0\n",
        "    if bumped == big\n",
        "        same = 1\n",
        "    same\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let text = text_bytes(&program);
    // The equality compare emits `sete al` + `setnp cl` + `and al, cl` so a
    // NaN operand yields the interpreter's `false`; here it also proves the
    // ordered-equality lowering is present.
    assert!(
        text.windows(3).any(|w| w == [0x0F, 0x94, 0xC0])
            && text.windows(3).any(|w| w == [0x0F, 0x9B, 0xC1]),
        "expected ordered f32 equality lowering (sete + setnp)"
    );
}

/// Locate a COFF section by name, returning its raw-data byte range
/// `(start, len)` in the object. Reads the section-header table
/// (`NumberOfSections` at header offset 2) and matches the 8-byte name field.
fn coff_section(bytes: &[u8], name: &str) -> Option<(usize, usize)> {
    let num_sections = read_u16(bytes, 2) as usize;
    let mut padded = [0u8; 8];
    padded[..name.len()].copy_from_slice(name.as_bytes());
    for i in 0..num_sections {
        let hdr = COFF_HEADER_SIZE as usize + i * SECTION_HEADER_SIZE as usize;
        if bytes[hdr..hdr + 8] == padded {
            let size = read_u32(bytes, hdr + 16) as usize;
            let ptr = read_u32(bytes, hdr + 20) as usize;
            return Some((ptr, size));
        }
    }
    None
}

#[test]
fn debug_off_is_byte_for_byte_identical_to_default() {
    // `--debug` must be strictly additive: with debug info off (or via the
    // non-debug entry point) the object bytes are exactly as before, so the
    // existing structural/snapshot tests are unaffected.
    let module = module_for("fn add x i64 -> i64\n    x + 1\n\nfn main -> i64\n    add(41)\n");
    let default = emit_native_program(&module).expect("default emit");
    let no_debug = emit_native_program_with_debug(&module, None).expect("no-debug emit");
    assert_eq!(default.bytes, no_debug.bytes, "no-debug path is unchanged");
    // And the default object carries no `.debug$S` section.
    assert!(
        coff_section(&default.bytes, ".debug$S").is_none(),
        "default object has no debug section"
    );
}

#[test]
fn emits_codeview_debug_section_with_per_function_line_info() {
    // A small multi-function program. `add` is declared on source line 1 and
    // `main` on line 4 (1-based; line 3 is the blank separator). With
    // `--debug` the object must carry a well-formed CodeView `.debug$S`
    // section: the C13 signature, a DEBUG_S_LINES subsection per function, the
    // source file name in the string table, and a line record mapping each
    // function's entry offset (0) to its declaration line.
    let src = "fn add x i64 -> i64\n    x + 1\n\nfn main -> i64\n    add(41)\n";
    let module = module_for(src);
    let debug = DebugOptions {
        source_file: "sample.lby".to_string(),
    };
    let program = emit_native_program_with_debug(&module, Some(&debug)).expect("debug emit");

    // The `.debug$S` section exists.
    let (start, len) = coff_section(&program.bytes, ".debug$S").expect("debug section present");
    let section = &program.bytes[start..start + len];

    // CodeView C13 signature leads the section.
    assert_eq!(read_u32(section, 0), CV_SIGNATURE_C13, "C13 signature");

    // Walk the subsections; collect the kinds seen and every DEBUG_S_LINES's
    // recorded line number (the packed value with the statement bit cleared).
    let mut kinds: Vec<u32> = Vec::new();
    let mut lines_records: Vec<u32> = Vec::new();
    let mut cursor = 4usize;
    while cursor + 8 <= section.len() {
        let kind = read_u32(section, cursor);
        let sub_len = read_u32(section, cursor + 4) as usize;
        let data_start = cursor + 8;
        kinds.push(kind);
        if kind == DEBUG_S_LINES {
            // Header is 12 bytes: offset(4) seg(2) flags(2) codesize(4). Then
            // the file block header is 12 bytes, then line pairs of 8 bytes.
            let first_line_off = data_start + 12 + 12;
            let packed = read_u32(section, first_line_off + 4);
            lines_records.push(packed & 0x7FFF_FFFF);
            // The line entry's function-relative offset is 0 (function entry).
            assert_eq!(
                read_u32(section, first_line_off),
                0,
                "line record sits at the function entry offset"
            );
        }
        cursor = data_start + sub_len;
        // Subsections are 4-byte aligned.
        while !cursor.is_multiple_of(4) {
            cursor += 1;
        }
    }

    assert!(
        kinds.contains(&DEBUG_S_SYMBOLS),
        "a DEBUG_S_SYMBOLS subsection is present"
    );
    assert!(
        kinds.contains(&DEBUG_S_FILECHKSMS),
        "a DEBUG_S_FILECHKSMS subsection is present"
    );
    assert!(
        kinds.contains(&DEBUG_S_STRINGTABLE),
        "a DEBUG_S_STRINGTABLE subsection is present"
    );
    // One DEBUG_S_LINES per compiled function (add + main).
    assert_eq!(
        kinds.iter().filter(|&&k| k == DEBUG_S_LINES).count(),
        2,
        "one line subsection per function"
    );

    // The recorded lines are exactly the two declaration lines: `add` on line
    // 1 and `main` on line 4.
    lines_records.sort_unstable();
    assert_eq!(lines_records, vec![1, 4], "per-function declaration lines");

    // The source file name is recorded in the CodeView string table.
    assert!(
        section
            .windows(b"sample.lby".len())
            .any(|w| w == b"sample.lby"),
        "source file name recorded in the debug section"
    );

    // The section carries relocations against the `.text` function symbols
    // (SECREL32 + SECTION per function = 4 total for two functions).
    let num_sections = read_u16(&program.bytes, 2) as usize;
    let mut debug_hdr = None;
    for i in 0..num_sections {
        let hdr = COFF_HEADER_SIZE as usize + i * SECTION_HEADER_SIZE as usize;
        if &program.bytes[hdr..hdr + 8] == b".debug\x24S" {
            debug_hdr = Some(hdr);
        }
    }
    let hdr = debug_hdr.expect("debug section header");
    assert_eq!(
        read_u16(&program.bytes, hdr + 32),
        4,
        "two SECREL32+SECTION relocation pairs for two functions"
    );
}

/// The compiled `.text` bytes contain a tag load followed by a conditional
/// branch — the signature of a native `match` dispatch (`cmp rax, imm32`
/// then `jne rel32`). Used by the enum tests below.
fn has_tag_dispatch(text: &[u8]) -> bool {
    // `cmp rax, imm32` is `48 3D` + 4 bytes; a `jne rel32` is `0F 85` + 4.
    text.windows(2).any(|w| w == [0x48, 0x3D]) && text.windows(2).any(|w| w == [0x0F, 0x85])
}

#[test]
fn compiles_option_match_natively() {
    // A function that builds an `option<i64>` local and matches both arms is
    // compiled to native code (tag dispatch + payload binding), not skipped.
    let program = emit_native_program(&module_for(concat!(
        "fn some_path -> i64\n",
        "    let hit option<i64> = some(40)\n",
        "    match hit\n",
        "        some(v) -> v + 2\n",
        "        none -> 7\n\n",
        "fn main -> i64\n",
        "    some_path()\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"some_path".to_string()),
        "expected `some_path` compiled: {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.compiled.contains(&"main".to_string()));
    assert!(
        has_tag_dispatch(text_bytes(&program)),
        "expected a tag load + conditional branch for the option match"
    );
}

#[test]
fn compiles_result_scalar_match_natively() {
    // A `result<i64, i64>` (both arms scalar) compiles natively: `ok`/`err`
    // are tags 0/1, and each arm binds its scalar payload.
    let program = emit_native_program(&module_for(concat!(
        "fn ok_path -> i64\n",
        "    let r result<i64, i64> = ok(30)\n",
        "    match r\n",
        "        ok(q) -> q + 5\n",
        "        err(e) -> e\n\n",
        "fn main -> i64\n",
        "    ok_path()\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"ok_path".to_string()),
        "expected `ok_path` compiled: {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    assert!(has_tag_dispatch(text_bytes(&program)));
}

#[test]
fn compiles_user_enum_match_with_wildcard_natively() {
    // A user enum with scalar payloads and a wildcard arm compiles natively.
    // The match is inside an i64-only function so the whole function is
    // native-eligible.
    let program = emit_native_program(&module_for(concat!(
        "enum Signal\n",
        "    Steady\n",
        "    Pulse i64\n",
        "    Burst i64\n\n",
        "fn score kind i64 amount i64 -> i64\n",
        "    let s Signal = Steady\n",
        "    if kind == 1\n",
        "        s = Pulse(amount)\n",
        "    match s\n",
        "        Pulse(n) -> n + 1\n",
        "        _ -> 100\n\n",
        "fn main -> i64\n",
        "    score(1, 5)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"score".to_string()),
        "expected `score` compiled: {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    let text = text_bytes(&program);
    assert!(
        has_tag_dispatch(text),
        "expected a tag load + conditional branch for the user-enum match"
    );
    // A wildcard covers the fallthrough, so no unreachable `ud2` (0F 0B) trap
    // is emitted for this match.
    assert!(
        !text.windows(2).any(|w| w == [0x0F, 0x0B]),
        "a wildcard-terminated match should not emit an unreachable trap"
    );
}

#[test]
fn exhaustive_variant_match_emits_unreachable_trap() {
    // An exhaustive variant match with no wildcard (e.g. `option` some/none)
    // ends with a `ud2` (0F 0B) on the impossible fallthrough, since
    // exhaustiveness guarantees a variant arm matched.
    let program = emit_native_program(&module_for(concat!(
        "fn some_path -> i64\n",
        "    let hit option<i64> = some(40)\n",
        "    match hit\n",
        "        some(v) -> v + 2\n",
        "        none -> 7\n\n",
        "fn main -> i64\n",
        "    some_path()\n",
    )))
    .expect("emit native program");
    let text = text_bytes(&program);
    assert!(
        text.windows(2).any(|w| w == [0x0F, 0x0B]),
        "expected a `ud2` trap for the wildcard-free exhaustive match"
    );
}

#[test]
fn compiles_enum_returning_call_and_match_on_it() {
    // A function that returns an enum (by the hidden-pointer aggregate return
    // ABI) and a caller that matches that call result now both compile.
    let program = emit_native_program(&module_for(concat!(
        "fn lookup key i64 -> option<i64>\n",
        "    if key == 1\n",
        "        return some(11)\n",
        "    none\n\n",
        "fn use_lookup key i64 -> i64\n",
        "    match lookup(key)\n",
        "        some(v) -> v + 3\n",
        "        none -> 1\n\n",
        "fn main -> i64\n",
        "    use_lookup(1)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"lookup".to_string())
            && program.compiled.contains(&"use_lookup".to_string())
            && program.compiled.contains(&"main".to_string()),
        "enum-returning call + match must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn compiles_result_with_string_payload_natively() {
    // A `result<i64, string>` now COMPILES: the `err` payload is an immutable
    // string pointer stored in one payload word, matched and bound as a flat
    // word (shared, never deep-recursed). Both arms are exercised; the tag
    // dispatch is the same as any other native match.
    let program = emit_native_program(&module_for(concat!(
        "fn classify n i64 -> i64\n",
        "    let r result<i64, string> = ok(n)\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> len(m)\n\n",
        "fn main -> i64\n",
        "    classify(3)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"classify".to_string())
            && program.compiled.contains(&"main".to_string()),
        "string-payload result must compile: {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    assert!(
        has_tag_dispatch(text_bytes(&program)),
        "expected tag load + conditional branch for the string-payload result match"
    );
}

#[test]
fn compiles_option_string_and_user_string_enum_natively() {
    // `option<string>` (the shape `map_get` on a `map<K, string>` returns) and a
    // user enum with a `string` payload both compile: the `some`/`Named` payload
    // slot is the immutable string pointer, bound as a flat word.
    let program = emit_native_program(&module_for(concat!(
        "enum Tag\n",
        "    Named string\n",
        "    Anon\n\n",
        "fn opt_len o option<string> -> i64\n",
        "    match o\n",
        "        some(s) -> len(s)\n",
        "        none -> 0\n\n",
        "fn tag_len t Tag -> i64\n",
        "    match t\n",
        "        Named(name) -> len(name)\n",
        "        Anon -> 0\n\n",
        "fn main -> i64\n",
        "    opt_len(some(\"ab\")) + tag_len(Named(\"cde\"))\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"opt_len".to_string())
            && program.compiled.contains(&"tag_len".to_string())
            && program.compiled.contains(&"main".to_string()),
        "option<string>/user string-payload enum must compile: {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn compiles_result_with_one_level_mutable_heap_payload() {
    // A `result<i64, list<i64>>` carries a ONE-LEVEL mutable heap payload in
    // `err`; the payload is deep-copied on the enum's value-semantic copy, so it
    // is now IN the native subset (mirroring the WASM backend) and compiles.
    let program = emit_native_program(&module_for(concat!(
        "fn classify n i64 -> i64\n",
        "    let xs list<i64> = list_new()\n",
        "    let r result<i64, list<i64>> = err(push(xs, n))\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> len(m)\n\n",
        "fn main -> i64\n",
        "    classify(3)\n",
    )))
    .expect("program with a one-level mutable enum payload compiles");
    assert!(
        program.compiled.iter().any(|n| n == "classify"),
        "expected `classify` to compile: skipped {:?}",
        program.skipped
    );
}

#[test]
fn defers_enum_with_two_level_mutable_heap_payload_gracefully() {
    // A `result<i64, list<list<list<i64>>>>` payload nests MUTABLE aggregates
    // past the one-level bound (`list<list<list<…>>>`), so it is still out of the
    // native subset and the function skips gracefully rather than miscompiling.
    let program = emit_native_program(&module_for(concat!(
        "fn classify n i64 -> i64\n",
        "    let r result<i64, list<list<list<i64>>>> = ok(n)\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> len(m)\n\n",
        "fn main -> i64\n",
        "    classify(3)\n",
    )));
    match program {
        Err(err) => assert!(
            err.skipped.iter().any(|s| s.name == "classify"),
            "expected `classify` skipped for its over-deep payload: {:?}",
            err.skipped
        ),
        Ok(program) => assert!(
            program.skipped.iter().any(|s| s.name == "classify"),
            "expected `classify` skipped for its over-deep payload: {:?}",
            program.skipped
        ),
    }
}

// -- Mutable-heap collection elements (list<struct>, list<list>, map<K,struct>) --

#[test]
fn compiles_list_of_structs_natively() {
    // A `list<struct>` now COMPILES: each element is a heap-struct pointer,
    // deep-copied per element on the list's value-semantic copy. `push`/`get`/
    // `set` cross the heap<->stack bridge (a struct field-block behind a pointer
    // for the element; a stack-flattened struct for the returned/consumed value).
    let program = emit_native_program(&module_for(concat!(
        "struct Point\n",
        "    x i64\n",
        "    y i64\n\n",
        "fn sum p Point -> i64\n",
        "    p.x + p.y\n\n",
        "fn build -> i64\n",
        "    let ps list<Point> = list_new()\n",
        "    ps = push(ps, Point(1, 2))\n",
        "    ps = push(ps, Point(3, 4))\n",
        "    sum(get(ps, 1))\n\n",
        "fn main -> i64\n",
        "    build()\n",
    )))
    .expect("emit native program for list<struct>");
    assert!(
        program.compiled.contains(&"build".to_string())
            && program.compiled.contains(&"main".to_string()),
        "list<struct> must compile: compiled {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    // The recursive per-element deep copy routes through the heap-struct copy
    // helper, so the object references `__lullaby_struct_copy`.
    assert!(
        coff_symbol(&program.bytes, STRUCT_COPY_SYMBOL).is_some(),
        "list<struct> deep copy must reference the heap-struct copy helper"
    );
}

#[test]
fn compiles_nested_list_of_lists_natively() {
    // A `list<list<i64>>` COMPILES: the outer element is a list pointer (already
    // one word); the outer copy deep-copies each inner list via the list-copy
    // helper (one mutable level, inner elements scalar). `get` returns an
    // independent inner list.
    let program = emit_native_program(&module_for(concat!(
        "fn build -> i64\n",
        "    let a list<i64> = list_new()\n",
        "    a = push(a, 5)\n",
        "    let rows list<list<i64>> = list_new()\n",
        "    rows = push(rows, a)\n",
        "    let r list<i64> = get(rows, 0)\n",
        "    get(r, 0)\n\n",
        "fn main -> i64\n",
        "    build()\n",
    )))
    .expect("emit native program for list<list<i64>>");
    assert!(
        program.compiled.contains(&"build".to_string()),
        "list<list<i64>> must compile: skipped {:?}",
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    assert!(
        coff_symbol(&program.bytes, LIST_COPY_SYMBOL).is_some(),
        "nested list deep copy must reference the list-copy helper"
    );
}

#[test]
fn compiles_map_of_structs_natively() {
    // A `map<i64, struct>` COMPILES: each entry value is a heap-struct pointer,
    // deep-copied per value on the map's value-semantic copy; `map_get` returns
    // `option<struct>` whose `some` payload is an independent heap struct.
    let program = emit_native_program(&module_for(concat!(
        "struct Point\n",
        "    x i64\n",
        "    y i64\n\n",
        "fn sum p Point -> i64\n",
        "    p.x + p.y\n\n",
        "fn build -> i64\n",
        "    let m map<i64, Point> = map_new()\n",
        "    m = map_set(m, 1, Point(2, 3))\n",
        "    match map_get(m, 1)\n",
        "        some(p) -> sum(p)\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    build()\n",
    )))
    .expect("emit native program for map<K, struct>");
    assert!(
        program.compiled.contains(&"build".to_string()),
        "map<i64, struct> must compile: skipped {:?}",
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    assert!(
        coff_symbol(&program.bytes, STRUCT_COPY_SYMBOL).is_some(),
        "map<K, struct> deep copy must reference the heap-struct copy helper"
    );
}

#[test]
fn heap_struct_copy_helper_is_emitted_and_recurses_via_alloc() {
    // The `__lullaby_struct_copy` helper is a real function in `.text` that calls
    // the bump allocator (a fresh, independent block) — the machine-code proof
    // that a heap-struct element deep copy is recursive, not a shared pointer.
    let program = emit_native_program(&module_for(concat!(
        "struct Cell\n",
        "    v i64\n\n",
        "fn read c Cell -> i64\n",
        "    c.v\n\n",
        "fn build -> i64\n",
        "    let xs list<Cell> = list_new()\n",
        "    xs = push(xs, Cell(7))\n",
        "    read(get(xs, 0))\n\n",
        "fn main -> i64\n",
        "    build()\n",
    )))
    .expect("emit native program");
    assert!(
        coff_symbol(&program.bytes, STRUCT_COPY_SYMBOL).is_some(),
        "the heap-struct copy helper must be emitted"
    );
    assert!(
        coff_symbol(&program.bytes, HEAP_ALLOC_SYMBOL).is_some(),
        "the heap-struct copy helper allocates a fresh block via the bump allocator"
    );
}

#[test]
fn defers_list_of_maps_gracefully() {
    // A `list<map<i64, i64>>` element is a MUTABLE `map` — outside the accepted
    // one-level struct/nested-list element set — so the function skips gracefully
    // (still runs on the interpreters) rather than miscompiling.
    let program = emit_native_program(&module_for(concat!(
        "fn build -> i64\n",
        "    let xs list<map<i64, i64>> = list_new()\n",
        "    len(xs)\n\n",
        "fn main -> i64\n",
        "    build()\n",
    )));
    match program {
        Err(err) => assert!(
            err.skipped.iter().any(|s| s.name == "build"),
            "list<map<..>> must skip: {:?}",
            err.skipped
        ),
        Ok(program) => assert!(
            program.skipped.iter().any(|s| s.name == "build"),
            "list<map<..>> must skip: {:?}",
            program.skipped
        ),
    }
}

// -- Aggregate parameter / return / call-argument ABI --------------------

#[test]
fn compiles_struct_parameter_and_return_with_by_pointer_abi() {
    // A function that TAKES a struct and returns an i64, a function that
    // RETURNS a struct, and a `main` that passes/receives both compile (not
    // skip). The by-pointer argument (`lea rax/rcx, [rbp+disp]` staged into an
    // argument register) and the hidden-return-pointer copy (`mov [rax-disp],
    // rcx` writing result words) must appear in the emitted code.
    let program = emit_native_program(&module_for(concat!(
        "struct Point\n    x i64\n    y i64\n\n",
        "fn taxicab p Point -> i64\n    p.x + p.y\n\n",
        "fn shift p Point d i64 -> Point\n    Point(p.x + d, p.y + d)\n\n",
        "fn main -> i64\n",
        "    let base Point = Point(3, 4)\n",
        "    let moved Point = shift(base, 10)\n",
        "    taxicab(base) + taxicab(moved)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"taxicab".to_string())
            && program.compiled.contains(&"shift".to_string())
            && program.compiled.contains(&"main".to_string()),
        "struct param/return functions must compile: compiled={:?} skipped={:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let text = text_bytes(&program);
    // Hidden-return-pointer write: `mov [rax - 8], rcx` (48 89 88 F8 FF FF FF)
    // — `shift` writes result word 1 through the caller-supplied pointer.
    assert!(
        text.windows(7)
            .any(|w| w == [0x48, 0x89, 0x88, 0xF8, 0xFF, 0xFF, 0xFF]),
        "expected a hidden-return-pointer word write (`mov [rax-8], rcx`)"
    );
    // By-pointer argument: `lea rax, [rbp+disp]` (48 8D 85 ..) stages the
    // address of a materialized aggregate argument copy before it is pushed.
    assert!(
        text.windows(3).any(|w| w == [0x48, 0x8D, 0x85]),
        "expected a `lea rax, [rbp+disp]` staging an aggregate argument address"
    );
    // Hidden return pointer passed in rcx: `lea rcx, [rbp+disp]` (48 8D 8D ..).
    assert!(
        text.windows(3).any(|w| w == [0x48, 0x8D, 0x8D]),
        "expected a `lea rcx, [rbp+disp]` for the hidden return pointer"
    );
}

#[test]
fn compiles_fixed_array_parameter_and_return() {
    // A function taking a fixed array and one returning a fixed array compile;
    // the array lengths are inferred from the call sites / returned literal.
    let program = emit_native_program(&module_for(concat!(
        "fn sum_array xs array<i64> -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to len(xs) - 1\n",
        "        total += xs[i]\n",
        "    total\n\n",
        "fn doubled xs array<i64> -> array<i64>\n",
        "    let out array<i64> = [0, 0, 0]\n",
        "    for i from 0 to len(xs) - 1\n",
        "        out[i] = xs[i] * 2\n",
        "    out\n\n",
        "fn main -> i64\n",
        "    let data array<i64> = [1, 2, 3]\n",
        "    let d array<i64> = doubled(data)\n",
        "    sum_array(data) + d[0] + d[1] + d[2]\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"sum_array".to_string())
            && program.compiled.contains(&"doubled".to_string())
            && program.compiled.contains(&"main".to_string()),
        "array param/return functions must compile: compiled={:?} skipped={:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn aggregate_parameter_copies_in_for_value_semantics() {
    // A struct parameter is copied into the callee's frame in the prologue
    // (`mov rcx, [rax - disp]` then `mov [rbp - slot], rcx`), so mutating the
    // parameter cannot affect the caller's copy. The prologue copy-in loads
    // from the argument pointer via `mov rcx, [rax + disp]` (48 8B 88 ..).
    let program = emit_native_program(&module_for(concat!(
        "struct Box\n    a i64\n    b i64\n\n",
        "fn clobber s Box -> i64\n",
        "    s.a = s.a + 1\n",
        "    s.a + s.b\n\n",
        "fn main -> i64\n",
        "    let box Box = Box(10, 20)\n",
        "    let inside i64 = clobber(box)\n",
        "    inside + box.a + box.b\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"clobber".to_string()),
        "clobber must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    let text = text_bytes(&program);
    // Copy-in read from the argument pointer: `mov rcx, [rax + disp32]`
    // (48 8B 88 ..) — the callee reads the caller's snapshot word-by-word.
    assert!(
        text.windows(3).any(|w| w == [0x48, 0x8B, 0x88]),
        "expected an aggregate-parameter copy-in read (`mov rcx, [rax+disp]`)"
    );
}

#[test]
fn compiles_enum_parameter_and_return_and_match_on_call() {
    // An `option<i64>` (a scalar-payload enum) as a parameter and a return
    // type compiles, including a `match` on an enum-returning call.
    let program = emit_native_program(&module_for(concat!(
        "fn classify n i64 -> option<i64>\n",
        "    if n > 0\n",
        "        return some(n)\n",
        "    none\n\n",
        "fn unwrap_or o option<i64> d i64 -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> d\n\n",
        "fn direct n i64 -> i64\n",
        "    match classify(n)\n",
        "        some(v) -> v + 1\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    unwrap_or(classify(2), 9) + direct(0)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"classify".to_string())
            && program.compiled.contains(&"unwrap_or".to_string())
            && program.compiled.contains(&"direct".to_string()),
        "enum param/return/match-on-call must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn compiles_string_field_aggregate_across_boundary() {
    // A struct with an immutable `string` field crosses a function boundary via the
    // by-pointer aggregate ABI: the field is a flat pointer word, copied-in by the
    // callee and shared (never deep-copied) since strings are immutable, so value
    // semantics hold. A function taking such a struct by value now COMPILES.
    let program = emit_native_program(&module_for(concat!(
        "struct Named\n    id i64\n    label string\n\n",
        "fn id_and_len n Named -> i64\n    n.id + len(n.label)\n\n",
        "fn main -> i64\n    id_and_len(Named(7, \"svc\"))\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"id_and_len".to_string())
            && program.compiled.contains(&"main".to_string()),
        "string-field aggregate boundary must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(
        program.skipped.is_empty(),
        "no skips: {:?}",
        program.skipped
    );
}

#[test]
fn defers_mutable_heap_field_aggregate_parameter() {
    // A struct field of a MUTABLE heap type (`list`/`map`) is still not a native
    // aggregate (the flat word copy would share the block), so a function taking it
    // by value skips gracefully rather than miscompiling.
    let program = emit_native_program(&module_for(concat!(
        "struct Bag\n    id i64\n    items list<i64>\n\n",
        "fn id_of n Bag -> i64\n    n.id\n\n",
        "fn main -> i64\n    7\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        program.skipped.iter().any(|s| s.name == "id_of"),
        "mutable-heap-field aggregate parameter must skip: {:?}",
        program.skipped
    );
}

#[test]
fn aggregate_return_with_four_params_uses_a_stack_argument() {
    // Four scalar parameters plus a hidden return pointer make five effective
    // register arguments; the fifth (the last parameter) now spills to the
    // stack rather than demoting the function. The callee reads its last
    // parameter from `[rbp+16]` and `main`'s call passes the hidden result
    // pointer in `rcx` and the 5th effective argument on the stack.
    let program = emit_native_program(&module_for(concat!(
        "struct Quad\n    a i64\n    b i64\n\n",
        "fn build w i64 x i64 y i64 z i64 -> Quad\n    Quad(w + x, y + z)\n\n",
        "fn main -> i64\n    let q Quad = build(1, 2, 3, 4)\n    q.a + q.b\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"build".to_string())
            && program.compiled.contains(&"main".to_string()),
        "aggregate return with four params must compile via a stack argument: {:?} / {:?}",
        program.compiled,
        program.skipped,
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);

    let text = text_bytes(&program);
    // `build`'s prologue reads its 4th (0-indexed effective position 4, since
    // the hidden return pointer is position 0) parameter `z` from `[rbp+48]`.
    assert!(
        text.windows(7)
            .any(|w| w == [0x48, 0x8B, 0x85, 0x30, 0x00, 0x00, 0x00]),
        "expected the 5th effective argument (param `z`) loaded from [rbp+48]"
    );
}

// -- Growable list<T> (scalar element) native codegen --------------------

#[test]
fn compiles_growable_list_function_natively() {
    // A function that builds a scalar-element `list<i64>` via `list_new`/
    // `push`/`set`/`pop`/`get`/`len` — including a signature returning
    // `list<i64>` and one taking it — now compiles natively (not skipped).
    let program = emit_native_program(&module_for(concat!(
        "fn build -> list<i64>\n",
        "    let xs list<i64> = list_new()\n",
        "    xs = push(xs, 10)\n",
        "    xs = push(xs, 20)\n",
        "    xs\n\n",
        "fn total xs list<i64> -> i64\n",
        "    let ys list<i64> = set(xs, 0, 5)\n",
        "    let zs list<i64> = pop(ys)\n",
        "    get(ys, 0) + len(zs)\n\n",
        "fn main -> i64\n",
        "    let xs list<i64> = build()\n",
        "    total(xs)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"build".to_string())
            && program.compiled.contains(&"total".to_string())
            && program.compiled.contains(&"main".to_string()),
        "growable list functions must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn list_object_emits_grow_and_copy_helpers() {
    // A list-using program (no string constants) still emits the heap path:
    // the object must contain the `__lullaby_list_new`/`_copy`/`_grow`
    // runtime-helper symbols and the bump allocator, proving grow/copy codegen
    // is present.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let xs list<i64> = list_new()\n",
        "    xs = push(xs, 1)\n",
        "    xs = push(xs, 2)\n",
        "    len(xs)\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    // The three list helpers + the bump allocator are external-defined symbols.
    for symbol in [
        LIST_NEW_SYMBOL,
        LIST_COPY_SYMBOL,
        LIST_GROW_SYMBOL,
        HEAP_ALLOC_SYMBOL,
    ] {
        let (section, _storage) =
            coff_symbol(&program.bytes, symbol).unwrap_or_else(|| panic!("missing {symbol}"));
        assert_eq!(section, 1, "{symbol} must be defined in .text");
    }
    // The bump-heap `.bss` cell/region symbols must be present too.
    assert!(
        coff_symbol(&program.bytes, HEAP_BASE_SYMBOL).is_some(),
        "the .bss heap region must be present for a list-using object"
    );
}

#[test]
fn push_call_site_calls_copy_then_grow() {
    // A `push` call site deep-copies the source list (value semantics) and then
    // grows it, so `main`'s text carries relocations against BOTH the copy and
    // grow helpers.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let xs list<i64> = list_new()\n",
        "    xs = push(xs, 7)\n",
        "    get(xs, 0)\n",
    )))
    .expect("emit native program");
    let main = program
        .compiled
        .iter()
        .position(|n| n == "main")
        .expect("main compiled");
    assert_eq!(main, 0);
    // The object references the copy + grow helpers (proving push emits both).
    assert!(
        coff_symbol(&program.bytes, LIST_COPY_SYMBOL).is_some(),
        "push must reference the list-copy helper"
    );
    assert!(
        coff_symbol(&program.bytes, LIST_GROW_SYMBOL).is_some(),
        "push must reference the list-grow helper"
    );
}

#[test]
fn compiles_string_element_list_natively() {
    // A `list<string>` now COMPILES: a `string` element is an immutable heap
    // pointer stored in one slot exactly like a scalar, appended by `push`,
    // loaded back by `get`, and shared (not deep-recursed) on the flat word-copy
    // deep copy. The list header and grow/copy helpers are the same as a scalar
    // list.
    let program = emit_native_program(&module_for(concat!(
        "fn names n i64 -> list<string>\n",
        "    let xs list<string> = list_new()\n",
        "    xs = push(xs, \"a\")\n",
        "    xs = push(xs, to_string(n))\n",
        "    xs\n\n",
        "fn head l list<string> -> i64\n",
        "    len(get(l, 0))\n\n",
        "fn main -> i64\n",
        "    head(names(3)) + len(names(3))\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"names".to_string())
            && program.compiled.contains(&"head".to_string())
            && program.compiled.contains(&"main".to_string()),
        "list<string> functions must compile: {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    assert!(
        coff_symbol(&program.bytes, LIST_COPY_SYMBOL).is_some()
            && coff_symbol(&program.bytes, LIST_GROW_SYMBOL).is_some(),
        "list<string> value-semantics still reference the list copy/grow helpers"
    );
}

#[test]
fn defers_mutable_heap_element_list_gracefully() {
    // A `list<array<i64>>` (MUTABLE heap element) is still DEFERRED: it would
    // need a recursive per-element deep copy, so the enclosing function skips
    // with a clear reason and still runs on the interpreters — never
    // miscompiled.
    let program = emit_native_program(&module_for(concat!(
        "fn grid -> list<array<i64>>\n",
        "    list_new()\n\n",
        "fn main -> i64\n",
        "    len(grid())\n",
    )));
    // `main` calls `grid` (which is skipped), so `main` demotes too; the whole
    // program has no eligible function -> the L0339 "nothing eligible" error.
    let err = program.expect_err("mutable-heap-element list must not compile");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(
        err.skipped.iter().any(|s| s.name == "grid"),
        "the mutable-heap-element list function must be recorded as skipped: {:?}",
        err.skipped
    );
}

#[test]
fn compiles_float_element_list_natively() {
    // A `list<f64>` (float scalar element) compiles: elements are stored as
    // bit-preserving 8-byte words, and a float `get` moves the word back into
    // an XMM register.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let xs list<f64> = list_new()\n",
        "    xs = push(xs, 1.5)\n",
        "    xs = push(xs, 2.5)\n",
        "    let a f64 = get(xs, 0)\n",
        "    let b f64 = get(xs, 1)\n",
        "    let flag i64 = 0\n",
        "    if a + b > 3.9\n",
        "        flag = 1\n",
        "    flag + len(xs)\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

// -- Growable map<K, V> (scalar key/value) native codegen ----------------

#[test]
fn compiles_growable_map_function_natively() {
    // A program that builds a scalar `map<i64, i64>` via `map_new`/`map_set`
    // (including a signature returning `map<i64, i64>`, one taking it, and a
    // `match map_get(...)`), reads it via `map_get`/`map_has`/`map_len` — all
    // compile natively (nothing skipped).
    let program = emit_native_program(&module_for(concat!(
        "fn build -> map<i64, i64>\n",
        "    let m map<i64, i64> = map_new()\n",
        "    m = map_set(m, 1, 10)\n",
        "    m = map_set(m, 2, 20)\n",
        "    m = map_set(m, 2, 99)\n",
        "    m\n\n",
        "fn lookup m map<i64, i64> k i64 -> i64\n",
        "    match map_get(m, k)\n",
        "        some(v) -> v\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    let m map<i64, i64> = build()\n",
        "    let has1 i64 = 0\n",
        "    if map_has(m, 1)\n",
        "        has1 = 1\n",
        "    map_len(m) + lookup(m, 2) + has1\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"build".to_string())
            && program.compiled.contains(&"lookup".to_string())
            && program.compiled.contains(&"main".to_string()),
        "growable map functions must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn rc_drop_inserted_for_owned_loop_string_but_not_when_it_escapes() {
    // The `__lullaby_rc_dec` call site (`E8` after `mov rcx, [rbp-slot]`) is the
    // observable signature of drop insertion. A uniquely-owned, borrow-only loop
    // string local gets one; an escaping one (returned via an accumulator) does
    // not — proving the default-deny analysis.
    let dropped = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 10\n",
        "        let s string = \"payload\"\n",
        "        total = total + len(s)\n",
        "    total\n",
    )))
    .expect("emit dropped program");
    // The drop SITE is `mov rcx, [rbp - slot]` (48 8B 8D disp32) immediately
    // followed by a `call` (E8) — a shape the runtime helpers never emit (they
    // have no `rbp` frame), so it uniquely marks a drop in a user function. The
    // `rc_dec` relocation alone is ambiguous: the always-emitted `concat_own`
    // helper references `rc_dec` too.
    let has_drop_site = |program: &NativeProgram| {
        program
            .bytes
            .windows(8)
            .any(|w| w[0..3] == [0x48, 0x8B, 0x8D] && w[7] == 0xE8)
    };
    assert!(
        has_drop_site(&dropped),
        "a uniquely-owned borrow-only loop string must be dropped"
    );

    // Escaping: the string is concatenated into an accumulator that survives the
    // loop, so ownership escapes and it must NOT be dropped.
    let escapes = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let acc string = \"\"\n",
        "    for i from 0 to 10\n",
        "        let s string = \"x\"\n",
        "        acc = acc + s\n",
        "    len(acc)\n",
    )))
    .expect("emit escaping program");
    assert!(
        !has_drop_site(&escapes),
        "a string whose ownership escapes into an accumulator must NOT be dropped"
    );
}

/// Arena-first memory (stage 1): the arena-mode prologue's distinctive byte
/// sequence — `mov eax, 1` (`B8 01 00 00 00`) then `mov [rip + alloc_mode], rax`
/// (`48 89 05`) — marks a function that routes its heap allocations through the
/// function-scoped arena. Only `emit_arena_prologue` emits this shape.
fn has_arena_prologue(program: &NativeProgram) -> bool {
    program
        .bytes
        .windows(8)
        .any(|w| w == [0xB8, 0x01, 0x00, 0x00, 0x00, 0x48, 0x89, 0x05])
}

#[test]
fn arena_region_used_for_provably_local_heap_function() {
    // `build_len` returns a scalar, allocates a string that stays local (only read
    // by `len`), calls no user function, and has no loop — so it is arena-eligible
    // and takes the arena path. `main` calls a user function, so it does not.
    let program = emit_native_program(&module_for(concat!(
        "fn build_len x i64 -> i64\n",
        "    let s string = to_string(x) + \"!\"\n",
        "    len(s)\n\n",
        "fn main -> i64\n",
        "    build_len(5)\n",
    )))
    .expect("emit arena program");
    assert!(
        program.compiled.contains(&"build_len".to_string()),
        "build_len must compile natively: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(
        has_arena_prologue(&program),
        "a provably-local heap-using function must take the arena path"
    );
}

#[test]
fn arena_not_used_when_a_heap_value_escapes() {
    // (a) A function that RETURNS a heap value cannot be arena (the value outlives
    // the call). `make_s` returns a string; `main` only calls a user function.
    // Neither function is arena, so no arena prologue is emitted anywhere.
    let returns_heap = emit_native_program(&module_for(concat!(
        "fn make_s -> string\n",
        "    to_string(5) + \"!\"\n\n",
        "fn main -> i64\n",
        "    len(make_s())\n",
    )))
    .expect("emit returns-heap program");
    assert!(
        !has_arena_prologue(&returns_heap),
        "a function returning a heap value must NOT take the arena path"
    );

    // (b) A function that PASSES a heap value to a user-defined function cannot be
    // arena (the callee could retain the pointer). `build` allocates a string and
    // passes it to `tag`; `tag` returns a string (also not arena); `main` only
    // calls a user function. No arena prologue anywhere.
    let escapes_via_call = emit_native_program(&module_for(concat!(
        "fn tag s string -> string\n",
        "    s + \"?\"\n\n",
        "fn build -> i64\n",
        "    let s string = to_string(5) + \"!\"\n",
        "    len(tag(s))\n\n",
        "fn main -> i64\n",
        "    build()\n",
    )))
    .expect("emit escapes-via-call program");
    assert!(
        !has_arena_prologue(&escapes_via_call),
        "a function passing a heap value to a user function must NOT take the arena path"
    );
}

/// Arena stage-2: the per-iteration bump-pointer rewind shape emitted at a loop
/// iteration edge — `mov r10, [rbp - mark]` (`4C 8B 95 disp32`) immediately
/// followed by `mov [rip + heap_next], r10` (`4C 89 15`). Only
/// `emit_arena_loop_rewind` emits this exact shape.
fn count_arena_loop_rewinds(program: &NativeProgram) -> usize {
    program
        .bytes
        .windows(10)
        .filter(|w| w[0..3] == [0x4C, 0x8B, 0x95] && w[7..10] == [0x4C, 0x89, 0x15])
        .count()
}

#[test]
fn arena_sub_region_used_for_confined_heap_loop() {
    // Stage 2: a scalar-returning LEAF function whose loop allocates per-iteration
    // scratch that stays LOCAL (a fresh `string` read only by `len`, accumulating a
    // SCALAR `total`) now routes through the arena AND gives the loop a per-iteration
    // sub-region. `sum_lens` therefore takes the arena path (function prologue) and
    // emits the loop rewind on the fallthrough back-edge.
    let program = emit_native_program(&module_for(concat!(
        "fn sum_lens n i64 -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to n\n",
        "        let s string = to_string(i) + \"!\"\n",
        "        total = total + len(s)\n",
        "    total\n\n",
        "fn main -> i64\n",
        "    sum_lens(10)\n",
    )))
    .expect("emit confined-loop program");
    assert!(
        program.compiled.contains(&"sum_lens".to_string()),
        "sum_lens must compile natively: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(
        has_arena_prologue(&program),
        "a scalar-returning leaf whose only heap loop is confined must take the arena path"
    );
    assert!(
        count_arena_loop_rewinds(&program) >= 1,
        "the confined heap loop must emit a per-iteration bump-pointer rewind"
    );
}

#[test]
fn arena_sub_region_not_used_when_loop_accumulator_escapes() {
    // Stage 2 default-deny: a loop that ACCUMULATES a heap value into a variable
    // living OUTSIDE the iteration (`acc = acc + "x"`) lets the heap escape the
    // iteration, so the loop cannot get a per-iteration rewind — and, because the
    // loop is then an UNBOUNDED heap loop, the whole function is not arena-routed
    // (it stays on the RC / free-list path). No arena prologue, no loop rewind.
    let program = emit_native_program(&module_for(concat!(
        "fn grow n i64 -> i64\n",
        "    let acc string = \"\"\n",
        "    for i from 0 to n\n",
        "        acc = acc + \"x\"\n",
        "    len(acc)\n\n",
        "fn main -> i64\n",
        "    grow(10)\n",
    )))
    .expect("emit escaping-accumulator program");
    assert!(
        program.compiled.contains(&"grow".to_string()),
        "grow must still compile natively (on the RC path): {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(
        !has_arena_prologue(&program),
        "a function whose loop accumulator escapes must NOT take the arena path"
    );
    assert_eq!(
        count_arena_loop_rewinds(&program),
        0,
        "an escaping-accumulator loop must NOT emit a per-iteration rewind"
    );
}

#[test]
fn arena_sub_region_not_used_when_loop_value_stored_outside() {
    // Stage 2 default-deny: storing a loop-allocated heap value into a variable
    // declared OUTSIDE the loop (`last = to_string(i) + "!"`) escapes the iteration,
    // so the loop is not confined and the function is not arena-routed. Returns the
    // outer value's length so the store is observable/kept.
    let program = emit_native_program(&module_for(concat!(
        "fn keep_last n i64 -> i64\n",
        "    let last string = \"\"\n",
        "    for i from 0 to n\n",
        "        last = to_string(i) + \"!\"\n",
        "    len(last)\n\n",
        "fn main -> i64\n",
        "    keep_last(10)\n",
    )))
    .expect("emit stored-outside program");
    assert!(
        !has_arena_prologue(&program),
        "a function that stores a loop-allocated value outside the loop must NOT be arena"
    );
    assert_eq!(
        count_arena_loop_rewinds(&program),
        0,
        "a loop whose value is stored outside must NOT emit a per-iteration rewind"
    );
}

#[test]
fn rc_drop_inserted_on_break_and_continue_early_exit_edges() {
    // RC stage 2: a uniquely-owned, borrow-only loop string is now dropped on the
    // `break`/`continue` early-exit edges too, not only the fallthrough back-edge.
    // The drop SITE is `mov rcx, [rbp - slot]` (48 8B 8D disp32) immediately followed
    // by a `call` (E8) — a shape the runtime helpers never emit (no rbp frame), so it
    // uniquely marks a drop in a user function. Counting sites lets us prove the
    // early-exit edge added exactly one more drop than the fallthrough-only baseline.
    let count_drop_sites = |program: &NativeProgram| -> usize {
        program
            .bytes
            .windows(8)
            .filter(|w| w[0..3] == [0x48, 0x8B, 0x8D] && w[7] == 0xE8)
            .count()
    };

    // Baseline: no early exit — one fallthrough drop of `s`.
    let baseline = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 10\n",
        "        let s string = to_string(i) + \"!\"\n",
        "        total = total + len(s)\n",
        "    total\n",
    )))
    .expect("emit baseline program");
    let baseline_sites = count_drop_sites(&baseline);
    assert!(
        baseline_sites >= 1,
        "the fallthrough back-edge must drop the owned loop string"
    );

    // Same loop with a conditional `continue`: the continue edge drops `s` too, so
    // there is exactly one more drop site than the baseline (continue-edge + fallthrough).
    let with_continue = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 10\n",
        "        let s string = to_string(i) + \"!\"\n",
        "        total = total + len(s)\n",
        "        if i > 5\n",
        "            continue\n",
        "        total = total + 1\n",
        "    total\n",
    )))
    .expect("emit continue program");
    assert_eq!(
        count_drop_sites(&with_continue),
        baseline_sites + 1,
        "a `continue` must drop the live owned string on its edge (one extra drop site)"
    );

    // Same loop with a conditional `break`: the break edge drops `s` too.
    let with_break = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 10\n",
        "        let s string = to_string(i) + \"!\"\n",
        "        total = total + len(s)\n",
        "        if i > 5\n",
        "            break\n",
        "    total\n",
    )))
    .expect("emit break program");
    assert_eq!(
        count_drop_sites(&with_break),
        baseline_sites + 1,
        "a `break` must drop the live owned string on its edge (one extra drop site)"
    );

    // Default-deny still holds on early-exit edges: a `continue` BEFORE the owned
    // `let` must not drop a slot whose `let` has not run — the early-exit drop set is
    // revealed only as declarations are lowered, so there is no extra drop here.
    let continue_before_let = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 10\n",
        "        if i > 5\n",
        "            continue\n",
        "        let s string = to_string(i) + \"!\"\n",
        "        total = total + len(s)\n",
        "    total\n",
    )))
    .expect("emit continue-before-let program");
    assert_eq!(
        count_drop_sites(&continue_before_let),
        baseline_sites,
        "a `continue` textually before the owned `let` must NOT drop that slot"
    );
}

#[test]
fn upper_lower_helpers_fold_ascii_case() {
    // The uppercase helper subtracts 0x20 (`83 E9 20`) from a byte in `a..=z`; the
    // lowercase helper adds 0x20 (`83 C1 20`) to a byte in `A..=Z`. Both allocate a
    // fresh same-size record (reference the bump allocator).
    let upper = emit_str_upper_helper();
    assert!(
        upper.code.windows(3).any(|w| w == [0x83, 0xE9, 0x20]),
        "upper must `sub ecx, 0x20`"
    );
    assert!(
        upper
            .relocations
            .iter()
            .any(|r| r.symbol == HEAP_ALLOC_SYMBOL),
        "upper must allocate a fresh record"
    );
    let lower = emit_str_lower_helper();
    assert!(
        lower.code.windows(3).any(|w| w == [0x83, 0xC1, 0x20]),
        "lower must `add ecx, 0x20`"
    );
    // A program using them compiles natively and defines the symbols.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    len(upper(\"aB\")) + len(lower(\"Cd\"))\n",
    )))
    .expect("emit upper/lower program");
    assert!(program.compiled.contains(&"main".to_string()));
    for symbol in [STR_UPPER_SYMBOL, STR_LOWER_SYMBOL] {
        assert!(
            coff_symbol(&program.bytes, symbol).is_some_and(|(s, _)| s == 1),
            "{symbol} must be defined in .text"
        );
    }
}

#[test]
fn read_own_helper_forwards_and_reclaims_the_source() {
    // The string-read ownership helper indirect-calls the op (`call r9`), then
    // `rc_dec`s the source. `substring(to_string(i), …)` on a fresh temp emits it.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 3\n",
        "        total = total + len(substring(to_string(i), 0, 1))\n",
        "    total\n",
    )))
    .expect("emit substring-of-temp program");
    let (section, _storage) = coff_symbol(&program.bytes, STR_READ_OWN_SYMBOL)
        .unwrap_or_else(|| panic!("missing helper symbol {STR_READ_OWN_SYMBOL}"));
    assert_eq!(section, 1, "{STR_READ_OWN_SYMBOL} must be defined in .text");
    let helper = emit_str_read_own_helper();
    assert!(
        helper.code.windows(3).any(|w| w == [0x41, 0xFF, 0xD1]),
        "read_own must indirect-call the op (`call r9`)"
    );
    assert!(
        helper.relocations.iter().any(|r| r.symbol == RC_DEC_SYMBOL),
        "read_own must rc_dec the fresh source"
    );
}

#[test]
fn binop_own_helper_forwards_operands_and_reclaims_fresh_ones() {
    // The two-string ownership-aware helper must `mov rsi, rdx` (48 89 D6 — NOT the
    // no-op 48 89 F6 that a prior typo emitted, which left the right operand as
    // garbage and crashed on its rc_dec), indirect-`call r9` the op, and `rc_dec`
    // the marked operands.
    let helper = emit_str_binop_own_helper();
    assert!(
        helper.code.windows(3).any(|w| w == [0x48, 0x89, 0xD6]),
        "binop_own must set the right operand with `mov rsi, rdx` (48 89 D6)"
    );
    assert!(
        !helper.code.windows(3).any(|w| w == [0x48, 0x89, 0xF6]),
        "binop_own must NOT contain the no-op `mov rsi, rsi` (48 89 F6)"
    );
    assert!(
        helper.code.windows(3).any(|w| w == [0x41, 0xFF, 0xD1]),
        "binop_own must indirect-call the op (`call r9`)"
    );
    assert!(
        helper
            .relocations
            .iter()
            .filter(|r| r.symbol == RC_DEC_SYMBOL)
            .count()
            >= 2,
        "binop_own must rc_dec both marked operands"
    );
}

#[test]
fn len_of_fresh_temp_emits_and_uses_ownership_aware_helper() {
    // `len(to_string(i))` reads a fresh temporary, so the object emits and
    // (per the reclamation parity test) calls `__lullaby_str_len_own`, which reads
    // the `char_len` header then `rc_dec`s the record. The helper is emitted only
    // on the heap path; assert it is defined in `.text` and has the right shape.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 3\n",
        "        total = total + len(to_string(i))\n",
        "    total\n",
    )))
    .expect("emit len-of-temp program");
    let (section, _storage) = coff_symbol(&program.bytes, STR_LEN_OWN_SYMBOL)
        .unwrap_or_else(|| panic!("missing helper symbol {STR_LEN_OWN_SYMBOL}"));
    assert_eq!(section, 1, "{STR_LEN_OWN_SYMBOL} must be defined in .text");
    // The helper reads the header (mov rbx, [rcx]) then rc_decs the record.
    let helper = emit_str_len_own_helper();
    assert!(
        helper
            .code
            .windows(4)
            .any(|w| w == [0x48, 0x8B, 0x59, 0x00]),
        "str_len_own must read the char_len header (mov rbx, [rcx])"
    );
    assert!(
        helper.relocations.iter().any(|r| r.symbol == RC_DEC_SYMBOL),
        "str_len_own must rc_dec the fresh temporary after reading its length"
    );
}

#[test]
fn concat_of_fresh_temps_uses_ownership_aware_helper() {
    // `to_string(i) + "…"` has two fresh-temp operands, so the concat lowers to
    // the ownership-aware helper (which `rc_dec`s the intermediates); a plain
    // `param + param` (both borrowed) keeps the bare concat with no operand drop.
    let owned = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let s string = to_string(7) + \"!\"\n",
        "    len(s)\n",
    )))
    .expect("emit owned-concat program");
    assert!(
        coff_symbol(&owned.bytes, STR_CONCAT_OWN_SYMBOL).is_some_and(|(sec, _)| sec == 1),
        "concat of fresh temps must reference the ownership-aware helper"
    );
    // The concat_own helper itself must chain concat and rc_dec the marked
    // operands.
    let helper = emit_str_concat_own_helper();
    assert!(
        helper
            .relocations
            .iter()
            .any(|r| r.symbol == STR_CONCAT_SYMBOL)
            && helper.relocations.iter().any(|r| r.symbol == RC_DEC_SYMBOL),
        "concat_own must call concat then rc_dec the owned operands"
    );
}

#[test]
fn array_string_loop_temp_uses_recursive_drop() {
    // A uniquely-owned `array<string>` loop temp (a `split` result) used only via
    // `len` gets the recursive `__lullaby_drop_string_array`; the helper itself
    // must `rc_dec` (elements + block) — its recursive element loop then block
    // drop.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let text string = \"a,b,c\"\n",
        "    let sep string = \",\"\n",
        "    let total i64 = 0\n",
        "    for i from 0 to 10\n",
        "        let parts array<string> = split(text, sep)\n",
        "        total = total + len(parts)\n",
        "        for j from 0 to len(parts) - 1\n",
        "            total = total + len(parts[j])\n",
        "    total\n",
    )))
    .expect("emit array-string program");
    assert!(
        coff_symbol(&program.bytes, DROP_STRING_ARRAY_SYMBOL).is_some_and(|(s, _)| s == 1),
        "an owned borrow-only array<string> loop temp must be recursively dropped"
    );
    let helper = emit_drop_string_array_helper();
    assert!(
        helper
            .relocations
            .iter()
            .filter(|r| r.symbol == RC_DEC_SYMBOL)
            .count()
            >= 2,
        "drop_string_array must rc_dec both each element and the block"
    );
}

#[test]
fn struct_string_field_loop_temp_uses_recursive_drop() {
    // A uniquely-owned, borrow-only `struct` loop temp with a `string` field gets
    // the recursive drop-glue: each owned string field is `rc_dec`'d at the loop
    // edges (the recursive-drop template — `rc_dec` per heap field). The field is a
    // plain string literal and the local is read only via `len(r.name)`, so the ONLY
    // source of an `rc_dec` reference in the program is that per-field drop.
    let program = emit_native_program(&module_for(concat!(
        "struct Rec\n    name string\n    id i64\n\n",
        "fn scan n i64 -> i64\n",
        "    let total i64 = 0\n",
        "    let i i64 = 0\n",
        "    loop\n",
        "        if i >= n\n",
        "            break\n",
        "        let r Rec = Rec(\"rec\", i)\n",
        "        total = total + len(r.name) + r.id\n",
        "        i = i + 1\n",
        "    total\n\n",
        "fn main -> i64\n    scan(3)\n",
    )))
    .expect("emit struct-string loop program");
    assert!(
        program.compiled.contains(&"scan".to_string()),
        "a struct-with-string-field loop function must compile: {:?}",
        program.skipped
    );
    assert!(
        coff_symbol(&program.bytes, RC_DEC_SYMBOL).is_some_and(|(s, _)| s == 1),
        "an owned borrow-only struct-string loop temp must be recursively dropped \
         (rc_dec per string field)"
    );
}

#[test]
fn struct_string_field_reclaim_on_rc_free_list_path() {
    // The recursive struct-string drop-glue composing with the RC / free-list path:
    // `sweep` calls a user function (`tick`), so it is NOT arena-eligible (a leaf
    // requirement fails) and keeps the RC codegen — its per-iteration `rc_dec` of the
    // struct's `string` field actually frees the record onto the free list, which the
    // next iteration's alloc reuses (bounded heap). The struct is borrow-only (read
    // only via `len(r.name)` + the scalar `r.id`), so exactly ONE `rc_dec` fires per
    // record: no double-free, no leak. Assert the function compiles, is NOT arena
    // (so `rc_free` really frees), and the per-field `rc_dec` drop is present.
    let program = emit_native_program(&module_for(concat!(
        "struct Rec\n    name string\n    id i64\n\n",
        "fn tick x i64 -> i64\n    x\n\n",
        "fn sweep n i64 -> i64\n",
        "    let total i64 = 0\n",
        "    for i from 0 to n\n",
        "        let r Rec = Rec(to_string(i) + \"!!!!!!!!!!\", i)\n",
        "        total = total + len(r.name) + r.id\n",
        "    tick(total)\n\n",
        "fn main -> i64\n    sweep(3)\n",
    )))
    .expect("emit struct-string RC reclaim program");
    assert!(
        program.compiled.contains(&"sweep".to_string()),
        "the struct-string RC reclaim function must compile: {:?}",
        program.skipped
    );
    assert!(
        !has_arena_prologue(&program),
        "a non-leaf function must NOT take the arena path (RC / free-list reclaims)"
    );
    assert!(
        coff_symbol(&program.bytes, RC_DEC_SYMBOL).is_some_and(|(s, _)| s == 1),
        "the borrow-only struct-string loop temp must be recursively dropped \
         (one rc_dec per string field) on the RC path"
    );
}

#[test]
fn struct_string_field_reclaim_on_arena_path() {
    // The recursive struct-string drop-glue composing with the ARENA path:
    // `build_len` is a provably-local heap-using LEAF (scalar return, no user calls,
    // no loop) that constructs a `struct` with a fresh `string` field kept local
    // (read only via `len`), so it is arena-eligible and takes the arena path — its
    // whole region is reclaimed by the bump-pointer rewind on return. `rc_free`
    // no-ops in arena mode, so the drop-glue and the arena rewind coexist without a
    // double-free.
    let program = emit_native_program(&module_for(concat!(
        "struct Rec\n    name string\n    id i64\n\n",
        "fn build_len x i64 -> i64\n",
        "    let r Rec = Rec(to_string(x) + \"!!!!!!!!!!\", x)\n",
        "    len(r.name) + r.id\n\n",
        "fn main -> i64\n    build_len(5)\n",
    )))
    .expect("emit struct-string arena reclaim program");
    assert!(
        program.compiled.contains(&"build_len".to_string()),
        "the struct-string arena reclaim function must compile: {:?}",
        program.skipped
    );
    assert!(
        has_arena_prologue(&program),
        "a provably-local struct-string heap function must take the arena path"
    );
}

#[test]
fn arena_not_used_when_a_struct_string_escapes_a_confined_loop() {
    // Default-deny soundness fix: a `struct` value transitively carries heap (its
    // `string` field), so storing it into an iteration-outliving location is an
    // ESCAPE. `f` reassigns the outer `acc` (a `Rec`) inside a loop; were the loop
    // treated as "confined", a per-iteration arena sub-region rewind would reclaim a
    // record `acc` still references — a use-after-free once a later allocation reuses
    // the rewound bytes. The escape analysis now recognizes the heap-carrying struct
    // store, so `f` is NOT arena-routed (no sub-region), keeping it sound.
    let program = emit_native_program(&module_for(concat!(
        "struct Rec\n    name string\n    id i64\n\n",
        "fn f n i64 -> i64\n",
        "    let acc Rec = Rec(\"start\", 0)\n",
        "    let total i64 = 0\n",
        "    for i from 1 to n\n",
        "        if i == 1\n",
        "            acc = Rec(to_string(i) + \"Z\", i)\n",
        "        let scratch string = to_string(i) + \"0123456789\"\n",
        "        total = total + len(acc.name) + len(scratch)\n",
        "    total\n\n",
        "fn main -> i64\n    f(5)\n",
    )))
    .expect("emit struct-string escape program");
    assert!(
        program.compiled.contains(&"f".to_string()),
        "the function still compiles (on the RC path): {:?}",
        program.skipped
    );
    assert!(
        !has_arena_prologue(&program),
        "a loop storing a heap-carrying struct to an outer var must NOT be arena-confined"
    );
}

#[test]
fn rc_dec_helper_frees_at_zero_and_tail_calls_free() {
    // rc_dec decrements the refcount at [rcx-8] and tail-jumps to rc_free only at
    // zero; rc_free pushes the block base (rcx-16) onto the free list.
    let dec = emit_rc_dec_helper();
    assert_eq!(
        &dec.code[0..4],
        &[0x48, 0xFF, 0x49, 0xF8],
        "rc_dec must start with `dec qword [rcx - 8]`"
    );
    assert!(
        dec.relocations.iter().any(|r| r.symbol == RC_FREE_SYMBOL),
        "rc_dec must tail-call rc_free at zero"
    );
    let free = emit_rc_free_helper();
    assert!(
        free.relocations
            .iter()
            .any(|r| r.symbol == HEAP_FREE_HEAD_SYMBOL),
        "rc_free must push onto the free list"
    );
    // Arena-first memory (stage 1): rc_free first checks the arena-mode flag and,
    // when set, returns without pushing (the arena reclaims by bump-pointer rewind).
    assert_eq!(
        &free.code[0..3],
        &[0x48, 0x8B, 0x05],
        "rc_free must begin by loading the arena-mode flag (`mov rax, [rip + alloc_mode]`)"
    );
    assert!(
        free.relocations
            .iter()
            .any(|r| r.symbol == ALLOC_MODE_SYMBOL),
        "rc_free must reference the arena-mode flag"
    );
    // The RC push path still computes the block base with `lea rax, [rcx - 16]`.
    assert!(
        free.code.windows(4).any(|w| w == [0x48, 0x8D, 0x41, 0xF0]),
        "rc_free must compute the block base with `lea rax, [rcx - 16]`"
    );
}

#[test]
fn alloc_helper_carries_rc_header_and_free_list() {
    // The allocator is a free-list allocator with a 16-byte RC header: its body
    // references the `.bss` free-list head symbol (the reuse scan) and seeds a
    // fresh block's refcount to 1 (`mov qword [rax+8], 1` / `[r11+8], 1`).
    let helper = emit_heap_alloc_helper();
    assert!(
        helper
            .relocations
            .iter()
            .any(|r| r.symbol == HEAP_FREE_HEAD_SYMBOL),
        "the allocator must scan the free list for reuse"
    );
    // Both the bump path and the reuse path write an immediate refcount of 1
    // into the block header (the `01 00 00 00` of a `mov qword [reg+8], 1`).
    assert!(
        helper
            .code
            .windows(8)
            .any(|w| w == [0x48, 0xC7, 0x40, 0x08, 0x01, 0x00, 0x00, 0x00])
            && helper
                .code
                .windows(8)
                .any(|w| w == [0x49, 0xC7, 0x43, 0x08, 0x01, 0x00, 0x00, 0x00]),
        "the allocator must seed refcount = 1 on both the bump and reuse paths"
    );
}

#[test]
fn alloc_helper_has_heap_overflow_guard() {
    // Part B (safety): the bump path must bounds-check against the end of the fixed
    // heap region and trap (`ud2`) on exhaustion instead of writing out of bounds.
    // The guard forms `r11 = heap_base + HEAP_REGION_SIZE` via `add r11, imm32`
    // (`49 81 C3 <region>`) then `cmp r9, r11` (`4D 39 D9`) + `jbe +2` (`76 02`) +
    // `ud2` (`0F 0B`).
    let helper = emit_heap_alloc_helper();
    let region = HEAP_REGION_SIZE.to_le_bytes();
    let add_r11 = [0x49, 0x81, 0xC3, region[0], region[1], region[2], region[3]];
    assert!(
        helper.code.windows(7).any(|w| w == add_r11),
        "the guard must compute heap_end = heap_base + HEAP_REGION_SIZE"
    );
    assert!(
        helper
            .code
            .windows(7)
            .any(|w| w == [0x4D, 0x39, 0xD9, 0x76, 0x02, 0x0F, 0x0B]),
        "the allocator must carry a `cmp r9,r11; jbe +2; ud2` heap-exhaustion guard"
    );
}

#[test]
fn map_object_emits_map_and_alloc_helpers() {
    // A map-using program (no string constants) still emits the heap path: the
    // object must contain the `__lullaby_map_new`/`_copy`/`_grow`/`_find`
    // runtime-helper symbols and the bump allocator, proving map codegen is
    // present and defined in `.text`.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let m map<i64, i64> = map_new()\n",
        "    m = map_set(m, 1, 5)\n",
        "    m = map_set(m, 2, 6)\n",
        "    map_len(m)\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    for symbol in [
        MAP_NEW_SYMBOL,
        MAP_COPY_SYMBOL,
        MAP_GROW_SYMBOL,
        MAP_FIND_SYMBOL,
        HEAP_ALLOC_SYMBOL,
    ] {
        let (section, _storage) =
            coff_symbol(&program.bytes, symbol).unwrap_or_else(|| panic!("missing {symbol}"));
        assert_eq!(section, 1, "{symbol} must be defined in .text");
    }
    assert!(
        coff_symbol(&program.bytes, HEAP_BASE_SYMBOL).is_some(),
        "the .bss heap region must be present for a map-using object"
    );
}

#[test]
fn map_set_call_site_calls_copy_then_find() {
    // A `map_set` call site deep-copies the source map (value semantics) and
    // then scans it, so `main`'s object carries relocations against BOTH the
    // map-copy and the map-find helpers.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let m map<i64, i64> = map_new()\n",
        "    m = map_set(m, 3, 7)\n",
        "    map_len(m)\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    assert!(
        coff_symbol(&program.bytes, MAP_COPY_SYMBOL).is_some(),
        "map_set must reference the map-copy helper"
    );
    assert!(
        coff_symbol(&program.bytes, MAP_FIND_SYMBOL).is_some(),
        "map_set must reference the map-find helper"
    );
}

#[test]
fn compiles_float_value_map_natively() {
    // A `map<i64, f64>` (float value) compiles: values are stored/loaded as
    // bit-preserving 8-byte words, and a `some(v)` float payload round-trips
    // through the option layout.
    let program = emit_native_program(&module_for(concat!(
        "fn probe m map<i64, f64> k i64 -> i64\n",
        "    match map_get(m, k)\n",
        "        some(v) -> 1\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    let m map<i64, f64> = map_new()\n",
        "    m = map_set(m, 1, 1.5)\n",
        "    m = map_set(m, 2, 2.5)\n",
        "    probe(m, 2) + map_len(m)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"probe".to_string())
            && program.compiled.contains(&"main".to_string()),
        "float-value map functions must compile: {:?} / {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn defers_string_key_map_gracefully() {
    // A `map<string, i64>` (heap key) is DEFERRED: the enclosing function skips
    // with a clear reason and still runs on the interpreters — never
    // miscompiled. String-key equality needs the string heap (content
    // comparison), matching the WASM map's first increment.
    let program = emit_native_program(&module_for(concat!(
        "fn build -> map<string, i64>\n",
        "    map_set(map_new(), \"a\", 1)\n\n",
        "fn main -> i64\n",
        "    map_len(build())\n",
    )));
    let err = program.expect_err("string-key map must not compile");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(
        err.skipped
            .iter()
            .any(|s| s.name == "build" && s.reason.contains("map")),
        "the skip reason must cite the deferred map key/value: {:?}",
        err.skipped
    );
}

#[test]
fn compiles_string_value_map_natively() {
    // A `map<i64, string>` (scalar key, `string` value) now COMPILES: the value
    // slot holds an immutable string pointer, shared on the flat two-word entry
    // copy. `map_set` inserts/updates the pointer, `map_get` returns
    // `option<string>` (the `some` payload slot is the string pointer), and
    // `map_has`/`map_len` work unchanged.
    let program = emit_native_program(&module_for(concat!(
        "fn build n i64 -> map<i64, string>\n",
        "    let m map<i64, string> = map_new()\n",
        "    m = map_set(m, 1, \"a\")\n",
        "    m = map_set(m, 2, to_string(n))\n",
        "    m = map_set(m, 1, \"zz\")\n",
        "    m\n\n",
        "fn probe n i64 -> i64\n",
        "    let m map<i64, string> = build(n)\n",
        "    let seen i64 = 0\n",
        "    if map_has(m, 2)\n",
        "        seen = 1\n",
        "    match map_get(m, 1)\n",
        "        some(s) -> len(s) + seen + map_len(m)\n",
        "        none -> 0\n\n",
        "fn main -> i64\n",
        "    probe(3)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"build".to_string())
            && program.compiled.contains(&"probe".to_string())
            && program.compiled.contains(&"main".to_string()),
        "map<i64, string> functions must compile: {:?} / skipped {:?}",
        program.compiled,
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    assert!(
        coff_symbol(&program.bytes, MAP_COPY_SYMBOL).is_some()
            && coff_symbol(&program.bytes, MAP_FIND_SYMBOL).is_some(),
        "map<i64, string> value-semantics still reference the map copy/find helpers"
    );
}

#[test]
fn defers_mutable_heap_value_map_gracefully() {
    // A `map<i64, array<i64>>` (MUTABLE heap value) is still DEFERRED: it would
    // need a recursive per-value deep copy, so the enclosing function skips with
    // a clear reason and still runs on the interpreters — never miscompiled.
    let program = emit_native_program(&module_for(concat!(
        "fn build -> map<i64, array<i64>>\n",
        "    map_new()\n\n",
        "fn main -> i64\n",
        "    map_len(build())\n",
    )));
    let err = program.expect_err("mutable-heap-value map must not compile");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(
        err.skipped
            .iter()
            .any(|s| s.name == "build" && s.reason.contains("map")),
        "the skip reason must cite the deferred map key/value: {:?}",
        err.skipped
    );
}

#[test]
fn string_value_functions_compile_natively() {
    // A program using first-class string values — a string literal as a value,
    // `+` concatenation, `to_string`, `len` on a string, and a string
    // parameter/return crossing a function boundary — compiles natively (not
    // skipped) across all its functions.
    let program = emit_native_program(&module_for(concat!(
        "fn greeting name string -> string\n",
        "    \"hi \" + name\n\n",
        "fn measure s string -> i64\n",
        "    len(s)\n\n",
        "fn main -> i64\n",
        "    let m string = greeting(\"x\")\n",
        "    let labeled string = m + to_string(2)\n",
        "    measure(labeled) + len(to_string(true))\n",
    )))
    .expect("emit native program");
    for func in ["greeting", "measure", "main"] {
        assert!(
            program.compiled.contains(&func.to_string()),
            "expected `{func}` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped
        );
    }
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
}

#[test]
fn string_object_emits_string_runtime_helpers() {
    // A string-using program emits the string runtime helpers + the bump
    // allocator as external-defined `.text` symbols, proving the literal /
    // concat / to_string codegen is present.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let s string = \"a\" + to_string(1)\n",
        "    len(s)\n",
    )))
    .expect("emit native program");
    assert_eq!(program.compiled, vec!["main".to_string()]);
    for symbol in [
        STR_LIT_SYMBOL,
        STR_CONCAT_SYMBOL,
        STR_FROM_INT_SYMBOL,
        STR_FROM_BOOL_SYMBOL,
        STR_FROM_CHAR_SYMBOL,
        STR_SUBSTRING_SYMBOL,
        STR_CHAR_AT_SYMBOL,
        STR_COUNT_SYMBOL,
        STR_REPEAT_SYMBOL,
        STR_TRIM_SYMBOL,
        STR_FIND_SYMBOL,
        STR_CONTAINS_SYMBOL,
        STR_STARTS_WITH_SYMBOL,
        STR_ENDS_WITH_SYMBOL,
        HEAP_ALLOC_SYMBOL,
    ] {
        let (section, _storage) =
            coff_symbol(&program.bytes, symbol).unwrap_or_else(|| panic!("missing {symbol}"));
        assert_eq!(section, 1, "{symbol} must be defined in .text");
    }
    // The `.bss` heap region must be present for a string-using object.
    assert!(
        coff_symbol(&program.bytes, HEAP_BASE_SYMBOL).is_some(),
        "the .bss heap region must be present for a string-using object"
    );
}

#[test]
fn concat_call_site_calls_the_concat_helper() {
    // A `s + t` concatenation lowers to a `call __lullaby_str_concat`, so a
    // concatenating function carries a relocation against the concat helper.
    // (The helper function is named `cat`, not `join`, to avoid the `join`
    // builtin, whose registered signature would type the arguments as
    // `array<string>`.)
    let program = emit_native_program(&module_for(concat!(
        "fn cat a string b string -> string\n",
        "    a + b\n\n",
        "fn main -> i64\n",
        "    len(cat(\"x\", \"y\"))\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"cat".to_string()),
        "cat must compile: {:?}",
        program.skipped
    );
    // The concat helper is emitted as a symbol (references it via a relocation).
    assert!(
        coff_symbol(&program.bytes, STR_CONCAT_SYMBOL).is_some(),
        "the concat helper symbol must be present"
    );
}

#[test]
fn index_string_op_call_sites_call_their_helpers() {
    // Each index-based string op lowers to a `call` of its `.text` helper, so a
    // function using them all references every helper symbol via a relocation
    // and compiles natively (never skips to the interpreters). The bool results
    // are folded to i64 through a tiny helper so `main` stays i64-returning.
    let program = emit_native_program(&module_for(concat!(
        "fn b2i x bool -> i64\n",
        "    if x\n",
        "        return 1\n",
        "    return 0\n\n",
        "fn main -> i64\n",
        "    let s string = \"café\"\n",
        "    let head string = substring(s, 0, 2)\n",
        "    let idx i64 = find(s, \"f\")\n",
        "    let c i64 = b2i(contains(s, \"af\"))\n",
        "    let sw i64 = b2i(starts_with(s, \"ca\"))\n",
        "    let ew i64 = b2i(ends_with(s, \"é\"))\n",
        "    len(head) + idx + c + sw + ew\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "main must compile natively: skipped {:?}",
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    for symbol in [
        STR_SUBSTRING_SYMBOL,
        STR_CHAR_AT_SYMBOL,
        STR_COUNT_SYMBOL,
        STR_REPEAT_SYMBOL,
        STR_TRIM_SYMBOL,
        STR_FIND_SYMBOL,
        STR_CONTAINS_SYMBOL,
        STR_STARTS_WITH_SYMBOL,
        STR_ENDS_WITH_SYMBOL,
    ] {
        let (section, _storage) = coff_symbol(&program.bytes, symbol)
            .unwrap_or_else(|| panic!("missing helper symbol {symbol}"));
        assert_eq!(section, 1, "{symbol} must be defined in .text");
    }
}

#[test]
fn parse_i64_compiles_native_and_emits_its_helper() {
    // A function that `match`es `parse_i64(s)` compiles natively (the aggregate
    // `result<i64, string>` is materialized into scratch and dispatched), and
    // the emitted object defines the `__lullaby_parse_i64` helper in `.text`.
    let program = emit_native_program(&module_for(concat!(
        "fn to_int s string -> i64\n",
        "    match parse_i64(s)\n",
        "        ok(n) -> n\n",
        "        err(m) -> 0 - len(m)\n\n",
        "fn main -> i64\n",
        "    to_int(\"41\") + to_int(\"nope\")\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"to_int".to_string())
            && program.compiled.contains(&"main".to_string()),
        "parse_i64 users must compile natively: skipped {:?}",
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    let (section, _storage) = coff_symbol(&program.bytes, PARSE_I64_SYMBOL)
        .unwrap_or_else(|| panic!("missing helper symbol {PARSE_I64_SYMBOL}"));
    assert_eq!(section, 1, "{PARSE_I64_SYMBOL} must be defined in .text");
}

#[test]
fn split_and_join_compile_native_and_emit_their_helpers() {
    // A function that builds an `array<string>` with `split`, indexes it,
    // reads `len`, and `join`s it back compiles natively (the heap
    // `array<string>` reuses the `list<string>` block layout), and the object
    // defines the `__lullaby_str_split`/`__lullaby_str_join` helpers in `.text`.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    let p array<string> = split(\"a,b,c\", \",\")\n",
        "    let j string = join(p, \"-\")\n",
        "    len(p) * 100 + len(p[0]) * 10 + len(j)\n",
    )))
    .expect("emit native program");
    assert!(
        program.compiled.contains(&"main".to_string()),
        "split/join user must compile natively: skipped {:?}",
        program.skipped
    );
    assert!(program.skipped.is_empty(), "{:?}", program.skipped);
    for symbol in [STR_SPLIT_SYMBOL, STR_JOIN_SYMBOL] {
        let (section, _storage) = coff_symbol(&program.bytes, symbol)
            .unwrap_or_else(|| panic!("missing helper symbol {symbol}"));
        assert_eq!(section, 1, "{symbol} must be defined in .text");
    }
}

#[test]
fn split_helper_traps_empty_separator_and_composes_string_helpers() {
    // The split helper composes the tested count/find/substring helpers and
    // traps (`ud2`) on an empty separator (the interpreters' `L0417`).
    let helper = emit_str_split_helper();
    assert!(
        helper.code.windows(2).any(|w| w == [0x0F, 0x0B]),
        "the split helper must carry a `ud2` trap for an empty separator"
    );
    for symbol in [
        STR_COUNT_SYMBOL,
        STR_FIND_SYMBOL,
        STR_SUBSTRING_SYMBOL,
        HEAP_ALLOC_SYMBOL,
    ] {
        assert!(
            helper.relocations.iter().any(|r| r.symbol == symbol),
            "the split helper must call {symbol}"
        );
    }
    // Join is built from the concat helper.
    let join = emit_str_join_helper();
    assert!(
        join.relocations
            .iter()
            .any(|r| r.symbol == STR_CONCAT_SYMBOL),
        "the join helper must chain the concat helper"
    );
}

#[test]
fn parse_i64_helper_allocates_its_error_message() {
    // The `err` path builds a fresh `cannot parse `{text}` as i64` record, so
    // the helper references the bump allocator and carries a checked `imul`
    // (`4D 6B D2 0A` = `imul r10, r10, 10`) for the overflow-detecting accumulate.
    let helper = emit_parse_i64_helper();
    assert!(
        helper
            .relocations
            .iter()
            .any(|r| r.symbol == HEAP_ALLOC_SYMBOL),
        "the parse_i64 helper must call the bump allocator to build its error string"
    );
    assert!(
        helper
            .code
            .windows(4)
            .any(|w| w == [0x4D, 0x6B, 0xD2, 0x0A]),
        "the parse_i64 helper must use a checked `imul r10, r10, 10` accumulate"
    );
}

#[test]
fn substring_helper_emits_the_l0413_trap() {
    // The `substring` helper bounds-checks the char range and traps with `ud2`
    // (0F 0B) on a violation, mirroring the interpreters' `L0413` — it must not
    // silently produce a wrong slice. Assert the helper body carries a `ud2`.
    let helper = emit_str_substring_helper();
    assert!(
        helper.code.windows(2).any(|w| w == [0x0F, 0x0B]),
        "the substring helper must carry a `ud2` trap for out-of-bounds ranges"
    );
    // It allocates a fresh record, so it references the bump allocator.
    assert!(
        helper
            .relocations
            .iter()
            .any(|r| r.symbol == HEAP_ALLOC_SYMBOL),
        "the substring helper must call the bump allocator"
    );
}

#[test]
fn index_string_scan_helpers_are_leaf_functions() {
    // `find`/`contains`/`starts_with`/`ends_with` are pure scans: no allocation,
    // so they carry no relocations, and each returns via a single `ret` (0xC3).
    for helper in [
        emit_str_find_helper(),
        emit_str_contains_helper(),
        emit_str_starts_with_helper(),
        emit_str_ends_with_helper(),
    ] {
        assert!(
            helper.relocations.is_empty(),
            "{} must be a leaf (no calls/relocations)",
            helper.name
        );
        assert_eq!(
            helper.code.last(),
            Some(&0xC3),
            "{} must end in `ret`",
            helper.name
        );
    }
}

#[test]
fn to_string_of_float_skips_gracefully() {
    // `to_string(f64)` needs dtoa, which is deferred: the function skips (runs
    // on the interpreters) rather than miscompiling. With no other eligible
    // function, the emitter returns the `L0339` no-eligible error naming the
    // skip.
    let program = emit_native_program(&module_for(concat!(
        "fn main -> i64\n",
        "    len(to_string(1.5))\n",
    )));
    let err = program.expect_err("float to_string must not compile");
    assert_eq!(err.code, NATIVE_NO_ELIGIBLE_CODE);
    assert!(
        err.skipped
            .iter()
            .any(|s| s.name == "main" && s.reason.contains("to_string")),
        "the skip reason must cite the deferred float to_string: {:?}",
        err.skipped
    );
}

#[test]
fn string_is_not_a_by_pointer_aggregate() {
    // A `string` is a single immutable pointer word, so it is classified as a
    // scalar (register value), not a by-pointer aggregate — unlike a struct or
    // enum. This keeps a string parameter/return in an integer register with no
    // deep copy.
    assert!(!NativeType::String.is_aggregate());
    assert_eq!(NativeType::String.words(), 1);
    let native = resolve_native_type(&TypeRef::new("string"), &[], &[]).expect("resolve string");
    assert_eq!(native, NativeType::String);
    assert!(
        !native_signature_type_is_aggregate(&TypeRef::new("string"), &[], &[])
            .expect("string classifies"),
        "a string signature slot is a scalar (register), not an aggregate"
    );
}

// -- Cross-format object emission (ELF / Mach-O) -------------------------
//
// These exercise the object-format abstraction end-to-end: the same lowered
// x86-64 program is re-serialized into an ELF64 or Mach-O container. The bytes
// are checked structurally (link+run is deferred to Phase 9 CI on the native
// platform), and the default Windows COFF path is confirmed unchanged.

const ADD_AND_MAIN: &str =
    "fn add a i64 b i64 -> i64\n    a + b\n\nfn main -> i64\n    return add(20, 22)\n";

#[test]
fn elf_target_emits_relocatable_elf64() {
    let program = emit_native_program_for_target(
        &module_for(ADD_AND_MAIN),
        &crate::native_contract::x86_64_linux_target(),
        None,
        false,
    )
    .expect("emit ELF program");

    assert_eq!(program.target.triple, "x86_64-unknown-linux-gnu");
    assert_eq!(program.entry_symbol, "_start");
    assert_eq!(&program.bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
    assert_eq!(program.bytes[4], 2, "ELFCLASS64");
    // e_type = ET_REL, e_machine = EM_X86_64.
    assert_eq!(read_u16(&program.bytes, 16), 1);
    assert_eq!(read_u16(&program.bytes, 18), 62);
    assert_eq!(
        program.compiled,
        vec!["add".to_string(), "main".to_string()]
    );
}

#[test]
fn macho_target_emits_relocatable_macho64() {
    let program = emit_native_program_for_target(
        &module_for(ADD_AND_MAIN),
        &crate::native_contract::x86_64_macos_target(),
        None,
        false,
    )
    .expect("emit Mach-O program");

    assert_eq!(program.target.triple, "x86_64-apple-darwin");
    assert_eq!(program.entry_symbol, "start");
    // MH_MAGIC_64 little-endian.
    assert_eq!(read_u32(&program.bytes, 0), 0xFEED_FACF);
    // filetype = MH_OBJECT.
    assert_eq!(read_u32(&program.bytes, 12), 1);
}

#[test]
fn elf_entry_stub_exits_via_the_linux_syscall() {
    // The freestanding `_start` stub must end in `mov eax, 60` (SYS_exit) then
    // `syscall`, and must NOT reference `ExitProcess` (a Windows-only import).
    let program = emit_native_program_for_target(
        &module_for(ADD_AND_MAIN),
        &crate::native_contract::x86_64_linux_target(),
        None,
        false,
    )
    .expect("emit ELF program");
    // Locate `.text` and confirm the exit-syscall byte sequence appears.
    let needle = [0xB8, 0x3C, 0x00, 0x00, 0x00, 0x0F, 0x05]; // mov eax,60; syscall
    assert!(
        program.bytes.windows(needle.len()).any(|w| w == needle),
        "ELF entry stub issues the Linux exit syscall"
    );
}

#[test]
fn default_target_is_unchanged_windows_coff() {
    // The default emit path and the explicit Windows target produce identical
    // COFF bytes, proving the abstraction did not disturb the default.
    let module = module_for(ADD_AND_MAIN);
    let default = emit_native_program(&module).expect("default");
    let windows = emit_native_program_for_target(
        &module,
        &crate::native_contract::x86_64_windows_target(),
        None,
        false,
    )
    .expect("windows");
    assert_eq!(
        default.bytes, windows.bytes,
        "COFF bytes byte-for-byte equal"
    );
    assert_eq!(default.target.triple, "x86_64-pc-windows-msvc");
    assert_eq!(read_u16(&default.bytes, 0), AMD64_MACHINE);
}

#[test]
fn elf_string_program_carries_rodata_and_bss() {
    // A program that interns a string constant must produce the data sections
    // in the ELF object too (.rodata for the constant, .bss for the heap).
    let program = emit_native_program_for_target(
            &module_for(
                "fn main -> i64\n    let a i64 = len(\"hello\")\n    let b i64 = len(\"native\")\n    return a + b\n",
            ),
            &crate::native_contract::x86_64_linux_target(),
            None,
            false,
        )
        .expect("emit ELF program with strings");
    assert_eq!(&program.bytes[0..4], &[0x7f, b'E', b'L', b'F']);
    // More than the text-only section count (null,.text,.symtab,.strtab,.shstrtab
    // = 5); the data path adds .rodata, .bss, and .rela.text.
    assert!(read_u16(&program.bytes, 60) > 5, "data sections present");
}

#[test]
fn read_only_array_param_compiles_as_fat_pointer() {
    // A read-only `array<i64>` parameter whose length is not inferable from a call
    // site is NO LONGER demoted: it compiles as a fat pointer (data_ptr + runtime
    // length). `sum_array` iterates with `for x in a` (needs `len(a)` and `a[i]`);
    // `count_at` indexes `a[i]` with a length from a separate `n` parameter. Both
    // are helpers with no native caller, so neither could infer a stack length.
    let program = emit_native_program(&module_for(concat!(
        "fn sum_array a array<i64> -> i64\n",
        "    let acc i64 = 0\n",
        "    for x in a\n",
        "        acc = acc + x\n",
        "    acc\n\n",
        "fn count_at a array<i64> n, x i64 -> i64\n",
        "    let c i64 = 0\n",
        "    for i from 0 to n - 1\n",
        "        if a[i] == x\n",
        "            c = c + 1\n",
        "    c\n\n",
        "fn main -> i64\n",
        "    let xs array<i64> = [4, 1, 4, 2]\n",
        "    sum_array(xs) + count_at(xs, 4, 4)\n",
    )))
    .expect("emit native program");
    for name in ["sum_array", "count_at", "main"] {
        assert!(
            program.compiled.contains(&name.to_string()),
            "expected `{name}` compiled: {:?} / skipped {:?}",
            program.compiled,
            program.skipped,
        );
    }
    assert!(
        !program
            .skipped
            .iter()
            .any(|s| s.reason.contains("no call site to infer its length")),
        "no fat-pointer helper should demote for a missing call-site length: {:?}",
        program.skipped,
    );

    let text = text_bytes(&program);
    // The fat-array element read bounds-checks the index in `rax` against the
    // descriptor's RUNTIME length word `[rbp - len_slot]`: `cmp rax, [rbp+disp32]`
    // = `48 3B 85 ..` followed by `jb +2` / `ud2` = `72 02 0F 0B`.
    assert!(
        text.windows(2).any(|w| w == [0x3B, 0x85])
            && text.windows(4).any(|w| w == [0x72, 0x02, 0x0F, 0x0B]),
        "expected a fat-array runtime-length bounds check (cmp rax,[rbp-len]; jb+2; ud2)"
    );
}
