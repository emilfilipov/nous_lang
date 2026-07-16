//! Tests for the ELF/Mach-O DWARF source-line path (`native_object_dwarf.rs`).
//!
//! # Why these tests decode with `gimli` rather than assert bytes
//!
//! This is a Windows host: an ELF or Mach-O object cannot be linked or executed
//! here, so there is no run-it-and-check-the-exit-code proof available for this
//! surface (the same constraint the port-I/O tests document).
//!
//! Asserting the emitted bytes against hand-written expected bytes would be
//! *self-consistently wrong* if this emitter's understanding of DWARF were
//! wrong — the test would encode the same misunderstanding as the code. So the
//! evidence here is a **round trip through an independent decoder**: the object
//! is parsed by the `object` crate, its relocations are applied the way a linker
//! would, and the DWARF is read back by `gimli` — a third-party DWARF reader
//! that shares no code with `native_object_dwarf.rs`. The assertions are made on
//! what `gimli` recovers (the address→line rows, the CU DIE, the subprogram
//! DIEs), not on bytes this emitter chose.
//!
//! `gimli` is the same DWARF reader that backs `addr2line` and Rust's own
//! backtrace symbolization, so "gimli reads it correctly" is a strong proxy for
//! "a debugger reads it correctly". It is not a substitute for a live `gdb`
//! session on Linux, which remains deferred to the cross-platform CI.

use super::*;
use crate::{
    BytecodeModule, emit_native_program_for_target, lower, lower_to_bytecode,
    native_contract::{
        NativeTarget, x86_64_linux_target, x86_64_macos_target, x86_64_windows_target,
    },
};
use gimli::{EndianSlice, LittleEndian};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate_executable;
use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};
use std::collections::HashMap;

/// Compile source through the full frontend into a `BytecodeModule`.
fn module_for(source: &str) -> BytecodeModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate_executable(&program).expect("semantic");
    let ir = lower(&checked).expect("lower");
    lower_to_bytecode(&ir)
}

/// A two-function program whose declaration lines are known exactly. `add` is
/// declared on line 1 and `main` on line 4 (1-based), which is what the line
/// table must report for their entry addresses.
const TWO_FUNCTIONS: &str = concat!(
    "fn add x, y i64 -> i64\n",
    "    return x + y\n",
    "\n",
    "fn main -> i64\n",
    "    return add(20, 22)\n",
);

/// The source path recorded in the debug info.
const SOURCE_PATH: &str = "example/two_functions.lby";

fn debug_options() -> DebugOptions {
    DebugOptions {
        source_file: SOURCE_PATH.to_string(),
    }
}

/// Emit the object for `target`, with or without debug info.
fn emit(source: &str, target: &NativeTarget, debug: Option<&DebugOptions>) -> Vec<u8> {
    let program = emit_native_program_for_target(&module_for(source), target, debug, false)
        .expect("emit native program");
    assert!(
        program.skipped.is_empty(),
        "no function should be skipped: {:?}",
        program.skipped
    );
    program.bytes
}

// -- Independent decode ------------------------------------------------------

