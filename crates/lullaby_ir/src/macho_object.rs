//! Mach-O x86-64 relocatable-object writer.
//!
//! Serializes an [`ObjectModel`] into an
//! `MH_OBJECT` Mach-O 64 object: a `mach_header_64`, a single `LC_SEGMENT_64`
//! holding the `__text`/`__const`/`__bss` sections, an `LC_SYMTAB`, an
//! `LC_DYSYMTAB`, and `relocation_info` entries that use `X86_64_RELOC_BRANCH`
//! for `call` sites and `X86_64_RELOC_SIGNED` for RIP-relative data references.
//! The shared machine code is emitted by the native backend; this module only
//! builds the Mach-O container plus the macOS freestanding entry stub's symbol
//! (`start`).
//!
//! # `--debug` (DWARF)
//!
//! With `--debug`, `native_object_dwarf.rs` attaches the DWARF sections and their
//! relocations to the model before it reaches this writer. Here that means the
//! `__debug_*` sections are placed in the `__DWARF` segment with `S_ATTR_DEBUG`,
//! relocations are emitted **per section** rather than only for `__text`, and the
//! DWARF address fields use `X86_64_RELOC_UNSIGNED` (`r_length` = 3, `r_pcrel` =
//! 0). Mach-O DWARF needs no section-offset relocations — `ld64` leaves each
//! object's DWARF intact for `dsymutil` rather than concatenating it — so that
//! relocation kind never reaches this writer. Without `--debug` the model carries
//! no debug section or relocation and the bytes are byte-for-byte unchanged.
//!
//! # Verification honesty
//!
//! This is a Windows host: the emitted object is verified *structurally* (magic,
//! header fields, load commands, sections, symbol table, relocation records —
//! see the unit tests) but is **not** linked or executed here. Link-and-run
//! verification is deferred to the cross-platform CI of the Phase 9 roadmap.
//! x86-64 only; ARM64 (`arm64`) Mach-O is a separate future effort.

use crate::object_model::{
    DwarfSection, ObjectModel, ObjectRelocationKind, ObjectSectionKind, ObjectSymbolKind,
};

// -- Mach-O constants --------------------------------------------------------

/// `MH_MAGIC_64`.
const MH_MAGIC_64: u32 = 0xFEED_FACF;
/// `CPU_TYPE_X86_64` (`CPU_TYPE_X86 | CPU_ARCH_ABI64`).
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
/// `CPU_SUBTYPE_X86_64_ALL`.
const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
/// `MH_OBJECT` — a relocatable object.
const MH_OBJECT: u32 = 1;

/// `LC_SEGMENT_64`.
const LC_SEGMENT_64: u32 = 0x19;
/// `LC_SYMTAB`.
const LC_SYMTAB: u32 = 0x2;
/// `LC_DYSYMTAB`.
const LC_DYSYMTAB: u32 = 0xb;

/// Size of `mach_header_64`.
const HEADER_SIZE: u64 = 32;
/// Base size of a `segment_command_64` (before its section entries).
const SEGMENT_CMD_BASE: u64 = 72;
/// Size of one `section_64`.
const SECTION_SIZE: u64 = 80;
/// Size of `symtab_command`.
const SYMTAB_CMD_SIZE: u64 = 24;
/// Size of `dysymtab_command`.
const DYSYMTAB_CMD_SIZE: u64 = 80;
/// Size of one `nlist_64`.
const NLIST_SIZE: u64 = 16;
/// Size of one `relocation_info`.
const RELOC_SIZE: u64 = 8;

/// `S_ZEROFILL` section type.
const S_ZEROFILL: u32 = 0x1;
/// `S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS` — an executable section.
const S_TEXT_ATTRS: u32 = 0x8000_0000 | 0x0000_0400;
/// `S_ATTR_DEBUG` — a debug section. `ld64` does not merge these into the linked
/// image; it records a per-object `OSO` entry and leaves `dsymutil` to collect
/// each object's DWARF, which is why the DWARF here needs no section-offset
/// relocations (see `ObjectRelocationKind::SectionOffset32`).
const S_ATTR_DEBUG: u32 = 0x0200_0000;

/// The Mach-O segment every DWARF section lives in.
const DWARF_SEGMENT: &str = "__DWARF";

