//! Object-file writers for the native backend: the COFF (Windows) writer and
//! the neutral object model consumed by the ELF/Mach-O emitters, plus the
//! heap-helper set assembly and the CodeView debug-section builder. Split out of
//! native_object.rs; sees the layout types and helper emitters via `use super::*`.

use super::*;

/// What entry stub a program's object/image gets, and how that stub derives the
/// process exit code from `main`.
///
/// This replaces a bare `has_main: bool`. That bool was keyed on the function
/// **name alone** and told the four stub emitters nothing about `main`'s return
/// shape, so every one of them unconditionally read `eax` as the exit code. For a
/// **void `main`** that is a real miscompile: `rax` is undefined on return from a
/// void function, so the process exited with whatever the body happened to leave
/// there (`fn main -> void` after a call returning 77 exited **77**; the
/// interpreters exit **0**). `main` is the one void function whose "no value" is
/// externally observable — the stub *is* a caller of it, and the void contract
/// says a caller must not read `rax`.
///
/// Making the distinction a type rather than a bool means a stub emitter cannot
/// read the exit code without first saying which `main` it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryStub {
    /// No stub: a library object with no `main` (a C `main` links against its
    /// exported symbols), so no `ExitProcess`/`exit` dependency is introduced.
    None,
    /// `main` returns a value: its result is in `rax` and becomes the exit code.
    MainValue,
    /// `main` returns NOTHING. `rax` is **undefined** on return, so the stub must
    /// not read it — the process exits **0**, matching all three interpreters.
    MainVoid,
}

impl EntryStub {
    /// Whether an entry stub is emitted at all (both `main` shapes emit one).
    pub(crate) fn emits(self) -> bool {
        !matches!(self, EntryStub::None)
    }

    /// Classify the program's entry from the module's `main`, if it compiled.
    /// `lowered` is consulted for presence (only a *compiled* `main` gets a stub)
    /// and the module for its declared return shape — the bool this replaces
    /// checked only the former.
    pub(crate) fn classify(
        lowered: &[LoweredNativeFunction],
        module: &BytecodeModule,
    ) -> EntryStub {
        if !lowered.iter().any(|f| f.name == "main") {
            return EntryStub::None;
        }
        // A compiled `main` always originates from a module function, so the
        // lookup succeeds. If it somehow did not, treating it as value-returning
        // reproduces the historical stub exactly.
        match module.functions.iter().find(|f| f.name == "main") {
            Some(f) if f.return_type.is_void() => EntryStub::MainVoid,
            _ => EntryStub::MainValue,
        }
    }
}

// -- COFF object writer (multi-function, relocations, imports) ---------------
//
// The object has a single `.text` section holding the entry stub followed by
// every compiled function. External symbols name each function and the imported
// `ExitProcess`; REL32 relocations bind inter-function calls, the entry stub's
// call to `main`, and the entry stub's call to `ExitProcess`. Long symbol names
// (> 8 bytes) are stored in a string table, as COFF requires.

/// Write the COFF object for a native program. Programs that reference no string
/// constants keep the original single-`.text` layout byte-for-byte; programs
/// that do use string constants get the extended layout with `.rdata` (the
/// constants), `.bss` (the heap region + bump pointer), and the heap helper
/// functions. Splitting keeps the string-free path — and its structural tests —
/// unchanged.
pub(crate) fn write_native_program_object(
    functions: &[LoweredNativeFunction],
    strings: &StringPool,
    entry_stub: EntryStub,
    debug: Option<&DebugOptions>,
) -> Vec<u8> {
    // The heap path (bump allocator + `.bss` region + helpers) is needed when the
    // program interns string constants OR references any growable-list/map or
    // string runtime helper. A program using none keeps the exact prior text-only
    // layout.
    if strings.is_empty() && !program_uses_heap_helpers(functions) {
        write_text_only_object(functions, entry_stub, debug)
    } else {
        write_object_with_data(functions, strings, entry_stub, debug)
    }
}

/// Emit the entry stub's exit-code setup into `reg`-in-`ecx` form (the Win64
/// `ExitProcess` argument register).
///
/// A **value-returning** `main` leaves its result in `rax`: `mov ecx, eax`.
/// A **void** `main` leaves `rax` UNDEFINED — reading it would leak whatever the
/// body last computed as the process exit code (the observed miscompile: exit 77
/// where the interpreters exit 0). So the stub zeroes `ecx` instead and never
/// reads `rax`, which is exactly what the void contract requires of a caller, and
/// which covers every return path of `main` at once (fallthrough, an explicit
/// `return`, and a `return` nested in a branch or loop).
fn emit_exit_code_into_ecx(text: &mut Vec<u8>, entry_stub: EntryStub) {
    match entry_stub {
        EntryStub::MainVoid => text.extend_from_slice(&[0x31, 0xC9]), // xor ecx, ecx
        _ => text.extend_from_slice(&[0x89, 0xC1]),                   // mov ecx, eax
    }
}

/// The ELF entry-point symbol. Linux's default linker entry is `_start`, so a
/// plain `ld`/`clang` link of the emitted object finds it without extra flags.
const ELF_ENTRY_SYMBOL: &str = "_start";

/// The Mach-O entry-point symbol. The macOS linker's default entry is `start`.
const MACHO_ENTRY_SYMBOL: &str = "start";

/// The freestanding platform ABI a non-Windows object targets: it selects the
/// entry-point symbol and the entry stub's process-exit mechanism (a raw `exit`
/// syscall) so the object needs no libc, mirroring the freestanding COFF path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlatformAbi {
    /// Linux System V: `_start`, `exit` via `syscall` rax=60.
    Linux,
    /// macOS: `start`, `exit` via `syscall` rax=0x2000001.
    MacOs,
}

impl PlatformAbi {
    /// The entry-point symbol name for this platform.
    fn entry_symbol(self) -> &'static str {
        match self {
            PlatformAbi::Linux => ELF_ENTRY_SYMBOL,
            PlatformAbi::MacOs => MACHO_ENTRY_SYMBOL,
        }
    }
}

/// A relocation carrying the target *symbol name*; resolved to a symbol index
/// once the neutral model's symbol table is assembled.
struct NamedTextReloc {
    offset: u64,
    symbol: String,
}

