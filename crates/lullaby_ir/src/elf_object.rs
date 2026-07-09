//! ELF64 (System V AMD64) relocatable-object writer.
//!
//! Serializes an [`ObjectModel`](crate::object_model::ObjectModel) into a
//! `ET_REL` ELF64 object: an `Elf64_Ehdr`, a section header table
//! (`.text`/`.rodata`/`.bss`/`.rela.text`/`.symtab`/`.strtab`/`.shstrtab`), a
//! global symbol table, and `Elf64_Rela` relocations that use
//! `R_X86_64_PLT32` for `call` sites and `R_X86_64_PC32` for RIP-relative data
//! references. The shared machine code is emitted by the native backend; this
//! module only builds the ELF container plus the Linux freestanding entry stub's
//! symbol (`_start`).
//!
//! # Verification honesty
//!
//! This is a Windows host: the emitted object is verified *structurally* (magic,
//! class/endianness, header fields, section and symbol tables, relocation
//! records — see the unit tests) but is **not** linked or executed here.
//! Link-and-run verification is deferred to the cross-platform CI of the Phase 9
//! roadmap. x86-64 only; ARM64 (`aarch64`) ELF is a separate future effort.

use crate::object_model::{ObjectModel, ObjectRelocationKind, ObjectSectionKind, ObjectSymbolKind};

// -- ELF64 constants ---------------------------------------------------------

/// Size in bytes of an `Elf64_Ehdr`.
const EHDR_SIZE: u64 = 64;
/// Size in bytes of an `Elf64_Shdr`.
const SHDR_SIZE: u64 = 64;
/// Size in bytes of an `Elf64_Sym`.
const SYM_SIZE: u64 = 24;
/// Size in bytes of an `Elf64_Rela`.
const RELA_SIZE: u64 = 24;

/// `ET_REL` — a relocatable object.
const ET_REL: u16 = 1;
/// `EM_X86_64`.
const EM_X86_64: u16 = 62;

/// `SHT_PROGBITS` — section holds program-defined bytes.
const SHT_PROGBITS: u32 = 1;
/// `SHT_SYMTAB` — a symbol table.
const SHT_SYMTAB: u32 = 2;
/// `SHT_STRTAB` — a string table.
const SHT_STRTAB: u32 = 3;
/// `SHT_RELA` — relocations with explicit addends.
const SHT_RELA: u32 = 4;
/// `SHT_NOBITS` — occupies no file space (`.bss`).
const SHT_NOBITS: u32 = 8;

/// `SHF_WRITE`.
const SHF_WRITE: u64 = 0x1;
/// `SHF_ALLOC`.
const SHF_ALLOC: u64 = 0x2;
/// `SHF_EXECINSTR`.
const SHF_EXECINSTR: u64 = 0x4;

/// `STB_GLOBAL` binding (shifted into the high nibble of `st_info`).
const STB_GLOBAL: u8 = 1;
/// `STT_OBJECT` symbol type.
const STT_OBJECT: u8 = 1;
/// `STT_FUNC` symbol type.
const STT_FUNC: u8 = 2;

/// `R_X86_64_PC32` — 32-bit PC-relative reference (`S + A - P`).
const R_X86_64_PC32: u32 = 2;
/// `R_X86_64_PLT32` — 32-bit PLT-relative reference; resolves like `PC32` for a
/// defined local target.
const R_X86_64_PLT32: u32 = 4;

/// The REL32 addend: the relocated field sits at `P` and its 4 bytes end at
/// `P + 4`, so a displacement to `S` is `S - (P + 4)` = `S + (-4) - P`.
const REL32_ADDEND: i64 = -4;

/// One planned ELF section: its `.shstrtab` name, header metadata, and the file
/// range its contents occupy (empty for `.bss`).
struct PlannedSection {
    name: &'static str,
    sh_type: u32,
    flags: u64,
    addralign: u64,
    entsize: u64,
    /// File offset of the section's bytes (0 for the null section).
    offset: u64,
    /// In-memory size (bytes for PROGBITS; zero-fill size for NOBITS).
    size: u64,
    /// `sh_link` (symtab→strtab index; rela→symtab index; else 0).
    link: u32,
    /// `sh_info` (rela→target section index; symtab→first global index; else 0).
    info: u32,
    /// The raw bytes to write at `offset` (empty for null/NOBITS sections).
    bytes: Vec<u8>,
}

