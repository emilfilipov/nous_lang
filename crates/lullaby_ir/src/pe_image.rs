//! Direct PE32+ executable writer for the **freestanding** native case.
//!
//! For a freestanding program — one that imports only `kernel32!ExitProcess`
//! and no C runtime — Lullaby lays a complete, runnable Windows `.exe` image
//! around the already-generated `.text` machine code itself, skipping the
//! external linker (`rust-lld`) entirely. The external linker is ~93% of the
//! `lullaby native` wall-clock, so writing the PE in-house drops the
//! freestanding edit→exe loop from ~100 ms to ~8 ms.
//!
//! The emitted image is a fixed-base (RELOCS_STRIPPED) PE32+ executable: a DOS
//! header, the PE signature, the COFF file header, a PE32+ optional header with
//! the Import data directory, a section table, and the mapped section bodies.
//! Because the image carries no base-relocation table and is not marked
//! `DYNAMIC_BASE`, the Windows loader maps it at its preferred `ImageBase`, so
//! every intra-image reference (inter-function `call`, RIP-relative data `lea`,
//! and the entry stub's indirect `call [rip+__imp_ExitProcess]`) is resolved to
//! a final displacement here at emit time — no loader fixups beyond the single
//! import.
//!
//! Only the freestanding (kernel32-only) case is in scope; a program that needs
//! the C runtime (`extern fn`, non-freestanding) keeps the existing `rust-lld`
//! path. This writer is a descendant module of `native_object`, so it reuses the
//! private `.text` layout types (`LoweredNativeFunction`, `CodeRelocation`), the
//! `StringPool`, the shared heap/string runtime helper set, and the byte-push
//! helpers via `use super::*`.

use super::*;

/// Preferred load address of the emitted executable. A 64-bit, fixed-base image
/// (matching `link.exe /FIXED`): with no relocation directory the loader maps the
/// image here, so all RVAs resolve to `PE_IMAGE_BASE + rva` deterministically.
const PE_IMAGE_BASE: u64 = 0x1_4000_0000;

/// In-memory section alignment (each section's RVA is a multiple of this).
const SECTION_ALIGNMENT: u32 = 0x1000;

/// On-disk file alignment (each section's raw pointer/size is a multiple of this).
const FILE_ALIGNMENT: u32 = 0x200;

/// Fixed sizes of the PE header block, in bytes.
const DOS_HEADER_SIZE: u32 = 0x40; // 64-byte DOS header; `e_lfanew` at 0x3C = 0x40.
const PE_SIGNATURE_SIZE: u32 = 4; // "PE\0\0"
const COFF_FILE_HEADER_SIZE: u32 = 20;
const OPTIONAL_HEADER_SIZE: u32 = 240; // PE32+ standard(24)+windows(88)+16 dirs(128)
const SECTION_HDR_SIZE: u32 = 40;

/// `.idata` internal layout (a single `kernel32!ExitProcess` import): an import
/// directory table (one descriptor + a null terminator), an import lookup table
/// (ILT), an import address table (IAT), a hint/name entry, and the DLL name.
const IMPORT_DIR_SIZE: u32 = 40; // 2 descriptors * 20 bytes
const ILT_OFF: u32 = IMPORT_DIR_SIZE; // 40
const ILT_SIZE: u32 = 16; // one 8-byte entry + a null terminator
const IAT_OFF: u32 = ILT_OFF + ILT_SIZE; // 56
const IAT_SIZE: u32 = 16; // one 8-byte entry + a null terminator
const HINTNAME_OFF: u32 = IAT_OFF + IAT_SIZE; // 72
/// hint(2) + b"ExitProcess\0"(12) = 14 bytes.
const HINTNAME_SIZE: u32 = 14;
const DLLNAME_OFF: u32 = HINTNAME_OFF + HINTNAME_SIZE; // 86
/// b"kernel32.dll\0" = 13 bytes.
const DLLNAME_SIZE: u32 = 13;
const IDATA_SIZE: u32 = DLLNAME_OFF + DLLNAME_SIZE; // 99

/// A relocation inside the assembled `.text` blob: a 4-byte REL32 field at
/// `offset` referencing `symbol`. Resolved to a final displacement once every
/// section RVA is known.
struct PeTextReloc {
    offset: u32,
    symbol: String,
}