/// Read `name`'s section bytes out of a **relocatable** object with its
/// relocations applied, exactly as a linker would resolve them.
///
/// This is what makes the decode meaningful. In a `.o` the DWARF address fields
/// are zero placeholders and the real value lives in the relocation, so a decode
/// of the raw bytes would report every function at address 0 and prove nothing
/// about the relocations. Applying them here means the recovered line-table
/// addresses are the addresses a linker would actually produce.
fn relocated_section(file: &object::File, name: &str) -> Vec<u8> {
    let Some(section) = file.section_by_name(name) else {
        return Vec::new();
    };
    let mut data = section.data().expect("section data").to_vec();
    for (offset, relocation) in section.relocations() {
        let target = match relocation.target() {
            RelocationTarget::Symbol(index) => file
                .symbol_by_index(index)
                .expect("relocation names a symbol in the table")
                .address(),
            RelocationTarget::Section(index) => file
                .section_by_index(index)
                .expect("relocation names a section")
                .address(),
            other => panic!("unexpected relocation target {other:?}"),
        };
        let offset = offset as usize;
        // Mach-O carries the addend in the field itself; ELF carries it in the
        // relocation record. Handle both so one decoder serves both formats.
        let implicit = if relocation.has_implicit_addend() {
            match relocation.size() {
                64 => i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()),
                32 => i64::from(i32::from_le_bytes(
                    data[offset..offset + 4].try_into().unwrap(),
                )),
                size => panic!("unexpected implicit-addend relocation size {size}"),
            }
        } else {
            relocation.addend()
        };
        let value = target.wrapping_add(implicit as u64);
        match relocation.size() {
            64 => data[offset..offset + 8].copy_from_slice(&value.to_le_bytes()),
            32 => data[offset..offset + 4].copy_from_slice(&(value as u32).to_le_bytes()),
            size => panic!("unexpected relocation size {size}"),
        }
    }
    data
}

/// One recovered line-table row.
#[derive(Debug, PartialEq, Eq)]
struct DecodedRow {
    address: u64,
    line: u64,
    file: String,
    end_sequence: bool,
}

/// A recovered `DW_TAG_subprogram`.
#[derive(Debug, PartialEq, Eq)]
struct DecodedSubprogram {
    name: String,
    low_pc: u64,
    high_pc: u64,
    decl_line: u64,
}

/// Everything the decoder recovered from one object's DWARF.
struct Decoded {
    rows: Vec<DecodedRow>,
    subprograms: Vec<DecodedSubprogram>,
    cu_name: String,
    producer: String,
}

/// Section-name mapping: ELF uses `.debug_line`, Mach-O uses `__debug_line`.
fn section_name_for(file: &object::File, id: gimli::SectionId) -> String {
    match file.format() {
        object::BinaryFormat::MachO => id.name().replacen('.', "__", 1),
        _ => id.name().to_string(),
    }
}

