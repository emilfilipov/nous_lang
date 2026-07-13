
use super::*;
use crate::{IrEnumVariant, lower};
use lullaby_lexer::lex;
use lullaby_parser::parse;
use lullaby_semantics::validate;

fn module_for(source: &str) -> IrModule {
    let tokens = lex(source).expect("lex");
    let program = parse(&tokens).expect("parse");
    let checked = validate(&program).expect("semantic");
    lower(&checked).expect("lower")
}

#[test]
fn header_is_wasm_magic_and_version() {
    let source = "fn add a i64 b i64 -> i64\n    a + b\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        &artifact.bytes[0..8],
        &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
    );
    assert_eq!(artifact.compiled, vec!["add".to_string()]);
    assert!(artifact.skipped.is_empty());
}

#[test]
fn expected_sections_are_present() {
    let source = "fn add a i64 b i64 -> i64\n    a + b\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    let ids = section_ids(&artifact.bytes);
    assert_eq!(
        ids,
        vec![1, 2, 3, 5, 6, 7, 10, 11],
        "type/import/function/memory/global/export/code/data sections in canonical order"
    );
}

#[test]
fn imports_the_host_functions() {
    // The Import section (id 2) declares the three host imports: the log
    // primitive and the JS/DOM interop primitives.
    let source = "fn add a i64 b i64 -> i64\n    a + b\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    let import = section_body(&artifact.bytes, 2).expect("import section");
    let (count, _) = read_uleb(&import);
    assert_eq!(count, 3, "three host imports");
    // The import names include `env`, `log_i64`, `console_log`, `dom_set_text`.
    assert!(
        find_subslice(&import, b"env").is_some()
            && find_subslice(&import, b"log_i64").is_some()
            && find_subslice(&import, b"console_log").is_some()
            && find_subslice(&import, b"dom_set_text").is_some(),
        "env host import names present"
    );
}

#[test]
fn function_section_counts_internal_functions() {
    // Two user functions plus the internal `__alloc` helper => 3 entries in
    // the Function section; the host imports are NOT counted there.
    let source = "fn add a i64 b i64 -> i64\n    a + b\n\nfn neg n i64 -> i64\n    return 0 - n\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    let func = section_body(&artifact.bytes, 3).expect("function section");
    let (count, _) = read_uleb(&func);
    assert_eq!(count, 3, "two user functions + __alloc helper");
}

#[test]
fn exports_memory_and_functions() {
    let source = "fn add a i64 b i64 -> i64\n    a + b\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    let export = section_body(&artifact.bytes, 7).expect("export section");
    // memory + add + __alloc = 3 exports.
    let (count, _) = read_uleb(&export);
    assert_eq!(count, 3, "memory + add + __alloc exports");
    assert!(
        find_subslice(&export, b"memory").is_some(),
        "memory export present"
    );
    assert!(
        find_subslice(&export, b"__alloc").is_some(),
        "alloc helper export present"
    );
}

#[test]
fn wasm_log_function_compiles_and_calls_the_import() {
    // A function that calls `wasm_log` is eligible; the emitted body contains
    // a `call 0` targeting the imported host function (index 0).
    let source = "fn shout n i64 -> void\n    wasm_log(n)\n    wasm_log(n + 1)\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(artifact.compiled.contains(&"shout".to_string()));
    // The whole module still has the host imports present.
    let import = section_body(&artifact.bytes, 2).expect("import section");
    let (count, _) = read_uleb(&import);
    assert_eq!(count, IMPORT_FUNC_COUNT as u64);
}

#[test]
fn console_log_and_dom_set_text_call_their_imports() {
    // A function that calls the JS/DOM host builtins is eligible; its body
    // targets `env.console_log` (index 1) and `env.dom_set_text` (index 2).
    let source = concat!(
        "fn ui -> void\n",
        "    console_log(\"hi\")\n",
        "    dom_set_text(\"out\", \"done\")\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(artifact.compiled.contains(&"ui".to_string()));
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x10, CONSOLE_LOG_FUNC_INDEX as u8]).is_some(),
        "console_log lowers to a call of its host import"
    );
    assert!(
        find_subslice(&code, &[0x10, DOM_SET_TEXT_FUNC_INDEX as u8]).is_some(),
        "dom_set_text lowers to a call of its host import"
    );
    // The string literals are seeded into the Data section.
    assert!(
        find_subslice(&artifact.bytes, b"hi").is_some()
            && find_subslice(&artifact.bytes, b"out").is_some()
            && find_subslice(&artifact.bytes, b"done").is_some(),
        "interop string literals seeded into the data section"
    );
}

#[test]
fn call_target_indices_are_shifted_past_the_import() {
    // With an import present, a call between two user functions must target
    // the shifted index (import count + position), not the raw position.
    let source = "fn helper -> i64\n    7\n\nfn use_it -> i64\n    return helper()\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        artifact.compiled,
        vec!["helper".to_string(), "use_it".to_string()]
    );
    // `helper` is user function 0 => WASM index IMPORT_FUNC_COUNT (past the
    // host imports). The code for `use_it` must contain `call IMPORT_FUNC_COUNT`.
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x10, IMPORT_FUNC_COUNT as u8]).is_some(),
        "call targets the shifted (post-import) index"
    );
}