/// Build the target-neutral [`ObjectModel`] for a native program, used by the
/// ELF and Mach-O writers. It assembles exactly the same `.text` (functions +
/// heap/string helpers), `.rodata` (string constants), and `.bss` (bump heap)
/// content as the COFF path, but leads `.text` with a *freestanding* entry stub
/// that exits through a raw `exit` syscall instead of `kernel32!ExitProcess` —
/// the only machine-code difference between the platforms. The shared internal
/// calling convention is kept unchanged (see `documents/native_backend_contract.md`).
///
/// When `entry_stub` emits, the object is a runnable program led by the entry
/// stub; for [`EntryStub::None`] it is a library object (no `main`) with no entry
/// stub and no exit dependency. A void `main` ([`EntryStub::MainVoid`]) zeroes the
/// exit-code register instead of reading `eax`, which is undefined after it.
pub(crate) fn build_object_model(
    functions: &[LoweredNativeFunction],
    strings: &StringPool,
    entry_stub: EntryStub,
    abi: PlatformAbi,
) -> ObjectModel {
    let use_heap = !strings.is_empty() || program_uses_heap_helpers(functions);

    // -- Assemble `.text`: entry stub, functions, then heap/string helpers ---
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<NamedTextReloc> = Vec::new();

    if entry_stub.emits() {
        // Freestanding entry stub. `sub rsp, 32` reserves the internal ABI's
        // shadow space and lands `rsp` 16-byte aligned for the `call main`
        // (the OS enters `_start`/`start` with a 16-aligned stack). After `main`
        // returns its exit code in `eax`, the stub moves it to `edi` and issues
        // the platform `exit` syscall, so the object needs no libc. A VOID `main`
        // leaves `rax` undefined, so `edi` is zeroed instead of read from `eax`
        // (the SysV mirror of the Win64 `xor ecx, ecx`); the process exits 0.
        text.extend_from_slice(&[0x48, 0x83, 0xEC, 0x20]); // sub rsp, 32
        text.push(0xE8); // call main (rel32)
        relocations.push(NamedTextReloc {
            offset: text.len() as u64,
            symbol: "main".to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        match entry_stub {
            EntryStub::MainVoid => text.extend_from_slice(&[0x31, 0xFF]), // xor edi, edi
            _ => text.extend_from_slice(&[0x89, 0xC7]),                   // mov edi, eax
        }
        match abi {
            PlatformAbi::Linux => {
                // mov eax, 60 (SYS_exit); syscall.
                text.extend_from_slice(&[0xB8, 0x3C, 0x00, 0x00, 0x00]);
            }
            PlatformAbi::MacOs => {
                // mov eax, 0x2000001 (BSD `exit`); syscall.
                text.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x02]);
            }
        }
        text.extend_from_slice(&[0x0F, 0x05]); // syscall
        text.push(0xCC); // int3 (unreachable; exit does not return)
    }

    let mut func_offsets: HashMap<String, u64> = HashMap::new();
    let mut append_code = |text: &mut Vec<u8>,
                           relocations: &mut Vec<NamedTextReloc>,
                           name: &str,
                           code: &[u8],
                           relocs: &[CodeRelocation]| {
        while !text.len().is_multiple_of(16) {
            text.push(0xCC);
        }
        let start = text.len() as u64;
        func_offsets.insert(name.to_string(), start);
        for reloc in relocs {
            relocations.push(NamedTextReloc {
                offset: start + u64::from(reloc.offset),
                symbol: reloc.symbol.clone(),
            });
        }
        text.extend_from_slice(code);
    };

    for function in functions {
        append_code(
            &mut text,
            &mut relocations,
            &function.name,
            &function.code,
            &function.relocations,
        );
    }

    // The heap/string runtime helpers are emitted as one fixed set (identical to
    // the COFF data path) whenever the heap is used, so the symbol set matches.
    if use_heap {
        for helper in heap_runtime_helpers() {
            append_code(
                &mut text,
                &mut relocations,
                &helper.name,
                &helper.code,
                &helper.relocations,
            );
        }
    }

    // -- `.rodata`: NUL-terminated string constants --------------------------
    let mut rdata: Vec<u8> = Vec::new();
    let mut str_offsets: Vec<u64> = Vec::new();
    if use_heap {
        for text_value in &strings.entries {
            str_offsets.push(rdata.len() as u64);
            rdata.extend_from_slice(text_value.as_bytes());
            rdata.push(0);
        }
    }

    // -- Sections ------------------------------------------------------------
    // `.text` is section 0. `.rodata` (1) and `.bss` (2) follow when the heap is
    // used, matching the COFF data layout.
    let text_len = text.len() as u64;
    let mut sections: Vec<ObjectSection> = vec![ObjectSection {
        kind: ObjectSectionKind::Text,
        data: text,
        size: text_len,
        relocations: Vec::new(),
    }];
    let (rodata_section, bss_section) = if use_heap {
        let rdata_len = rdata.len() as u64;
        sections.push(ObjectSection {
            kind: ObjectSectionKind::ReadOnlyData,
            data: rdata,
            size: rdata_len,
            relocations: Vec::new(),
        });
        sections.push(ObjectSection {
            kind: ObjectSectionKind::Bss,
            data: Vec::new(),
            size: u64::from(HEAP_BSS_SIZE),
            relocations: Vec::new(),
        });
        (Some(1usize), Some(2usize))
    } else {
        (None, None)
    };

    // -- Symbols (defined first, then undefined externs) ---------------------
    let mut symbols: Vec<ObjectSymbol> = Vec::new();
    if entry_stub.emits() {
        symbols.push(ObjectSymbol {
            name: abi.entry_symbol().to_string(),
            section: Some(0),
            value: 0,
            kind: ObjectSymbolKind::Function,
        });
    }
    for function in functions {
        symbols.push(ObjectSymbol {
            name: function.name.clone(),
            section: Some(0),
            value: func_offsets[&function.name],
            kind: ObjectSymbolKind::Function,
        });
    }
    if use_heap {
        for helper in heap_runtime_helpers() {
            symbols.push(ObjectSymbol {
                name: helper.name.clone(),
                section: Some(0),
                value: func_offsets[&helper.name],
                kind: ObjectSymbolKind::Function,
            });
        }
        for (index, offset) in str_offsets.iter().enumerate() {
            symbols.push(ObjectSymbol {
                name: format!("__str{index}"),
                section: rodata_section,
                value: *offset,
                kind: ObjectSymbolKind::Data,
            });
        }
        symbols.push(ObjectSymbol {
            name: HEAP_NEXT_SYMBOL.to_string(),
            section: bss_section,
            value: 0,
            kind: ObjectSymbolKind::Data,
        });
        symbols.push(ObjectSymbol {
            name: HEAP_FREE_HEAD_SYMBOL.to_string(),
            section: bss_section,
            value: 8,
            kind: ObjectSymbolKind::Data,
        });
        symbols.push(ObjectSymbol {
            name: HEAP_BASE_SYMBOL.to_string(),
            section: bss_section,
            value: 16,
            kind: ObjectSymbolKind::Data,
        });
        symbols.push(ObjectSymbol {
            name: ALLOC_MODE_SYMBOL.to_string(),
            section: bss_section,
            value: u64::from(ALLOC_MODE_OFFSET),
            kind: ObjectSymbolKind::Data,
        });
    }
    // Undefined externals (an `extern fn` bound by the linker) come last so the
    // Mach-O `LC_DYSYMTAB` defined/undefined ranges stay contiguous.
    for reloc in &relocations {
        if !symbols.iter().any(|s| s.name == reloc.symbol) {
            symbols.push(ObjectSymbol {
                name: reloc.symbol.clone(),
                section: None,
                value: 0,
                kind: ObjectSymbolKind::Function,
            });
        }
    }

    // -- Resolve relocation symbol indices + classify branch vs data ---------
    let index_of = |name: &str| -> usize {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("relocation symbol is defined")
    };
    let text_relocs: Vec<ObjectRelocation> = relocations
        .iter()
        .map(|reloc| {
            let symbol = index_of(&reloc.symbol);
            let kind = match symbols[symbol].kind {
                ObjectSymbolKind::Function => ObjectRelocationKind::Branch,
                ObjectSymbolKind::Data => ObjectRelocationKind::PcRel32,
            };
            ObjectRelocation {
                offset: reloc.offset,
                symbol,
                kind,
            }
        })
        .collect();
    sections[0].relocations = text_relocs;

    ObjectModel {
        sections,
        symbols,
        entry_symbol: entry_stub.emits().then(|| abi.entry_symbol().to_string()),
        // This builder lowers x86-64 machine code (shared by the ELF and Mach-O
        // paths). The AArch64 ELF path has its own model builder in `aarch64`.
        machine: crate::object_model::ObjectMachine::X86_64,
    }
}

