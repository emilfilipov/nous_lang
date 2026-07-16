//! DWARF source-line debug info for the ELF and Mach-O native targets.
//!
//! This is the portable counterpart of the COFF `.debug$S` CodeView path in
//! `native_object_writers.rs`: it gives `lullaby native --debug` the same capability on
//! Linux/macOS objects that CodeView already gives on Windows — a debugger can
//! break at a compiled function and be shown its `.lby` declaration line.
//!
//! # What is emitted, and why this section set
//!
//! Three sections, which is the minimum for a *consumable* line table:
//!
//! - **`.debug_line`** — the line-number program. This is the load-bearing
//!   artifact: one row per compiled function mapping its entry address to its
//!   `BytecodeFunction.span.line`.
//! - **`.debug_info`** — a single compile-unit DIE. A consumer does not scan
//!   `.debug_line` directly; it walks `.debug_info` and follows the CU's
//!   `DW_AT_stmt_list` to the line program, so a line table with no CU is
//!   unreachable. The CU carries one `DW_TAG_subprogram` child per function
//!   (name, decl line, `low_pc`/`high_pc`) so an address→function lookup also
//!   resolves.
//! - **`.debug_abbrev`** — the abbreviation table `.debug_info` is encoded
//!   against; `.debug_info` is unparseable without it.
//!
//! `.debug_str` is deliberately **not** emitted: every string here is used once,
//! so the DIEs use inline `DW_FORM_string` instead of `DW_FORM_strp`. That drops
//! a whole section and, with it, a class of cross-section offset relocations, at
//! a cost of a few duplicated bytes in a CU this small.
//!
//! **DWARF version 4** is emitted throughout. It is universally consumed (gdb,
//! lldb, `llvm-dwarfdump`, `gimli`) and its line-program header is markedly
//! simpler than DWARF 5's, whose directory/file tables carry their own
//! form-encoded descriptors. Nothing here needs a DWARF 5 feature.
//!
//! # Line granularity
//!
//! **Per function**, matching what the CodeView path delivers: one row at each
//! function's entry offset, carrying its declaration line. Per-statement mapping
//! is deferred — see "Deferred" below.
//!
//! Each function is emitted as its **own line-program sequence** (a
//! `DW_LNE_set_address` through to a `DW_LNE_end_sequence`). A sequence must have
//! monotonically increasing addresses; giving each function a private sequence
//! makes that hold by construction regardless of how the linker orders `.text`,
//! and lets every address be a clean relocation against the function's own symbol
//! rather than a computed displacement.
//!
//! # Deferred (honestly)
//!
//! Local/parameter **variables**, **types** (`.debug_str`/`DW_TAG_base_type` and
//! the type graph), **frame info** (`.debug_frame`/`.eh_frame` CFI, so a debugger
//! cannot unwind a Lullaby stack), per-statement line rows, lexical scopes, and
//! inlined-frame records. This increment is source lines only.
//!
//! # Verification honesty
//!
//! This is a Windows host: the ELF/Mach-O objects here are **never executed or
//! linked** by the test suite. The emitted DWARF is instead verified by decoding
//! it back with `gimli` — an independent, third-party DWARF reader that shares no
//! code with this emitter — and asserting the recovered address→line rows. See
//! `native_object_dwarf_tests.rs` and `documents/native_backend_contract.md`.

use super::{DebugOptions, LoweredNativeFunction, PlatformAbi};
use crate::object_model::{
    DwarfSection, ObjectModel, ObjectRelocation, ObjectRelocationKind, ObjectSection,
    ObjectSectionKind, ObjectSymbol, ObjectSymbolKind,
};

// -- DWARF constants ---------------------------------------------------------

/// The DWARF version emitted in every unit header (see the module docs).
const DWARF_VERSION: u16 = 4;
/// Target pointer width in bytes (x86-64).
const ADDRESS_SIZE: u8 = 8;

/// `DW_TAG_compile_unit`.
const DW_TAG_COMPILE_UNIT: u64 = 0x11;
/// `DW_TAG_subprogram`.
const DW_TAG_SUBPROGRAM: u64 = 0x2e;

/// `DW_CHILDREN_no` / `DW_CHILDREN_yes`.
const DW_CHILDREN_NO: u8 = 0;
const DW_CHILDREN_YES: u8 = 1;