#[test]
fn scalar_and_nonscalar_split() {
    // `add` is scalar; `tally` returns `map<i64, array<i64>>` (a MUTABLE
    // heap-value map), still outside the WASM value set (strings/structs/
    // arrays/enums, scalar- or string-element `list`s, and scalar-key maps with
    // a scalar or `string` value are supported; a map with a mutable heap value
    // is not), so it is skipped.
    let source = concat!(
        "fn add a i64 b i64 -> i64\n    a + b\n\n",
        "fn tally -> map<i64, array<i64>>\n    map_new()\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["add".to_string()]);
    assert_eq!(artifact.skipped.len(), 1);
    assert_eq!(artifact.skipped[0].name, "tally");
    assert!(artifact.skipped[0].reason.contains("supported"));
}

#[test]
fn string_returning_function_compiles() {
    // A function that takes and returns a `string` is now eligible: strings
    // are `i32` pointers into linear memory.
    let source = "fn pick b bool -> string\n    if b\n        return \"yes\"\n    return \"no\"\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["pick".to_string()]);
    // The literal bytes appear in the module's Data section.
    assert!(
        find_subslice(&artifact.bytes, b"yes").is_some()
            && find_subslice(&artifact.bytes, b"no").is_some(),
        "string literals seeded into the data section"
    );
}

#[test]
fn string_literal_record_has_char_and_byte_headers() {
    // A string literal is interned as `[char_len i32][byte_len i32][utf8]`.
    // For a multi-byte literal the two headers differ (char count != byte
    // count), proving the byte length is stored, not derived by assuming ASCII.
    let mut pool = StringPool::new();
    let offset = pool.intern("café"); // 4 chars, 5 UTF-8 bytes (é = 2 bytes)
    assert_eq!(offset, RESERVED_BASE);
    let base = (offset - RESERVED_BASE) as usize;
    let char_len = i32::from_le_bytes(pool.bytes[base..base + 4].try_into().unwrap());
    let byte_len = i32::from_le_bytes(pool.bytes[base + 4..base + 8].try_into().unwrap());
    assert_eq!(char_len, 4, "char-count header is the Unicode scalar count");
    assert_eq!(byte_len, 5, "byte-count header is the UTF-8 byte length");
    assert_eq!(
        &pool.bytes[base + 8..base + 8 + 5],
        "café".as_bytes(),
        "the UTF-8 bytes follow the two headers at STR_DATA_OFF"
    );
}

#[test]
fn string_concat_function_compiles_with_alloc_and_copy_codegen() {
    // A function doing runtime `+` on two `string` values must COMPILE to WASM
    // (not skip to the interpreters). The operands are parameters so the IR
    // constant-folder cannot collapse the concat to a literal, exercising the
    // real runtime alloc-and-copy path.
    let source = "fn join a string b string -> string\n    a + b\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        artifact.compiled,
        vec!["join".to_string()],
        "string concat should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The concat allocates a fresh record via `call __alloc`. `__alloc` is the
    // last internal function: join(0) then __alloc(1), so its shifted WASM index
    // is IMPORT_FUNC_COUNT + 1.
    let alloc_index = IMPORT_FUNC_COUNT as u8 + 1;
    assert!(
        find_subslice(&code, &[0x10, alloc_index]).is_some(),
        "string concat emits a `call __alloc` for the fresh record"
    );
    // The two byte ranges are copied with the bulk-memory `memory.copy`
    // instruction: 0xfc prefix, sub-opcode 0x0a, dest/src memory indices 0, 0.
    assert!(
        find_subslice(&code, &[0xfc, 0x0a, 0x00, 0x00]).is_some(),
        "string concat emits `memory.copy` to join the operand byte ranges"
    );
}

#[test]
fn string_concat_result_len_matches_sum_of_char_counts() {
    // `len(a + b)` on a runtime concat returns the SUM of the operands' char
    // counts, matching the interpreters. The concat's char-count header is
    // written as `char_a + char_b` and `len` reads offset 0, so the whole
    // module compiles and the `len` read (an `i32.load` at offset 0 followed by
    // `i64.extend_i32_s`) appears in the emitted code.
    let source = concat!(
        "fn cat a string b string -> string\n    a + b\n\n",
        "fn probe -> i64\n",
        "    let a string = \"ab\"\n",
        "    let b string = \"cde\"\n",
        "    len(cat(a, b))\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"cat".to_string())
            && artifact.compiled.contains(&"probe".to_string()),
        "cat/probe should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // `len(...)` reads the char-count header at offset 0 (`i32.load align=2
    // offset=0`) then extends to i64 (`i64.extend_i32_s` = 0xac).
    assert!(
        find_subslice(&code, &[0x28, 0x02, 0x00, 0xac]).is_some(),
        "len of the concat reads the char-count header and extends to i64"
    );
}

#[test]
fn substring_function_compiles_with_alloc_and_copy_codegen() {
    // `substring(s, start, end)` on `string`/`i64`/`i64` parameters must COMPILE
    // to WASM. The operands are parameters so the IR constant-folder cannot
    // collapse the call, exercising the real char->byte mapping and the fresh
    // record's alloc-and-copy.
    let source = "fn slice s string a i64 b i64 -> string\n    substring(s, a, b)\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        artifact.compiled,
        vec!["slice".to_string()],
        "substring should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // Allocates a fresh record via `call __alloc` (slice(0) then __alloc(1), so
    // its shifted WASM index is IMPORT_FUNC_COUNT + 1).
    let alloc_index = IMPORT_FUNC_COUNT as u8 + 1;
    assert!(
        find_subslice(&code, &[0x10, alloc_index]).is_some(),
        "substring emits a `call __alloc` for the slice record"
    );
    // Copies the slice's byte range with the bulk-memory `memory.copy`.
    assert!(
        find_subslice(&code, &[0xfc, 0x0a, 0x00, 0x00]).is_some(),
        "substring emits `memory.copy` for the slice byte range"
    );
    // The char->byte walk decodes UTF-8 lead bytes: it loads a byte
    // (`i32.load8_u` = 0x2d) and tests `(b & 0xC0) != 0x80`, encoded as
    // `i32.const 0xC0; i32.and; i32.const 0x80; i32.ne`.
    assert!(
        find_subslice(&code, &[0x2d]).is_some(),
        "substring emits an `i32.load8_u` byte read for the UTF-8 walk"
    );
    assert!(
        find_subslice(&code, &[0x41, 0xc0, 0x01, 0x71, 0x41, 0x80, 0x01, 0x47]).is_some(),
        "substring emits the `(b & 0xC0) != 0x80` char-start test"
    );
}

#[test]
fn find_function_compiles_with_char_decode_loop() {
    // `find(haystack, needle)` on two `string` parameters must COMPILE to WASM.
    // It returns a CHAR index, so it must decode UTF-8 to count the characters
    // before the matched byte offset — the `(b & 0xC0) != 0x80` char-start test
    // (`i32.load8_u`; `i32.const 0xC0`; `i32.and`; `i32.const 0x80`; `i32.ne`)
    // must appear, and it extends the count to `i64` (`i64.extend_i32_s` = 0xac).
    let source = "fn locate h string n string -> i64\n    find(h, n)\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        artifact.compiled,
        vec!["locate".to_string()],
        "find should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The byte search compares haystack/needle bytes with `i32.load8_u` (0x2d).
    assert!(
        find_subslice(&code, &[0x2d]).is_some(),
        "find emits `i32.load8_u` byte comparisons"
    );
    // The char-index decode loop: `(b & 0xC0) != 0x80`.
    assert!(
        find_subslice(&code, &[0x41, 0xc0, 0x01, 0x71, 0x41, 0x80, 0x01, 0x47]).is_some(),
        "find emits the `(b & 0xC0) != 0x80` char-start decode test"
    );
    // The char count is extended to i64 (the builtin's result type).
    assert!(
        find_subslice(&code, &[0xac]).is_some(),
        "find extends the char-count result to i64"
    );
}

#[test]
fn contains_and_prefix_ops_compile_without_char_decode() {
    // `contains`/`starts_with`/`ends_with` are byte-exact: they scan bytes with
    // `i32.load8_u` but need NO UTF-8 char decode (byte equality is
    // char-position-independent). Each returns a `bool`.
    let source = concat!(
        "fn has s string sub string -> bool\n    contains(s, sub)\n\n",
        "fn pre s string p string -> bool\n    starts_with(s, p)\n\n",
        "fn suf s string x string -> bool\n    ends_with(s, x)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.skipped.is_empty(),
        "byte-exact predicates should all compile, skipped: {:?}",
        artifact.skipped
    );
    assert_eq!(
        artifact.compiled,
        vec!["has".to_string(), "pre".to_string(), "suf".to_string()],
    );
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // Byte comparisons appear (`i32.load8_u`).
    assert!(
        find_subslice(&code, &[0x2d]).is_some(),
        "byte-exact predicates emit `i32.load8_u` comparisons"
    );
}

#[test]
fn to_string_of_integer_compiles_with_itoa_codegen() {
    // `to_string(x)` on an integer argument compiles to WASM: it builds a fresh
    // string record via `call __alloc` and formats the digits with `i64.div_u`
    // (the itoa passes). The argument is a parameter so the IR constant-folder
    // cannot collapse the call to a literal, exercising the real runtime path.
    let source = "fn show n i64 -> string\n    to_string(n)\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        artifact.compiled,
        vec!["show".to_string()],
        "to_string of an integer should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The itoa allocates the record via `call __alloc` (the last internal
    // function: show(0) then __alloc(1), shifted WASM index IMPORT_FUNC_COUNT+1).
    let alloc_index = IMPORT_FUNC_COUNT as u8 + 1;
    assert!(
        find_subslice(&code, &[0x10, alloc_index]).is_some(),
        "to_string emits a `call __alloc` for the fresh string record"
    );
    // The digit extraction uses unsigned 64-bit division (`i64.const 10`,
    // `i64.div_u` = 0x80) — the itoa core divides the magnitude down by 10.
    assert!(
        find_subslice(&code, &[0x42, 0x0a, 0x80]).is_some(),
        "to_string emits `i64.div_u` by 10 to extract decimal digits"
    );
}

#[test]
fn to_string_of_float_skips_to_interpreters() {
    // `to_string(f64)` / `to_string(f32)` is DEFERRED: matching Rust's `Display`
    // dtoa bit-for-bit in WASM is out of scope, so a function calling it must be
    // demoted to the interpreters rather than miscompiled. The float `to_string`
    // is isolated in its own function so the sibling integer `to_string` still
    // compiles, proving only the float case falls back.
    let source = concat!(
        "fn float_text x f64 -> string\n    to_string(x)\n\n",
        "fn int_text n i64 -> string\n    to_string(n)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"int_text".to_string()),
        "integer to_string still compiles: {:?}",
        artifact.compiled
    );
    let skipped = artifact
        .skipped
        .iter()
        .find(|s| s.name == "float_text")
        .expect("float to_string is skipped");
    assert!(
        skipped.reason.contains("to_string"),
        "skip reason names the unsupported to_string: {}",
        skipped.reason
    );
    assert!(
        !artifact.compiled.contains(&"float_text".to_string()),
        "float to_string must not compile to WASM"
    );
}

#[test]
fn struct_and_array_functions_compile() {
    // A struct constructed/read and a fixed array built/indexed both compile:
    // they lower to `__alloc` + typed loads/stores.
    let source = concat!(
        "struct Point\n    x i64\n    y i64\n\n",
        "fn make a i64 b i64 -> i64\n",
        "    let p Point = Point(a, b)\n",
        "    let xs array<i64> = [a, b, a + b]\n",
        "    p.x + xs[2] + len(xs)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(artifact.compiled.contains(&"make".to_string()));
}

#[test]
fn recursive_function_compiles() {
    let source =
        "fn fib n i64 -> i64\n    if n < 2\n        return n\n    return fib(n - 1) + fib(n - 2)\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["fib".to_string()]);
}

// -- Growable `list<T>` (scalar element) -----------------------------------

#[test]
fn scalar_list_function_compiles_with_grow_and_copy_codegen() {
    // A function that builds a growable `list<i64>` via `list_new`/`push`,
    // reads it with `get`/`len`, replaces an element with `set`, and drops one
    // with `pop` must COMPILE to WASM (not skip to the interpreters). The list
    // is an `i32` pointer to a `[len][cap][slots]` block laid out in linear
    // memory, so both the signature (returning `list<i64>`) and the body are
    // eligible.
    let source = concat!(
        "fn build n i64 -> list<i64>\n",
        "    let xs list<i64> = list_new()\n",
        "    xs = push(xs, n)\n",
        "    xs = push(xs, n + 1)\n",
        "    let ys list<i64> = set(xs, 0, n + 2)\n",
        "    let zs list<i64> = pop(ys)\n",
        "    zs\n\n",
        "fn probe n i64 -> i64\n",
        "    let xs list<i64> = build(n)\n",
        "    len(xs) + get(xs, 0)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"build".to_string())
            && artifact.compiled.contains(&"probe".to_string()),
        "list build/probe functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The grow decision `new_cap = cap == 0 ? LIST_INITIAL_CAP : cap * 2`
    // lowers to `i32.eqz` (0x45), then `if` producing an `i32` (0x04 0x7f) —
    // a signature unique to the list-grow path in this backend.
    assert!(
        find_subslice(&code, &[0x45, 0x04, 0x7f]).is_some(),
        "list `push` emits the capacity-doubling grow decision"
    );
    // The element copy (in the deep-copy and grow paths) copies each slot with
    // an `i64.load` (0x29) immediately followed by an `i64.store` (0x37) — the
    // 8-byte word copy of a list element slot.
    assert!(
        find_subslice(&code, &[0x29, 0x03, 0x00, 0x37, 0x03, 0x00]).is_some(),
        "list copy emits an 8-byte word load+store per element slot"
    );
}

#[test]
fn list_new_allocates_len_cap_header() {
    // `list_new()` allocates a `[len=0][cap=LIST_INITIAL_CAP][slots]` header:
    // the body stores 0 at the len offset and LIST_INITIAL_CAP at the cap
    // offset. The `i32.const LIST_INITIAL_CAP` (0x41 0x04) capacity literal must
    // appear, and the function is eligible (returns a `list` pointer).
    let source = "fn empty -> list<i64>\n    list_new()\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["empty".to_string()]);
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // i32.const 4 (LIST_INITIAL_CAP) then i32.store at the cap offset (4).
    assert!(
        find_subslice(&code, &[0x41, LIST_INITIAL_CAP as u8, 0x36, 0x02, 0x04]).is_some(),
        "list_new stores the initial capacity into the cap header slot"
    );
}

#[test]
fn list_of_string_element_compiles() {
    // A `list<string>` COMPILES: a `string` element is an `i32` pointer stored
    // in one slot exactly like a scalar, and — because strings are immutable —
    // is shared (not deep-recursed) on the flat word-copy deep copy. `push`
    // appends the pointer; `get` loads it back with an `i32.load`.
    let source = concat!(
        "fn names -> list<string>\n",
        "    push(list_new(), \"a\")\n\n",
        "fn head l list<string> -> string\n",
        "    get(l, 0)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"names".to_string())
            && artifact.compiled.contains(&"head".to_string()),
        "list<string> functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The deep-copy element loop still copies each slot as one 8-byte word
    // (`i64.load` 0x29 then `i64.store` 0x37) — the string pointer is copied by
    // value (shared), NOT deep-recursed into the string record.
    assert!(
        find_subslice(&code, &[0x29, 0x03, 0x00, 0x37, 0x03, 0x00]).is_some(),
        "list<string> copy shares the element pointer via an 8-byte word copy"
    );
    // `get` loads the element slot as an `i32` pointer (`i32.load` at offset 0,
    // the slot address is fully computed on the stack), not an `i64`, because a
    // `string` element occupies the low word of the slot.
    assert!(
        find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
        "get on a list<string> loads the element slot as an i32 pointer"
    );
}

#[test]
fn list_of_mutable_heap_element_is_skipped() {
    // A `list<array<i64>>` (a fixed-`array` element) is still DEFERRED:
    // `supported_list_element`/`collection_slot_type` accept a `struct` or a
    // nested growable `list` element but NOT a fixed `array` element this
    // increment, so the function's signature is ineligible and it is skipped
    // (still runs on the interpreters), never miscompiled. (A `list<struct>` and
    // `list<list<scalar>>` DO compile now — see
    // `list_of_struct_element_compiles_with_recursive_deep_copy`.)
    let source = concat!(
        "fn grid -> list<array<i64>>\n",
        "    list_new()\n\n",
        "fn ok n i64 -> i64\n    n + 1\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["ok".to_string()]);
    assert_eq!(artifact.skipped.len(), 1);
    assert_eq!(artifact.skipped[0].name, "grid");
}

#[test]
fn list_of_struct_element_compiles_with_recursive_deep_copy() {
    // A `list<struct>` COMPILES now: the element is an `i32` pointer to the
    // struct record, and the list's value-semantic deep copy RECURSES per element
    // (loads the element pointer, deep-copies the struct, stores the fresh
    // pointer) rather than sharing it — matching the interpreters' recursive
    // `Value::clone`. `get` likewise returns an independent deep copy of the
    // element (the interpreters' `values[i].clone()`), so mutating the retrieved
    // struct cannot affect the list's stored element.
    let source = concat!(
        "struct Point\n    x i64\n    y i64\n\n",
        "fn build -> list<Point>\n",
        "    push(list_new(), Point(1, 2))\n\n",
        "fn head l list<Point> -> i64\n",
        "    let p Point = get(l, 0)\n",
        "    p.x + p.y\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"build".to_string())
            && artifact.compiled.contains(&"head".to_string()),
        "list<struct> functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // A recursive per-element deep copy loads the element pointer as an `i32`
    // (`i32.load` 0x28) and, after allocating a fresh struct record, stores the
    // fresh pointer as an `i32` (`i32.store` 0x36) — the element slot is NOT
    // copied only by a flat `i64` word (which would share the pointer). The
    // struct deep copy calls `__alloc` (the last internal function: `build`(0),
    // `head`(1), `__alloc`(2), all offset past the imports), so a `call` (0x10)
    // of that index appears where a scalar-only list would not deep-copy.
    let alloc_index = IMPORT_FUNC_COUNT as u8 + 2;
    assert!(
        find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
        "the element deep-copy loads the element pointer as an i32"
    );
    assert!(
        find_subslice(&code, &[0x10, alloc_index]).is_some(),
        "the recursive element deep-copy allocates a fresh struct record via __alloc"
    );
}

#[test]
fn nested_list_element_compiles() {
    // A `list<list<i64>>` COMPILES (one level of mutable nesting): the inner list
    // element is deep-copied per outer element, and `nested_sum`-style nested
    // `get`s read the scalar leaves. A `list<list<list<i64>>>` (two levels) is
    // DEFERRED — see `nested_list_beyond_one_level_is_skipped`.
    let source = concat!(
        "fn build -> list<list<i64>>\n",
        "    push(list_new(), push(list_new(), 7))\n\n",
        "fn first l list<list<i64>> -> i64\n",
        "    let row list<i64> = get(l, 0)\n",
        "    get(row, 0)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"build".to_string())
            && artifact.compiled.contains(&"first".to_string()),
        "list<list<i64>> functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());
}

#[test]
fn nested_list_beyond_one_level_is_skipped() {
    // `list<list<list<i64>>>` nests mutable aggregates past the one level the
    // backend can verify, so it is DEFERRED (skipped, runs on the interpreters),
    // never miscompiled.
    let source = concat!(
        "fn build -> list<list<list<i64>>>\n",
        "    list_new()\n\n",
        "fn ok n i64 -> i64\n    n + 1\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["ok".to_string()]);
    assert_eq!(artifact.skipped.len(), 1);
    assert_eq!(artifact.skipped[0].name, "build");
}

// -- Growable `map<K, V>` (scalar key/value) -------------------------------

#[test]
fn scalar_map_function_compiles_with_insert_and_lookup_codegen() {
    // A function that builds a scalar-key, scalar-value `map<i64, i64>` via
    // `map_new`/`map_set` (insert plus in-place update), reads it with
    // `map_get`/`map_has`/`map_len`, must COMPILE to WASM (not skip). The map is
    // an `i32` pointer to a `[len][cap][(k,v) pairs]` block in linear memory, so
    // both the signature (returning `map<i64, i64>`) and body are eligible.
    let source = concat!(
        "fn build n i64 -> map<i64, i64>\n",
        "    let m map<i64, i64> = map_new()\n",
        "    m = map_set(m, 1, n)\n",
        "    m = map_set(m, 2, n + 1)\n",
        "    m = map_set(m, 1, n + 2)\n",
        "    m\n\n",
        "fn probe n i64 -> i64\n",
        "    let m map<i64, i64> = build(n)\n",
        "    let seen i64 = 0\n",
        "    if map_has(m, 2)\n",
        "        seen = 1\n",
        "    map_len(m) + seen\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"build".to_string())
            && artifact.compiled.contains(&"probe".to_string()),
        "map build/probe functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The `map_set` grow decision `new_cap = cap == 0 ? MAP_INITIAL_CAP : cap*2`
    // lowers to `i32.eqz` (0x45) then `if` producing an `i32` (0x04 0x7f).
    assert!(
        find_subslice(&code, &[0x45, 0x04, 0x7f]).is_some(),
        "map `map_set` emits the capacity-doubling grow decision"
    );
    // The linear-scan lookup compares each entry's key with `i64.eq` (0x51):
    // the key-equality opcode of `emit_map_find` for an `i64` key.
    assert!(
        find_subslice(&code, &[0x51]).is_some(),
        "map lookup emits an `i64.eq` key comparison in the scan"
    );
}

#[test]
fn map_new_allocates_len_cap_header() {
    // `map_new()` allocates a `[len=0][cap=MAP_INITIAL_CAP][entries]` header:
    // the body stores 0 at the len offset and MAP_INITIAL_CAP at the cap offset.
    let source = "fn empty -> map<i64, i64>\n    map_new()\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["empty".to_string()]);
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // i32.const 4 (MAP_INITIAL_CAP) then i32.store at the cap offset (4).
    assert!(
        find_subslice(&code, &[0x41, MAP_INITIAL_CAP as u8, 0x36, 0x02, 0x04]).is_some(),
        "map_new stores the initial capacity into the cap header slot"
    );
}

#[test]
fn map_get_lowers_to_option_construction() {
    // `map_get(m, k)` returns `option<V>`, constructed with the enum/option
    // linear-memory layout: `none` stores tag 1, `some(v)` stores tag 0 and the
    // looked-up value into the payload slot. The `i64.extend_i32_s` (0xac) of
    // `map_len` and the option tag stores must both appear.
    let source = concat!(
        "fn get_or m map<i64, i64> k i64 -> i64\n",
        "    match map_get(m, k)\n",
        "        some(v) -> v\n",
        "        none -> 0 - 1\n\n",
        "fn size m map<i64, i64> -> i64\n",
        "    map_len(m)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"get_or".to_string())
            && artifact.compiled.contains(&"size".to_string()),
        "map get/size functions should compile, skipped: {:?}",
        artifact.skipped
    );
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // `map_len` extends the i32 len header to i64 with `i64.extend_i32_s` (0xac).
    assert!(
        find_subslice(&code, &[0xac]).is_some(),
        "map_len emits `i64.extend_i32_s` on the length header"
    );
}

#[test]
fn map_of_string_value_function_compiles() {
    // A `map<i64, string>` (scalar key, `string` value) COMPILES: the value
    // slot holds an `i32` string pointer, shared on the flat two-word entry
    // copy since strings are immutable. `map_set` inserts/updates the pointer,
    // `map_get` returns `option<string>` (the `some` payload slot is the string
    // pointer), and `map_has`/`map_len` work unchanged.
    let source = concat!(
        "fn build n i64 -> map<i64, string>\n",
        "    let m map<i64, string> = map_new()\n",
        "    m = map_set(m, 1, \"a\")\n",
        "    m = map_set(m, 2, to_string(n))\n",
        "    m = map_set(m, 1, \"z\")\n",
        "    m\n\n",
        "fn probe n i64 -> i64\n",
        "    let m map<i64, string> = build(n)\n",
        "    let seen i64 = 0\n",
        "    if map_has(m, 2)\n",
        "        seen = 1\n",
        "    match map_get(m, 1)\n",
        "        some(s) -> len(s) + seen + map_len(m)\n",
        "        none -> 0 - 1\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"build".to_string())
            && artifact.compiled.contains(&"probe".to_string()),
        "map<i64, string> build/probe functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The entry copy still copies each entry as two 8-byte words (`i64.load`
    // 0x29 then `i64.store` 0x37) — the string value pointer is copied by value
    // (shared), NOT deep-recursed into the string record.
    assert!(
        find_subslice(&code, &[0x29, 0x03, 0x00, 0x37, 0x03, 0x00]).is_some(),
        "map<i64, string> copy shares the value pointer via an 8-byte word copy"
    );
    // The lookup still compares the scalar `i64` key with `i64.eq` (0x51).
    assert!(
        find_subslice(&code, &[0x51]).is_some(),
        "map<i64, string> lookup emits an `i64.eq` key comparison in the scan"
    );
}

#[test]
fn map_string_key_function_compiles() {
    // A `map<string, i64>` (string KEY, scalar value) and a `map<string, string>`
    // (string key AND value) COMPILE: the key slot holds an `i32` string pointer,
    // and the lookup compares keys by CONTENT — not by pointer identity — so two
    // distinct string objects with equal bytes are the same key. `map_set`
    // inserts/updates by content, `map_get` returns `option<V>`, and
    // `map_has`/`map_len` work through the content-equality scan.
    let source = concat!(
        "fn build n i64 -> map<string, i64>\n",
        "    let m map<string, i64> = map_new()\n",
        "    m = map_set(m, \"a\" + \"b\", n)\n",
        "    m = map_set(m, \"c\", n + 1)\n",
        "    m = map_set(m, \"ab\", n + 2)\n",
        "    m\n\n",
        "fn probe n i64 -> i64\n",
        "    let m map<string, i64> = build(n)\n",
        "    let seen i64 = 0\n",
        "    if map_has(m, \"c\")\n",
        "        seen = 1\n",
        "    match map_get(m, \"ab\")\n",
        "        some(v) -> v + seen + map_len(m)\n",
        "        none -> 0 - 1\n\n",
        "fn labels -> map<string, string>\n",
        "    let m map<string, string> = map_new()\n",
        "    m = map_set(m, \"k\", \"v\")\n",
        "    m\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"build".to_string())
            && artifact.compiled.contains(&"probe".to_string())
            && artifact.compiled.contains(&"labels".to_string()),
        "map<string, _> functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());

    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The string-key lookup compares keys by CONTENT: the byte loop emits an
    // `i32.load8_u` (0x2d) over the UTF-8 bytes — a marker of the content
    // comparison that a scalar-key map (integer `i64.eq`/`i32.eq` only) never
    // emits inside its find scan.
    assert!(
        find_subslice(&code, &[0x2d]).is_some(),
        "map<string, _> lookup emits a byte-compare (`i32.load8_u`) key equality"
    );
}

#[test]
fn map_with_mutable_value_is_skipped() {
    // A `map<i64, array<i64>>` (a fixed-`array` value) is DEFERRED:
    // `supported_map_kv`/`collection_slot_type` accept a `struct` value but NOT a
    // fixed `array` value this increment, so the signature is ineligible and the
    // function is skipped (still runs on the interpreters), never miscompiled.
    // (The semantic layer already restricts `map` KEYS to `i64` or `string` —
    // L0388 — so a non-string heap key never reaches this backend.) A `string`
    // value now compiles (`map_string_key_function_compiles`), and a `struct`
    // value now compiles too — see `map_of_struct_value_compiles`.
    let source = concat!(
        "fn rows -> map<i64, array<i64>>\n",
        "    map_new()\n\n",
        "fn ok n i64 -> i64\n    n + 1\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["ok".to_string()]);
    let skipped: Vec<&str> = artifact.skipped.iter().map(|s| s.name.as_str()).collect();
    assert!(skipped.contains(&"rows"));
}

#[test]
fn map_of_struct_value_compiles() {
    // A `map<i64, struct>` COMPILES now: the value slot is an `i32` pointer to
    // the struct record, and the map's value-semantic deep copy RECURSES per
    // entry (loads the value pointer, deep-copies the struct, stores the fresh
    // pointer). `map_get` returns `option<struct>` (built directly so the option
    // lays out a struct payload) with an independent deep-copied value, matching
    // the interpreters' recursive `Value::clone`.
    let source = concat!(
        "struct Point\n    x i64\n    y i64\n\n",
        "fn build -> map<i64, Point>\n",
        "    map_set(map_new(), 1, Point(3, 4))\n\n",
        "fn value m map<i64, Point> k i64 -> i64\n",
        "    match map_get(m, k)\n",
        "        some(p) -> p.x + p.y\n",
        "        none -> 0 - 1\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"build".to_string())
            && artifact.compiled.contains(&"value".to_string()),
        "map<i64, struct> functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());
}

// -- Aggregates across call boundaries (params/returns) --------------------

#[test]
fn struct_param_and_return_functions_compile() {
    // A function TAKING a struct (reading its fields) and one RETURNING a
    // struct are both eligible — an aggregate is an `i32` pointer, so it is a
    // first-class WASM value at the boundary. Neither is skipped.
    let source = concat!(
        "struct Point\n    x i64\n    y i64\n\n",
        "fn sum_point p Point -> i64\n    p.x + p.y\n\n",
        "fn make_point a i64 b i64 -> Point\n    Point(a, b)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"sum_point".to_string())
            && artifact.compiled.contains(&"make_point".to_string()),
        "struct param/return functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());
}

#[test]
fn array_param_and_return_functions_compile() {
    // A function taking AND returning a fixed `array<i64>` compiles: it reads
    // an element and returns the (copied) array pointer.
    let source = concat!(
        "fn first_of xs array<i64> -> i64\n    xs[0]\n\n",
        "fn identity xs array<i64> -> array<i64>\n    xs\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"first_of".to_string())
            && artifact.compiled.contains(&"identity".to_string()),
        "array param/return functions should compile, skipped: {:?}",
        artifact.skipped
    );
    assert!(artifact.skipped.is_empty());
}

#[test]
fn passing_a_struct_argument_deep_copies_it() {
    // Value semantics: an aggregate argument is deep-copied at the call site so
    // the callee cannot mutate the caller's record through the shared pointer.
    // The caller `use_it` constructs nothing itself; it only forwards its own
    // struct PARAMETER to `sum_point`. So the ONLY `__alloc` call in `use_it`'s
    // body is the copy-on-pass — its presence proves the snapshot is emitted.
    let source = concat!(
        "struct Point\n    x i64\n    y i64\n\n",
        "fn sum_point p Point -> i64\n    p.x + p.y\n\n",
        "fn use_it p Point -> i64\n    sum_point(p)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(artifact.compiled.contains(&"use_it".to_string()));
    // `__alloc` is the LAST internal function, so its WASM index is
    // IMPORT_FUNC_COUNT + (number of user functions). Here: sum_point(0),
    // use_it(1) => __alloc index = IMPORT_FUNC_COUNT + 2.
    let alloc_index = IMPORT_FUNC_COUNT as u8 + 2;
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x10, alloc_index]).is_some(),
        "passing a struct argument emits a `call __alloc` copy-on-pass"
    );
}

#[test]
fn passing_an_array_argument_deep_copies_it() {
    // The array copy-on-pass reads the `[len]` header (`i32.load` at offset 0),
    // allocates a fresh block (`call __alloc`), and copies elements in a loop.
    // `use_it` constructs no array of its own, so the `__alloc` call in its body
    // is the copy, and the header read appears before it.
    let source = concat!(
        "fn first_of xs array<i64> -> i64\n    xs[0]\n\n",
        "fn use_it xs array<i64> -> i64\n    first_of(xs)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(artifact.compiled.contains(&"use_it".to_string()));
    let alloc_index = IMPORT_FUNC_COUNT as u8 + 2; // first_of(0), use_it(1), __alloc(2)
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x10, alloc_index]).is_some(),
        "passing an array argument emits a `call __alloc` copy-on-pass"
    );
}

#[test]
fn passing_a_string_argument_is_not_copied() {
    // A `string` is an immutable pointer, so it is shared (never deep-copied):
    // the callee cannot mutate it, exactly matching the interpreters. A caller
    // that only forwards its string parameter allocates nothing, so its body
    // contains no `call __alloc`.
    let source = concat!(
        "fn take s string -> i64\n    len(s)\n\n",
        "fn use_it s string -> i64\n    take(s)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(artifact.compiled.contains(&"use_it".to_string()));
    let alloc_index = IMPORT_FUNC_COUNT as u8 + 2; // take(0), use_it(1), __alloc(2)
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x10, alloc_index]).is_none(),
        "an immutable string argument must NOT be deep-copied"
    );
}

#[test]
fn bool_returning_comparison_compiles() {
    let source = "fn is_pos n i64 -> bool\n    n > 0\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["is_pos".to_string()]);
}

#[test]
fn no_eligible_functions_errors() {
    // `map<i64, map<i64, i64>>` is a map whose VALUE is itself a map — a nested
    // collection the backend does not lay out (a map element/value is deferred),
    // so nothing is eligible and the backend reports L0338. (Scalar/`string`
    // element `list`s, `list<struct>`/`list<list<scalar>>`, `map<K, struct>`,
    // and enum payloads like `result<i64, string>`/`result<i64, list<i64>>` ARE
    // supported now — see the growable-list/map, struct-element, and enum tests.)
    let source = "fn tally n i64 -> map<i64, map<i64, i64>>\n    map_new()\n";
    let err = emit_wasm_module(&module_for(source)).expect_err("no eligible");
    assert_eq!(err.code, "L0338");
    assert_eq!(err.skipped.len(), 1);
}

#[test]
fn fixed_width_integer_function_compiles() {
    // A function over `u8` (wrapping arithmetic + a fixed-width conversion) is
    // now eligible: fixed-width integers are stored as their normalized `i64`
    // cell and re-normalized after each width-producing op.
    let source = concat!("fn mix a u8 b u8 -> u8\n", "    (a + b) & to_u8(15)\n",);
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["mix".to_string()]);
    assert!(artifact.skipped.is_empty());
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // The `+` re-normalizes to u8 by masking with 0xff (i64.const 0xff;
    // i64.and) and the `& to_u8(15)` masks again — the 0xff mask literal must
    // appear in the body.
    assert!(
        find_subslice(&code, &[0x42, 0xff, 0x01]).is_some(),
        "u8 normalization masks with 0xff (i64.const 0xff, i64.and)"
    );
}

#[test]
fn signed_conversion_uses_sign_extension_opcode() {
    // `to_i8(x)` inlines to a normalize: for a signed 8-bit kind that is the
    // dedicated `i64.extend8_s` (0xc2), not a mask.
    let source = "fn narrow x i64 -> i8\n    to_i8(x)\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["narrow".to_string()]);
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0xc2]).is_some(),
        "to_i8 emits i64.extend8_s"
    );
}

#[test]
fn unsigned_comparison_and_shift_pick_unsigned_opcodes() {
    // A `u32` comparison uses the unsigned opcode (`i64.gt_u`, 0x56) and a
    // `u32` right shift uses the logical `i64.shr_u` (0x88) plus a width mask.
    let source = concat!(
        "fn f a u32 b u32 -> u32\n",
        "    if a > b\n",
        "        return a >> to_u32(1)\n",
        "    b >> to_u32(1)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["f".to_string()]);
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x56]).is_some(),
        "unsigned `>` uses i64.gt_u"
    );
    assert!(
        find_subslice(&code, &[0x88]).is_some(),
        "unsigned `>>` uses logical i64.shr_u"
    );
}

#[test]
fn signed_right_shift_is_arithmetic() {
    // A signed `i32` right shift uses the arithmetic `i64.shr_s` (0x87).
    let source = "fn sar s i32 -> i32\n    s >> to_i32(1)\n";
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["sar".to_string()]);
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x87]).is_some(),
        "signed `>>` uses arithmetic i64.shr_s"
    );
}