/// The fixed set of heap/string runtime helper functions emitted (in this order)
/// whenever a native program uses the heap. Shared by the COFF data path, the
/// neutral (ELF/Mach-O) model, and the direct-PE image writer so every container
/// carries the same helper symbol set.
pub(crate) fn heap_runtime_helpers() -> Vec<HelperFunction> {
    vec![
        emit_heap_alloc_helper(),
        emit_rc_free_helper(),
        emit_rc_dec_helper(),
        emit_drop_string_array_helper(),
        emit_heap_strlen_helper(),
        emit_list_new_helper(),
        emit_list_copy_helper(),
        emit_list_grow_helper(),
        emit_struct_copy_helper(),
        emit_map_new_helper(),
        emit_map_copy_helper(),
        emit_map_grow_helper(),
        emit_map_find_helper(),
        emit_str_lit_helper(),
        emit_str_concat_helper(),
        emit_str_concat_own_helper(),
        emit_str_len_own_helper(),
        emit_str_binop_own_helper(),
        emit_str_read_own_helper(),
        emit_str_from_int_helper(),
        emit_str_from_bool_helper(),
        emit_str_from_char_helper(),
        emit_str_substring_helper(),
        emit_str_char_at_helper(),
        emit_str_count_helper(),
        emit_str_repeat_helper(),
        emit_str_trim_helper(),
        emit_str_upper_helper(),
        emit_str_lower_helper(),
        emit_str_find_helper(),
        emit_str_contains_helper(),
        emit_str_starts_with_helper(),
        emit_str_ends_with_helper(),
        emit_str_split_helper(),
        emit_str_join_helper(),
        emit_parse_i64_helper(),
        emit_to_cstr_helper(),
    ]
}

/// Whether any lowered function references a runtime heap helper (a growable-list
/// or growable-map helper, or a string helper), which requires the heap sections +
/// helper `.text` even when the program interns no `.rdata` string constants (e.g.
/// a `to_string(i64)`-only program builds records without any literal).
pub(crate) fn program_uses_heap_helpers(functions: &[LoweredNativeFunction]) -> bool {
    functions.iter().any(|f| {
        f.relocations.iter().any(|r| {
            matches!(
                r.symbol.as_str(),
                // A closure literal allocates its `[code_ptr][captures…]` block by
                // calling `__lullaby_alloc` directly (not via a list/map/string
                // helper), so this reference alone must force the heap sections in
                // the direct-PE path.
                HEAP_ALLOC_SYMBOL
                    | LIST_NEW_SYMBOL
                    | LIST_COPY_SYMBOL
                    | LIST_GROW_SYMBOL
                    | STRUCT_COPY_SYMBOL
                    | MAP_NEW_SYMBOL
                    | MAP_COPY_SYMBOL
                    | MAP_GROW_SYMBOL
                    | MAP_FIND_SYMBOL
                    | STR_LIT_SYMBOL
                    | STR_CONCAT_SYMBOL
                    | STR_FROM_INT_SYMBOL
                    | STR_FROM_BOOL_SYMBOL
                    | STR_FROM_CHAR_SYMBOL
                    | STR_SUBSTRING_SYMBOL
                    | STR_CHAR_AT_SYMBOL
                    | STR_COUNT_SYMBOL
                    | STR_REPEAT_SYMBOL
                    | STR_TRIM_SYMBOL
                    | STR_UPPER_SYMBOL
                    | STR_LOWER_SYMBOL
                    | STR_FIND_SYMBOL
                    | STR_CONTAINS_SYMBOL
                    | STR_STARTS_WITH_SYMBOL
                    | STR_ENDS_WITH_SYMBOL
                    | STR_SPLIT_SYMBOL
                    | STR_JOIN_SYMBOL
                    | PARSE_I64_SYMBOL
                    | RC_DEC_SYMBOL
                    | STR_LEN_OWN_SYMBOL
                    | STR_BINOP_OWN_SYMBOL
                    | STR_READ_OWN_SYMBOL
                    | DROP_STRING_ARRAY_SYMBOL
                    | TO_CSTR_SYMBOL
                    // An arena-eligible function references the bump-pointer cell
                    // (and the arena-mode flag) in its prologue/return reset, so
                    // seeing either forces the heap sections + helpers to be emitted.
                    | HEAP_NEXT_SYMBOL
                    | ALLOC_MODE_SYMBOL
            )
        })
    })
}