/// Attribute codes used by this CU.
const DW_AT_NAME: u64 = 0x03;
const DW_AT_STMT_LIST: u64 = 0x10;
const DW_AT_LOW_PC: u64 = 0x11;
const DW_AT_HIGH_PC: u64 = 0x12;
const DW_AT_LANGUAGE: u64 = 0x13;
const DW_AT_COMP_DIR: u64 = 0x1b;
const DW_AT_PRODUCER: u64 = 0x25;
const DW_AT_DECL_FILE: u64 = 0x3a;
const DW_AT_DECL_LINE: u64 = 0x3b;

/// Form codes used by this CU.
const DW_FORM_ADDR: u64 = 0x01;
const DW_FORM_DATA2: u64 = 0x05;
const DW_FORM_DATA8: u64 = 0x07;
const DW_FORM_STRING: u64 = 0x08;
const DW_FORM_DATA1: u64 = 0x0b;
const DW_FORM_UDATA: u64 = 0x0f;
const DW_FORM_SEC_OFFSET: u64 = 0x17;

/// Abbreviation codes assigned by [`build_abbrev`].
const ABBREV_COMPILE_UNIT: u64 = 1;
const ABBREV_SUBPROGRAM: u64 = 2;

/// `DW_LANG_lo_user`. Lullaby has no DWARF-assigned language code, so the CU
/// declares one from the standard user range rather than misreporting itself as
/// another language. The language attribute does not participate in line-table
/// lookup, so this does not affect the delivered capability; it only means a
/// debugger will not apply a built-in expression grammar to Lullaby frames.
const DW_LANG_LO_USER: u16 = 0x8000;

/// The `DW_AT_producer` string.
const PRODUCER: &str = "lullaby native";

/// Standard line-program opcodes.
const DW_LNS_COPY: u8 = 0x01;
const DW_LNS_ADVANCE_PC: u8 = 0x02;
const DW_LNS_ADVANCE_LINE: u8 = 0x03;

/// Extended line-program opcodes (introduced by a `0x00` lead byte).
const DW_LNE_END_SEQUENCE: u8 = 0x01;
const DW_LNE_SET_ADDRESS: u8 = 0x02;

/// Line-program header tuning. `opcode_base` 13 is the DWARF 2-4 standard set;
/// `line_base`/`line_range` only affect how compactly special opcodes can encode
/// a row, and this emitter never uses special opcodes (it emits explicit
/// `advance_line` + `copy`), so these are the conventional values.
const LINE_BASE: i8 = -5;
const LINE_RANGE: u8 = 14;
const OPCODE_BASE: u8 = 13;
/// Operand counts for the 12 standard opcodes below `OPCODE_BASE`.
const STANDARD_OPCODE_LENGTHS: [u8; 12] = [0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1];

/// The 1-based `.debug_line` file-table index of the single source file. DWARF 4
/// file numbering starts at 1.
const SOURCE_FILE_INDEX: u64 = 1;

/// One compiled function's debug line entry: its `.text` symbol name, the byte
/// length of its machine code, and its 1-based `.lby` declaration line.
///
/// Line `0` is meaningful and intentional: synthesized closure bodies carry no
/// declaration line of their own, and DWARF defines line 0 as "no source line",
/// which is exactly the right statement to make about them.
pub(crate) struct DwarfFunctionLine {
    pub(crate) symbol: String,
    pub(crate) code_len: u64,
    pub(crate) line: u32,
}

/// Project the lowered functions onto the line entries the DWARF emitter needs:
/// each function's `.text` symbol, its code length, and its `.lby` declaration
/// line. This is the exact projection the COFF/CodeView path makes for its
/// `DEBUG_S_LINES` subsections, so both formats describe the same functions with
/// the same lines.
pub(crate) fn dwarf_function_lines(functions: &[LoweredNativeFunction]) -> Vec<DwarfFunctionLine> {
    functions
        .iter()
        .map(|function| DwarfFunctionLine {
            symbol: function.name.clone(),
            code_len: function.code.len() as u64,
            line: function.line,
        })
        .collect()
}

/// A relocation the DWARF sections need, described symbolically (by the name of
/// the `.text` function symbol, or by the debug section whose start is
/// referenced) so the caller can resolve it against the assembled model.
struct DwarfReloc {
    /// Byte offset of the field within its own section.
    offset: u64,
    target: DwarfRelocTarget,
    kind: ObjectRelocationKind,
}

/// What a [`DwarfReloc`] points at.
enum DwarfRelocTarget {
    /// The named `.text` function symbol.
    Function(String),
    /// The start of another debug section.
    SectionStart(DwarfSection),
}