/// Serialize `model` into a relocatable ELF64 object.
pub fn write_elf64(model: &ObjectModel) -> Vec<u8> {
    // Map a model section index to its ELF section-header index. The ELF table
    // always leads with the null section (index 0), so model section `i` (the
    // content sections start at `.text` = model 0) is ELF index `i + 1`.
    let elf_index_of_model = |model_section: usize| -> u32 { model_section as u32 + 1 };
    // The symbol table sits after every content section (and the `.rela.text`),
    // so its ELF index depends on how many content sections exist. Content
    // sections: one per model section. Then `.rela.text` (if `.text` has any
    // relocations), then `.symtab`, `.strtab`, `.shstrtab`.
    let text_index = model.text_section_index();
    let has_text_relocs = !model.sections[text_index].relocations.is_empty();

    // Section-header indices are assigned in table order:
    //   0 null, 1.. content sections, then rela.text?, symtab, strtab, shstrtab.
    let content_count = model.sections.len() as u32;
    let symtab_shidx = 1 + content_count + u32::from(has_text_relocs);
    let strtab_shidx = symtab_shidx + 1;

    // -- Build the symbol table + its string table --------------------------
    // Symbol 0 is the reserved null symbol (all zero, STB_LOCAL). Every model
    // symbol follows as STB_GLOBAL, so the first global is index 1.
    let mut strtab: Vec<u8> = vec![0]; // leading NUL
    let mut symtab: Vec<u8> = Vec::new();
    // Null symbol.
    symtab.extend_from_slice(&[0u8; SYM_SIZE as usize]);
    for symbol in &model.symbols {
        let name_off = strtab.len() as u32;
        strtab.extend_from_slice(symbol.name.as_bytes());
        strtab.push(0);
        let (st_type, st_shndx) = match symbol.section {
            Some(model_section) => {
                let ty = match symbol.kind {
                    ObjectSymbolKind::Function => STT_FUNC,
                    ObjectSymbolKind::Data => STT_OBJECT,
                };
                (ty, elf_index_of_model(model_section) as u16)
            }
            // Undefined external: no defining section (SHN_UNDEF), no type.
            None => (0u8, 0u16),
        };
        let st_info = (STB_GLOBAL << 4) | st_type;
        push_u32(&mut symtab, name_off);
        symtab.push(st_info);
        symtab.push(0); // st_other
        push_u16(&mut symtab, st_shndx);
        push_u64(&mut symtab, symbol.value);
        push_u64(&mut symtab, 0); // st_size
    }
    // The `.symtab` index into a model symbol is `1 + model_index` (null shift).
    let symtab_index_of = |model_index: usize| -> u64 { 1 + model_index as u64 };

    // -- Build `.rela.text` --------------------------------------------------
    let mut rela: Vec<u8> = Vec::new();
    if has_text_relocs {
        for reloc in &model.sections[text_index].relocations {
            let r_type = match reloc.kind {
                ObjectRelocationKind::Branch => R_X86_64_PLT32,
                ObjectRelocationKind::PcRel32 => R_X86_64_PC32,
            };
            let r_info = (symtab_index_of(reloc.symbol) << 32) | u64::from(r_type);
            push_u64(&mut rela, reloc.offset);
            push_u64(&mut rela, r_info);
            push_i64(&mut rela, REL32_ADDEND);
        }
    }

    // -- Build `.shstrtab` (section-name string table) ----------------------
    // Names are interned as they are appended; the null section uses offset 0.
    let mut shstrtab: Vec<u8> = vec![0];
    let mut intern_shstr = |name: &str| -> u32 {
        if name.is_empty() {
            return 0;
        }
        let off = shstrtab.len() as u32;
        shstrtab.extend_from_slice(name.as_bytes());
        shstrtab.push(0);
        off
    };

    // -- Plan every section, laying out file offsets in order ---------------
    let mut planned: Vec<PlannedSection> = Vec::new();
    // Null section.
    planned.push(PlannedSection {
        name: "",
        sh_type: 0,
        flags: 0,
        addralign: 0,
        entsize: 0,
        offset: 0,
        size: 0,
        link: 0,
        info: 0,
        bytes: Vec::new(),
    });

    // Content sections (from the model).
    for section in &model.sections {
        let (name, sh_type, flags, addralign): (&'static str, u32, u64, u64) = match section.kind {
            ObjectSectionKind::Text => (".text", SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, 16),
            ObjectSectionKind::ReadOnlyData => (".rodata", SHT_PROGBITS, SHF_ALLOC, 1),
            ObjectSectionKind::Bss => (".bss", SHT_NOBITS, SHF_ALLOC | SHF_WRITE, 16),
        };
        let bytes = if section.kind == ObjectSectionKind::Bss {
            Vec::new()
        } else {
            section.data.clone()
        };
        planned.push(PlannedSection {
            name,
            sh_type,
            flags,
            addralign,
            entsize: 0,
            offset: 0, // filled during layout
            size: section.size,
            link: 0,
            info: 0,
            bytes,
        });
    }

    if has_text_relocs {
        planned.push(PlannedSection {
            name: ".rela.text",
            sh_type: SHT_RELA,
            flags: 0,
            addralign: 8,
            entsize: RELA_SIZE,
            offset: 0,
            size: rela.len() as u64,
            link: symtab_shidx,
            info: elf_index_of_model(text_index),
            bytes: rela,
        });
    }

    // `.symtab`: sh_link = strtab index, sh_info = index of first global (1).
    planned.push(PlannedSection {
        name: ".symtab",
        sh_type: SHT_SYMTAB,
        flags: 0,
        addralign: 8,
        entsize: SYM_SIZE,
        offset: 0,
        size: symtab.len() as u64,
        link: strtab_shidx,
        info: 1,
        bytes: symtab,
    });
    planned.push(PlannedSection {
        name: ".strtab",
        sh_type: SHT_STRTAB,
        flags: 0,
        addralign: 1,
        entsize: 0,
        offset: 0,
        size: strtab.len() as u64,
        link: 0,
        info: 0,
        bytes: strtab,
    });

    // `.shstrtab` is the last section; intern all names (including its own)
    // first so its size is known before layout.
    let name_offsets: Vec<u32> = planned.iter().map(|s| intern_shstr(s.name)).collect();
    let shstrtab_name_off = intern_shstr(".shstrtab");
    let shstrtab_shidx = planned.len() as u32;
    planned.push(PlannedSection {
        name: ".shstrtab",
        sh_type: SHT_STRTAB,
        flags: 0,
        addralign: 1,
        entsize: 0,
        offset: 0,
        size: shstrtab.len() as u64,
        link: 0,
        info: 0,
        bytes: shstrtab.clone(),
    });

    // -- Lay out file offsets ------------------------------------------------
    // The ELF header sits first; section contents follow (NOBITS occupy no file
    // space); then the section header table (8-aligned).
    let mut cursor = EHDR_SIZE;
    for section in planned.iter_mut() {
        if section.sh_type == 0 {
            continue; // null section: no content
        }
        if section.sh_type == SHT_NOBITS {
            // NOBITS has a conceptual file offset but no bytes.
            cursor = align_up(cursor, section.addralign.max(1));
            section.offset = cursor;
            continue;
        }
        cursor = align_up(cursor, section.addralign.max(1));
        section.offset = cursor;
        cursor += section.bytes.len() as u64;
    }
    let shoff = align_up(cursor, 8);

    // -- Emit ----------------------------------------------------------------
    let shnum = planned.len() as u16;
    let mut out: Vec<u8> = Vec::new();

    // Elf64_Ehdr.
    out.extend_from_slice(&[0x7f, b'E', b'L', b'F']);
    out.push(2); // EI_CLASS = ELFCLASS64
    out.push(1); // EI_DATA = ELFDATA2LSB
    out.push(1); // EI_VERSION = EV_CURRENT
    out.push(0); // EI_OSABI = ELFOSABI_SYSV
    out.push(0); // EI_ABIVERSION
    out.extend_from_slice(&[0u8; 7]); // EI_PAD
    push_u16(&mut out, ET_REL);
    push_u16(&mut out, EM_X86_64);
    push_u32(&mut out, 1); // e_version
    push_u64(&mut out, 0); // e_entry (relocatable: none)
    push_u64(&mut out, 0); // e_phoff
    push_u64(&mut out, shoff); // e_shoff
    push_u32(&mut out, 0); // e_flags
    push_u16(&mut out, EHDR_SIZE as u16); // e_ehsize
    push_u16(&mut out, 0); // e_phentsize
    push_u16(&mut out, 0); // e_phnum
    push_u16(&mut out, SHDR_SIZE as u16); // e_shentsize
    push_u16(&mut out, shnum); // e_shnum
    push_u16(&mut out, shstrtab_shidx as u16); // e_shstrndx

    // Section contents, in the same order the layout walked.
    for section in &planned {
        if section.sh_type == 0 || section.sh_type == SHT_NOBITS {
            continue;
        }
        pad_to(&mut out, section.offset);
        out.extend_from_slice(&section.bytes);
    }

    // Section header table.
    pad_to(&mut out, shoff);
    for (index, section) in planned.iter().enumerate() {
        push_u32(
            &mut out,
            name_offsets_for(index, &name_offsets, shstrtab_name_off, shnum),
        );
        push_u32(&mut out, section.sh_type);
        push_u64(&mut out, section.flags);
        push_u64(&mut out, 0); // sh_addr
        push_u64(&mut out, section.offset);
        push_u64(&mut out, section.size);
        push_u32(&mut out, section.link);
        push_u32(&mut out, section.info);
        push_u64(&mut out, section.addralign);
        push_u64(&mut out, section.entsize);
    }

    out
}