/// Assemble the whole `.text` blob (entry stub + functions) and the section
/// relocations, then write the COFF headers, section data, symbol table, and
/// string table.
///
/// When `entry_stub` emits, the object leads with the `_lullaby_start` entry stub
/// that calls `main` and forwards its result to `ExitProcess` (a runnable
/// program). When false, no stub is emitted and no `ExitProcess` dependency is
/// introduced: the object is a *library* whose exported functions a C `main` (or
/// another object) links against and calls. A string-free stubbed program keeps
/// its exact prior byte-for-byte layout.
fn write_text_only_object(
    functions: &[LoweredNativeFunction],
    entry_stub: EntryStub,
    debug: Option<&DebugOptions>,
) -> Vec<u8> {
    // Lay out `.text`: entry stub first, then each function. Record each
    // function's start offset so relocations resolve.
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<TextRelocation> = Vec::new();

    if entry_stub.emits() {
        // Entry stub: sub rsp, 40 (align + shadow); call main; set the exit code
        // (from `eax`, or zeroed for a void `main`); call ExitProcess; (int3
        // padding). The `sub rsp,40` keeps rsp 16-aligned at each `call` (return
        // address makes 8; 40 = 0x28 restores alignment).
        text.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 40
        text.push(0xE8); // call main (rel32)
        relocations.push(TextRelocation {
            offset: (text.len()) as u32,
            symbol_index: 0, // filled after we know the symbol order (main is known)
            symbol_name: "main".to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        emit_exit_code_into_ecx(&mut text, entry_stub);
        text.push(0xE8); // call ExitProcess (rel32)
        relocations.push(TextRelocation {
            offset: (text.len()) as u32,
            symbol_index: 0,
            symbol_name: EXIT_PROCESS_SYMBOL.to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.push(0xCC); // int3 (unreachable; ExitProcess does not return)
    }

    // Each compiled function, remembering its start offset for symbol addresses.
    let mut func_offsets: HashMap<String, u32> = HashMap::new();
    for function in functions {
        // Align each function start to 16 bytes with int3 padding for tidy
        // disassembly (not required, but conventional).
        while !text.len().is_multiple_of(16) {
            text.push(0xCC);
        }
        let start = text.len() as u32;
        func_offsets.insert(function.name.clone(), start);
        let body_base = text.len();
        text.extend_from_slice(&function.code);
        // Translate each per-function relocation into a section relocation.
        for reloc in &function.relocations {
            relocations.push(TextRelocation {
                offset: body_base as u32 + reloc.offset,
                symbol_index: 0,
                symbol_name: reloc.symbol.clone(),
            });
        }
    }

    // Build the symbol table. Symbol 0 is the entry stub; then every function;
    // then the imported ExitProcess (undefined). Callers (relocations) reference
    // symbols by name, resolved to an index here.
    struct SymbolDef {
        name: String,
        section_number: i16, // 1 = .text, 0 = undefined (external import)
        value: u32,          // offset within the section
    }

    let mut symbols: Vec<SymbolDef> = Vec::new();
    if entry_stub.emits() {
        symbols.push(SymbolDef {
            name: NATIVE_ENTRY_SYMBOL.to_string(),
            section_number: 1,
            value: 0,
        });
    }
    for function in functions {
        symbols.push(SymbolDef {
            name: function.name.clone(),
            section_number: 1,
            value: *func_offsets.get(&function.name).expect("function offset"),
        });
    }
    if entry_stub.emits() {
        symbols.push(SymbolDef {
            name: EXIT_PROCESS_SYMBOL.to_string(),
            section_number: 0,
            value: 0,
        });
    }
    // Any relocation target not defined above is an undefined external symbol —
    // an `extern fn` C function bound by the linker (section 0), exactly like
    // `ExitProcess`. Add each such symbol once.
    for reloc in &relocations {
        if !symbols.iter().any(|s| s.name == reloc.symbol_name) {
            symbols.push(SymbolDef {
                name: reloc.symbol_name.clone(),
                section_number: 0,
                value: 0,
            });
        }
    }

    let symbol_index_of = |name: &str| -> u32 {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("symbol exists") as u32
    };

    // Resolve relocation symbol indices now that the table is known.
    for reloc in &mut relocations {
        reloc.symbol_index = symbol_index_of(&reloc.symbol_name);
    }

    // -- Optional CodeView `.debug$S` line info -----------------------------
    // Built only when `--debug` is requested. Each user function contributes one
    // line record at its entry offset; helper/stub symbols carry no source line.
    let debug_built = debug.map(|options| {
        let entries: Vec<DebugFunctionLine<'_>> = functions
            .iter()
            .map(|function| DebugFunctionLine {
                symbol: &function.name,
                code_len: function.code.len() as u32,
                line: function.line,
            })
            .collect();
        let (data, relocs) = build_debug_section(&options.source_file, &entries);
        (data, relocs)
    });

    // -- Compute layout offsets ---------------------------------------------
    let num_relocs = relocations.len() as u32;
    let num_sections: u16 = if debug_built.is_some() { 2 } else { 1 };
    let headers_end = COFF_HEADER_SIZE + u32::from(num_sections) * SECTION_HEADER_SIZE;
    let raw_text_offset = headers_end;
    let debug_raw_offset = raw_text_offset + text.len() as u32;
    let debug_len = debug_built
        .as_ref()
        .map(|(data, _)| data.len() as u32)
        .unwrap_or(0);
    // `.text` relocations follow all raw section data; `.debug$S` relocations
    // follow the `.text` relocations.
    let reloc_table_offset = debug_raw_offset + debug_len;
    let num_debug_relocs = debug_built
        .as_ref()
        .map(|(_, relocs)| relocs.len() as u32)
        .unwrap_or(0);
    let debug_reloc_offset = reloc_table_offset + num_relocs * COFF_RELOC_SIZE;
    let symbol_table_offset = debug_reloc_offset + num_debug_relocs * COFF_RELOC_SIZE;
    let num_symbols = symbols.len() as u32;

    // -- Emit ----------------------------------------------------------------
    let mut bytes = Vec::new();

    // COFF header.
    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, num_sections);
    push_u32(&mut bytes, 0); // timestamp
    push_u32(&mut bytes, symbol_table_offset);
    push_u32(&mut bytes, num_symbols);
    push_u16(&mut bytes, 0); // optional header size
    push_u16(&mut bytes, 0); // characteristics

    // Section header for `.text`.
    push_fixed_name(&mut bytes, ".text", 8);
    push_u32(&mut bytes, 0); // VirtualSize
    push_u32(&mut bytes, 0); // VirtualAddress
    push_u32(&mut bytes, text.len() as u32); // SizeOfRawData
    push_u32(&mut bytes, raw_text_offset); // PointerToRawData
    push_u32(
        &mut bytes,
        if num_relocs == 0 {
            0
        } else {
            reloc_table_offset
        },
    ); // PointerToRelocations
    push_u32(&mut bytes, 0); // PointerToLinenumbers
    push_u16(&mut bytes, num_relocs as u16); // NumberOfRelocations
    push_u16(&mut bytes, 0); // NumberOfLinenumbers
    push_u32(&mut bytes, TEXT_CHARACTERISTICS);

    // Section header for `.debug$S` (only when debug info is requested).
    if debug_built.is_some() {
        push_fixed_name(&mut bytes, ".debug$S", 8);
        push_u32(&mut bytes, 0); // VirtualSize
        push_u32(&mut bytes, 0); // VirtualAddress
        push_u32(&mut bytes, debug_len); // SizeOfRawData
        push_u32(&mut bytes, debug_raw_offset); // PointerToRawData
        push_u32(
            &mut bytes,
            if num_debug_relocs == 0 {
                0
            } else {
                debug_reloc_offset
            },
        ); // PointerToRelocations
        push_u32(&mut bytes, 0); // PointerToLinenumbers
        push_u16(&mut bytes, num_debug_relocs as u16); // NumberOfRelocations
        push_u16(&mut bytes, 0); // NumberOfLinenumbers
        push_u32(&mut bytes, DEBUG_S_CHARACTERISTICS);
    }

    // Section raw data: `.text`, then `.debug$S`.
    bytes.extend_from_slice(&text);
    if let Some((data, _)) = &debug_built {
        bytes.extend_from_slice(data);
    }

    // Relocation records: VirtualAddress (u32), SymbolTableIndex (u32), Type (u16).
    for reloc in &relocations {
        push_u32(&mut bytes, reloc.offset);
        push_u32(&mut bytes, reloc.symbol_index);
        push_u16(&mut bytes, IMAGE_REL_AMD64_REL32);
    }
    // `.debug$S` relocations reference the `.text` function symbols by index.
    if let Some((_, debug_relocs)) = &debug_built {
        for reloc in debug_relocs {
            push_u32(&mut bytes, reloc.offset);
            push_u32(&mut bytes, symbol_index_of(&reloc.symbol));
            push_u16(&mut bytes, reloc.reloc_type);
        }
    }

    // Symbol table + string table. Long names go to the string table, which is
    // appended immediately after the symbol records and begins with its own
    // 4-byte length field.
    let mut string_table: Vec<u8> = Vec::new();
    string_table.extend_from_slice(&[0, 0, 0, 0]); // placeholder for size
    for symbol in &symbols {
        if symbol.name.len() <= 8 {
            push_fixed_name(&mut bytes, &symbol.name, 8);
        } else {
            // Name field: 4 zero bytes then a 4-byte offset into the string table
            // (offset counts from the start of the string table, incl. the size).
            let offset = string_table.len() as u32;
            push_u32(&mut bytes, 0);
            push_u32(&mut bytes, offset);
            string_table.extend_from_slice(symbol.name.as_bytes());
            string_table.push(0);
        }
        push_u32(&mut bytes, symbol.value); // Value (section offset)
        push_u16(&mut bytes, section_number_field(symbol.section_number)); // SectionNumber
        push_u16(&mut bytes, 0x20); // Type: function
        bytes.push(2); // StorageClass: EXTERNAL
        bytes.push(0); // NumberOfAuxSymbols
    }

    // Patch and append the string table.
    let string_table_size = string_table.len() as u32;
    string_table[0..4].copy_from_slice(&string_table_size.to_le_bytes());
    bytes.extend_from_slice(&string_table);

    bytes
}

/// A relocation within the `.text` section.
struct TextRelocation {
    /// Byte offset of the 4-byte field within the section.
    offset: u32,
    /// Index into the symbol table.
    symbol_index: u32,
    /// Symbol name (resolved to `symbol_index` once the table is built).
    symbol_name: String,
}

/// Encode a signed COFF section number (`1` for `.text`, `0` for undefined) into
/// the unsigned 16-bit field.
fn section_number_field(section_number: i16) -> u16 {
    section_number as u16
}

const COFF_RELOC_SIZE: u32 = 10;

// ===========================================================================
// First heap step: `.rdata` string constants + `.bss` bump heap + helpers
// ===========================================================================
//
// When a program references string constants (`len("...")`), the object gains:
//   * `.rdata` — the NUL-terminated string bytes, each named `__str{i}`;
//   * `.bss`   — an 8-byte bump-pointer cell (`__lullaby_heap_next`) followed by
//                a fixed reserved heap region (`__lullaby_heap_base`);
//   * two helper functions in `.text` — `__lullaby_alloc` (a bump allocator) and
//     `__lullaby_strlen_copy` (allocate a heap copy of a `.rdata` string and
//     return its byte length by scanning the copy).
//
// This is the smallest end-to-end heap increment: a read-only constant, a REL32
// relocation to its address, a real bump allocation into a writable region, and
// per-byte reads of both `.rdata` and the heap — all observable through the i64
// `len` result and hence the process exit code.

/// A machine-code blob plus the symbols it references via REL32 relocations.
pub(crate) struct HelperFunction {
    pub(crate) name: String,
    pub(crate) code: Vec<u8>,
    pub(crate) relocations: Vec<CodeRelocation>,
}

/// Write the extended COFF object with `.text`, `.rdata`, and `.bss` sections.
/// Used only when the program references string constants.
fn write_object_with_data(
    functions: &[LoweredNativeFunction],
    strings: &StringPool,
    entry_stub: EntryStub,
    debug: Option<&DebugOptions>,
) -> Vec<u8> {
    // -- Build .text: entry stub, user functions, heap helpers ---------------
    let mut text: Vec<u8> = Vec::new();
    let mut relocations: Vec<TextRelocation> = Vec::new();

    if entry_stub.emits() {
        // Entry stub (identical to the text-only path).
        text.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 40
        text.push(0xE8); // call main
        relocations.push(TextRelocation {
            offset: text.len() as u32,
            symbol_index: 0,
            symbol_name: "main".to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        emit_exit_code_into_ecx(&mut text, entry_stub);
        text.push(0xE8); // call ExitProcess
        relocations.push(TextRelocation {
            offset: text.len() as u32,
            symbol_index: 0,
            symbol_name: EXIT_PROCESS_SYMBOL.to_string(),
        });
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.push(0xCC);
    }

    let mut func_offsets: HashMap<String, u32> = HashMap::new();

    // A closure-free helper to append a code blob with relocations, 16-aligned.
    let append_code = |text: &mut Vec<u8>,
                       relocations: &mut Vec<TextRelocation>,
                       func_offsets: &mut HashMap<String, u32>,
                       name: &str,
                       code: &[u8],
                       relocs: &[CodeRelocation]| {
        while !text.len().is_multiple_of(16) {
            text.push(0xCC);
        }
        let start = text.len() as u32;
        func_offsets.insert(name.to_string(), start);
        let body_base = text.len() as u32;
        text.extend_from_slice(code);
        for reloc in relocs {
            relocations.push(TextRelocation {
                offset: body_base + reloc.offset,
                symbol_index: 0,
                symbol_name: reloc.symbol.clone(),
            });
        }
    };

    for function in functions {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &function.name,
            &function.code,
            &function.relocations,
        );
    }
    let alloc = emit_heap_alloc_helper();
    append_code(
        &mut text,
        &mut relocations,
        &mut func_offsets,
        &alloc.name,
        &alloc.code,
        &alloc.relocations,
    );
    for rc_helper in [
        emit_rc_free_helper(),
        emit_rc_dec_helper(),
        emit_drop_string_array_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &rc_helper.name,
            &rc_helper.code,
            &rc_helper.relocations,
        );
    }
    let strlen = emit_heap_strlen_helper();
    append_code(
        &mut text,
        &mut relocations,
        &mut func_offsets,
        &strlen.name,
        &strlen.code,
        &strlen.relocations,
    );
    // Growable-list runtime helpers (list_new / list_copy / list_grow). Emitted
    // unconditionally alongside the string helpers whenever the heap path runs; a
    // program that never references them simply carries three unused `.text`
    // functions (the linker's dead-strip removes them from the final image).
    for list_helper in [
        emit_list_new_helper(),
        emit_list_copy_helper(),
        emit_list_grow_helper(),
        emit_struct_copy_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &list_helper.name,
            &list_helper.code,
            &list_helper.relocations,
        );
    }
    // Growable-map runtime helpers (map_new / map_copy / map_grow / map_find),
    // emitted alongside the list helpers whenever the heap path runs. A program
    // that never references them carries unused `.text` functions (dead-stripped
    // by the linker).
    for map_helper in [
        emit_map_new_helper(),
        emit_map_copy_helper(),
        emit_map_grow_helper(),
        emit_map_find_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &map_helper.name,
            &map_helper.code,
            &map_helper.relocations,
        );
    }
    // String runtime helpers (str_lit / str_concat / str_from_int / str_from_bool
    // / str_from_char), emitted alongside the list/map helpers whenever the heap
    // path runs. A program that never references them carries unused `.text`
    // functions (dead-stripped by the linker).
    for str_helper in [
        emit_str_lit_helper(),
        emit_str_concat_helper(),
        emit_str_concat_own_helper(),
        emit_str_len_own_helper(),
        emit_str_binop_own_helper(),
        emit_str_read_own_helper(),
        emit_str_from_int_helper(),
        emit_str_from_bool_helper(),
        emit_str_from_char_helper(),
        emit_str_substring_helper(),
        emit_str_char_at_helper(),
        emit_str_count_helper(),
        emit_str_repeat_helper(),
        emit_str_trim_helper(),
        emit_str_upper_helper(),
        emit_str_lower_helper(),
        emit_str_find_helper(),
        emit_str_contains_helper(),
        emit_str_starts_with_helper(),
        emit_str_ends_with_helper(),
        emit_str_split_helper(),
        emit_str_join_helper(),
        emit_parse_i64_helper(),
        emit_to_cstr_helper(),
    ] {
        append_code(
            &mut text,
            &mut relocations,
            &mut func_offsets,
            &str_helper.name,
            &str_helper.code,
            &str_helper.relocations,
        );
    }

    // -- Build .rdata: NUL-terminated string constants -----------------------
    let mut rdata: Vec<u8> = Vec::new();
    let mut str_offsets: Vec<u32> = Vec::new();
    for text_value in &strings.entries {
        str_offsets.push(rdata.len() as u32);
        rdata.extend_from_slice(text_value.as_bytes());
        rdata.push(0);
    }

    // -- Symbol table --------------------------------------------------------
    // Sections: 1 = .text, 2 = .rdata, 3 = .bss.
    struct SymbolDef {
        name: String,
        section_number: i16,
        value: u32,
        is_function: bool,
    }

    let mut symbols: Vec<SymbolDef> = Vec::new();
    if entry_stub.emits() {
        symbols.push(SymbolDef {
            name: NATIVE_ENTRY_SYMBOL.to_string(),
            section_number: 1,
            value: 0,
            is_function: true,
        });
    }
    for function in functions {
        symbols.push(SymbolDef {
            name: function.name.clone(),
            section_number: 1,
            value: *func_offsets.get(&function.name).expect("function offset"),
            is_function: true,
        });
    }
    for helper in [
        HEAP_ALLOC_SYMBOL,
        RC_FREE_SYMBOL,
        RC_DEC_SYMBOL,
        DROP_STRING_ARRAY_SYMBOL,
        HEAP_STRLEN_SYMBOL,
        LIST_NEW_SYMBOL,
        LIST_COPY_SYMBOL,
        LIST_GROW_SYMBOL,
        STRUCT_COPY_SYMBOL,
        MAP_NEW_SYMBOL,
        MAP_COPY_SYMBOL,
        MAP_GROW_SYMBOL,
        MAP_FIND_SYMBOL,
        STR_LIT_SYMBOL,
        STR_CONCAT_SYMBOL,
        STR_CONCAT_OWN_SYMBOL,
        STR_LEN_OWN_SYMBOL,
        STR_BINOP_OWN_SYMBOL,
        STR_READ_OWN_SYMBOL,
        STR_FROM_INT_SYMBOL,
        STR_FROM_BOOL_SYMBOL,
        STR_FROM_CHAR_SYMBOL,
        STR_SUBSTRING_SYMBOL,
        STR_CHAR_AT_SYMBOL,
        STR_COUNT_SYMBOL,
        STR_REPEAT_SYMBOL,
        STR_TRIM_SYMBOL,
        STR_UPPER_SYMBOL,
        STR_LOWER_SYMBOL,
        STR_FIND_SYMBOL,
        STR_CONTAINS_SYMBOL,
        STR_STARTS_WITH_SYMBOL,
        STR_ENDS_WITH_SYMBOL,
        STR_SPLIT_SYMBOL,
        STR_JOIN_SYMBOL,
        PARSE_I64_SYMBOL,
        TO_CSTR_SYMBOL,
    ] {
        symbols.push(SymbolDef {
            name: helper.to_string(),
            section_number: 1,
            value: *func_offsets.get(helper).expect("helper offset"),
            is_function: true,
        });
    }
    if entry_stub.emits() {
        symbols.push(SymbolDef {
            name: EXIT_PROCESS_SYMBOL.to_string(),
            section_number: 0,
            value: 0,
            is_function: true,
        });
    }
    for (index, offset) in str_offsets.iter().enumerate() {
        symbols.push(SymbolDef {
            name: format!("__str{index}"),
            section_number: 2,
            value: *offset,
            is_function: false,
        });
    }
    // .bss: the bump pointer at offset 0, the free-list head at offset 8, the heap
    // region at offset 16.
    symbols.push(SymbolDef {
        name: HEAP_NEXT_SYMBOL.to_string(),
        section_number: 3,
        value: 0,
        is_function: false,
    });
    symbols.push(SymbolDef {
        name: HEAP_FREE_HEAD_SYMBOL.to_string(),
        section_number: 3,
        value: 8,
        is_function: false,
    });
    symbols.push(SymbolDef {
        name: HEAP_BASE_SYMBOL.to_string(),
        section_number: 3,
        value: 16,
        is_function: false,
    });
    // The arena-mode flag cell sits past the 1 MiB heap region.
    symbols.push(SymbolDef {
        name: ALLOC_MODE_SYMBOL.to_string(),
        section_number: 3,
        value: ALLOC_MODE_OFFSET,
        is_function: false,
    });
    // Undefined external symbols for any unresolved relocation target — the
    // `extern fn` C functions bound by the linker (section 0), like `ExitProcess`.
    for reloc in &relocations {
        if !symbols.iter().any(|s| s.name == reloc.symbol_name) {
            symbols.push(SymbolDef {
                name: reloc.symbol_name.clone(),
                section_number: 0,
                value: 0,
                is_function: true,
            });
        }
    }

    let symbol_index_of = |name: &str| -> u32 {
        symbols
            .iter()
            .position(|s| s.name == name)
            .expect("symbol exists") as u32
    };
    for reloc in &mut relocations {
        reloc.symbol_index = symbol_index_of(&reloc.symbol_name);
    }

    // -- Optional CodeView `.debug$S` line info -----------------------------
    let debug_built = debug.map(|options| {
        let entries: Vec<DebugFunctionLine<'_>> = functions
            .iter()
            .map(|function| DebugFunctionLine {
                symbol: &function.name,
                code_len: function.code.len() as u32,
                line: function.line,
            })
            .collect();
        build_debug_section(&options.source_file, &entries)
    });

    // -- Section layout ------------------------------------------------------
    // Sections: .text, .rdata, .bss, and (with --debug) .debug$S.
    let num_sections: u32 = if debug_built.is_some() { 4 } else { 3 };
    let bss_size = HEAP_BSS_SIZE;
    let num_relocs = relocations.len() as u32;

    let headers_end = COFF_HEADER_SIZE + num_sections * SECTION_HEADER_SIZE;
    let text_raw = headers_end;
    let rdata_raw = text_raw + text.len() as u32;
    // `.debug$S` raw data (if any) follows `.rdata`; `.bss` has no raw data.
    let debug_raw_offset = rdata_raw + rdata.len() as u32;
    let debug_len = debug_built
        .as_ref()
        .map(|(data, _)| data.len() as u32)
        .unwrap_or(0);
    // Relocations follow the raw section data: `.text` relocs, then `.debug$S`.
    let reloc_table_offset = debug_raw_offset + debug_len;
    let num_debug_relocs = debug_built
        .as_ref()
        .map(|(_, relocs)| relocs.len() as u32)
        .unwrap_or(0);
    let debug_reloc_offset = reloc_table_offset + num_relocs * COFF_RELOC_SIZE;
    let symbol_table_offset = debug_reloc_offset + num_debug_relocs * COFF_RELOC_SIZE;
    let num_symbols = symbols.len() as u32;

    // -- Emit ----------------------------------------------------------------
    let mut bytes = Vec::new();

    // COFF header.
    push_u16(&mut bytes, AMD64_MACHINE);
    push_u16(&mut bytes, num_sections as u16);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, symbol_table_offset);
    push_u32(&mut bytes, num_symbols);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);

    // .text section header.
    push_fixed_name(&mut bytes, ".text", 8);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, text.len() as u32);
    push_u32(&mut bytes, text_raw);
    push_u32(
        &mut bytes,
        if num_relocs == 0 {
            0
        } else {
            reloc_table_offset
        },
    );
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, num_relocs as u16);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, TEXT_CHARACTERISTICS);

    // .rdata section header.
    push_fixed_name(&mut bytes, ".rdata", 8);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, rdata.len() as u32);
    push_u32(&mut bytes, rdata_raw);
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, RDATA_CHARACTERISTICS);

    // .bss section header. In a COFF *object*, an uninitialized-data section
    // carries its size in SizeOfRawData with PointerToRawData = 0 (there is no
    // raw data on disk); VirtualSize is 0. `IMAGE_SCN_CNT_UNINITIALIZED_DATA`
    // tells the linker to reserve zeroed space.
    push_fixed_name(&mut bytes, ".bss", 8);
    push_u32(&mut bytes, 0); // VirtualSize (0 for object files)
    push_u32(&mut bytes, 0); // VirtualAddress
    push_u32(&mut bytes, bss_size); // SizeOfRawData (reserved zeroed bytes)
    push_u32(&mut bytes, 0); // PointerToRawData (none for uninitialized data)
    push_u32(&mut bytes, 0);
    push_u32(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u16(&mut bytes, 0);
    push_u32(&mut bytes, BSS_CHARACTERISTICS);

    // .debug$S section header (only when debug info is requested).
    if debug_built.is_some() {
        push_fixed_name(&mut bytes, ".debug$S", 8);
        push_u32(&mut bytes, 0); // VirtualSize
        push_u32(&mut bytes, 0); // VirtualAddress
        push_u32(&mut bytes, debug_len); // SizeOfRawData
        push_u32(&mut bytes, debug_raw_offset); // PointerToRawData
        push_u32(
            &mut bytes,
            if num_debug_relocs == 0 {
                0
            } else {
                debug_reloc_offset
            },
        ); // PointerToRelocations
        push_u32(&mut bytes, 0); // PointerToLinenumbers
        push_u16(&mut bytes, num_debug_relocs as u16); // NumberOfRelocations
        push_u16(&mut bytes, 0); // NumberOfLinenumbers
        push_u32(&mut bytes, DEBUG_S_CHARACTERISTICS);
    }

    // Section raw data: .text, .rdata, then .debug$S (.bss has none).
    bytes.extend_from_slice(&text);
    bytes.extend_from_slice(&rdata);
    if let Some((data, _)) = &debug_built {
        bytes.extend_from_slice(data);
    }

    // Relocations: .text relocations, then .debug$S relocations.
    for reloc in &relocations {
        push_u32(&mut bytes, reloc.offset);
        push_u32(&mut bytes, reloc.symbol_index);
        push_u16(&mut bytes, IMAGE_REL_AMD64_REL32);
    }
    if let Some((_, debug_relocs)) = &debug_built {
        for reloc in debug_relocs {
            push_u32(&mut bytes, reloc.offset);
            push_u32(&mut bytes, symbol_index_of(&reloc.symbol));
            push_u16(&mut bytes, reloc.reloc_type);
        }
    }

    // Symbol table + string table.
    let mut string_table: Vec<u8> = Vec::new();
    string_table.extend_from_slice(&[0, 0, 0, 0]);
    for symbol in &symbols {
        if symbol.name.len() <= 8 {
            push_fixed_name(&mut bytes, &symbol.name, 8);
        } else {
            let offset = string_table.len() as u32;
            push_u32(&mut bytes, 0);
            push_u32(&mut bytes, offset);
            string_table.extend_from_slice(symbol.name.as_bytes());
            string_table.push(0);
        }
        push_u32(&mut bytes, symbol.value);
        push_u16(&mut bytes, section_number_field(symbol.section_number));
        push_u16(&mut bytes, if symbol.is_function { 0x20 } else { 0x00 });
        bytes.push(2); // StorageClass: EXTERNAL
        bytes.push(0); // NumberOfAuxSymbols
    }

    let string_table_size = string_table.len() as u32;
    string_table[0..4].copy_from_slice(&string_table_size.to_le_bytes());
    bytes.extend_from_slice(&string_table);

    bytes
}