/// Attach DWARF source-line debug info to an already-assembled `model`.
///
/// A no-op when `debug` is `None` — which is what keeps the default build
/// **byte-identical**: no debug section, symbol, or relocation is added, so the
/// ELF/Mach-O writers serialize exactly the bytes they did before.
///
/// `functions` must list every compiled function whose symbol is defined in the
/// model's `.text`; any entry with no matching `.text` symbol is skipped rather
/// than emitting a line row pointing at an address the linker cannot resolve.
pub(crate) fn attach_dwarf_line_info(
    model: &mut ObjectModel,
    debug: Option<&DebugOptions>,
    functions: &[DwarfFunctionLine],
    abi: PlatformAbi,
) {
    let Some(debug) = debug else {
        return;
    };

    // Only functions with a real `.text` symbol in this model can be described:
    // every line row anchors to a relocation against that symbol.
    let described: Vec<&DwarfFunctionLine> = functions
        .iter()
        .filter(|function| {
            model.symbols.iter().any(|symbol| {
                symbol.name == function.symbol
                    && symbol.section == Some(model.text_section_index())
                    && symbol.kind == ObjectSymbolKind::Function
            })
        })
        .collect();
    if described.is_empty() {
        // Nothing to describe (a library object with no compiled function).
        // Emitting an empty CU would be noise, so emit no DWARF at all.
        return;
    }

    let (line_bytes, line_relocs) = build_line_program(&debug.source_file, &described);
    let abbrev_bytes = build_abbrev();
    let (info_bytes, info_relocs) = build_info(&debug.source_file, &described);

    // Section order is fixed and independent of `abi` so both containers describe
    // the same DWARF.
    let sections = [
        (DwarfSection::Line, line_bytes, line_relocs),
        (DwarfSection::Abbrev, abbrev_bytes, Vec::new()),
        (DwarfSection::Info, info_bytes, info_relocs),
    ];

    // Reserve the model section indices first: `.debug_info`'s relocations
    // reference `.debug_line`/`.debug_abbrev`, which requires knowing their final
    // indices before any relocation is resolved.
    let first_debug_index = model.sections.len();
    let index_of = |wanted: DwarfSection| -> usize {
        first_debug_index
            + sections
                .iter()
                .position(|(kind, _, _)| *kind == wanted)
                .expect("every DWARF section this emitter references is in `sections`")
    };

    // ELF concatenates `.debug_abbrev`/`.debug_line` across objects, so
    // `.debug_info`'s offsets into them must be linker-rebased through
    // `STT_SECTION` symbols. Mach-O keeps DWARF per-object and needs none (see
    // `ObjectRelocationKind::SectionOffset32`).
    let section_symbols_needed = matches!(abi, PlatformAbi::Linux);
    let mut section_symbol_of: Vec<(DwarfSection, usize)> = Vec::new();
    if section_symbols_needed {
        for (kind, _, _) in &sections {
            // Only `.debug_line`/`.debug_abbrev` are ever referenced by offset.
            if matches!(kind, DwarfSection::Info) {
                continue;
            }
            let symbol_index = model.symbols.len();
            model.symbols.push(ObjectSymbol {
                name: kind.elf_name().to_string(),
                section: Some(index_of(*kind)),
                value: 0,
                kind: ObjectSymbolKind::Section,
            });
            section_symbol_of.push((*kind, symbol_index));
        }
    }

    for (kind, bytes, relocs) in sections {
        let relocations = relocs
            .into_iter()
            .filter_map(|reloc| {
                let symbol = match &reloc.target {
                    DwarfRelocTarget::Function(name) => model
                        .symbols
                        .iter()
                        .position(|symbol| &symbol.name == name)
                        .expect("described functions were filtered to defined `.text` symbols"),
                    DwarfRelocTarget::SectionStart(target) => {
                        // On Mach-O these relocations are intentionally absent;
                        // the field's as-written offset is already correct.
                        section_symbol_of
                            .iter()
                            .find(|(kind, _)| kind == target)
                            .map(|(_, index)| *index)?
                    }
                };
                Some(ObjectRelocation {
                    offset: reloc.offset,
                    symbol,
                    kind: reloc.kind,
                })
            })
            .collect();
        let size = bytes.len() as u64;
        model.sections.push(ObjectSection {
            kind: ObjectSectionKind::Debug(kind),
            data: bytes,
            size,
            relocations,
        });
    }
}

// -- `.debug_abbrev` ---------------------------------------------------------