/// The `.shstrtab` name offset for planned section `index`. The precomputed
/// `name_offsets` covers every section except the trailing `.shstrtab` header,
/// whose own name was interned separately as `shstrtab_name_off`.
fn name_offsets_for(index: usize, name_offsets: &[u32], shstrtab_name_off: u32, shnum: u16) -> u32 {
    if index + 1 == shnum as usize {
        shstrtab_name_off
    } else {
        name_offsets[index]
    }
}

/// Round `value` up to the next multiple of `align` (`align` >= 1).
fn align_up(value: u64, align: u64) -> u64 {
    if align <= 1 {
        return value;
    }
    value.div_ceil(align) * align
}

/// Zero-pad `out` until its length reaches `offset`.
fn pad_to(out: &mut Vec<u8>, offset: u64) {
    while (out.len() as u64) < offset {
        out.push(0);
    }
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_model::{
        ObjectModel, ObjectRelocation, ObjectRelocationKind, ObjectSection, ObjectSectionKind,
        ObjectSymbol, ObjectSymbolKind,
    };

    fn rd_u16(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
    }
    fn rd_u32(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }
    fn rd_u64(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }
    fn rd_i64(b: &[u8], off: usize) -> i64 {
        i64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    /// Read a NUL-terminated string at `off` in a string-table slice.
    fn cstr(strtab: &[u8], off: usize) -> String {
        let end = strtab[off..].iter().position(|&c| c == 0).unwrap() + off;
        String::from_utf8(strtab[off..end].to_vec()).unwrap()
    }

    /// A representative model: a `.text` (entry stub + one function with a branch
    /// relocation to `main` and a data relocation to `__str0`), a `.rodata`
    /// constant, and a `.bss` cell — exercising every section and relocation kind.
    fn sample_model() -> ObjectModel {
        // 20 bytes of placeholder code: two 4-byte reloc fields at offsets 4 and 12.
        let text = vec![0u8; 20];
        ObjectModel {
            sections: vec![
                ObjectSection {
                    kind: ObjectSectionKind::Text,
                    data: text,
                    size: 20,
                    relocations: vec![
                        ObjectRelocation {
                            offset: 4,
                            symbol: 1, // main
                            kind: ObjectRelocationKind::Branch,
                        },
                        ObjectRelocation {
                            offset: 12,
                            symbol: 2, // __str0
                            kind: ObjectRelocationKind::PcRel32,
                        },
                    ],
                },
                ObjectSection {
                    kind: ObjectSectionKind::ReadOnlyData,
                    data: b"hi\0".to_vec(),
                    size: 3,
                    relocations: Vec::new(),
                },
                ObjectSection {
                    kind: ObjectSectionKind::Bss,
                    data: Vec::new(),
                    size: 4096,
                    relocations: Vec::new(),
                },
            ],
            symbols: vec![
                ObjectSymbol {
                    name: "_start".to_string(),
                    section: Some(0),
                    value: 0,
                    kind: ObjectSymbolKind::Function,
                },
                ObjectSymbol {
                    name: "main".to_string(),
                    section: Some(0),
                    value: 16,
                    kind: ObjectSymbolKind::Function,
                },
                ObjectSymbol {
                    name: "__str0".to_string(),
                    section: Some(1),
                    value: 0,
                    kind: ObjectSymbolKind::Data,
                },
            ],
            entry_symbol: Some("_start".to_string()),
        }
    }

    /// Locate a section header by its `.shstrtab` name; return `(shdr_offset,
    /// sh_type, sh_flags, sh_offset, sh_size, sh_link, sh_info, sh_entsize)`.
    #[allow(clippy::type_complexity)]
    fn find_section(
        bytes: &[u8],
        name: &str,
    ) -> Option<(usize, u32, u64, u64, u64, u32, u32, u64)> {
        let shoff = rd_u64(bytes, 40) as usize;
        let shnum = rd_u16(bytes, 60) as usize;
        let shstrndx = rd_u16(bytes, 62) as usize;
        let shstr_hdr = shoff + shstrndx * 64;
        let shstr_off = rd_u64(bytes, shstr_hdr + 24) as usize;
        for i in 0..shnum {
            let hdr = shoff + i * 64;
            let name_off = rd_u32(bytes, hdr) as usize;
            if cstr(&bytes[shstr_off..], name_off) == name {
                return Some((
                    hdr,
                    rd_u32(bytes, hdr + 4),
                    rd_u64(bytes, hdr + 8),
                    rd_u64(bytes, hdr + 24),
                    rd_u64(bytes, hdr + 32),
                    rd_u32(bytes, hdr + 40),
                    rd_u32(bytes, hdr + 44),
                    rd_u64(bytes, hdr + 56),
                ));
            }
        }
        None
    }

    #[test]
    fn header_identifies_a_relocatable_x86_64_elf64() {
        let bytes = write_elf64(&sample_model());
        assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
        assert_eq!(bytes[4], 2, "ELFCLASS64");
        assert_eq!(bytes[5], 1, "ELFDATA2LSB");
        assert_eq!(bytes[6], 1, "EI_VERSION");
        assert_eq!(rd_u16(&bytes, 16), ET_REL, "e_type = ET_REL");
        assert_eq!(rd_u16(&bytes, 18), EM_X86_64, "e_machine = EM_X86_64");
        assert_eq!(rd_u16(&bytes, 52), EHDR_SIZE as u16, "e_ehsize");
        assert_eq!(rd_u16(&bytes, 58), SHDR_SIZE as u16, "e_shentsize");
        // Sections: null, .text, .rodata, .bss, .rela.text, .symtab, .strtab, .shstrtab.
        assert_eq!(rd_u16(&bytes, 60), 8, "e_shnum");
    }

    #[test]
    fn text_section_is_executable_progbits() {
        let bytes = write_elf64(&sample_model());
        let (_, sh_type, flags, offset, size, _, _, _) =
            find_section(&bytes, ".text").expect(".text present");
        assert_eq!(sh_type, SHT_PROGBITS);
        assert_eq!(flags & SHF_EXECINSTR, SHF_EXECINSTR, "executable");
        assert_eq!(flags & SHF_ALLOC, SHF_ALLOC, "allocated");
        assert_eq!(size, 20);
        assert_eq!(&bytes[offset as usize..offset as usize + 20], &[0u8; 20]);
    }

    #[test]
    fn bss_is_nobits_with_the_heap_size() {
        let bytes = write_elf64(&sample_model());
        let (_, sh_type, flags, _, size, _, _, _) =
            find_section(&bytes, ".bss").expect(".bss present");
        assert_eq!(sh_type, SHT_NOBITS);
        assert_eq!(flags & SHF_WRITE, SHF_WRITE);
        assert_eq!(size, 4096, "zero-fill virtual size");
    }

    #[test]
    fn symtab_carries_named_global_symbols() {
        let bytes = write_elf64(&sample_model());
        let (_, sh_type, _, sym_off, sym_size, sh_link, sh_info, entsize) =
            find_section(&bytes, ".symtab").expect(".symtab present");
        assert_eq!(sh_type, SHT_SYMTAB);
        assert_eq!(entsize, SYM_SIZE);
        assert_eq!(
            sh_info, 1,
            "first global symbol index (only the null is local)"
        );
        // sh_link points at the string table.
        let (str_hdr, ..) = find_section(&bytes, ".strtab").expect(".strtab present");
        let strtab_index = (str_hdr - rd_u64(&bytes, 40) as usize) / 64;
        assert_eq!(sh_link as usize, strtab_index, ".symtab links .strtab");

        let str_off = rd_u64(&bytes, str_hdr + 24) as usize;
        let count = (sym_size / SYM_SIZE) as usize;
        assert_eq!(count, 4, "null + 3 model symbols");
        let mut names = Vec::new();
        let mut main_is_func_in_text = false;
        let text_index = {
            let (h, ..) = find_section(&bytes, ".text").unwrap();
            ((h - rd_u64(&bytes, 40) as usize) / 64) as u16
        };
        for i in 0..count {
            let rec = sym_off as usize + i * SYM_SIZE as usize;
            let name = cstr(&bytes[str_off..], rd_u32(&bytes, rec) as usize);
            let info = bytes[rec + 4];
            let shndx = rd_u16(&bytes, rec + 6);
            if name == "main" {
                main_is_func_in_text =
                    (info & 0xf) == STT_FUNC && (info >> 4) == STB_GLOBAL && shndx == text_index;
            }
            if !name.is_empty() {
                names.push(name);
            }
        }
        assert!(names.contains(&"_start".to_string()));
        assert!(names.contains(&"main".to_string()));
        assert!(names.contains(&"__str0".to_string()));
        assert!(main_is_func_in_text, "main is a STT_FUNC defined in .text");
    }

    #[test]
    fn rela_text_uses_plt32_for_calls_and_pc32_for_data() {
        let bytes = write_elf64(&sample_model());
        let (_, sh_type, _, rela_off, rela_size, sh_link, sh_info, entsize) =
            find_section(&bytes, ".rela.text").expect(".rela.text present");
        assert_eq!(sh_type, SHT_RELA);
        assert_eq!(entsize, RELA_SIZE);
        // sh_info references the .text section index; sh_link the .symtab index.
        let text_index = {
            let (h, ..) = find_section(&bytes, ".text").unwrap();
            ((h - rd_u64(&bytes, 40) as usize) / 64) as u32
        };
        let symtab_index = {
            let (h, ..) = find_section(&bytes, ".symtab").unwrap();
            ((h - rd_u64(&bytes, 40) as usize) / 64) as u32
        };
        assert_eq!(sh_info, text_index, ".rela.text targets .text");
        assert_eq!(sh_link, symtab_index, ".rela.text links .symtab");

        let count = (rela_size / RELA_SIZE) as usize;
        assert_eq!(count, 2);
        // Reloc 0: branch to `main` (symtab index 2 = 1 null + model index 1).
        let r0 = rela_off as usize;
        assert_eq!(rd_u64(&bytes, r0), 4, "r_offset");
        assert_eq!(rd_u32(&bytes, r0 + 8), R_X86_64_PLT32, "type = PLT32");
        assert_eq!(rd_u32(&bytes, r0 + 12), 2, "symbol index");
        assert_eq!(rd_i64(&bytes, r0 + 16), -4, "addend");
        // Reloc 1: data reference to `__str0` (symtab index 3).
        let r1 = r0 + RELA_SIZE as usize;
        assert_eq!(rd_u64(&bytes, r1), 12, "r_offset");
        assert_eq!(rd_u32(&bytes, r1 + 8), R_X86_64_PC32, "type = PC32");
        assert_eq!(rd_u32(&bytes, r1 + 12), 3, "symbol index");
        assert_eq!(rd_i64(&bytes, r1 + 16), -4, "addend");
    }

    #[test]
    fn text_only_model_omits_data_sections() {
        // A program with no strings/heap: just `.text`, no `.rodata`/`.bss`.
        let model = ObjectModel {
            sections: vec![ObjectSection {
                kind: ObjectSectionKind::Text,
                data: vec![0xc3], // ret
                size: 1,
                relocations: Vec::new(),
            }],
            symbols: vec![ObjectSymbol {
                name: "main".to_string(),
                section: Some(0),
                value: 0,
                kind: ObjectSymbolKind::Function,
            }],
            entry_symbol: None,
        };
        let bytes = write_elf64(&model);
        assert!(find_section(&bytes, ".text").is_some());
        assert!(find_section(&bytes, ".rodata").is_none());
        assert!(find_section(&bytes, ".bss").is_none());
        // No relocations → no `.rela.text` section.
        assert!(find_section(&bytes, ".rela.text").is_none());
        // Sections: null, .text, .symtab, .strtab, .shstrtab.
        assert_eq!(rd_u16(&bytes, 60), 5, "e_shnum");
    }
}