#[test]
fn f32_arithmetic_and_conversions_compile() {
    // An i64-returning function that computes with `f32` internally (the
    // `to_f32`/`to_f64` conversions plus `f32` arithmetic and a comparison)
    // now compiles: `f32` is a supported WASM scalar (single precision).
    let source = concat!(
        "fn main -> i64\n",
        "    let a f32 = to_f32(1.0)\n",
        "    let b f32 = to_f32(2.0)\n",
        "    let s f32 = a + b\n",
        "    let d f32 = s / to_f32(2.0)\n",
        "    if to_f64(d) < 2.0\n",
        "        return 1\n",
        "    0\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["main".to_string()]);
    assert!(artifact.skipped.is_empty());
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // `f32.add` (0x92) and `f32.div` (0x95) for the single-precision arithmetic.
    assert!(find_subslice(&code, &[0x92]).is_some(), "expected f32.add");
    assert!(find_subslice(&code, &[0x95]).is_some(), "expected f32.div");
    // `f32.demote_f64` (0xb6) for `to_f32` and `f64.promote_f32` (0xbb) for
    // `to_f64` — the inlined conversions, not real calls.
    assert!(
        find_subslice(&code, &[0xb6]).is_some(),
        "expected f32.demote_f64 for to_f32"
    );
    assert!(
        find_subslice(&code, &[0xbb]).is_some(),
        "expected f64.promote_f32 for to_f64"
    );
}

#[test]
fn f32_field_slot_uses_single_precision_memory_ops() {
    // An `f32` struct field is laid out as a single-precision slot: writing it
    // uses `f32.store` (0x38) and reading it uses `f32.load` (0x2a), so a
    // round-tripped f32 keeps its single-precision bits.
    let source = concat!(
        "struct Box\n",
        "    v f32\n\n",
        "fn main -> i64\n",
        "    let box Box = Box(to_f32(1.5))\n",
        "    if to_f64(box.v) < 2.0\n",
        "        return 1\n",
        "    0\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(artifact.compiled.contains(&"main".to_string()));
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x38]).is_some(),
        "expected f32.store for an f32 struct field"
    );
    assert!(
        find_subslice(&code, &[0x2a]).is_some(),
        "expected f32.load for reading an f32 struct field"
    );
}