/// `N_EXT` — external symbol bit.
const N_EXT: u8 = 0x01;
/// `N_SECT` — symbol defined in a section.
const N_SECT: u8 = 0x0e;

/// `X86_64_RELOC_UNSIGNED` — an absolute (non-PC-relative) reference.
const X86_64_RELOC_UNSIGNED: u32 = 0;
/// `X86_64_RELOC_SIGNED` — signed 32-bit PC-relative (RIP-relative data ref).
const X86_64_RELOC_SIGNED: u32 = 1;
/// `X86_64_RELOC_BRANCH` — a `call`/`jmp` branch relocation.
const X86_64_RELOC_BRANCH: u32 = 2;

/// One planned Mach-O section: its names, memory/file placement, and flags.
struct PlannedSection {
    sectname: &'static str,
    segname: &'static str,
    addr: u64,
    size: u64,
    /// File offset of the section bytes (0 for zerofill).
    offset: u32,
    /// log2 of the alignment.
    align: u32,
    flags: u32,
    /// The raw bytes (empty for zerofill).
    bytes: Vec<u8>,
    /// File offset of this section's `relocation_info` array (0 when it has none).
    reloff: u32,
    /// How many relocations this section has.
    nreloc: u32,
    /// Index of the originating model section, so the emitter can find its
    /// relocations again after planning.
    model_index: usize,
}

/// The `__DWARF` section name for a DWARF section.
fn dwarf_sectname(section: DwarfSection) -> &'static str {
    match section {
        DwarfSection::Line => "__debug_line",
        DwarfSection::Info => "__debug_info",
        DwarfSection::Abbrev => "__debug_abbrev",
    }
}