/// Parse `bytes` as an object and decode its DWARF with `gimli`.
fn decode_dwarf(bytes: &[u8]) -> Decoded {
    let file = object::File::parse(bytes).expect("parse object");
    // Every section the object actually has, with relocations applied, keyed by
    // its container name. A DWARF section `gimli` asks for that this emitter does
    // not produce (`.debug_str`, `.debug_ranges`, ...) resolves to empty, which is
    // how `gimli` expects an absent section to be reported.
    let loaded: HashMap<String, Vec<u8>> = file
        .sections()
        .filter_map(|section| section.name().ok().map(str::to_string))
        .map(|name| {
            let data = relocated_section(&file, &name);
            (name, data)
        })
        .collect();
    let empty: Vec<u8> = Vec::new();
    let dwarf = gimli::Dwarf::load(
        |id| -> Result<EndianSlice<'_, LittleEndian>, gimli::Error> {
            let name = section_name_for(&file, id);
            Ok(EndianSlice::new(
                loaded.get(&name).unwrap_or(&empty),
                LittleEndian,
            ))
        },
    )
    .expect("load dwarf");

    let mut units = dwarf.units();
    let header = units
        .next()
        .expect("iterate units")
        .expect("the object has exactly one compile unit");
    let unit = dwarf.unit(header).expect("parse compile unit");
    assert!(
        units.next().expect("iterate units").is_none(),
        "the emitter produces exactly one compile unit per object"
    );

    // -- The CU DIE and its subprogram children.
    let mut cu_name = String::new();
    let mut producer = String::new();
    let mut subprograms = Vec::new();
    let mut entries = unit.entries();
    while let Some(entry) = entries.next_dfs().expect("walk DIEs") {
        let string_of = |attr: gimli::DwAt| -> Option<String> {
            let value = entry.attr_value(attr)?;
            Some(
                dwarf
                    .attr_string(&unit, value)
                    .expect("attribute is a string")
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        let udata_of = |attr: gimli::DwAt| -> Option<u64> { entry.attr_value(attr)?.udata_value() };
        match entry.tag() {
            gimli::DW_TAG_compile_unit => {
                cu_name = string_of(gimli::DW_AT_name).expect("CU has DW_AT_name");
                producer = string_of(gimli::DW_AT_producer).expect("CU has DW_AT_producer");
            }
            gimli::DW_TAG_subprogram => {
                let low_pc = match entry.attr_value(gimli::DW_AT_low_pc) {
                    Some(gimli::AttributeValue::Addr(address)) => address,
                    other => panic!("subprogram DW_AT_low_pc must be an address, got {other:?}"),
                };
                subprograms.push(DecodedSubprogram {
                    name: string_of(gimli::DW_AT_name).expect("subprogram has DW_AT_name"),
                    low_pc,
                    high_pc: udata_of(gimli::DW_AT_high_pc).expect("subprogram has DW_AT_high_pc"),
                    decl_line: udata_of(gimli::DW_AT_decl_line).expect("subprogram has decl_line"),
                });
            }
            _ => {}
        }
    }

    // -- The line program: run it and collect every row.
    let program = unit
        .line_program
        .clone()
        .expect("the CU's DW_AT_stmt_list resolves to a line program");
    let mut rows = Vec::new();
    let mut state = program.rows();
    while let Some((header, row)) = state.next_row().expect("run the line program") {
        let file = row
            .file(header)
            .map(|file| {
                dwarf
                    .attr_string(&unit, file.path_name())
                    .expect("file name is a string")
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        rows.push(DecodedRow {
            address: row.address(),
            line: row.line().map(|line| line.get()).unwrap_or(0),
            file,
            end_sequence: row.end_sequence(),
        });
    }

    Decoded {
        rows,
        subprograms,
        cu_name,
        producer,
    }
}

/// The `.text` offset of `name` in a relocatable object, read from its symbol
/// table — the ground truth the line table's address must agree with.
fn symbol_address(bytes: &[u8], name: &str) -> u64 {
    let file = object::File::parse(bytes).expect("parse object");
    file.symbols()
        .find(|symbol| symbol.name() == Ok(name))
        .unwrap_or_else(|| panic!("object defines a `{name}` symbol"))
        .address()
}

// -- The headline: an independent decoder recovers the right lines ------------

/// The core claim of this feature, on ELF: after relocation, `gimli` reports each
/// compiled function's entry address as mapping to its `.lby` declaration line.
#[test]
fn elf_line_table_maps_each_function_entry_to_its_source_line() {
    let bytes = emit(
        TWO_FUNCTIONS,
        &x86_64_linux_target(),
        Some(&debug_options()),
    );
    let decoded = decode_dwarf(&bytes);

    for (function, expected_line) in [("add", 1), ("main", 4)] {
        let address = symbol_address(&bytes, function);
        let row = decoded
            .rows
            .iter()
            .find(|row| row.address == address && !row.end_sequence)
            .unwrap_or_else(|| {
                panic!(
                    "no line row at `{function}`'s entry {address:#x}: {:?}",
                    decoded.rows
                )
            });
        assert_eq!(
            row.line, expected_line,
            "`{function}` entry must map to its declaration line"
        );
        assert_eq!(
            row.file, SOURCE_PATH,
            "row must name the `.lby` source file"
        );
    }
}

/// The same claim on Mach-O, decoded through the same independent reader.
#[test]
fn macho_line_table_maps_each_function_entry_to_its_source_line() {
    let bytes = emit(
        TWO_FUNCTIONS,
        &x86_64_macos_target(),
        Some(&debug_options()),
    );
    let decoded = decode_dwarf(&bytes);

    for (function, expected_line) in [("add", 1), ("main", 4)] {
        let address = symbol_address(&bytes, function);
        let row = decoded
            .rows
            .iter()
            .find(|row| row.address == address && !row.end_sequence)
            .unwrap_or_else(|| {
                panic!(
                    "no line row at `{function}`'s entry {address:#x}: {:?}",
                    decoded.rows
                )
            });
        assert_eq!(
            row.line, expected_line,
            "`{function}` entry must map to its declaration line"
        );
        assert_eq!(
            row.file, SOURCE_PATH,
            "row must name the `.lby` source file"
        );
    }
}

/// Every sequence must be terminated, and each function's row range must cover
/// exactly its own code — an unterminated or overlapping sequence is what makes
/// a debugger report the wrong line (or reject the table).
#[test]
fn each_function_is_a_terminated_sequence_covering_exactly_its_code() {
    let bytes = emit(
        TWO_FUNCTIONS,
        &x86_64_linux_target(),
        Some(&debug_options()),
    );
    let decoded = decode_dwarf(&bytes);

    // One row + one end_sequence per compiled function.
    let ends = decoded.rows.iter().filter(|row| row.end_sequence).count();
    let starts = decoded.rows.iter().filter(|row| !row.end_sequence).count();
    assert_eq!(starts, ends, "every sequence must be terminated");

    for subprogram in &decoded.subprograms {
        let end = subprogram.low_pc + subprogram.high_pc;
        assert!(
            decoded
                .rows
                .iter()
                .any(|row| row.end_sequence && row.address == end),
            "`{}`'s sequence must end one past its last byte ({end:#x}): {:?}",
            subprogram.name,
            decoded.rows
        );
    }
}

/// The CU DIE must carry the source file and producer, and one subprogram per
/// compiled function whose `low_pc`/`high_pc` match the symbol table. This is
/// what lets a debugger resolve an address to a function at all.
#[test]
fn compile_unit_describes_every_function_with_matching_addresses() {
    let bytes = emit(
        TWO_FUNCTIONS,
        &x86_64_linux_target(),
        Some(&debug_options()),
    );
    let decoded = decode_dwarf(&bytes);

    assert_eq!(decoded.cu_name, SOURCE_PATH);
    assert_eq!(decoded.producer, "lullaby native");

    for (function, expected_line) in [("add", 1), ("main", 4)] {
        let subprogram = decoded
            .subprograms
            .iter()
            .find(|subprogram| subprogram.name == function)
            .unwrap_or_else(|| panic!("CU must describe `{function}`"));
        assert_eq!(
            subprogram.low_pc,
            symbol_address(&bytes, function),
            "`{function}`'s DW_AT_low_pc must match its symbol address"
        );
        assert_eq!(
            subprogram.decl_line, expected_line,
            "`{function}`'s DW_AT_decl_line must be its declaration line"
        );
        assert!(subprogram.high_pc > 0, "`{function}` must have a code size");
    }
}

/// A relocation that names the wrong symbol would still decode to *a* plausible
/// address, so pin the addresses to distinct, non-zero values: this catches an
/// emitter that relocated every row against the same symbol (or none).
#[test]
fn each_function_row_relocates_to_its_own_distinct_address() {
    let bytes = emit(
        TWO_FUNCTIONS,
        &x86_64_linux_target(),
        Some(&debug_options()),
    );
    let decoded = decode_dwarf(&bytes);

    let add = symbol_address(&bytes, "add");
    let main = symbol_address(&bytes, "main");
    assert_ne!(add, main, "the two functions must be at distinct offsets");
    assert!(
        decoded.subprograms.iter().any(|s| s.low_pc == add)
            && decoded.subprograms.iter().any(|s| s.low_pc == main),
        "each subprogram must relocate to its own function: {:?}",
        decoded.subprograms
    );
}

// -- Opt-in: no `--debug`, no DWARF ------------------------------------------

/// Without `--debug` the object must carry no DWARF at all — no section, and
/// therefore no relocation or symbol for one. This is the structural half of the
/// byte-identity claim; `elf_and_macho_bytes_are_identical_without_debug` below
/// pins the bytes themselves.
#[test]
fn no_debug_sections_are_emitted_without_debug_options() {
    for target in [x86_64_linux_target(), x86_64_macos_target()] {
        let bytes = emit(TWO_FUNCTIONS, &target, None);
        let file = object::File::parse(&bytes[..]).expect("parse object");
        let debug_sections: Vec<String> = file
            .sections()
            .filter_map(|section| section.name().ok().map(str::to_string))
            .filter(|name| name.contains("debug"))
            .collect();
        assert!(
            debug_sections.is_empty(),
            "a non-debug build must emit no debug section, found {debug_sections:?}"
        );
    }
}

/// The `--debug` path must be **purely additive**: everything a non-debug object
/// contains must be byte-identical inside a debug object. Proven by checking that
/// the `.text` bytes and the full symbol set are unchanged, and that the only
/// difference is added debug sections.
#[test]
fn debug_info_is_additive_and_leaves_text_unchanged() {
    for target in [x86_64_linux_target(), x86_64_macos_target()] {
        let plain = emit(TWO_FUNCTIONS, &target, None);
        let debugged = emit(TWO_FUNCTIONS, &target, Some(&debug_options()));

        let plain_file = object::File::parse(&plain[..]).expect("parse object");
        let debug_file = object::File::parse(&debugged[..]).expect("parse object");

        let text_of = |file: &object::File| -> Vec<u8> {
            file.sections()
                .find(|section| matches!(section.name(), Ok(".text") | Ok("__text")))
                .expect("object has a text section")
                .data()
                .expect("text data")
                .to_vec()
        };
        assert_eq!(
            text_of(&plain_file),
            text_of(&debug_file),
            "`--debug` must not change a single byte of machine code"
        );

        let names = |file: &object::File| -> Vec<String> {
            file.symbols()
                .filter_map(|symbol| symbol.name().ok().map(str::to_string))
                .filter(|name| !name.is_empty())
                .collect()
        };
        let plain_names = names(&plain_file);
        for name in plain_names {
            assert!(
                names(&debug_file).contains(&name),
                "`--debug` must not drop the `{name}` symbol"
            );
        }
    }
}

/// The COFF/CodeView path must be untouched by the DWARF work: a Windows object
/// still gets its `.debug$S` section and no DWARF section.
#[test]
fn coff_still_emits_codeview_and_never_dwarf() {
    let bytes = emit(
        TWO_FUNCTIONS,
        &x86_64_windows_target(),
        Some(&debug_options()),
    );
    let file = object::File::parse(&bytes[..]).expect("parse object");
    let names: Vec<String> = file
        .sections()
        .filter_map(|section| section.name().ok().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|name| name == ".debug$S"),
        "the COFF target keeps its CodeView section: {names:?}"
    );
    assert!(
        !names.iter().any(|name| name.starts_with(".debug_")),
        "the COFF target must not gain DWARF sections: {names:?}"
    );
}

// -- Relocation shape --------------------------------------------------------

/// ELF must rebase `.debug_info`'s offsets into `.debug_abbrev`/`.debug_line`
/// through section symbols, or a link against another DWARF-bearing object would
/// silently point this CU at the other object's tables. Mach-O must not: `ld64`
/// keeps DWARF per-object, so the offsets stay object-local.
#[test]
fn elf_relocates_debug_info_section_offsets_and_macho_does_not() {
    let elf = emit(
        TWO_FUNCTIONS,
        &x86_64_linux_target(),
        Some(&debug_options()),
    );
    let elf_file = object::File::parse(&elf[..]).expect("parse elf");
    let info = elf_file
        .section_by_name(".debug_info")
        .expect("elf has .debug_info");
    let section_offset_relocs = info
        .relocations()
        .filter(|(_, reloc)| reloc.size() == 32)
        .count();
    assert_eq!(
        section_offset_relocs, 2,
        "`.debug_info` must relocate both its abbrev offset and DW_AT_stmt_list"
    );

    let macho = emit(
        TWO_FUNCTIONS,
        &x86_64_macos_target(),
        Some(&debug_options()),
    );
    let macho_file = object::File::parse(&macho[..]).expect("parse macho");
    let macho_info = macho_file
        .section_by_name("__debug_info")
        .expect("macho has __debug_info");
    assert!(
        macho_info
            .relocations()
            .all(|(_, reloc)| reloc.size() == 64),
        "Mach-O `__debug_info` carries only 64-bit address relocations"
    );
}

/// Each `DW_LNE_set_address` must be an 8-byte absolute relocation against the
/// function's own symbol — the encoding a linker needs to place the row.
#[test]
fn line_program_addresses_are_absolute64_relocations_against_functions() {
    let bytes = emit(
        TWO_FUNCTIONS,
        &x86_64_linux_target(),
        Some(&debug_options()),
    );
    let file = object::File::parse(&bytes[..]).expect("parse object");
    let line = file
        .section_by_name(".debug_line")
        .expect("object has .debug_line");
    let targets: Vec<String> = line
        .relocations()
        .map(|(_, reloc)| {
            assert_eq!(reloc.size(), 64, "set_address operands are 8 bytes");
            assert_eq!(reloc.addend(), 0, "an absolute address needs no addend");
            match reloc.target() {
                RelocationTarget::Symbol(index) => file
                    .symbol_by_index(index)
                    .expect("symbol")
                    .name()
                    .expect("symbol name")
                    .to_string(),
                other => panic!("expected a symbol target, got {other:?}"),
            }
        })
        .collect();
    assert_eq!(
        targets,
        vec!["add".to_string(), "main".to_string()],
        "one set_address per compiled function, against that function"
    );
}

// -- Edge cases --------------------------------------------------------------

/// A single-function program still produces a well-formed, decodable CU. Guards
/// the smallest possible line program.
#[test]
fn single_function_program_decodes() {
    let source = "fn main -> i64\n    return 7\n";
    let bytes = emit(source, &x86_64_linux_target(), Some(&debug_options()));
    let decoded = decode_dwarf(&bytes);
    assert_eq!(decoded.subprograms.len(), 1);
    assert_eq!(decoded.subprograms[0].name, "main");
    assert_eq!(decoded.subprograms[0].decl_line, 1);
}

/// A function declared far down the file must report its real line, not a
/// truncated or defaulted one. This also exercises a multi-byte LEB128 line
/// advance, which a single-byte-only encoder would get wrong.
#[test]
fn a_large_line_number_survives_leb128_encoding() {
    let padding = "\n".repeat(200);
    let source = format!("{padding}fn main -> i64\n    return 1\n");
    let bytes = emit(&source, &x86_64_linux_target(), Some(&debug_options()));
    let decoded = decode_dwarf(&bytes);
    assert_eq!(
        decoded.subprograms[0].decl_line, 201,
        "the declaration line must survive LEB128 round-tripping"
    );
    let address = symbol_address(&bytes, "main");
    let row = decoded
        .rows
        .iter()
        .find(|row| row.address == address && !row.end_sequence)
        .expect("a row at main's entry");
    assert_eq!(row.line, 201);
}

/// The line program's own encoding primitives, checked against the DWARF spec's
/// worked cases rather than against this emitter's output.
#[test]
fn leb128_encodings_match_the_dwarf_spec_examples() {
    // Unsigned (DWARF 5, Appendix C).
    for (value, expected) in [
        (2u64, vec![2u8]),
        (127, vec![127]),
        (128, vec![0x80, 1]),
        (129, vec![0x81, 1]),
        (12857, vec![0xB9, 0x64]),
    ] {
        let mut out = Vec::new();
        push_uleb(&mut out, value);
        assert_eq!(out, expected, "ULEB128 of {value}");
    }
    // Signed.
    for (value, expected) in [
        (2i64, vec![2u8]),
        (-2, vec![0x7e]),
        (127, vec![0xFF, 0]),
        (-127, vec![0x81, 0x7F]),
        (128, vec![0x80, 1]),
        (-128, vec![0x80, 0x7F]),
    ] {
        let mut out = Vec::new();
        push_sleb(&mut out, value);
        assert_eq!(out, expected, "SLEB128 of {value}");
    }
}