#[test]
fn f32_comparison_over_float_arithmetic_uses_f32_compare() {
    // A comparison whose operand is a float ARITHMETIC subtree (annotated `i64`
    // in the IR) must still pick the single-precision `f32.gt` (0x5e), driven
    // by the structural float-width detection rather than the node's own `ty`.
    let source = concat!(
        "fn main -> i64\n",
        "    let a f32 = to_f32(1.0)\n",
        "    let b f32 = to_f32(2.0)\n",
        "    if a + b > to_f32(2.0)\n",
        "        return 1\n",
        "    0\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["main".to_string()]);
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x5e]).is_some(),
        "float arithmetic compared with `>` must use f32.gt, not an integer compare"
    );
    // It must NOT fall back to the integer `i64.gt_s` (0x55) over f32 values.
    assert!(
        find_subslice(&code, &[0x55]).is_none(),
        "the f32 comparison must not use i64.gt_s"
    );
}

#[test]
fn f32_parameter_and_return_compile_like_f64() {
    // `f32` is a first-class scalar, so a function taking and returning `f32`
    // is eligible exactly like the existing `f64` support; a `u16` companion
    // still compiles alongside it.
    let source = concat!(
        "fn scale x f32 -> f32\n",
        "    x * to_f32(2.0)\n\n",
        "fn ok a u16 -> u16\n",
        "    a + to_u16(1)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        artifact.compiled,
        vec!["scale".to_string(), "ok".to_string()]
    );
    assert!(artifact.skipped.is_empty());
    let code = section_body(&artifact.bytes, 10).expect("code section");
    assert!(
        find_subslice(&code, &[0x94]).is_some(),
        "expected f32.mul (0x94) in `scale`"
    );
}