/// Planned placement of one section within the image.
struct PeSectionLayout {
    name: &'static str,
    characteristics: u32,
    rva: u32,
    virtual_size: u32,
    raw_ptr: u32,
    raw_size: u32,
    /// Whether the section has on-disk raw data (`.bss` does not).
    has_raw: bool,
}

fn align_up(value: u32, align: u32) -> u32 {
    value.div_ceil(align) * align
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

/// Write a runnable PE32+ (`x86-64`) executable for a freestanding native
/// program, given its lowered functions and interned string constants. Returns
/// `None` (so the caller falls back to the `rust-lld` path) if any `.text`
/// relocation names a symbol this writer cannot resolve — i.e. anything beyond
/// the compiled functions, the heap/string runtime helpers, the `.rdata` string
/// constants, the three `.bss` heap cells, and the single `ExitProcess` import.
///
/// The caller guarantees the program is freestanding-eligible (a `main` is
/// present and no `extern fn` C import is required); this writer emits the entry
/// stub that calls `main` and forwards its result to `ExitProcess`.
pub(crate) fn write_pe_executable(
    functions: &[LoweredNativeFunction],
    strings: &StringPool,
) -> Option<Vec<u8>> {
    let use_heap = !strings.entries.is_empty() || program_uses_heap_helpers(functions);
    let has_rdata = !strings.entries.is_empty();

    // -- Assemble `.text`: entry stub, functions, then heap/string helpers ----
    let mut text: Vec<u8> = Vec::new();
    let mut relocs: Vec<PeTextReloc> = Vec::new();
    let mut func_offsets: HashMap<String, u32> = HashMap::new();

    // Entry stub `_lullaby_start`: `sub rsp, 40` (align + shadow) ; `call main`
    // (rel32) ; `mov ecx, eax` (exit code = main's result) ; an INDIRECT
    // `call qword ptr [rip + __imp_ExitProcess]` (FF 15) through the IAT slot ;
    // `int3` (unreachable; ExitProcess does not return). The `sub rsp, 40` keeps
    // `rsp` 16-aligned at each `call`, identical to the linked COFF entry stub.
    text.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 40
    text.push(0xE8); // call main (rel32)
    relocs.push(PeTextReloc {
        offset: text.len() as u32,
        symbol: "main".to_string(),
    });
    text.extend_from_slice(&[0, 0, 0, 0]);
    text.extend_from_slice(&[0x89, 0xC1]); // mov ecx, eax
    text.extend_from_slice(&[0xFF, 0x15]); // call qword ptr [rip + disp32]
    relocs.push(PeTextReloc {
        offset: text.len() as u32,
        symbol: EXIT_PROCESS_SYMBOL.to_string(),
    });
    text.extend_from_slice(&[0, 0, 0, 0]);
    text.push(0xCC); // int3

    let append = |text: &mut Vec<u8>,
                  relocs: &mut Vec<PeTextReloc>,
                  func_offsets: &mut HashMap<String, u32>,
                  name: &str,
                  code: &[u8],
                  code_relocs: &[CodeRelocation]| {
        while !text.len().is_multiple_of(16) {
            text.push(0xCC);
        }
        let start = text.len() as u32;
        func_offsets.insert(name.to_string(), start);
        for reloc in code_relocs {
            relocs.push(PeTextReloc {
                offset: start + reloc.offset,
                symbol: reloc.symbol.clone(),
            });
        }
        text.extend_from_slice(code);
    };

    for function in functions {
        append(
            &mut text,
            &mut relocs,
            &mut func_offsets,
            &function.name,
            &function.code,
            &function.relocations,
        );
    }
    if use_heap {
        for helper in heap_runtime_helpers() {
            append(
                &mut text,
                &mut relocs,
                &mut func_offsets,
                &helper.name,
                &helper.code,
                &helper.relocations,
            );
        }
    }

    // -- `.rdata`: NUL-terminated string constants ---------------------------
    let mut rdata: Vec<u8> = Vec::new();
    let mut str_offsets: Vec<u32> = Vec::new();
    if has_rdata {
        for text_value in &strings.entries {
            str_offsets.push(rdata.len() as u32);
            rdata.extend_from_slice(text_value.as_bytes());
            rdata.push(0);
        }
    }

    // -- Plan the section table ----------------------------------------------
    // Order: `.text` (RX), `.rdata` (R, if strings), `.idata` (RW, import table),
    // `.bss` (RW zero-fill, if the heap is used). Characteristics:
    //   .text  = CODE | EXECUTE | READ           (0x60000020)
    //   .rdata = INITIALIZED_DATA | READ         (0x40000040)
    //   .idata = INITIALIZED_DATA | READ | WRITE (0xC0000040) — loader fills IAT
    //   .bss   = UNINITIALIZED_DATA | READ | WRITE (0xC0000080)
    struct SectionPlan {
        name: &'static str,
        characteristics: u32,
        virtual_size: u32,
        has_raw: bool,
    }
    let mut plan: Vec<SectionPlan> = vec![SectionPlan {
        name: ".text",
        characteristics: 0x6000_0020,
        virtual_size: text.len() as u32,
        has_raw: true,
    }];
    if has_rdata {
        plan.push(SectionPlan {
            name: ".rdata",
            characteristics: 0x4000_0040,
            virtual_size: rdata.len() as u32,
            has_raw: true,
        });
    }
    plan.push(SectionPlan {
        name: ".idata",
        characteristics: 0xC000_0040,
        virtual_size: IDATA_SIZE,
        has_raw: true,
    });
    if use_heap {
        plan.push(SectionPlan {
            name: ".bss",
            characteristics: 0xC000_0080,
            virtual_size: 16 + HEAP_REGION_SIZE,
            has_raw: false,
        });
    }

    let num_sections = plan.len() as u32;
    let headers_raw = DOS_HEADER_SIZE
        + PE_SIGNATURE_SIZE
        + COFF_FILE_HEADER_SIZE
        + OPTIONAL_HEADER_SIZE
        + SECTION_HDR_SIZE * num_sections;
    let size_of_headers = align_up(headers_raw, FILE_ALIGNMENT);

    // Assign each section its RVA (mapped, `SECTION_ALIGNMENT`) and file pointer
    // (on-disk, `FILE_ALIGNMENT`). `.bss` consumes address space but no file
    // bytes. The final RVA cursor is `SizeOfImage`.
    let mut layout: Vec<PeSectionLayout> = Vec::new();
    let mut rva = align_up(size_of_headers, SECTION_ALIGNMENT);
    let mut raw = size_of_headers;
    for section in &plan {
        let raw_size = if section.has_raw {
            align_up(section.virtual_size, FILE_ALIGNMENT)
        } else {
            0
        };
        layout.push(PeSectionLayout {
            name: section.name,
            characteristics: section.characteristics,
            rva,
            virtual_size: section.virtual_size,
            raw_ptr: if section.has_raw { raw } else { 0 },
            raw_size,
            has_raw: section.has_raw,
        });
        rva = align_up(rva + section.virtual_size, SECTION_ALIGNMENT);
        if section.has_raw {
            raw += raw_size;
        }
    }
    let size_of_image = rva;

    let find_rva = |name: &str| layout.iter().find(|s| s.name == name).map(|s| s.rva);
    let text_rva = find_rva(".text").expect(".text is always present");
    let rdata_rva = find_rva(".rdata");
    let idata_rva = find_rva(".idata").expect(".idata is always present");
    let bss_rva = find_rva(".bss");

    // Import table RVAs (all within `.idata`). The IAT slot is what the entry
    // stub's `call [rip+...]` references; the loader overwrites it with the
    // resolved `ExitProcess` address at load time.
    let ilt_rva = idata_rva + ILT_OFF;
    let iat_slot_rva = idata_rva + IAT_OFF;
    let hintname_rva = idata_rva + HINTNAME_OFF;
    let dllname_rva = idata_rva + DLLNAME_OFF;

    // -- Resolve every `.text` relocation to a final displacement ------------
    for reloc in &relocs {
        let target_rva = if reloc.symbol == EXIT_PROCESS_SYMBOL {
            iat_slot_rva
        } else if let Some(offset) = func_offsets.get(&reloc.symbol) {
            text_rva + offset
        } else if let Some(index) = reloc
            .symbol
            .strip_prefix("__str")
            .and_then(|n| n.parse::<usize>().ok())
        {
            rdata_rva? + str_offsets[index]
        } else if reloc.symbol == HEAP_NEXT_SYMBOL {
            bss_rva?
        } else if reloc.symbol == HEAP_FREE_HEAD_SYMBOL {
            bss_rva? + 8
        } else if reloc.symbol == HEAP_BASE_SYMBOL {
            bss_rva? + 16
        } else {
            // Unresolvable symbol (e.g. an unexpected external): refuse to emit a
            // direct PE and let the caller fall back to the linker path.
            return None;
        };
        let field_rva = text_rva + reloc.offset;
        let displacement = i64::from(target_rva) - (i64::from(field_rva) + 4);
        let displacement = i32::try_from(displacement).ok()?;
        let field = reloc.offset as usize;
        text[field..field + 4].copy_from_slice(&displacement.to_le_bytes());
    }

    // -- Build `.idata` bytes (now that its RVA is known) --------------------
    let mut idata: Vec<u8> = Vec::new();
    // Import Directory Table: one descriptor for kernel32.dll + a null terminator.
    push_u32(&mut idata, ilt_rva); // OriginalFirstThunk (ILT)
    push_u32(&mut idata, 0); // TimeDateStamp
    push_u32(&mut idata, 0); // ForwarderChain
    push_u32(&mut idata, dllname_rva); // Name (DLL name string)
    push_u32(&mut idata, iat_slot_rva); // FirstThunk (IAT)
    for _ in 0..5 {
        push_u32(&mut idata, 0); // null terminator descriptor
    }
    // Import Lookup Table: one by-name entry (RVA of the hint/name), then null.
    push_u64(&mut idata, u64::from(hintname_rva));
    push_u64(&mut idata, 0);
    // Import Address Table: same shape; the loader overwrites the first slot.
    push_u64(&mut idata, u64::from(hintname_rva));
    push_u64(&mut idata, 0);
    // Hint/Name entry: hint (0) + NUL-terminated function name.
    push_u16(&mut idata, 0);
    idata.extend_from_slice(b"ExitProcess\0");
    // DLL name.
    idata.extend_from_slice(b"kernel32.dll\0");
    debug_assert_eq!(idata.len() as u32, IDATA_SIZE);

    // -- Emit the image ------------------------------------------------------
    let mut bytes: Vec<u8> = Vec::new();

    // DOS header: `MZ`, zeros, and `e_lfanew` (offset of the PE header) at 0x3C.
    bytes.extend_from_slice(b"MZ");
    bytes.resize(0x3C, 0);
    push_u32(&mut bytes, DOS_HEADER_SIZE); // e_lfanew = 0x40
    debug_assert_eq!(bytes.len() as u32, DOS_HEADER_SIZE);

    // PE signature + COFF file header.
    bytes.extend_from_slice(b"PE\0\0");
    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, num_sections as u16);
    push_u32(&mut bytes, 0); // TimeDateStamp (0 for determinism)
    push_u32(&mut bytes, 0); // PointerToSymbolTable
    push_u32(&mut bytes, 0); // NumberOfSymbols
    push_u16(&mut bytes, OPTIONAL_HEADER_SIZE as u16);
    // Characteristics: EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE | RELOCS_STRIPPED.
    // RELOCS_STRIPPED (with no `.reloc` and no DYNAMIC_BASE) forces the loader to
    // map at `ImageBase`, so the emit-time displacements stay valid.
    push_u16(&mut bytes, 0x0002 | 0x0020 | 0x0001);

    // Optional header (PE32+). Standard fields.
    let size_of_code = layout[0].raw_size;
    let size_of_initialized: u32 = layout
        .iter()
        .filter(|s| s.name != ".text" && s.has_raw)
        .map(|s| s.raw_size)
        .sum();
    let size_of_uninitialized: u32 = layout
        .iter()
        .filter(|s| !s.has_raw)
        .map(|s| align_up(s.virtual_size, FILE_ALIGNMENT))
        .sum();
    push_u16(&mut bytes, 0x020B); // Magic: PE32+
    bytes.push(14); // MajorLinkerVersion
    bytes.push(0); // MinorLinkerVersion
    push_u32(&mut bytes, size_of_code);
    push_u32(&mut bytes, size_of_initialized);
    push_u32(&mut bytes, size_of_uninitialized);
    push_u32(&mut bytes, text_rva); // AddressOfEntryPoint (entry stub at .text+0)
    push_u32(&mut bytes, text_rva); // BaseOfCode
    // Windows-specific fields.
    push_u64(&mut bytes, PE_IMAGE_BASE);
    push_u32(&mut bytes, SECTION_ALIGNMENT);
    push_u32(&mut bytes, FILE_ALIGNMENT);
    push_u16(&mut bytes, 6); // MajorOperatingSystemVersion
    push_u16(&mut bytes, 0); // MinorOperatingSystemVersion
    push_u16(&mut bytes, 0); // MajorImageVersion
    push_u16(&mut bytes, 0); // MinorImageVersion
    push_u16(&mut bytes, 6); // MajorSubsystemVersion (>= 6 for a modern loader)
    push_u16(&mut bytes, 0); // MinorSubsystemVersion
    push_u32(&mut bytes, 0); // Win32VersionValue
    push_u32(&mut bytes, size_of_image);
    push_u32(&mut bytes, size_of_headers);
    push_u32(&mut bytes, 0); // CheckSum (0; not required for an EXE)
    push_u16(&mut bytes, 3); // Subsystem: IMAGE_SUBSYSTEM_WINDOWS_CUI
    push_u16(&mut bytes, 0); // DllCharacteristics (no ASLR: fixed-base image)
    push_u64(&mut bytes, 0x0010_0000); // SizeOfStackReserve
    push_u64(&mut bytes, 0x0000_1000); // SizeOfStackCommit
    push_u64(&mut bytes, 0x0010_0000); // SizeOfHeapReserve
    push_u64(&mut bytes, 0x0000_1000); // SizeOfHeapCommit
    push_u32(&mut bytes, 0); // LoaderFlags
    push_u32(&mut bytes, 16); // NumberOfRvaAndSizes
    // Data directories (16). Only Import (1) and IAT (12) are non-zero.
    for index in 0..16u32 {
        match index {
            1 => {
                push_u32(&mut bytes, idata_rva); // Import Directory Table RVA
                push_u32(&mut bytes, IMPORT_DIR_SIZE);
            }
            12 => {
                push_u32(&mut bytes, iat_slot_rva); // IAT RVA
                push_u32(&mut bytes, IAT_SIZE);
            }
            _ => {
                push_u32(&mut bytes, 0);
                push_u32(&mut bytes, 0);
            }
        }
    }

    // Section table.
    for section in &layout {
        push_fixed_name(&mut bytes, section.name, 8);
        push_u32(&mut bytes, section.virtual_size);
        push_u32(&mut bytes, section.rva);
        push_u32(&mut bytes, section.raw_size);
        push_u32(&mut bytes, section.raw_ptr);
        push_u32(&mut bytes, 0); // PointerToRelocations
        push_u32(&mut bytes, 0); // PointerToLinenumbers
        push_u16(&mut bytes, 0); // NumberOfRelocations
        push_u16(&mut bytes, 0); // NumberOfLinenumbers
        push_u32(&mut bytes, section.characteristics);
    }

    // Pad the header block to `SizeOfHeaders`.
    bytes.resize(size_of_headers as usize, 0);

    // Section raw data, in file order, each padded to its aligned raw size.
    for section in &layout {
        if !section.has_raw {
            continue;
        }
        debug_assert_eq!(bytes.len() as u32, section.raw_ptr);
        match section.name {
            ".text" => bytes.extend_from_slice(&text),
            ".rdata" => bytes.extend_from_slice(&rdata),
            ".idata" => bytes.extend_from_slice(&idata),
            _ => {}
        }
        bytes.resize((section.raw_ptr + section.raw_size) as usize, 0);
    }

    Some(bytes)
}

#[cfg(test)]
#[path = "pe_image_tests.rs"]
mod tests;
