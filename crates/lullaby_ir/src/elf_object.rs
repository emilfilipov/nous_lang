//! ELF64 (System V AMD64) relocatable-object writer.
//!
//! Serializes an [`ObjectModel`](crate::object_model::ObjectModel) into a
//! `ET_REL` ELF64 object: an `Elf64_Ehdr`, a section header table
//! (`.text`/`.rodata`/`.bss`, the DWARF `.debug_*` sections when `--debug` was
//! requested, one `.rela.<section>` per relocated section, then
//! `.symtab`/`.strtab`/`.shstrtab`), a symbol table, and `Elf64_Rela`
//! relocations. The `e_machine` field and
//! the relocation types are selected from the model's
//! [`ObjectMachine`](crate::object_model::ObjectMachine): x86-64 objects use
//! `R_X86_64_PLT32` for `call` sites and `R_X86_64_PC32` for RIP-relative data
//! references; AArch64 objects use `R_AARCH64_CALL26` for `bl` call sites. The
//! shared machine code is emitted by the native backend; this module only builds
//! the ELF container plus the Linux freestanding entry stub's symbol (`_start`).
//!
//! # `--debug` (DWARF)
//!
//! With `--debug`, `native_object_dwarf.rs` attaches the DWARF sections,
//! `STT_SECTION` symbols, and their relocations to the model *before* it reaches
//! this writer, so nothing here is DWARF-aware beyond three mechanical
//! consequences: relocations are emitted **per section** rather than only for
//! `.text`; the two DWARF relocation kinds map to `R_X86_64_64` (an absolute
//! address) and `R_X86_64_32` (a section-relative offset); and, since those
//! section symbols are the first `STB_LOCAL` symbols this writer has ever
//! emitted, the symbol table is now ordered locals-before-globals with
//! `.symtab`'s `sh_info` pointing at the first non-local symbol, as ELF
//! requires. Without `--debug` the model carries no debug section, symbol, or
//! relocation, and the emitted bytes are byte-for-byte unchanged.
//!
//! # Verification honesty
//!
//! This is a Windows host, so the tests in *this module* are purely structural:
//! they parse the emitted bytes back and check the magic, class/endianness,
//! header fields, section and symbol tables, and relocation records. They never
//! link or execute anything.
//!
//! That is not the whole picture, though. Both architectures are additionally
//! **link-and-run verified** by the CLI test suite when the tooling is present
//! (and skipped gracefully when it is not): `native_elf_x86_64_links_and_runs_under_docker`
//! links this writer's x86-64 output with `ld.lld -m elf_x86_64` and runs it in
//! a `linux/amd64` container, and the AArch64 counterpart runs under
//! QEMU-emulated arm64 Docker — each asserting the process exit code matches the
//! interpreter. See `documents/native_backend_contract.md`.
//!
//! One cross-check was done by hand rather than by the suite, and it is recorded
//! here because its *negative* half is easy to get wrong. The `.symtab`
//! `sh_info` values were confirmed against GNU binutils 2.42 under WSL: it
//! agreed with this writer on both a plain and a `--debug` object, and `ld`
//! linked both cleanly. The check is discriminating — injecting an off-by-one
//! `sh_info` makes `ld` report "cannot find entry symbol `_start`", because the
//! bad index reclassifies `_start` as local. But `readelf` did **not** flag that
//! same corrupted object: it renders each symbol's binding from `st_info` and
//! never validates `sh_info` against it, so "readelf is happy" is *not* evidence
//! about this field. `ld` is. Being manual, that check guards nothing on its
//! own; the committed guard is `symtab_sh_info_*` in the test module below.

use crate::object_model::{
    DwarfSection, ObjectMachine, ObjectModel, ObjectRelocationKind, ObjectSectionKind,
    ObjectSymbol, ObjectSymbolKind,
};

/// The `.shstrtab` name of a model section.
fn section_name(kind: ObjectSectionKind) -> &'static str {
    match kind {
        ObjectSectionKind::Text => ".text",
        ObjectSectionKind::ReadOnlyData => ".rodata",
        ObjectSectionKind::Bss => ".bss",
        ObjectSectionKind::Debug(dwarf) => dwarf.elf_name(),
    }
}

