//! Structural tests for the direct PE32+ executable writer. They emit a native
//! program through the full frontend, then parse the produced `.exe` bytes back
//! and assert the DOS/PE headers, the PE32+ optional header, the section table,
//! and the `kernel32!ExitProcess` import table are all well-formed and mutually
//! consistent — mirroring the parse-back structural tests in `elf_object.rs` /
//! `macho_object.rs`. Actual load-and-run parity is proven by the CLI test
//! `native_freestanding_direct_pe_runs` (no linker involved).

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

fn pe_for(source: &str) -> Vec<u8> {
    emit_native_program(&module_for(source))
        .expect("native program emits")
        .pe_image
        .expect("freestanding program emits a direct PE")
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

/// The file offset of the PE header (`e_lfanew`) and a sanity check of the DOS
/// magic and PE signature.
fn pe_header(bytes: &[u8]) -> usize {
    assert_eq!(&bytes[0..2], b"MZ", "DOS magic");
    let e_lfanew = u32_at(bytes, 0x3C) as usize;
    assert_eq!(&bytes[e_lfanew..e_lfanew + 4], b"PE\0\0", "PE signature");
    e_lfanew
}

/// The parsed section table: `(name, virtual_addr, virtual_size, raw_ptr,
/// raw_size, characteristics)` per section.
fn sections(bytes: &[u8]) -> Vec<(String, u32, u32, u32, u32, u32)> {
    let pe = pe_header(bytes);
    let num_sections = u16_at(bytes, pe + 6) as usize;
    let opt_size = u16_at(bytes, pe + 20) as usize;
    let table = pe + 24 + opt_size;
    (0..num_sections)
        .map(|i| {
            let rec = table + i * 40;
            let name = String::from_utf8_lossy(&bytes[rec..rec + 8])
                .trim_end_matches('\0')
                .to_string();
            (
                name,
                u32_at(bytes, rec + 12),
                u32_at(bytes, rec + 8),
                u32_at(bytes, rec + 20),
                u32_at(bytes, rec + 16),
                u32_at(bytes, rec + 36),
            )
        })
        .collect()
}

/// Resolve an image RVA to a file offset via the section table.
fn rva_to_offset(bytes: &[u8], rva: u32) -> usize {
    for (_, va, vsize, raw_ptr, raw_size, _) in sections(bytes) {
        // Use the larger of virtual/raw size as the mapped span (`.bss` has no
        // raw bytes, but the import table lives in `.idata` which does).
        let span = vsize.max(raw_size);
        if rva >= va && rva < va + span {
            return (raw_ptr + (rva - va)) as usize;
        }
    }
    panic!("RVA {rva:#x} is not inside any section");
}

fn data_dir(bytes: &[u8], index: usize) -> (u32, u32) {
    let pe = pe_header(bytes);
    let opt = pe + 24;
    let dir = opt + 112 + index * 8;
    (u32_at(bytes, dir), u32_at(bytes, dir + 4))
}

#[test]
fn trivial_program_emits_valid_pe32plus() {
    let pe = pe_for("fn main -> i64\n    42\n");
    let header = pe_header(&pe);
    let coff = header + 4;

    assert_eq!(u16_at(&pe, coff), 0x8664, "machine = AMD64");
    // Characteristics: EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE | RELOCS_STRIPPED.
    let characteristics = u16_at(&pe, coff + 18);
    assert_eq!(
        characteristics & 0x0002,
        0x0002,
        "IMAGE_FILE_EXECUTABLE_IMAGE"
    );
    assert_eq!(characteristics & 0x0020, 0x0020, "LARGE_ADDRESS_AWARE");
    assert_eq!(
        characteristics & 0x0001,
        0x0001,
        "RELOCS_STRIPPED (fixed base)"
    );

    let opt = coff + 20;
    assert_eq!(u16_at(&pe, opt), 0x020B, "PE32+ optional header magic");
    assert_eq!(u64_at(&pe, opt + 24), 0x1_4000_0000, "ImageBase");
    assert_eq!(u32_at(&pe, opt + 32), 0x1000, "SectionAlignment");
    assert_eq!(u32_at(&pe, opt + 36), 0x200, "FileAlignment");
    assert_eq!(u16_at(&pe, opt + 68), 3, "Subsystem = WINDOWS_CUI");
    assert_eq!(u32_at(&pe, opt + 108), 16, "NumberOfRvaAndSizes");

    // The entry point RVA is the start of `.text` (the entry stub) and its bytes
    // begin with `sub rsp, 40` then a `call rel32`.
    let entry_rva = u32_at(&pe, opt + 16);
    let entry = rva_to_offset(&pe, entry_rva);
    assert_eq!(
        &pe[entry..entry + 4],
        &[0x48, 0x83, 0xEC, 0x28],
        "sub rsp, 40"
    );
    assert_eq!(pe[entry + 4], 0xE8, "call main (rel32)");
    // Then `mov ecx, eax` and the indirect `call [rip+__imp_ExitProcess]`.
    assert_eq!(&pe[entry + 9..entry + 11], &[0x89, 0xC1], "mov ecx, eax");
    assert_eq!(
        &pe[entry + 11..entry + 13],
        &[0xFF, 0x15],
        "call [rip+disp32]"
    );

    // SizeOfImage / SizeOfHeaders are alignment-consistent.
    assert_eq!(u32_at(&pe, opt + 56) % 0x1000, 0, "SizeOfImage aligned");
    assert_eq!(u32_at(&pe, opt + 60) % 0x200, 0, "SizeOfHeaders aligned");
}

#[test]
fn trivial_program_has_text_and_idata_sections_only() {
    let pe = pe_for("fn main -> i64\n    42\n");
    let names: Vec<String> = sections(&pe).into_iter().map(|s| s.0).collect();
    assert_eq!(names, vec![".text".to_string(), ".idata".to_string()]);

    // `.text` is executable+readable code; `.idata` is readable+writable data.
    let text = sections(&pe).into_iter().find(|s| s.0 == ".text").unwrap();
    assert_eq!(text.5, 0x6000_0020, ".text = CODE|EXECUTE|READ");
    let idata = sections(&pe).into_iter().find(|s| s.0 == ".idata").unwrap();
    assert_eq!(idata.5, 0xC000_0040, ".idata = INIT_DATA|READ|WRITE");
}

#[test]
fn import_table_names_kernel32_exitprocess() {
    let pe = pe_for("fn main -> i64\n    7\n");

    // Import directory data directory (index 1) points at the descriptor table.
    let (import_rva, import_size) = data_dir(&pe, 1);
    assert_eq!(import_size, 40, "one descriptor + null terminator");
    let desc = rva_to_offset(&pe, import_rva);

    // Descriptor fields: OriginalFirstThunk, TimeDateStamp, ForwarderChain, Name,
    // FirstThunk. The Name RVA must resolve to "kernel32.dll".
    let ilt_rva = u32_at(&pe, desc);
    let name_rva = u32_at(&pe, desc + 12);
    let iat_rva = u32_at(&pe, desc + 16);
    let name = rva_to_offset(&pe, name_rva);
    assert_eq!(&pe[name..name + 13], b"kernel32.dll\0", "imported DLL name");

    // The IAT data directory (index 12) matches the descriptor's FirstThunk.
    let (iat_dir_rva, iat_dir_size) = data_dir(&pe, 12);
    assert_eq!(iat_dir_rva, iat_rva, "IAT data dir == FirstThunk");
    assert_eq!(iat_dir_size, 16, "one IAT entry + null terminator");

    // Both the ILT and the IAT first entry are by-name (high bit clear) and point
    // at a hint/name whose name is "ExitProcess".
    for thunk_rva in [ilt_rva, iat_rva] {
        let thunk = rva_to_offset(&pe, thunk_rva);
        let entry = u64_at(&pe, thunk);
        assert_eq!(entry & (1 << 63), 0, "import by name, not ordinal");
        let hint_name = rva_to_offset(&pe, entry as u32);
        assert_eq!(u16_at(&pe, hint_name), 0, "hint = 0");
        assert_eq!(
            &pe[hint_name + 2..hint_name + 2 + 12],
            b"ExitProcess\0",
            "imported function name"
        );
        // The second (terminator) thunk entry is zero.
        assert_eq!(u64_at(&pe, thunk + 8), 0, "thunk null terminator");
    }
}

#[test]
fn program_with_calls_and_loops_emits_valid_pe() {
    // Recursion, an inter-function call, and a range `for` — the entry stub's
    // `call main` and the internal `call`s must all resolve to in-image
    // displacements without a linker.
    let source = "fn add a i64 b i64 -> i64\n    a + b\n\n\
                  fn fib n i64 -> i64\n    if n < 2\n        return n\n    return fib(n - 1) + fib(n - 2)\n\n\
                  fn sum_to n i64 -> i64\n    let total i64 = 0\n    for i from 1 to n\n        total += i\n    return total\n\n\
                  fn main -> i64\n    add(fib(6), sum_to(4))\n";
    let pe = pe_for(source);
    let header = pe_header(&pe);
    assert_eq!(u16_at(&pe, header + 4), 0x8664);
    // Entry point resolves inside `.text` and starts with the stub prologue.
    let opt = header + 24;
    let entry = rva_to_offset(&pe, u32_at(&pe, opt + 16));
    assert_eq!(&pe[entry..entry + 4], &[0x48, 0x83, 0xEC, 0x28]);
    // Import table still names ExitProcess.
    assert!(
        pe.windows(b"ExitProcess".len())
            .any(|w| w == b"ExitProcess"),
        "ExitProcess import present"
    );
}

#[test]
fn heap_program_emits_rdata_and_bss_sections() {
    // `len("hello")` interns a `.rdata` string constant and uses the `.bss` bump
    // heap, so the image carries all four sections plus the intact import table.
    let pe = pe_for("fn main -> i64\n    len(\"hello\")\n");
    let names: Vec<String> = sections(&pe).into_iter().map(|s| s.0).collect();
    assert_eq!(
        names,
        vec![
            ".text".to_string(),
            ".rdata".to_string(),
            ".idata".to_string(),
            ".bss".to_string(),
        ]
    );

    // The `.rdata` section contains the NUL-terminated literal.
    let rdata = sections(&pe).into_iter().find(|s| s.0 == ".rdata").unwrap();
    let start = rdata.3 as usize;
    let end = start + rdata.4 as usize;
    assert!(
        pe[start..end].windows(6).any(|w| w == b"hello\0"),
        "string constant interned in .rdata"
    );

    // `.bss` reserves address space (>= the 1 MiB heap) but no file bytes.
    let bss = sections(&pe).into_iter().find(|s| s.0 == ".bss").unwrap();
    assert!(bss.2 >= 1024 * 1024, ".bss reserves the heap region");
    assert_eq!(bss.4, 0, ".bss has no raw data on disk");
    assert_eq!(bss.5, 0xC000_0080, ".bss = UNINIT_DATA|READ|WRITE");

    // Import table remains well-formed.
    let (import_rva, _) = data_dir(&pe, 1);
    let desc = rva_to_offset(&pe, import_rva);
    let name = rva_to_offset(&pe, u32_at(&pe, desc + 12));
    assert_eq!(&pe[name..name + 13], b"kernel32.dll\0");
}

#[test]
fn direct_pe_emission_is_deterministic() {
    let source = "fn main -> i64\n    fib(6)\n\n\
                  fn fib n i64 -> i64\n    if n < 2\n        return n\n    return fib(n - 1) + fib(n - 2)\n";
    let first = pe_for(source);
    let second = pe_for(source);
    assert_eq!(first, second, "PE emission is byte-for-byte deterministic");
}

#[test]
fn library_object_has_no_direct_pe() {
    // An export-only program (no `main`) is a C-callable library object; it must
    // not get a direct PE image (there is no entry point to run).
    let tokens = lex("export fn twice x i64 -> i64\n    x * 2\n").expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = lullaby_semantics::validate(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    let module = lower_to_bytecode(&ir);
    let emitted = emit_native_program(&module).expect("library emits");
    assert!(
        emitted.pe_image.is_none(),
        "a library object (no main) has no direct PE"
    );
}