// ===========================================================================
// CodeView source-line debug info (`.debug$S`, `lullaby native --debug`)
// ===========================================================================
//
// When `--debug` is requested, the object gains a CodeView `.debug$S` section
// carrying a per-function line-number table that maps each compiled function's
// entry code offset to its `.lby` source declaration line, plus the source file
// name. rust-lld/link.exe fold `.debug$S` into a PDB; a debugger (or
// `llvm-pdbutil`) can then place a breakpoint at a function and show its source
// line. Line granularity is **per function** for this increment: one line record
// at each function's entry offset (its declaration line). Finer per-statement
// mapping is deferred (see the native backend contract).
//
// The `.debug$S` stream is: a C13 signature, a `DEBUG_S_SYMBOLS` subsection with
// a minimal `S_COMPILE3`, one `DEBUG_S_LINES` subsection per function (whose
// header's function-offset/segment fields are patched by SECREL/SECTION
// relocations against that function's `.text` symbol), a `DEBUG_S_FILECHKSMS`
// table with one file entry, and a `DEBUG_S_STRINGTABLE` holding the source file
// name. Emission is fully additive: without `--debug` no `.debug$S` section is
// produced and the object bytes are byte-for-byte unchanged.

/// A relocation the `.debug$S` section needs against a `.text` function symbol:
/// the `DEBUG_S_LINES` header's function-offset (`SECREL32`) and segment
/// (`SECTION`) fields must be fixed up by the linker.
struct DebugReloc {
    /// Byte offset of the 4-byte field within the `.debug$S` section.
    offset: u32,
    /// The `.text` function symbol referenced.
    symbol: String,
    /// COFF relocation type (`SECREL32` or `SECTION`).
    reloc_type: u16,
}