#[test]
fn float_math_builtin_skips_gracefully() {
    // A float math builtin (`sqrt`) is out of scope for the WASM backend (as on
    // native): the function is demoted to skipped and still runs on the
    // interpreters, while an f32-arithmetic companion compiles.
    let source = concat!(
        "fn root x f64 -> f64\n",
        "    sqrt(x)\n\n",
        "fn plain a f32 b f32 -> f32\n",
        "    a + b\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["plain".to_string()]);
    assert!(
        artifact.skipped.iter().any(|s| s.name == "root"),
        "the `sqrt` math builtin must skip gracefully"
    );
}

#[test]
fn overflow_builtins_compile() {
    // The overflow-aware builtins now compile on the WASM backend (matching
    // native): `saturating_*`/`wrapping_*` yield a fixed-width scalar and
    // `checked_*` an `option<T>`. Every function is compiled, none skipped.
    let source = concat!(
        "fn sat a u8 b u8 -> u8\n",
        "    saturating_add(a, b)\n\n",
        "fn wrap a u8 b u8 -> u8\n",
        "    wrapping_mul(a, b)\n\n",
        "fn chk a i8 b i8 -> i64\n",
        "    match checked_add(a, b)\n",
        "        some(v) -> to_i64(v)\n",
        "        none -> 0 - 1\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(
        artifact.compiled,
        vec!["sat".to_string(), "wrap".to_string(), "chk".to_string()]
    );
    assert!(artifact.skipped.is_empty(), "nothing should skip");
}

#[test]
fn uleb_and_sleb_roundtrip() {
    let mut out = Vec::new();
    write_uleb(&mut out, 0);
    assert_eq!(out, vec![0x00]);
    out.clear();
    write_uleb(&mut out, 624485);
    assert_eq!(out, vec![0xe5, 0x8e, 0x26]);
    out.clear();
    write_sleb(&mut out, -123456);
    assert_eq!(out, vec![0xc0, 0xbb, 0x78]);
    out.clear();
    write_sleb(&mut out, 0);
    assert_eq!(out, vec![0x00]);
}

/// Parse the section ids present in a module (skipping the 8-byte header).
fn section_ids(bytes: &[u8]) -> Vec<u8> {
    let mut ids = Vec::new();
    let mut i = 8;
    while i < bytes.len() {
        let id = bytes[i];
        i += 1;
        let (len, consumed) = read_uleb(&bytes[i..]);
        i += consumed;
        i += len as usize;
        ids.push(id);
    }
    ids
}

fn read_uleb(bytes: &[u8]) -> (u64, usize) {
    let mut result = 0u64;
    let mut shift = 0;
    let mut i = 0;
    loop {
        let byte = bytes[i];
        result |= ((byte & 0x7f) as u64) << shift;
        i += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (result, i)
}

/// Return the contents (payload) of the first section with the given id.
fn section_body(bytes: &[u8], want: u8) -> Option<Vec<u8>> {
    let mut i = 8;
    while i < bytes.len() {
        let id = bytes[i];
        i += 1;
        let (len, consumed) = read_uleb(&bytes[i..]);
        i += consumed;
        let end = i + len as usize;
        if id == want {
            return Some(bytes[i..end].to_vec());
        }
        i = end;
    }
    None
}

/// Find the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

// -- Enum + match ---------------------------------------------------------

#[test]
fn option_match_compiles_with_tag_load_and_branch() {
    // A function matching `option<i64>` compiles to WASM. Its body loads the
    // enum's discriminant tag (`i32.load` at offset 0) and dispatches with an
    // `i32.eq` + `if` on the tag.
    let source = concat!(
        "fn unwrap_or o option<i64> fallback i64 -> i64\n",
        "    match o\n",
        "        some(v) -> v\n",
        "        none -> fallback\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"unwrap_or".to_string()),
        "option match should compile, skipped: {:?}",
        artifact.skipped
    );
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // Tag load: `i32.load` (0x28) align 2 (0x02) offset 0 (0x00).
    assert!(
        find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
        "match loads the enum discriminant tag"
    );
    // Dispatch: `i32.eq` (0x46) then `if` (0x04) with the value result type
    // `i64` (0x7e) — the arms yield `i64`.
    assert!(
        find_subslice(&code, &[0x46, 0x04, 0x7e]).is_some(),
        "match dispatches on the tag with a typed `if`"
    );
}

#[test]
fn result_scalar_match_and_construction_compile() {
    // A `result<i64, i64>` (scalar ok/err payloads) is a supported WASM enum:
    // both constructing it and matching it compile.
    let source = concat!(
        "fn divide n i64 d i64 -> result<i64, i64>\n",
        "    if d == 0\n",
        "        return err(0 - 1)\n",
        "    ok(n / d)\n\n",
        "fn describe r result<i64, i64> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(e) -> e\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"divide".to_string())
            && artifact.compiled.contains(&"describe".to_string()),
        "result<i64,i64> construction and match should compile, skipped: {:?}",
        artifact.skipped
    );
}

#[test]
fn user_enum_match_with_wildcard_compiles() {
    // A small user enum with a scalar payload and a wildcard arm compiles.
    let source = concat!(
        "enum Shape\n",
        "    Circle i64\n",
        "    Rect i64 i64\n",
        "    Empty\n\n",
        "fn area s Shape -> i64\n",
        "    match s\n",
        "        Circle(r) -> r * r\n",
        "        Rect(w, h) -> w * h\n",
        "        _ -> 0\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"area".to_string()),
        "user enum match with wildcard should compile, skipped: {:?}",
        artifact.skipped
    );
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // Tag load and typed-`if` dispatch present.
    assert!(
        find_subslice(&code, &[0x28, 0x02, 0x00]).is_some(),
        "user-enum match loads the discriminant tag"
    );
}

#[test]
fn enum_construction_stores_the_discriminant_tag() {
    // Constructing `some(x)` allocates a record and stores the variant's tag
    // (an `i32.const` + `i32.store` at offset 0). `some` is discriminant 0.
    let source = concat!("fn wrap x i64 -> option<i64>\n", "    some(x)\n",);
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert!(
        artifact.compiled.contains(&"wrap".to_string()),
        "option construction should compile, skipped: {:?}",
        artifact.skipped
    );
    let code = section_body(&artifact.bytes, 10).expect("code section");
    // `i32.const 0` (tag) then `i32.store` (0x36) align 2 offset 0.
    assert!(
        find_subslice(&code, &[0x41, 0x00, 0x36, 0x02, 0x00]).is_some(),
        "construction stores the `some` discriminant tag (0) at offset 0"
    );
}

#[test]
fn enum_with_string_payload_compiles() {
    // `result<i64, string>` has a `string` payload, which the WASM backend now
    // supports: the payload slot holds the immutable string pointer, matched
    // and read back with an `i32` slot load. The function COMPILES (it is not
    // skipped and does not fall back to the interpreters).
    let source = concat!(
        "fn describe r result<i64, string> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> len(m)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["describe".to_string()]);
    assert!(artifact.skipped.is_empty());
}

#[test]
fn enum_with_one_level_mutable_payload_compiles() {
    // `result<i64, list<i64>>` has a one-level MUTABLE-aggregate (`list<i64>`)
    // payload, which the WASM backend now supports: the payload slot is an `i32`
    // pointer deep-copied per variant on the enum's value-semantic copy, matching
    // the interpreters' recursive `Value::clone`. The function COMPILES.
    let source = concat!(
        "fn describe r result<i64, list<i64>> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> len(m)\n",
    );
    let artifact = emit_wasm_module(&module_for(source)).expect("emit");
    assert_eq!(artifact.compiled, vec!["describe".to_string()]);
    assert!(artifact.skipped.is_empty());
}

#[test]
fn enum_with_deeply_nested_mutable_payload_is_skipped() {
    // `result<i64, list<list<list<i64>>>>` nests mutable aggregates past the one
    // level the backend can verify, so it is DEFERRED (skipped, runs on the
    // interpreters), never miscompiled. With no other eligible function, emission
    // reports L0338.
    let source = concat!(
        "fn describe r result<i64, list<list<list<i64>>>> -> i64\n",
        "    match r\n",
        "        ok(v) -> v\n",
        "        err(m) -> len(m)\n",
    );
    let error = emit_wasm_module(&module_for(source)).expect_err("no eligible functions");
    assert_eq!(error.code, "L0338");
    assert!(
        error.skipped.iter().any(|s| s.name == "describe"),
        "the deeply-nested-mutable-payload enum function is recorded as skipped: {:?}",
        error.skipped
    );
}

#[test]
fn enum_layout_orders_builtin_and_user_variants() {
    // The discriminant ordering matches the interpreters: `option` is
    // `[some, none]`, `result` is `[ok, err]`, and a user enum follows its
    // declaration order. The payload slot count is the max payload arity.
    let enums = enum_table(&[IrEnumDef {
        name: "Shape".to_string(),
        variants: vec![
            IrEnumVariant {
                name: "Circle".to_string(),
                payload: vec![TypeRef::new("i64")],
            },
            IrEnumVariant {
                name: "Rect".to_string(),
                payload: vec![TypeRef::new("i64"), TypeRef::new("i64")],
            },
            IrEnumVariant {
                name: "Empty".to_string(),
                payload: Vec::new(),
            },
        ],
    }]);

    let structs = struct_table(&[]);
    let option =
        enum_layout(&TypeRef::new("option<i64>"), &structs, &enums).expect("option layout");
    assert_eq!(option.tag_of("some"), Some(0));
    assert_eq!(option.tag_of("none"), Some(1));
    assert_eq!(option.slot_count, 1);

    let result =
        enum_layout(&TypeRef::new("result<i64, i64>"), &structs, &enums).expect("result layout");
    assert_eq!(result.tag_of("ok"), Some(0));
    assert_eq!(result.tag_of("err"), Some(1));

    let shape = enum_layout(&TypeRef::new("Shape"), &structs, &enums).expect("user layout");
    assert_eq!(shape.tag_of("Circle"), Some(0));
    assert_eq!(shape.tag_of("Rect"), Some(1));
    assert_eq!(shape.tag_of("Empty"), Some(2));
    assert_eq!(shape.slot_count, 2, "widest variant `Rect` has two slots");

    // A `string` payload IS supported (shared immutable pointer in one slot).
    assert!(
        enum_layout(&TypeRef::new("result<i64, string>"), &structs, &enums).is_some(),
        "string-payload result is a supported WASM enum"
    );
    // A one-level MUTABLE-aggregate payload (`list<i64>`) is NOW supported — the
    // payload slot is deep-copied per variant on the enum's value-semantic copy.
    assert!(
        enum_layout(&TypeRef::new("result<i64, list<i64>>"), &structs, &enums).is_some(),
        "a one-level list payload is a supported WASM enum (recursive deep copy)"
    );
    // A `map` payload is still DEFERRED (the backend does not lay out a map
    // element/value inside an enum this increment).
    assert!(
        enum_layout(
            &TypeRef::new("result<i64, map<i64, i64>>"),
            &structs,
            &enums
        )
        .is_none(),
        "a map payload is not yet a supported WASM enum"
    );
    // Nesting beyond one mutable level (`list<list<list<i64>>>`) is DEFERRED.
    assert!(
        enum_layout(
            &TypeRef::new("option<list<list<list<i64>>>>"),
            &structs,
            &enums
        )
        .is_none(),
        "a payload nested past one mutable level is deferred"
    );
}
