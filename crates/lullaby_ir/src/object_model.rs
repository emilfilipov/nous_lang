//! Target-neutral relocatable-object model shared by the ELF64 and Mach-O
//! writers.
//!
//! The native backend generates one self-consistent x86-64 machine-code image
//! (the entry stub, the compiled functions, and the heap/string runtime helpers)
//! plus a symbol table and a set of REL32 relocation sites. That image is
//! platform-agnostic — only the *object-file container* and the *entry/exit
//! mechanism* differ between Windows (COFF + `kernel32!ExitProcess`), Linux
//! (ELF + `exit` syscall), and macOS (Mach-O + `exit` syscall).
//!
//! This module captures the container-neutral part of that image: a list of
//! sections (`.text`, `.rodata`, `.bss`), a flat symbol table, and per-section
//! relocations. [`crate::native_object`] builds an [`ObjectModel`] from the
//! lowered functions with a platform-appropriate freestanding entry stub, and
//! [`crate::elf_object`] / [`crate::macho_object`] serialize it into the
//! respective container.
//!
//! The Windows COFF path deliberately does **not** flow through this model: its
//! writers are locked by byte-for-byte snapshot tests, so it keeps its own
//! serialization. The model exists to give ELF and Mach-O a single shared source
//! of section/symbol/relocation truth.
//!
//! # Verification honesty
//!
//! This is a Windows host. The ELF and Mach-O bytes are verified *structurally*
//! (parsed back and checked for correct magic, header fields, section table,
//! symbol table, and relocation records) but are **not** linked or executed
//! here. Link-and-run verification for those two formats is deferred to the
//! cross-platform CI described in the Phase 9 roadmap. See
//! `documents/native_backend_contract.md`.

/// The kind of a section in the neutral model. The three kinds map to the
/// concrete section flags of each container (`SHT_PROGBITS`/`SHT_NOBITS` for
/// ELF, `S_REGULAR`/`S_ZEROFILL` for Mach-O).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectSectionKind {
    /// Executable machine code (`.text` / `__text`). Read + execute.
    Text,
    /// Read-only initialized data (`.rodata` / `__const`) — the NUL-terminated
    /// string constants. Read only.
    ReadOnlyData,
    /// Zero-initialized data (`.bss` / `__bss`) — the bump-heap region and its
    /// next-pointer cell. Occupies no bytes in the object file; only a size.
    Bss,
}

/// The instruction-set architecture an [`ObjectModel`]'s machine code targets.
/// The ELF writer keys `e_machine` and its relocation types off this; the Mach-O
/// writer is x86-64 only and ignores it. Existing x86-64 models set
/// [`ObjectMachine::X86_64`], so the byte-for-byte x86-64 output is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectMachine {
    /// x86-64 (`EM_X86_64` = 62).
    X86_64,
    /// AArch64 / ARM64 (`EM_AARCH64` = 183).
    Aarch64,
}

/// How a relocation site is resolved. The two x86-64 kinds are 32-bit PC-relative
/// fixups; the AArch64 kind patches a 26-bit branch-immediate instruction word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectRelocationKind {
    /// A `call`/`jmp rel32` targeting a function symbol (ELF `R_X86_64_PLT32`,
    /// Mach-O `X86_64_RELOC_BRANCH`).
    Branch,
    /// A RIP-relative `lea`/`mov` targeting a data symbol (ELF `R_X86_64_PC32`,
    /// Mach-O `X86_64_RELOC_SIGNED`).
    PcRel32,
    /// An AArch64 `bl` call site: patch the 26-bit branch immediate of the
    /// instruction word at `offset` (ELF `R_AARCH64_CALL26` = 283, addend 0).
    Aarch64Call26,
}

/// A relocation within one section: patch the 4-byte little-endian field at
/// `offset` (section-relative) to reference `symbol`.
///
/// Both container encodings resolve the field to `S - (P + 4)` (the displacement
/// from the end of the 4-byte field to the target symbol `S`). ELF encodes the
/// `-4` as an explicit `r_addend`; Mach-O encodes it implicitly through its
/// PC-relative relocation semantics (the field content stays zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRelocation {
    /// Byte offset of the 4-byte field within its owning section.
    pub offset: u64,
    /// Index into [`ObjectModel::symbols`] of the referenced symbol.
    pub symbol: usize,
    /// Branch (function) vs PC-relative (data) classification.
    pub kind: ObjectRelocationKind,
}

/// A section in the neutral model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSection {
    /// The section's role.
    pub kind: ObjectSectionKind,
    /// The raw bytes. Empty for [`ObjectSectionKind::Bss`].
    pub data: Vec<u8>,
    /// The in-memory size. Equals `data.len()` for `Text`/`ReadOnlyData`; the
    /// zero-initialized virtual size for `Bss`.
    pub size: u64,
    /// Relocations applied within this section, in ascending `offset` order.
    pub relocations: Vec<ObjectRelocation>,
}

/// Whether a symbol names code or data. Controls the symbol-type field each
/// container records (`STT_FUNC`/`STT_OBJECT` for ELF; the Mach-O `n_desc` is
/// left neutral but the classification still drives relocation kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectSymbolKind {
    /// A function / code label.
    Function,
    /// A data object (a string constant or a `.bss` cell).
    Data,
}

/// A symbol in the neutral model. Every emitted symbol is global (external) — a
/// relocatable object exposes its function and data labels for the linker to
/// resolve, exactly like the COFF path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSymbol {
    /// The symbol name (never mangled; the linker sees it verbatim).
    pub name: String,
    /// Index into [`ObjectModel::sections`] of the defining section, or `None`
    /// for an undefined external symbol (an `extern fn` bound by the linker).
    pub section: Option<usize>,
    /// The symbol's offset within its defining section (0 for undefined).
    pub value: u64,
    /// Code vs data.
    pub kind: ObjectSymbolKind,
}

/// A complete target-neutral relocatable object: its sections, its flat global
/// symbol table, and the entry-point symbol name (if the object is a runnable
/// program rather than a library).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectModel {
    /// Sections in emission order. Index 0 is always `.text`; `.rodata` and
    /// `.bss` follow when the program uses string constants or the heap.
    pub sections: Vec<ObjectSection>,
    /// The flat global symbol table. Relocations reference entries by index.
    pub symbols: Vec<ObjectSymbol>,
    /// The freestanding entry-point symbol (`_start` on ELF, `start` on Mach-O),
    /// or `None` for a library object with no `main`.
    pub entry_symbol: Option<String>,
    /// The instruction-set architecture of the `.text` machine code. Selects the
    /// ELF `e_machine` and relocation types; ignored by the (x86-64-only) Mach-O
    /// writer.
    pub machine: ObjectMachine,
}

impl ObjectModel {
    /// The index of the single `.text` section, always 0 by construction (the
    /// builder emits `.text` first, then optional `.rodata`/`.bss`).
    pub fn text_section_index(&self) -> usize {
        0
    }
}
