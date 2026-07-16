//! CLI integration tests, part 20 — **DWARF source-line debug info** on the ELF
//! and Mach-O native targets (`road_to_1_0_stable.md` B3).
//!
//! # The gap these close
//!
//! `lullaby native --debug` emitted a CodeView `.debug$S` section for the COFF
//! target only. An ELF or Mach-O object got **no debug info at all**, so a
//! Linux/macOS debugger could not map a compiled function back to its `.lby`
//! line. `--debug` now emits `.debug_line`/`.debug_info`/`.debug_abbrev` for
//! those two targets.
//!
//! # What these tests are, and are not
//!
//! These are **CLI-surface** tests: they drive the real `lullaby native` binary
//! and check that the flag produces (and, without it, does not produce) DWARF in
//! the object it writes. They deliberately do NOT re-verify the DWARF *encoding* —
//! that is proven by decoding the bytes back with `gimli`, an independent DWARF
//! reader, in `crates/lullaby_ir/src/native_object_dwarf_tests.rs`.
//!
//! # Why nothing is executed here
//!
//! This is a Windows host with no cross-linker: an ELF/Mach-O object cannot be
//! linked or run, so there is no run-a-debugger-and-check-the-line proof
//! available (the same constraint as the port-I/O surface, which is likewise
//! verified by inspecting emitted bytes and never executed). A live `gdb`/`lldb`
//! session against a linked binary remains deferred to the Phase 9
//! cross-platform CI.

use crate::*;

/// A two-function fixture with known declaration lines: `add` on line 1, `main`
/// on line 4.
const SOURCE: &str = concat!(
    "fn add x, y i64 -> i64\n",
    "    return x + y\n",
    "\n",
    "fn main -> i64\n",
    "    return add(20, 22)\n",
);

/// Compile `SOURCE` for `triple` and return the emitted object's bytes.
/// `debug` selects whether `--debug` is passed.
fn emit_object(test_name: &str, triple: &str, debug: bool) -> Vec<u8> {
    let (dir, _) = fs_temp_dir(test_name);
    let source_path = dir.join("prog.lby");
    std::fs::write(&source_path, SOURCE).expect("write fixture");
    let object_path = dir.join("prog.o");

    let mut command = lullaby();
    command.args([
        "native",
        "--target",
        triple,
        "-o",
        object_path.to_str().expect("object path"),
    ]);
    if debug {
        command.arg("--debug");
    }
    command.arg(source_path.to_str().expect("source path"));
    let output = command.output().expect("run cli");
    assert!(
        output.status.success(),
        "`lullaby native --target {triple}` must succeed: {}",
        stderr(&output)
    );
    std::fs::read(&object_path).expect("the relocatable object is always written")
}

/// The DWARF section names each container uses.
fn dwarf_section_names(triple: &str) -> [&'static str; 3] {
    if triple.contains("darwin") {
        ["__debug_line", "__debug_info", "__debug_abbrev"]
    } else {
        [".debug_line", ".debug_info", ".debug_abbrev"]
    }
}

/// `--debug` must put a DWARF line table into the ELF object, and the object must
/// still name the source file the rows point at.
#[test]
fn elf_debug_emits_dwarf_sections_and_the_source_path() {
    let bytes = emit_object("dwarf_elf_debug", "x86_64-unknown-linux-gnu", true);
    for name in dwarf_section_names("x86_64-unknown-linux-gnu") {
        assert!(
            contains_subslice(&bytes, name.as_bytes()),
            "`--debug` must emit a `{name}` section for the ELF target"
        );
    }
    assert!(
        contains_subslice(&bytes, b"prog.lby"),
        "the DWARF must record the `.lby` source path"
    );
}