/// Serialize `model` into a relocatable Mach-O x86-64 object.
pub fn write_macho64(model: &ObjectModel) -> Vec<u8> {
    // -- Plan sections (vm addresses laid out contiguously from 0) ----------
    let mut planned: Vec<PlannedSection> = Vec::new();
    let mut vm_cursor: u64 = 0;
    for (model_index, section) in model.sections.iter().enumerate() {
        let (sectname, segname, align_log2, flags) = match section.kind {
            ObjectSectionKind::Text => ("__text", "__TEXT", 4u32, S_TEXT_ATTRS),
            ObjectSectionKind::ReadOnlyData => ("__const", "__TEXT", 0u32, 0u32),
            ObjectSectionKind::Bss => ("__bss", "__DATA", 4u32, S_ZEROFILL),
            // DWARF is a byte stream, so alignment 1 (log2 = 0).
            ObjectSectionKind::Debug(dwarf) => {
                (dwarf_sectname(dwarf), DWARF_SEGMENT, 0u32, S_ATTR_DEBUG)
            }
        };
        let align = 1u64 << align_log2;
        vm_cursor = align_up(vm_cursor, align);
        let bytes = if section.kind == ObjectSectionKind::Bss {
            Vec::new()
        } else {
            section.data.clone()
        };
        planned.push(PlannedSection {
            sectname,
            segname,
            addr: vm_cursor,
            size: section.size,
            offset: 0, // filled during file layout
            align: align_log2,
            flags,
            bytes,
            reloff: 0, // filled during file layout
            nreloc: section.relocations.len() as u32,
            model_index,
        });
        vm_cursor += section.size;
    }
    let vm_size = align_up(vm_cursor, 16);
    let nsects = planned.len() as u32;

    // -- Load-command sizes --------------------------------------------------
    let segment_cmd_size = SEGMENT_CMD_BASE + u64::from(nsects) * SECTION_SIZE;
    let sizeofcmds = segment_cmd_size + SYMTAB_CMD_SIZE + DYSYMTAB_CMD_SIZE;
    let ncmds: u32 = 3;

    // -- File layout ---------------------------------------------------------
    // Header, then all load commands, then section data (16-aligned), then
    // relocations, then the symbol table, then the string table.
    let commands_end = HEADER_SIZE + sizeofcmds;
    let mut file_cursor = align_up(commands_end, 16);
    let seg_fileoff = file_cursor;
    for section in planned.iter_mut() {
        if section.flags & S_ZEROFILL != 0 {
            continue; // zerofill: no file bytes
        }
        let align = 1u64 << section.align;
        file_cursor = align_up(file_cursor, align.max(1));
        section.offset = file_cursor as u32;
        file_cursor += section.bytes.len() as u64;
    }
    let seg_filesize = file_cursor - seg_fileoff;

    // Relocations follow the section data: one `relocation_info` array per
    // relocated section, in section order. Without `--debug` only `__text` has
    // any, so this lays out exactly the single array it did before.
    file_cursor = align_up(file_cursor, 8);
    for section in planned.iter_mut() {
        if section.nreloc == 0 {
            continue;
        }
        section.reloff = file_cursor as u32;
        file_cursor += u64::from(section.nreloc) * RELOC_SIZE;
    }

    // Symbol table (nlist_64 array) follows the relocations.
    file_cursor = align_up(file_cursor, 8);
    let symoff = file_cursor as u32;
    let nsyms = model.symbols.len() as u32;
    file_cursor += u64::from(nsyms) * NLIST_SIZE;

    // -- Build the string table + nlist entries ------------------------------
    // Mach-O string tables begin with a NUL so index 0 is the empty name.
    let mut strtab: Vec<u8> = vec![0];
    let mut nlist: Vec<u8> = Vec::new();
    // The model orders every defined symbol before every undefined one, which is
    // exactly what LC_DYSYMTAB's defined/undefined ranges require.
    let mut first_undef: Option<u32> = None;
    for (index, symbol) in model.symbols.iter().enumerate() {
        let n_strx = strtab.len() as u32;
        strtab.extend_from_slice(symbol.name.as_bytes());
        strtab.push(0);
        let (n_type, n_sect, n_value) = match symbol.section {
            Some(model_section) => {
                // 1-based section number in load order.
                let sect_no = (model_section + 1) as u8;
                (N_SECT | N_EXT, sect_no, symbol.value)
            }
            None => {
                if first_undef.is_none() {
                    first_undef = Some(index as u32);
                }
                (N_EXT, 0u8, 0u64)
            }
        };
        // `kind` is retained for parity with the ELF path; Mach-O records the
        // code/data split through section placement rather than the symbol type.
        let _ = matches!(symbol.kind, ObjectSymbolKind::Function);
        push_u32(&mut nlist, n_strx);
        nlist.push(n_type);
        nlist.push(n_sect);
        push_u16(&mut nlist, 0); // n_desc
        push_u64(&mut nlist, n_value);
    }
    let nundef = first_undef.map(|first| nsyms - first).unwrap_or(0);
    let ndef = nsyms - nundef;
    let iundef = first_undef.unwrap_or(nsyms);

    let stroff = file_cursor as u32;
    let strsize = strtab.len() as u32;

    // -- Emit ----------------------------------------------------------------
    let mut out: Vec<u8> = Vec::new();

    // mach_header_64.
    push_u32(&mut out, MH_MAGIC_64);
    push_u32(&mut out, CPU_TYPE_X86_64);
    push_u32(&mut out, CPU_SUBTYPE_X86_64_ALL);
    push_u32(&mut out, MH_OBJECT);
    push_u32(&mut out, ncmds);
    push_u32(&mut out, sizeofcmds as u32);
    push_u32(&mut out, 0); // flags
    push_u32(&mut out, 0); // reserved

    // LC_SEGMENT_64 (single unnamed segment holding every section).
    push_u32(&mut out, LC_SEGMENT_64);
    push_u32(&mut out, segment_cmd_size as u32);
    push_fixed_name(&mut out, "", 16); // segname (empty for MH_OBJECT)
    push_u64(&mut out, 0); // vmaddr
    push_u64(&mut out, vm_size); // vmsize
    push_u64(&mut out, seg_fileoff); // fileoff
    push_u64(&mut out, seg_filesize); // filesize
    push_u32(&mut out, 7); // maxprot rwx
    push_u32(&mut out, 7); // initprot rwx
    push_u32(&mut out, nsects);
    push_u32(&mut out, 0); // flags

    for section in &planned {
        push_fixed_name(&mut out, section.sectname, 16);
        push_fixed_name(&mut out, section.segname, 16);
        push_u64(&mut out, section.addr);
        push_u64(&mut out, section.size);
        push_u32(&mut out, section.offset);
        push_u32(&mut out, section.align);
        push_u32(&mut out, section.reloff);
        push_u32(&mut out, section.nreloc);
        push_u32(&mut out, section.flags);
        push_u32(&mut out, 0); // reserved1
        push_u32(&mut out, 0); // reserved2
        push_u32(&mut out, 0); // reserved3
    }

    // LC_SYMTAB.
    push_u32(&mut out, LC_SYMTAB);
    push_u32(&mut out, SYMTAB_CMD_SIZE as u32);
    push_u32(&mut out, symoff);
    push_u32(&mut out, nsyms);
    push_u32(&mut out, stroff);
    push_u32(&mut out, strsize);

    // LC_DYSYMTAB (defined-symbol / undefined-symbol ranges).
    push_u32(&mut out, LC_DYSYMTAB);
    push_u32(&mut out, DYSYMTAB_CMD_SIZE as u32);
    push_u32(&mut out, 0); // ilocalsym
    push_u32(&mut out, 0); // nlocalsym
    push_u32(&mut out, 0); // iextdefsym
    push_u32(&mut out, ndef); // nextdefsym
    push_u32(&mut out, iundef); // iundefsym
    push_u32(&mut out, nundef); // nundefsym
    push_u32(&mut out, 0); // tocoff
    push_u32(&mut out, 0); // ntoc
    push_u32(&mut out, 0); // modtaboff
    push_u32(&mut out, 0); // nmodtab
    push_u32(&mut out, 0); // extrefsymoff
    push_u32(&mut out, 0); // nextrefsyms
    push_u32(&mut out, 0); // indirectsymoff
    push_u32(&mut out, 0); // nindirectsyms
    push_u32(&mut out, 0); // extreloff
    push_u32(&mut out, 0); // nextrel
    push_u32(&mut out, 0); // locreloff
    push_u32(&mut out, 0); // nlocrel

    // Section data.
    for section in &planned {
        if section.flags & S_ZEROFILL != 0 {
            continue;
        }
        pad_to(&mut out, u64::from(section.offset));
        out.extend_from_slice(&section.bytes);
    }

    // Relocations, one array per relocated section (matching the layout above).
    for section in &planned {
        if section.nreloc == 0 {
            continue;
        }
        pad_to(&mut out, u64::from(section.reloff));
        for reloc in &model.sections[section.model_index].relocations {
            // (r_type, r_pcrel, r_length): the code relocations are 4-byte
            // PC-relative fixups; the DWARF address relocation is an 8-byte
            // absolute address (`DW_LNE_set_address` / `DW_AT_low_pc`).
            let (r_type, r_pcrel, r_length) = match reloc.kind {
                ObjectRelocationKind::Branch => (X86_64_RELOC_BRANCH, 1, 2),
                ObjectRelocationKind::PcRel32 => (X86_64_RELOC_SIGNED, 1, 2),
                ObjectRelocationKind::Absolute64 => (X86_64_RELOC_UNSIGNED, 0, 3),
                // The Mach-O writer is x86-64 only; AArch64 programs are always
                // emitted as ELF, so an AArch64 branch relocation can never reach
                // a Mach-O container by construction.
                ObjectRelocationKind::Aarch64Call26 => {
                    panic!("the Mach-O writer is x86-64 only; AArch64 objects are emitted as ELF")
                }
                // Section-offset relocations are an ELF-only need: `ld64` keeps
                // each object's DWARF intact rather than concatenating it, so the
                // DWARF emitter never attaches one to a Mach-O model.
                ObjectRelocationKind::SectionOffset32 => {
                    panic!("Mach-O DWARF is object-local and needs no section-offset relocations")
                }
            };
            let packed: u32 = (reloc.symbol as u32 & 0x00FF_FFFF)
                | (r_pcrel << 24)
                | (r_length << 25)
                | (1 << 27) // r_extern: r_symbolnum is a symbol table index
                | (r_type << 28);
            push_u32(&mut out, reloc.offset as u32); // r_address
            push_u32(&mut out, packed);
        }
    }

    // Symbol table.
    pad_to(&mut out, u64::from(symoff));
    out.extend_from_slice(&nlist);

    // String table.
    pad_to(&mut out, u64::from(stroff));
    out.extend_from_slice(&strtab);

    out
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

/// Write `name` into a fixed `width`-byte field, NUL-padded (and NUL-truncated
/// if longer than the field, which never happens for the fixed section names).
fn push_fixed_name(out: &mut Vec<u8>, name: &str, width: usize) {
    let bytes = name.as_bytes();
    let take = bytes.len().min(width);
    out.extend_from_slice(&bytes[..take]);
    for _ in take..width {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object_model::{
        ObjectMachine, ObjectModel, ObjectRelocation, ObjectRelocationKind, ObjectSection,
        ObjectSectionKind, ObjectSymbol, ObjectSymbolKind,
    };

    fn rd_u32(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }
    fn rd_u64(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }
    fn cstr(strtab: &[u8], off: usize) -> String {
        let end = strtab[off..].iter().position(|&c| c == 0).unwrap() + off;
        String::from_utf8(strtab[off..end].to_vec()).unwrap()
    }
    fn fixed_name(b: &[u8], off: usize) -> String {
        let end = b[off..off + 16].iter().position(|&c| c == 0).unwrap_or(16);
        String::from_utf8(b[off..off + end].to_vec()).unwrap()
    }

    /// A representative model: a `.text` (entry stub + one function with a branch
    /// relocation to `main` and a data relocation to `__str0`), a `.rodata`
    /// constant, an undefined external, and a `.bss` cell.
    fn sample_model() -> ObjectModel {
        ObjectModel {
            sections: vec![
                ObjectSection {
                    kind: ObjectSectionKind::Text,
                    data: vec![0u8; 20],
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
                    name: "start".to_string(),
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
                // A trailing undefined external, e.g. an `extern fn` C symbol.
                ObjectSymbol {
                    name: "puts".to_string(),
                    section: None,
                    value: 0,
                    kind: ObjectSymbolKind::Function,
                },
            ],
            entry_symbol: Some("start".to_string()),
            machine: ObjectMachine::X86_64,
        }
    }

    #[test]
    fn header_identifies_a_relocatable_x86_64_macho() {
        let bytes = write_macho64(&sample_model());
        assert_eq!(rd_u32(&bytes, 0), MH_MAGIC_64, "MH_MAGIC_64");
        assert_eq!(rd_u32(&bytes, 4), CPU_TYPE_X86_64, "cputype");
        assert_eq!(rd_u32(&bytes, 8), CPU_SUBTYPE_X86_64_ALL, "cpusubtype");
        assert_eq!(rd_u32(&bytes, 12), MH_OBJECT, "filetype = MH_OBJECT");
        assert_eq!(rd_u32(&bytes, 16), 3, "ncmds = segment + symtab + dysymtab");
    }

    /// Walk the load commands, returning `(cmd, offset)` for each.
    fn load_commands(bytes: &[u8]) -> Vec<(u32, usize)> {
        let ncmds = rd_u32(bytes, 16) as usize;
        let mut cmds = Vec::new();
        let mut off = HEADER_SIZE as usize;
        for _ in 0..ncmds {
            let cmd = rd_u32(bytes, off);
            let size = rd_u32(bytes, off + 4) as usize;
            cmds.push((cmd, off));
            off += size;
        }
        cmds
    }

    #[test]
    fn segment_holds_text_const_and_bss_sections() {
        let bytes = write_macho64(&sample_model());
        let (_, seg_off) = load_commands(&bytes)
            .into_iter()
            .find(|&(cmd, _)| cmd == LC_SEGMENT_64)
            .expect("LC_SEGMENT_64");
        let nsects = rd_u32(&bytes, seg_off + 64);
        assert_eq!(nsects, 3);
        let mut sect = seg_off + SEGMENT_CMD_BASE as usize;
        let mut sect_names = Vec::new();
        let mut text_reloc = (0u32, 0u32);
        let mut bss_is_zerofill = false;
        for _ in 0..nsects {
            let sectname = fixed_name(&bytes, sect);
            let segname = fixed_name(&bytes, sect + 16);
            let size = rd_u64(&bytes, sect + 40);
            let reloff = rd_u32(&bytes, sect + 56);
            let nreloc = rd_u32(&bytes, sect + 60);
            let flags = rd_u32(&bytes, sect + 64);
            if sectname == "__text" {
                assert_eq!(segname, "__TEXT");
                text_reloc = (reloff, nreloc);
            }
            if sectname == "__bss" {
                bss_is_zerofill = flags & S_ZEROFILL != 0 && size == 4096;
            }
            sect_names.push(sectname);
            sect += SECTION_SIZE as usize;
        }
        assert_eq!(sect_names, vec!["__text", "__const", "__bss"]);
        assert_eq!(text_reloc.1, 2, "__text has two relocations");
        assert!(text_reloc.0 > 0, "__text reloff set");
        assert!(bss_is_zerofill, "__bss is S_ZEROFILL with the heap size");
    }

    #[test]
    fn symtab_and_dysymtab_split_defined_from_undefined() {
        let bytes = write_macho64(&sample_model());
        let cmds = load_commands(&bytes);
        let (_, sym_off) = cmds
            .iter()
            .copied()
            .find(|&(cmd, _)| cmd == LC_SYMTAB)
            .expect("LC_SYMTAB");
        let symoff = rd_u32(&bytes, sym_off + 8) as usize;
        let nsyms = rd_u32(&bytes, sym_off + 12) as usize;
        let stroff = rd_u32(&bytes, sym_off + 16) as usize;
        assert_eq!(nsyms, 4);

        let mut names = Vec::new();
        let mut undef_last = true;
        for i in 0..nsyms {
            let rec = symoff + i * NLIST_SIZE as usize;
            let n_strx = rd_u32(&bytes, rec) as usize;
            let n_type = bytes[rec + 4];
            let n_sect = bytes[rec + 5];
            let name = cstr(&bytes[stroff..], n_strx);
            if name == "puts" {
                assert_eq!(n_type, N_EXT, "undefined external: N_UNDF|N_EXT");
                assert_eq!(n_sect, 0, "undefined has NO_SECT");
                assert_eq!(i, nsyms - 1, "undefined symbol is last");
            }
            if name == "main" {
                assert_eq!(n_type, N_SECT | N_EXT);
                assert_eq!(n_sect, 1, "defined in section 1 (__text)");
            }
            names.push(name);
            let _ = &mut undef_last;
        }
        assert!(names.contains(&"start".to_string()));
        assert!(names.contains(&"__str0".to_string()));

        // LC_DYSYMTAB ranges: 3 defined, then 1 undefined at index 3.
        let (_, dsym_off) = cmds
            .iter()
            .copied()
            .find(|&(cmd, _)| cmd == LC_DYSYMTAB)
            .expect("LC_DYSYMTAB");
        // dysymtab_command: cmd,cmdsize,ilocalsym,nlocalsym,iextdefsym,
        // nextdefsym,iundefsym,nundefsym,... — nextdefsym is at byte offset 20.
        assert_eq!(rd_u32(&bytes, dsym_off + 20), 3, "nextdefsym");
        assert_eq!(rd_u32(&bytes, dsym_off + 24), 3, "iundefsym");
        assert_eq!(rd_u32(&bytes, dsym_off + 28), 1, "nundefsym");
    }

    #[test]
    fn relocations_use_branch_and_signed_pcrel() {
        let bytes = write_macho64(&sample_model());
        let (_, seg_off) = load_commands(&bytes)
            .into_iter()
            .find(|&(cmd, _)| cmd == LC_SEGMENT_64)
            .unwrap();
        // __text is the first section entry.
        let sect = seg_off + SEGMENT_CMD_BASE as usize;
        let reloff = rd_u32(&bytes, sect + 56) as usize;
        // Reloc 0: BRANCH to symbol index 1 (main).
        let r0 = reloff;
        assert_eq!(rd_u32(&bytes, r0), 4, "r_address");
        let packed0 = rd_u32(&bytes, r0 + 4);
        assert_eq!(packed0 & 0x00FF_FFFF, 1, "r_symbolnum");
        assert_eq!((packed0 >> 24) & 1, 1, "r_pcrel");
        assert_eq!((packed0 >> 25) & 3, 2, "r_length = 2");
        assert_eq!((packed0 >> 27) & 1, 1, "r_extern");
        assert_eq!(packed0 >> 28, X86_64_RELOC_BRANCH, "type = BRANCH");
        // Reloc 1: SIGNED (data) to symbol index 2 (__str0).
        let r1 = reloff + RELOC_SIZE as usize;
        assert_eq!(rd_u32(&bytes, r1), 12, "r_address");
        let packed1 = rd_u32(&bytes, r1 + 4);
        assert_eq!(packed1 & 0x00FF_FFFF, 2, "r_symbolnum");
        assert_eq!(packed1 >> 28, X86_64_RELOC_SIGNED, "type = SIGNED");
    }
}