/// The `.shstrtab` name of the `SHT_RELA` section carrying `kind`'s relocations.
/// Static, because `PlannedSection` interns `&'static str` names.
fn rela_section_name(kind: ObjectSectionKind) -> &'static str {
    match kind {
        ObjectSectionKind::Text => ".rela.text",
        ObjectSectionKind::ReadOnlyData => ".rela.rodata",
        ObjectSectionKind::Bss => ".rela.bss",
        ObjectSectionKind::Debug(DwarfSection::Line) => ".rela.debug_line",
        ObjectSectionKind::Debug(DwarfSection::Info) => ".rela.debug_info",
        ObjectSectionKind::Debug(DwarfSection::Abbrev) => ".rela.debug_abbrev",
    }
}

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
/// `EM_AARCH64` — the ARM 64-bit architecture.
const EM_AARCH64: u16 = 183;

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

/// `STB_LOCAL` binding.
const STB_LOCAL: u8 = 0;
/// `STB_GLOBAL` binding (shifted into the high nibble of `st_info`).
const STB_GLOBAL: u8 = 1;
/// `STT_OBJECT` symbol type.
const STT_OBJECT: u8 = 1;
/// `STT_FUNC` symbol type.
const STT_FUNC: u8 = 2;
/// `STT_SECTION` symbol type — names the start of a section.
const STT_SECTION: u8 = 3;

/// `R_X86_64_64` — 64-bit absolute address (`S + A`).
const R_X86_64_64: u32 = 1;
/// `R_X86_64_32` — 32-bit zero-extended absolute reference (`S + A`); used
/// against an `STT_SECTION` symbol to express a section-relative DWARF offset.
const R_X86_64_32: u32 = 10;
/// `R_X86_64_PC32` — 32-bit PC-relative reference (`S + A - P`).
const R_X86_64_PC32: u32 = 2;
/// `R_X86_64_PLT32` — 32-bit PLT-relative reference; resolves like `PC32` for a
/// defined local target.
const R_X86_64_PLT32: u32 = 4;
/// `R_AARCH64_CALL26` — a `bl`/`call` site: the linker writes `(S + A - P) >> 2`
/// into the low 26 bits of the branch instruction word.
const R_AARCH64_CALL26: u32 = 283;

/// The REL32 addend: the relocated field sits at `P` and its 4 bytes end at
/// `P + 4`, so a displacement to `S` is `S - (P + 4)` = `S + (-4) - P`.
const REL32_ADDEND: i64 = -4;