/// The same for Mach-O, whose DWARF lives in the `__DWARF` segment.
#[test]
fn macho_debug_emits_dwarf_sections_in_the_dwarf_segment() {
    let bytes = emit_object("dwarf_macho_debug", "x86_64-apple-darwin", true);
    for name in dwarf_section_names("x86_64-apple-darwin") {
        assert!(
            contains_subslice(&bytes, name.as_bytes()),
            "`--debug` must emit a `{name}` section for the Mach-O target"
        );
    }
    assert!(
        contains_subslice(&bytes, b"__DWARF"),
        "Mach-O DWARF sections must live in the `__DWARF` segment"
    );
}

/// The opt-in guarantee at the CLI surface: **without** `--debug`, the ELF and
/// Mach-O objects must be byte-for-byte what they always were — no DWARF section,
/// no trace of the source path.
#[test]
fn without_debug_no_dwarf_reaches_the_elf_or_macho_object() {
    for (test, triple) in [
        ("dwarf_elf_plain", "x86_64-unknown-linux-gnu"),
        ("dwarf_macho_plain", "x86_64-apple-darwin"),
    ] {
        let bytes = emit_object(test, triple, false);
        for name in dwarf_section_names(triple) {
            assert!(
                !contains_subslice(&bytes, name.as_bytes()),
                "a default build must not emit `{name}` for {triple}"
            );
        }
        assert!(
            !contains_subslice(&bytes, b"prog.lby"),
            "a default build must not record the source path for {triple}"
        );
    }
}

/// `--debug` must be purely additive: the object grows, and everything it
/// contained without the flag it still contains.
#[test]
fn debug_only_adds_to_the_elf_object() {
    let plain = emit_object(
        "dwarf_elf_additive_plain",
        "x86_64-unknown-linux-gnu",
        false,
    );
    let debugged = emit_object("dwarf_elf_additive_debug", "x86_64-unknown-linux-gnu", true);
    assert!(
        debugged.len() > plain.len(),
        "the debug object must carry the extra DWARF"
    );
    // Every symbol name the plain object defines must survive into the debug one.
    for name in ["add", "main", "_start"] {
        assert!(
            contains_subslice(&plain, name.as_bytes()),
            "the plain object defines `{name}`"
        );
        assert!(
            contains_subslice(&debugged, name.as_bytes()),
            "`--debug` must not drop the `{name}` symbol"
        );
    }
}

/// `-g` is the documented short form of `--debug` and must behave identically.
#[test]
fn short_g_flag_emits_dwarf_like_long_debug() {
    let (dir, _) = fs_temp_dir("dwarf_elf_short_g");
    let source_path = dir.join("prog.lby");
    std::fs::write(&source_path, SOURCE).expect("write fixture");
    let object_path = dir.join("prog.o");
    let output = lullaby()
        .args([
            "native",
            "--target",
            "x86_64-unknown-linux-gnu",
            "-g",
            "-o",
            object_path.to_str().expect("object path"),
            source_path.to_str().expect("source path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let bytes = std::fs::read(&object_path).expect("object written");
    assert!(
        contains_subslice(&bytes, b".debug_line"),
        "`-g` must emit DWARF just like `--debug`"
    );
}

/// The COFF target must be untouched by the DWARF work: `--debug` still gets it a
/// CodeView `.debug$S` section and never a DWARF one.
#[test]
fn coff_debug_still_emits_codeview_not_dwarf() {
    let (dir, _) = fs_temp_dir("dwarf_coff_codeview");
    let source_path = dir.join("prog.lby");
    std::fs::write(&source_path, SOURCE).expect("write fixture");
    let exe_path = dir.join("prog.exe");
    let output = lullaby()
        .args([
            "native",
            "--debug",
            "-o",
            exe_path.to_str().expect("exe path"),
            source_path.to_str().expect("source path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let object_path = exe_path.with_extension("obj");
    let bytes = std::fs::read(&object_path).expect("the COFF object is written alongside the exe");
    assert!(
        contains_subslice(&bytes, b".debug$S"),
        "the COFF target keeps its CodeView section"
    );
    assert!(
        !contains_subslice(&bytes, b".debug_line"),
        "the COFF target must not gain DWARF sections"
    );
}