/// One compiled function's debug line entry: the `.text` symbol name, its code
/// length in bytes, and its 1-based source declaration line.
struct DebugFunctionLine<'a> {
    symbol: &'a str,
    code_len: u32,
    line: u32,
}

/// Build the CodeView `.debug$S` section bytes plus the relocations it needs
/// against the `.text` function symbols. `source_file` is recorded as the source
/// file name; `functions` provides one entry per compiled function.
fn build_debug_section(
    source_file: &str,
    functions: &[DebugFunctionLine<'_>],
) -> (Vec<u8>, Vec<DebugReloc>) {
    let mut relocs: Vec<DebugReloc> = Vec::new();
    let mut section: Vec<u8> = Vec::new();
    push_u32(&mut section, CV_SIGNATURE_C13);

    // -- DEBUG_S_STRINGTABLE contents (built first so file/line subsections can
    //    reference the source-file name by its offset). The table starts with a
    //    zero byte (offset 0 is the empty string), then each NUL-terminated name.
    let mut strtab: Vec<u8> = Vec::new();
    strtab.push(0);
    let source_name_offset = strtab.len() as u32;
    strtab.extend_from_slice(source_file.as_bytes());
    strtab.push(0);

    // -- DEBUG_S_FILECHKSMS contents. One 8-byte file entry: u32 offset into the
    //    string table, u8 checksum-byte-count (0 = no checksum), u8 checksum kind
    //    (0 = None), then 2 pad bytes to a 4-byte boundary. The byte offset of
    //    this entry within the table (0) is what DEBUG_S_LINES references.
    let mut filechksms: Vec<u8> = Vec::new();
    push_u32(&mut filechksms, source_name_offset);
    filechksms.push(0); // checksum size
    filechksms.push(0); // checksum kind: None
    filechksms.push(0); // pad
    filechksms.push(0); // pad
    let file_entry_offset: u32 = 0;

    // -- DEBUG_S_SYMBOLS: a minimal S_COMPILE3 so the stream is a well-formed
    //    CodeView symbol subsection. Record body: flags(u32)=0, machine(u16)=
    //    CV_CFL_X64(0xD0), front-end + back-end version words (all 0), then a
    //    NUL-terminated compiler version name. Each CV symbol record is prefixed
    //    by a u16 length that counts everything after the length field.
    {
        let mut sym: Vec<u8> = Vec::new();
        let mut record: Vec<u8> = Vec::new();
        push_u16(&mut record, S_COMPILE3);
        push_u32(&mut record, 0); // flags + language (Language=0, no flags)
        push_u16(&mut record, 0x00D0); // Machine: CV_CFL_X64
        for _ in 0..8 {
            push_u16(&mut record, 0); // FE/BE major/minor/build/QFE
        }
        record.extend_from_slice(b"lullaby native");
        record.push(0);
        // Record length prefix: the count of bytes after the u16 length field.
        push_u16(&mut sym, record.len() as u16);
        sym.extend_from_slice(&record);
        push_subsection(&mut section, DEBUG_S_SYMBOLS, &sym);
    }

    // -- DEBUG_S_LINES, one subsection per function. The header's first two fields
    //    (function offset, then segment) are relocated against the `.text` symbol.
    for function in functions {
        let sub_data_start = section.len() + 8; // after the subsection kind+length
        let mut lines: Vec<u8> = Vec::new();
        // Subsection header: offset(u32, SECREL32), segment(u16, SECTION),
        // flags(u16)=0, code size(u32).
        let off_field = lines.len();
        push_u32(&mut lines, 0); // offset (patched via SECREL32)
        let seg_field = lines.len();
        push_u16(&mut lines, 0); // segment (patched via SECTION)
        push_u16(&mut lines, 0); // flags
        push_u32(&mut lines, function.code_len); // code size covered

        // One file block: file offset into FILECHKSMS, line count, block size.
        push_u32(&mut lines, file_entry_offset);
        push_u32(&mut lines, 1); // one line entry
        // Block byte size: header(12) + one line pair(8) = 20.
        push_u32(&mut lines, 12 + 8);
        // Line entry: code offset within the function (0 = its entry), then the
        // packed line number. Bit 31 marks a statement (is-statement) line.
        push_u32(&mut lines, 0);
        push_u32(&mut lines, function.line | 0x8000_0000);

        // Record the two header relocations (offsets are relative to the section).
        relocs.push(DebugReloc {
            offset: (sub_data_start + off_field) as u32,
            symbol: function.symbol.to_string(),
            reloc_type: IMAGE_REL_AMD64_SECREL,
        });
        relocs.push(DebugReloc {
            offset: (sub_data_start + seg_field) as u32,
            symbol: function.symbol.to_string(),
            reloc_type: IMAGE_REL_AMD64_SECTION,
        });

        push_subsection(&mut section, DEBUG_S_LINES, &lines);
    }

    push_subsection(&mut section, DEBUG_S_FILECHKSMS, &filechksms);
    push_subsection(&mut section, DEBUG_S_STRINGTABLE, &strtab);

    (section, relocs)
}

/// Append a CodeView subsection (`u32 kind`, `u32 length`, then `data` padded to
/// a 4-byte boundary) to `section`. The caller computes any relocation offsets
/// before appending, since the LINES header field positions must be known
/// precisely to place `SECREL32`/`SECTION` relocations.
fn push_subsection(section: &mut Vec<u8>, kind: u32, data: &[u8]) {
    push_u32(section, kind);
    push_u32(section, data.len() as u32);
    section.extend_from_slice(data);
    while !section.len().is_multiple_of(4) {
        section.push(0);
    }
}

#[cfg(test)]
#[path = "native_object_coff_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "native_program_tests.rs"]
mod native_program_tests;

// The raw-pointer surface's codegen tests live in their own file rather than in
// `native_program_tests.rs`, which is already well past the test-file size cap
// (see `documents/large_file_split_plan.md`).
#[cfg(test)]
#[path = "native_object_rawptr_tests.rs"]
mod native_object_rawptr_tests;

// Value-position tail lowering (branch/arm-local aggregate and float tails) has
// its own codegen tests for the same size-cap reason.
#[cfg(test)]
#[path = "native_object_tailvalue_tests.rs"]
mod native_object_tailvalue_tests;

// Void-returning functions (eligibility, the no-hidden-pointer classification, and
// the statement-position tail that keeps them clear of the value-position routing)
// likewise have their own codegen tests for the size-cap reason.
#[cfg(test)]
#[path = "native_object_void_tests.rs"]
mod native_object_void_tests;

// Port-mapped I/O (`in`/`out`) codegen tests, in their own file for the same
// size-cap reason. These assert the emitted BYTES because port I/O cannot be
// executed: `in`/`out` fault at CPL 3, so there is no run-it-and-check-the-exit
// -code proof available for this surface.
#[cfg(test)]
#[path = "native_object_portio_tests.rs"]
mod native_object_portio_tests;