/// The ELF binding of a model symbol. Section symbols are the only locals the
/// model ever carries; every other symbol is a global the linker resolves.
///
/// This is the **single source of truth** for the binding. The symbol *order*,
/// `.symtab`'s `sh_info`, and each `st_info` byte all derive from this one
/// function, so the ELF invariant they jointly encode — every `STB_LOCAL`
/// symbol precedes every non-local, and `sh_info` names the first non-local —
/// cannot drift apart between them. Deriving the three independently is the
/// hazard: a symbol classified local by one and global by another silently
/// yields an `sh_info` that linkers reject or mis-resolve.
///
/// Note the return values are ordered `STB_LOCAL` (0) < `STB_GLOBAL` (1), which
/// is exactly the sort key the symbol table needs.
fn elf_binding(symbol: &ObjectSymbol) -> u8 {
    match symbol.kind {
        ObjectSymbolKind::Section => STB_LOCAL,
        ObjectSymbolKind::Function | ObjectSymbolKind::Data => STB_GLOBAL,
    }
}

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
    // Every section carrying relocations gets its own `SHT_RELA` section, in
    // model-section order. Without `--debug` only `.text` ever has any, so the
    // planned table — and the emitted bytes — are exactly as before.
    let reloc_section_indices: Vec<usize> = model
        .sections
        .iter()
        .enumerate()
        .filter(|(_, section)| !section.relocations.is_empty())
        .map(|(index, _)| index)
        .collect();

    // Section-header indices are assigned in table order:
    //   0 null, 1.. content sections, then one rela per relocated section,
    //   then symtab, strtab, shstrtab.
    let content_count = model.sections.len() as u32;
    let symtab_shidx = 1 + content_count + reloc_section_indices.len() as u32;
    let strtab_shidx = symtab_shidx + 1;

    // -- Build the symbol table + its string table --------------------------
    // Symbol 0 is the reserved null symbol (all zero, STB_LOCAL). ELF requires
    // every STB_LOCAL symbol to precede every STB_GLOBAL one, so the model's
    // symbols are emitted section-symbols-first (the only locals; created solely
    // by the ELF DWARF path). The sort is stable, so the globals keep the model's
    // defined-before-undefined order, and with no section symbols the order — and
    // the bytes — are unchanged.
    let mut symbol_order: Vec<usize> = (0..model.symbols.len()).collect();
    symbol_order.sort_by_key(|&index| elf_binding(&model.symbols[index]));
    let local_count = model
        .symbols
        .iter()
        .filter(|symbol| elf_binding(symbol) == STB_LOCAL)
        .count();

    // Model symbol index -> `.symtab` index (1-based; the null symbol shifts it).
    let mut symtab_index_by_model = vec![0u64; model.symbols.len()];
    for (position, &model_index) in symbol_order.iter().enumerate() {
        symtab_index_by_model[model_index] = 1 + position as u64;
    }

    let mut strtab: Vec<u8> = vec![0]; // leading NUL
    let mut symtab: Vec<u8> = Vec::new();
    // Null symbol.
    symtab.extend_from_slice(&[0u8; SYM_SIZE as usize]);
    for &model_index in &symbol_order {
        let symbol = &model.symbols[model_index];
        // A section symbol conventionally carries no name of its own; its
        // identity is its `st_shndx`.
        let name_off = if symbol.kind == ObjectSymbolKind::Section {
            0
        } else {
            let off = strtab.len() as u32;
            strtab.extend_from_slice(symbol.name.as_bytes());
            strtab.push(0);
            off
        };
        // The binding comes from `elf_binding` alone — the same function that
        // ordered this table and sized `sh_info` — so those three cannot
        // disagree about which symbols are local. The type and `st_shndx` are an
        // independent question: an undefined external has no defining section
        // (SHN_UNDEF) and no type.
        let st_bind = elf_binding(symbol);
        let (st_type, st_shndx) = match symbol.section {
            Some(model_section) => {
                let ty = match symbol.kind {
                    ObjectSymbolKind::Function => STT_FUNC,
                    ObjectSymbolKind::Data => STT_OBJECT,
                    ObjectSymbolKind::Section => STT_SECTION,
                };
                (ty, elf_index_of_model(model_section) as u16)
            }
            None => (0u8, 0u16),
        };
        let st_info = (st_bind << 4) | st_type;
        push_u32(&mut symtab, name_off);
        symtab.push(st_info);
        symtab.push(0); // st_other
        push_u16(&mut symtab, st_shndx);
        push_u64(&mut symtab, symbol.value);
        push_u64(&mut symtab, 0); // st_size
    }
    let symtab_index_of = |model_index: usize| -> u64 { symtab_index_by_model[model_index] };

    // -- Build one `.rela.<section>` per relocated section --------------------
    // Each relocation kind maps to its architecture-specific ELF type and addend.
    // The x86-64 REL32 kinds carry the `-4` end-of-field addend; the AArch64
    // `CALL26` kind carries addend 0 (the branch immediate is PC-relative to the
    // instruction word itself); the DWARF kinds are absolute (`S + A`) with no
    // implicit bias, so addend 0.
    let rela_bytes: Vec<Vec<u8>> = reloc_section_indices
        .iter()
        .map(|&section_index| {
            let mut rela: Vec<u8> = Vec::new();
            for reloc in &model.sections[section_index].relocations {
                let (r_type, addend) = match reloc.kind {
                    ObjectRelocationKind::Branch => (R_X86_64_PLT32, REL32_ADDEND),
                    ObjectRelocationKind::PcRel32 => (R_X86_64_PC32, REL32_ADDEND),
                    ObjectRelocationKind::Aarch64Call26 => (R_AARCH64_CALL26, 0),
                    ObjectRelocationKind::Absolute64 => (R_X86_64_64, 0),
                    ObjectRelocationKind::SectionOffset32 => (R_X86_64_32, 0),
                };
                let r_info = (symtab_index_of(reloc.symbol) << 32) | u64::from(r_type);
                push_u64(&mut rela, reloc.offset);
                push_u64(&mut rela, r_info);
                push_i64(&mut rela, addend);
            }
            rela
        })
        .collect();

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
        let name = section_name(section.kind);
        let (sh_type, flags, addralign): (u32, u64, u64) = match section.kind {
            ObjectSectionKind::Text => (SHT_PROGBITS, SHF_ALLOC | SHF_EXECINSTR, 16),
            ObjectSectionKind::ReadOnlyData => (SHT_PROGBITS, SHF_ALLOC, 1),
            ObjectSectionKind::Bss => (SHT_NOBITS, SHF_ALLOC | SHF_WRITE, 16),
            // A DWARF section occupies file space but no memory image, so it is
            // PROGBITS with no SHF_ALLOC. Byte-aligned: DWARF is a byte stream.
            ObjectSectionKind::Debug(_) => (SHT_PROGBITS, 0, 1),
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

    for (&section_index, rela) in reloc_section_indices.iter().zip(rela_bytes) {
        planned.push(PlannedSection {
            name: rela_section_name(model.sections[section_index].kind),
            sh_type: SHT_RELA,
            flags: 0,
            addralign: 8,
            entsize: RELA_SIZE,
            offset: 0,
            size: rela.len() as u64,
            link: symtab_shidx,
            info: elf_index_of_model(section_index),
            bytes: rela,
        });
    }

    // `.symtab`: sh_link = strtab index. `sh_info` must be the index of the
    // first NON-LOCAL symbol — the gABI states it as "one greater than the
    // symbol table index of the last local symbol". The table is laid out as
    // [null, locals..., globals...] and the reserved null symbol is itself
    // STB_LOCAL, so that index is `1 + local_count`. Get this wrong and linkers
    // reject the object or mis-resolve symbols; `symtab_sh_info_*` in the test
    // module below pins both the exact value and the partition it names.
    planned.push(PlannedSection {
        name: ".symtab",
        sh_type: SHT_SYMTAB,
        flags: 0,
        addralign: 8,
        entsize: SYM_SIZE,
        offset: 0,
        size: symtab.len() as u64,
        link: strtab_shidx,
        info: 1 + local_count as u32,
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
    let e_machine = match model.machine {
        ObjectMachine::X86_64 => EM_X86_64,
        ObjectMachine::Aarch64 => EM_AARCH64,
    };
    push_u16(&mut out, ET_REL);
    push_u16(&mut out, e_machine);
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
        ObjectMachine, ObjectModel, ObjectRelocation, ObjectRelocationKind, ObjectSection,
        ObjectSectionKind, ObjectSymbol, ObjectSymbolKind,
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
            machine: ObjectMachine::X86_64,
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
            machine: ObjectMachine::X86_64,
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

    #[test]
    fn aarch64_machine_selects_em_aarch64_and_call26() {
        // A minimal AArch64 model: `_start` with a `bl main` (`Aarch64Call26`)
        // relocation at offset 0, and `main` at offset 4. The header must report
        // `EM_AARCH64` and the relocation must be `R_AARCH64_CALL26` with addend 0.
        let model = ObjectModel {
            sections: vec![ObjectSection {
                kind: ObjectSectionKind::Text,
                data: vec![0u8; 8],
                size: 8,
                relocations: vec![ObjectRelocation {
                    offset: 0,
                    symbol: 1, // main
                    kind: ObjectRelocationKind::Aarch64Call26,
                }],
            }],
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
                    value: 4,
                    kind: ObjectSymbolKind::Function,
                },
            ],
            entry_symbol: Some("_start".to_string()),
            machine: ObjectMachine::Aarch64,
        };
        let bytes = write_elf64(&model);
        assert_eq!(&bytes[0..4], &[0x7f, b'E', b'L', b'F'], "ELF magic");
        assert_eq!(rd_u16(&bytes, 18), EM_AARCH64, "e_machine = EM_AARCH64");
        let (_, sh_type, _, rela_off, rela_size, _, _, entsize) =
            find_section(&bytes, ".rela.text").expect(".rela.text present");
        assert_eq!(sh_type, SHT_RELA);
        assert_eq!(entsize, RELA_SIZE);
        assert_eq!((rela_size / RELA_SIZE) as usize, 1);
        let r0 = rela_off as usize;
        assert_eq!(rd_u64(&bytes, r0), 0, "r_offset");
        assert_eq!(rd_u32(&bytes, r0 + 8), R_AARCH64_CALL26, "type = CALL26");
        assert_eq!(rd_u32(&bytes, r0 + 12), 2, "symbol index (main)");
        assert_eq!(rd_i64(&bytes, r0 + 16), 0, "CALL26 addend is 0");
    }

    // -- `.symtab` sh_info -------------------------------------------------
    //
    // `sh_info` on a `SHT_SYMTAB` section must be, per the gABI, "one greater
    // than the symbol table index of the last local symbol" — equivalently, the
    // index of the first non-local symbol. A wrong value makes linkers reject
    // the object or mis-resolve symbols, and it is precisely the kind of field
    // that is correct once by hand and then silently broken when symbol
    // emission order changes. These tests pin it from both directions: the
    // exact expected number, and the partition that number is *defined* by.

    /// Every `Elf64_Sym` in `.symtab`, as `(name, binding, type, st_shndx)`.
    fn symtab_entries(bytes: &[u8]) -> Vec<(String, u8, u8, u16)> {
        let (_, _, _, sym_off, sym_size, ..) = find_section(bytes, ".symtab").expect(".symtab");
        let (str_hdr, ..) = find_section(bytes, ".strtab").expect(".strtab");
        let str_off = rd_u64(bytes, str_hdr + 24) as usize;
        let count = (sym_size / SYM_SIZE) as usize;
        (0..count)
            .map(|i| {
                let rec = sym_off as usize + i * SYM_SIZE as usize;
                let name = cstr(&bytes[str_off..], rd_u32(bytes, rec) as usize);
                let info = bytes[rec + 4];
                (name, info >> 4, info & 0xf, rd_u16(bytes, rec + 6))
            })
            .collect()
    }

    /// Assert `.symtab`'s `sh_info` genuinely names the local/non-local
    /// partition of the symbol table, and return it.
    ///
    /// This checks the property the ELF spec actually states, rather than a
    /// number read back off the current implementation: every symbol below
    /// `sh_info` is `STB_LOCAL` and every symbol at or above it is not. A
    /// hardcoded expected value alone is the weaker test — it would survive a
    /// reordering of symbol emission that moved a global below `sh_info`. This
    /// would not. Each caller asserts the exact value *as well*, so the pair
    /// pins both the number and its meaning.
    fn assert_sh_info_partitions_symtab(bytes: &[u8]) -> u32 {
        let (_, _, _, _, _, _, sh_info, _) = find_section(bytes, ".symtab").expect(".symtab");
        let symbols = symtab_entries(bytes);
        assert!(
            (sh_info as usize) <= symbols.len(),
            "sh_info {sh_info} runs past the end of a {}-symbol table",
            symbols.len()
        );
        assert!(sh_info >= 1, "the reserved null symbol is STB_LOCAL");
        for (index, (name, bind, _, _)) in symbols.iter().enumerate() {
            if index < sh_info as usize {
                assert_eq!(
                    *bind, STB_LOCAL,
                    "symbol {index} ({name:?}) is below sh_info={sh_info}, so it must be STB_LOCAL"
                );
            } else {
                assert_ne!(
                    *bind, STB_LOCAL,
                    "symbol {index} ({name:?}) is at/above sh_info={sh_info}, so it must not be \
                     STB_LOCAL"
                );
            }
        }
        sh_info
    }

    /// A model section symbol for model section `index`.
    fn section_symbol(name: &str, index: usize) -> ObjectSymbol {
        ObjectSymbol {
            name: name.to_string(),
            section: Some(index),
            value: 0,
            kind: ObjectSymbolKind::Section,
        }
    }

    /// A `.text`-defined global function symbol at `value`.
    fn function_symbol(name: &str, value: u64) -> ObjectSymbol {
        ObjectSymbol {
            name: name.to_string(),
            section: Some(0),
            value,
            kind: ObjectSymbolKind::Function,
        }
    }

    /// A `.text` plus the two DWARF sections the section symbols name.
    fn text_and_debug_sections() -> Vec<ObjectSection> {
        vec![
            ObjectSection {
                kind: ObjectSectionKind::Text,
                data: vec![0xc3],
                size: 1,
                relocations: Vec::new(),
            },
            ObjectSection {
                kind: ObjectSectionKind::Debug(DwarfSection::Abbrev),
                data: vec![0u8; 8],
                size: 8,
                relocations: Vec::new(),
            },
            ObjectSection {
                kind: ObjectSectionKind::Debug(DwarfSection::Line),
                data: vec![0u8; 8],
                size: 8,
                relocations: Vec::new(),
            },
        ]
    }

    #[test]
    fn symtab_sh_info_with_only_local_symbols() {
        // Nothing but section symbols: every symbol in the table is local, so
        // the first non-local index is one past the last of them.
        let model = ObjectModel {
            sections: text_and_debug_sections(),
            symbols: vec![
                section_symbol(".debug_abbrev", 1),
                section_symbol(".debug_line", 2),
            ],
            entry_symbol: None,
            machine: ObjectMachine::X86_64,
        };
        let bytes = write_elf64(&model);
        let sh_info = assert_sh_info_partitions_symtab(&bytes);
        assert_eq!(sh_info, 3, "null + 2 section locals, and no globals at all");
        let symbols = symtab_entries(&bytes);
        assert_eq!(symbols.len(), 3, "null + 2 section symbols");
        assert!(
            symbols.iter().all(|(_, bind, _, _)| *bind == STB_LOCAL),
            "every symbol is local: {symbols:?}"
        );
    }

    #[test]
    fn symtab_sh_info_with_locals_then_globals() {
        // The normal `--debug` shape. The section symbols are appended *after*
        // the globals in model order — exactly as `attach_dwarf_line_info` adds
        // them — so this only passes if the writer really reorders locals to the
        // front and counts them, rather than trusting the model's order.
        let model = ObjectModel {
            sections: text_and_debug_sections(),
            symbols: vec![
                function_symbol("_start", 0),
                function_symbol("main", 0),
                section_symbol(".debug_abbrev", 1),
                section_symbol(".debug_line", 2),
            ],
            entry_symbol: Some("_start".to_string()),
            machine: ObjectMachine::X86_64,
        };
        let bytes = write_elf64(&model);
        let sh_info = assert_sh_info_partitions_symtab(&bytes);
        assert_eq!(sh_info, 3, "null + 2 section locals precede the 2 globals");

        let symbols = symtab_entries(&bytes);
        assert_eq!(symbols.len(), 5, "null + 2 sections + 2 functions");
        // The locals are the STT_SECTION symbols, hoisted ahead of the globals.
        assert_eq!(symbols[1].2, STT_SECTION, "symbol 1 is a section symbol");
        assert_eq!(symbols[2].2, STT_SECTION, "symbol 2 is a section symbol");
        // ...and the globals keep the model's relative order behind them.
        assert_eq!(symbols[3].0, "_start");
        assert_eq!(symbols[4].0, "main");
    }

    #[test]
    fn symtab_sh_info_with_no_locals_beyond_the_null_symbol() {
        // No `--debug`, so no section symbols exist: the null symbol is the only
        // local and the very first model symbol is already global.
        let bytes = write_elf64(&sample_model());
        let sh_info = assert_sh_info_partitions_symtab(&bytes);
        assert_eq!(sh_info, 1, "only the reserved null symbol is local");
        let symbols = symtab_entries(&bytes);
        assert_eq!(symbols.len(), 4, "null + 3 model symbols");
        assert_eq!(symbols[0].1, STB_LOCAL, "the null symbol is local");
        assert!(
            symbols[1..]
                .iter()
                .all(|(_, bind, _, _)| *bind == STB_GLOBAL),
            "every model symbol is global: {symbols:?}"
        );
    }

    #[test]
    fn symtab_sh_info_with_an_undefined_external_global() {
        // An `extern fn` is a global with no defining section (SHN_UNDEF). It
        // takes a different arm of the symbol writer than a defined global, so
        // pin that it still lands on the non-local side of `sh_info`.
        let model = ObjectModel {
            sections: text_and_debug_sections(),
            symbols: vec![
                function_symbol("main", 0),
                ObjectSymbol {
                    name: "puts".to_string(),
                    section: None,
                    value: 0,
                    kind: ObjectSymbolKind::Function,
                },
                section_symbol(".debug_line", 2),
            ],
            entry_symbol: None,
            machine: ObjectMachine::X86_64,
        };
        let bytes = write_elf64(&model);
        let sh_info = assert_sh_info_partitions_symtab(&bytes);
        assert_eq!(sh_info, 2, "null + 1 section local; the extern is global");
        let symbols = symtab_entries(&bytes);
        let puts = symbols
            .iter()
            .position(|(n, ..)| n == "puts")
            .expect("puts");
        assert!(puts >= sh_info as usize, "the undefined external is global");
        assert_eq!(symbols[puts].3, 0, "SHN_UNDEF");
    }

    /// End-to-end `sh_info` coverage: real `.lby` programs compiled all the way
    /// through the native backend into ELF bytes.
    ///
    /// The tests above build `ObjectModel`s by hand, which pins the writer but
    /// not the *models the compiler actually produces*. These compile real
    /// source, so they also catch a symbol-emission change upstream in
    /// `native_object*.rs` that would invalidate `sh_info` without touching this
    /// file — the exact regression the coverage gap was about.
    mod programs {
        use super::*;
        use crate::native_contract::x86_64_linux_target;
        use crate::{
            BytecodeModule, DebugOptions, emit_native_program_for_target, lower, lower_to_bytecode,
        };
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

        /// Emit a Linux ELF object for `source`, with or without debug info.
        fn emit_linux_elf(source: &str, debug: Option<&DebugOptions>) -> Vec<u8> {
            let program = emit_native_program_for_target(
                &module_for(source),
                &x86_64_linux_target(),
                debug,
                false,
            )
            .expect("emit native program");
            assert!(
                program.skipped.is_empty(),
                "no function should be skipped: {:?}",
                program.skipped
            );
            program.bytes
        }

        /// Several functions plus a C-callable `export fn` — mixed bindings through
        /// the real lowering path.
        const MIXED_BINDING_PROGRAM: &str = concat!(
            "fn add x, y i64 -> i64\n",
            "    return x + y\n",
            "\n",
            "fn double x i64 -> i64\n",
            "    return add(x, x)\n",
            "\n",
            "export fn add_seven x i64 -> i64\n",
            "    return add(x, 7)\n",
            "\n",
            "fn main -> i64\n",
            "    return double(add_seven(7))\n",
        );

        #[test]
        fn multiple_functions_with_an_export_are_all_global() {
            let bytes = emit_linux_elf(MIXED_BINDING_PROGRAM, None);
            let sh_info = assert_sh_info_partitions_symtab(&bytes);
            // No `--debug`, so nothing created a section symbol: every compiled
            // function — plain, exported, and the entry stub alike — is global.
            assert_eq!(sh_info, 1, "only the null symbol is local without --debug");

            let symbols = symtab_entries(&bytes);
            for name in ["add", "double", "add_seven", "main"] {
                let index = symbols
                    .iter()
                    .position(|(n, ..)| n == name)
                    .unwrap_or_else(|| panic!("{name} in .symtab: {symbols:?}"));
                assert!(
                    index >= sh_info as usize,
                    "{name} is global so it must sit at/above sh_info={sh_info}"
                );
            }
        }

        #[test]
        fn debug_section_symbols_are_the_locals_of_a_real_program() {
            // With `--debug` the DWARF path attaches STT_SECTION symbols. They are
            // the only locals, they must precede every function symbol, and
            // `sh_info` must count them.
            let debug = DebugOptions {
                source_file: "example/mixed.lby".to_string(),
            };
            let bytes = emit_linux_elf(MIXED_BINDING_PROGRAM, Some(&debug));
            let sh_info = assert_sh_info_partitions_symtab(&bytes);

            let symbols = symtab_entries(&bytes);
            let section_symbols = symbols
                .iter()
                .filter(|(_, _, ty, _)| *ty == STT_SECTION)
                .count();
            assert!(
                section_symbols > 0,
                "--debug must attach section symbols: {symbols:?}"
            );
            assert_eq!(
                sh_info as usize,
                1 + section_symbols,
                "sh_info = null + every STT_SECTION local"
            );
            // The function symbols all sit on the global side.
            for name in ["add", "double", "add_seven", "main"] {
                let index = symbols
                    .iter()
                    .position(|(n, ..)| n == name)
                    .unwrap_or_else(|| panic!("{name} in .symtab: {symbols:?}"));
                assert!(
                    index >= sh_info as usize,
                    "{name} is global so it must sit at/above sh_info={sh_info}"
                );
            }
        }
    }
}