/// Build the abbreviation table: one `DW_TAG_compile_unit` (with children) and
/// one `DW_TAG_subprogram` (a leaf), terminated by a zero abbrev code.
fn build_abbrev() -> Vec<u8> {
    let mut out = Vec::new();

    push_uleb(&mut out, ABBREV_COMPILE_UNIT);
    push_uleb(&mut out, DW_TAG_COMPILE_UNIT);
    out.push(DW_CHILDREN_YES);
    for (attribute, form) in [
        (DW_AT_PRODUCER, DW_FORM_STRING),
        (DW_AT_LANGUAGE, DW_FORM_DATA2),
        (DW_AT_NAME, DW_FORM_STRING),
        (DW_AT_COMP_DIR, DW_FORM_STRING),
        (DW_AT_STMT_LIST, DW_FORM_SEC_OFFSET),
    ] {
        push_uleb(&mut out, attribute);
        push_uleb(&mut out, form);
    }
    // A (0, 0) attribute pair ends an abbreviation declaration.
    push_uleb(&mut out, 0);
    push_uleb(&mut out, 0);

    push_uleb(&mut out, ABBREV_SUBPROGRAM);
    push_uleb(&mut out, DW_TAG_SUBPROGRAM);
    out.push(DW_CHILDREN_NO);
    for (attribute, form) in [
        (DW_AT_NAME, DW_FORM_STRING),
        (DW_AT_DECL_FILE, DW_FORM_DATA1),
        (DW_AT_DECL_LINE, DW_FORM_UDATA),
        (DW_AT_LOW_PC, DW_FORM_ADDR),
        // DWARF 4: a constant-class `DW_AT_high_pc` is an *offset* from
        // `DW_AT_low_pc`, not an address — so it needs no relocation.
        (DW_AT_HIGH_PC, DW_FORM_DATA8),
    ] {
        push_uleb(&mut out, attribute);
        push_uleb(&mut out, form);
    }
    push_uleb(&mut out, 0);
    push_uleb(&mut out, 0);

    // A zero abbrev code terminates the table.
    push_uleb(&mut out, 0);
    out
}

// -- `.debug_info` -----------------------------------------------------------

/// Build the single compile unit: the unit header, the CU DIE, and one
/// `DW_TAG_subprogram` child per function. Returns the bytes plus the
/// relocations for the abbrev/stmt_list offsets and each subprogram's `low_pc`.
fn build_info(source_file: &str, functions: &[&DwarfFunctionLine]) -> (Vec<u8>, Vec<DwarfReloc>) {
    let mut out = Vec::new();
    let mut relocs = Vec::new();

    // Unit header. `unit_length` counts every byte *after* itself and is
    // backpatched once the unit is complete.
    push_u32(&mut out, 0);
    let unit_start = out.len();
    push_u16(&mut out, DWARF_VERSION);
    relocs.push(DwarfReloc {
        offset: out.len() as u64,
        target: DwarfRelocTarget::SectionStart(DwarfSection::Abbrev),
        kind: ObjectRelocationKind::SectionOffset32,
    });
    push_u32(&mut out, 0); // debug_abbrev_offset (this object's only CU)
    out.push(ADDRESS_SIZE);

    // -- CU DIE.
    push_uleb(&mut out, ABBREV_COMPILE_UNIT);
    push_string(&mut out, PRODUCER);
    push_u16(&mut out, DW_LANG_LO_USER);
    push_string(&mut out, source_file);
    // `DW_AT_comp_dir`: the compiler records the source path as given on the
    // command line and does not resolve it against a working directory, so the
    // honest value is the empty string rather than a fabricated absolute path.
    push_string(&mut out, "");
    relocs.push(DwarfReloc {
        offset: out.len() as u64,
        target: DwarfRelocTarget::SectionStart(DwarfSection::Line),
        kind: ObjectRelocationKind::SectionOffset32,
    });
    push_u32(&mut out, 0); // DW_AT_stmt_list -> this object's line program

    // -- One subprogram DIE per compiled function.
    for function in functions {
        push_uleb(&mut out, ABBREV_SUBPROGRAM);
        push_string(&mut out, &function.symbol);
        out.push(SOURCE_FILE_INDEX as u8);
        push_uleb(&mut out, u64::from(function.line));
        relocs.push(DwarfReloc {
            offset: out.len() as u64,
            target: DwarfRelocTarget::Function(function.symbol.clone()),
            kind: ObjectRelocationKind::Absolute64,
        });
        push_u64(&mut out, 0); // DW_AT_low_pc (relocated)
        push_u64(&mut out, function.code_len); // DW_AT_high_pc (offset from low_pc)
    }

    // A zero abbrev code terminates the CU DIE's child list.
    push_uleb(&mut out, 0);

    let unit_length = (out.len() - unit_start) as u32;
    out[..4].copy_from_slice(&unit_length.to_le_bytes());
    (out, relocs)
}

// -- `.debug_line` -----------------------------------------------------------

/// Build the line-number program: the header (with the single-entry file table)
/// followed by one self-contained sequence per function. Returns the bytes plus
/// one `Absolute64` relocation per `DW_LNE_set_address` operand.
fn build_line_program(
    source_file: &str,
    functions: &[&DwarfFunctionLine],
) -> (Vec<u8>, Vec<DwarfReloc>) {
    let mut out = Vec::new();
    let mut relocs = Vec::new();

    // `unit_length`, backpatched below.
    push_u32(&mut out, 0);
    let unit_start = out.len();
    push_u16(&mut out, DWARF_VERSION);

    // `header_length`, backpatched: the byte count from just after this field to
    // the first byte of the line program.
    push_u32(&mut out, 0);
    let header_length_field = unit_start + 2;
    let header_start = out.len();

    out.push(1); // minimum_instruction_length
    out.push(1); // maximum_operations_per_instruction (DWARF 4; no VLIW here)
    out.push(1); // default_is_stmt: every row we emit is a statement
    out.push(LINE_BASE as u8);
    out.push(LINE_RANGE);
    out.push(OPCODE_BASE);
    out.extend_from_slice(&STANDARD_OPCODE_LENGTHS);

    // include_directories: none. A single zero byte terminates the list.
    out.push(0);

    // file_names: one entry, then a terminating zero byte. Each entry is the
    // name, then ULEB dir index / mtime / length (0 = unknown, which is the
    // truth here — the compiler does not stat the source).
    push_string(&mut out, source_file);
    push_uleb(&mut out, 0); // directory index
    push_uleb(&mut out, 0); // mtime
    push_uleb(&mut out, 0); // file length
    out.push(0);

    let header_length = (out.len() - header_start) as u32;
    out[header_length_field..header_length_field + 4].copy_from_slice(&header_length.to_le_bytes());

    // -- The program: one sequence per function (see the module docs).
    for function in functions {
        // DW_LNE_set_address <8-byte address>. Extended opcodes are a 0x00 lead
        // byte, a ULEB length covering the sub-opcode + operands, then the body.
        out.push(0x00);
        push_uleb(&mut out, 1 + u64::from(ADDRESS_SIZE));
        out.push(DW_LNE_SET_ADDRESS);
        relocs.push(DwarfReloc {
            offset: out.len() as u64,
            target: DwarfRelocTarget::Function(function.symbol.clone()),
            kind: ObjectRelocationKind::Absolute64,
        });
        push_u64(&mut out, 0);

        // The line register starts at 1, so reaching `function.line` is a signed
        // advance from 1. For a closure (line 0) this is a legitimate advance to
        // DWARF's "no source line".
        let advance = i64::from(function.line) - 1;
        if advance != 0 {
            out.push(DW_LNS_ADVANCE_LINE);
            push_sleb(&mut out, advance);
        }
        // Emit the row: (function entry address, function.line).
        out.push(DW_LNS_COPY);

        // Advance to one past the function's last byte and close the sequence, so
        // the row's address range covers exactly this function.
        if function.code_len != 0 {
            out.push(DW_LNS_ADVANCE_PC);
            push_uleb(&mut out, function.code_len);
        }
        out.push(0x00);
        push_uleb(&mut out, 1);
        out.push(DW_LNE_END_SEQUENCE);
    }

    let unit_length = (out.len() - unit_start) as u32;
    out[..4].copy_from_slice(&unit_length.to_le_bytes());
    (out, relocs)
}

// -- Encoding primitives -----------------------------------------------------

/// Append an unsigned LEB128.
fn push_uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            return;
        }
    }
}

/// Append a signed LEB128.
fn push_sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        // An arithmetic shift keeps the sign bit, so this terminates for both
        // signs once the remaining bits are all copies of the sign.
        value >>= 7;
        let sign_bit_set = byte & 0x40 != 0;
        let done = (value == 0 && !sign_bit_set) || (value == -1 && sign_bit_set);
        out.push(if done { byte } else { byte | 0x80 });
        if done {
            return;
        }
    }
}

/// Append a NUL-terminated string (`DW_FORM_string` / a line-table name).
fn push_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
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
#[path = "native_object_dwarf_tests.rs"]
mod tests;
