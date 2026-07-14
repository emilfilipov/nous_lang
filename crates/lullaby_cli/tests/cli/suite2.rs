//! CLI integration tests, part 2 (I/O, sockets, processes, threads, async,
//! WASM). Split out of tests/cli.rs; shares its helpers via `use crate::*`.

use crate::*;
use std::process::Command;

#[test]
pub(crate) fn wasm_emits_module_and_lists_functions() {
    // The scalar fixture: an arithmetic function, a recursive `if` function, a
    // bool-returning comparison, a `for`-loop function, plus a `main` the
    // interpreter uses for ground truth. Every function is in the scalar subset.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_scalars.wasm");
    let output = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(output.status.success(), "{}", stderr(&output));
    let listing = stdout(&output);
    for name in ["add", "fib", "is_even", "sum_to", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // The emitted file starts with the WASM magic + version 1.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert_eq!(
        &bytes[0..8],
        &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00],
        "wasm header"
    );
}

#[test]
pub(crate) fn wasm_reports_no_eligible_functions() {
    // A file whose only function uses a type outside the supported WASM value set
    // (strings/structs/arrays/enums, scalar-/string-/struct-/nested-list-element
    // `list`s, maps with a scalar or `string` value or a `struct` value, and enums
    // with scalar/`string`/one-level-mutable payloads are now supported): a map
    // whose VALUE is itself a map — `map<i64, map<i64, i64>>` — nests a collection
    // the backend does not lay out, so nothing is eligible and the WASM backend
    // reports L0338. `wasm` reuses the executable pipeline, which requires `main`;
    // make `main` itself return that type so nothing is eligible and the emitter
    // reports L0338 rather than compiling anything.
    let source = "fn main -> map<i64, map<i64, i64>>\n    map_new()\n";
    let tmp = std::env::temp_dir().join("lullaby_wasm_none.lby");
    std::fs::write(&tmp, source).expect("write temp");
    let output = lullaby()
        .args(["wasm", "--verbose", tmp.to_str().expect("temp path")])
        .output()
        .expect("run cli");
    assert!(!output.status.success());
    let rendered = format!("{}{}", stdout(&output), stderr(&output));
    assert!(rendered.contains("L0338"), "expected L0338: {rendered}");
    assert!(
        rendered.contains("skipped main"),
        "expected verbose skip reason: {rendered}"
    );
}

#[test]
pub(crate) fn wasm_execution_parity_with_node() {
    // Emit the module, then (if `node` is available) instantiate it and assert
    // each exported function matches the interpreter's ground truth.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_parity.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Interpreter ground truth for `main` (which calls the others).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "152");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM execution parity");
        return;
    }

    // A tiny JS runner: print several exported results. i64 params/returns are
    // BigInt in JS, so pass `10n` and stringify the BigInt result.
    let runner = std::env::temp_dir().join("lullaby_wasm_runner.js");
    // The module imports the host functions `env.log_i64`, `env.console_log`, and
    // `env.dom_set_text`, so instantiation must supply all three even though these
    // scalar functions do not call them.
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const lines=[\
             'add='+e.add(20n,22n).toString(),\
             'fib='+e.fib(10n).toString(),\
             'is_even10='+e.is_even(10n).toString(),\
             'is_even55='+e.is_even(55n).toString(),\
             'sum='+e.sum_to(10n).toString(),\
             'main='+e.main().toString()\
           ];\
           process.stdout.write(lines.join(';'));\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    // Arithmetic function.
    assert!(out_text.contains("add=42"), "{out_text}");
    // Recursive function with `if`.
    assert!(out_text.contains("fib=55"), "{out_text}");
    // Bool-returning comparison exports as i32 0/1.
    assert!(out_text.contains("is_even10=1"), "{out_text}");
    assert!(out_text.contains("is_even55=0"), "{out_text}");
    // `for`-loop function.
    assert!(out_text.contains("sum=55"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
}

#[test]
pub(crate) fn wasm_log_import_execution_parity_with_node() {
    // The linear-memory step: a program whose exported function calls the
    // `wasm_log` host import with several computed values. The generated JS
    // harness supplies `env.log_i64`, capturing each call into an array, then
    // asserts the captured sequence equals what the interpreter computes.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_log.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_log.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // The emitted module exports `memory` (the linear memory) — a quick check on
    // the raw bytes independent of any runtime.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );

    // Interpreter ground truth. `main` calls `emit()` (which logs 4, 10, 42) and
    // then returns 36, which the CLI prints as the final line — drop that so we
    // compare only the `wasm_log` side-effect sequence.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let mut interp_lines: Vec<String> = stdout(&run)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let interp_return = interp_lines.pop();
    let interp_logged = interp_lines;
    assert_eq!(interp_logged, vec!["4", "10", "42"]);
    assert_eq!(interp_return.as_deref(), Some("36"));

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM host-import execution parity");
        return;
    }

    // The harness provides `env.log_i64`, capturing each call into `logged`,
    // then calls the exported `emit` and prints the captured BigInts.
    let runner = std::env::temp_dir().join("lullaby_wasm_log_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const logged=[];\
         const imports={{env:{{log_i64:(x)=>logged.push(x.toString()),console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           r.instance.exports.emit();\
           process.stdout.write(logged.join(';'));\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let captured: Vec<String> = String::from_utf8_lossy(&node.stdout)
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        captured, interp_logged,
        "WASM host-log call sequence must equal the interpreter's"
    );
}

#[test]
pub(crate) fn wasm_heap_types_execution_parity_with_node() {
    // The heap-types step: a program that builds a string, a struct (with a field
    // mutation), and a fixed array (with an indexed write and a `for`-loop read),
    // all laid out in linear memory. Each exported function's WASM result must
    // match the interpreter, and the emitted `memory` must hold the interned
    // string literal.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_heap.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_heap.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // The emitted module exports `memory` and seeds the `"hello"` literal into
    // its Data section — a raw-bytes check independent of any runtime.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );
    assert!(
        contains_subslice(&bytes, b"hello"),
        "string literal seeded into the data section"
    );

    // Interpreter ground truth for `main` (which calls every heap function).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "133");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM heap-types execution parity");
        return;
    }

    // The runner instantiates the module (a no-op `env.log_i64`), calls each
    // export, and additionally reads the interned
    // `[char_len i32][byte_len i32][utf8]` string layout straight out of `memory`
    // at the reserved base (offset 16): char count at +0, byte count at +4, bytes
    // at +8.
    let runner = std::env::temp_dir().join("lullaby_wasm_heap_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const slen=dv.getInt32(16,true);\
           const sblen=dv.getInt32(20,true);\
           const sbytes=new Uint8Array(e.memory.buffer).slice(24,24+sblen);\
           const lines=[\
             'greet_len='+e.greet_len().toString(),\
             'point_sum='+e.point_sum(3n,4n).toString(),\
             'point_mutated='+e.point_mutated(1n).toString(),\
             'array_probe='+e.array_probe().toString(),\
             'main='+e.main().toString(),\
             'str='+Buffer.from(sbytes).toString()+'/'+slen\
           ];\
           process.stdout.write(lines.join(';'));\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    // `len` on a string literal read from linear memory.
    assert!(out_text.contains("greet_len=5"), "{out_text}");
    // Struct field reads.
    assert!(out_text.contains("point_sum=7"), "{out_text}");
    // Struct field mutation.
    assert!(out_text.contains("point_mutated=12"), "{out_text}");
    // Array literal, indexed write, `for`-loop indexed read, and array `len`.
    assert!(out_text.contains("array_probe=109"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
    // The interned string layout in `memory` decodes back to the literal.
    assert!(out_text.contains("str=hello/5"), "{out_text}");
}

#[test]
pub(crate) fn wasm_string_concat_execution_parity_with_node() {
    // Runtime string concatenation (`a + b` on two `string` values) compiles to
    // WASM: each function allocates a fresh `[char_len][byte_len][utf8]` record and
    // copies both operands' byte ranges. The fixture exercises direct concat, a
    // chained `a + b + c`, and concatenation through a helper function, returning
    // deterministic `i64` char counts via `len(...)`. Every export's WASM result
    // must match the interpreter bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_string_concat.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_string_concat.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Every function — including the ones doing runtime `+` on strings — compiles
    // to WASM (none is skipped/demoted to the interpreters).
    let emit_out = stdout(&emit);
    for name in [
        "concat_two",
        "concat_three",
        "simple_len",
        "chained_len",
        "helper_len",
        "deep_len",
        "main",
    ] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile to WASM, got: {emit_out}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "33");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM string-concat execution parity");
        return;
    }

    // Instantiate under node, call each export, and additionally decode a
    // concatenated record built at runtime straight out of `memory` (char count at
    // +0, byte count at +4, bytes at +8) to prove the layout round-trips.
    let runner = std::env::temp_dir().join("lullaby_wasm_string_concat_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const u8=new Uint8Array(e.memory.buffer);\
           const ptr=e.concat_two(16,16);\
           const cl=dv.getInt32(ptr,true);\
           const bl=dv.getInt32(ptr+4,true);\
           const s=Buffer.from(u8.slice(ptr+8,ptr+8+bl)).toString();\
           const lines=[\
             'simple_len='+e.simple_len().toString(),\
             'chained_len='+e.chained_len().toString(),\
             'helper_len='+e.helper_len().toString(),\
             'deep_len='+e.deep_len().toString(),\
             'main='+e.main().toString(),\
             'rec='+s+'/'+cl+'/'+bl\
           ];\
           process.stdout.write(lines.join(';'));\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(out_text.contains("simple_len=6"), "{out_text}");
    assert!(out_text.contains("chained_len=6"), "{out_text}");
    assert!(out_text.contains("helper_len=10"), "{out_text}");
    assert!(out_text.contains("deep_len=11"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
    // `concat_two("", "")` over the two `[16..]` records at the reserved base —
    // both point at the same interned first literal — concatenates its bytes and
    // sums its headers, so the runtime-built record decodes correctly. The first
    // interned literal is `foo` (from `simple_len`), so `concat_two(16, 16)`
    // yields `foofoo` with char count 6 and byte count 6.
    assert!(out_text.contains("rec=foofoo/6/6"), "{out_text}");
}

#[test]
pub(crate) fn wasm_string_ops_execution_parity_with_node() {
    // Index-based string operations compile to WASM: char-indexed `substring`/`find`
    // (which decode UTF-8 to map char indices to byte offsets) and byte-exact
    // `contains`/`starts_with`/`ends_with`. The fixture exercises a multi-byte
    // ("café", where `é` is 2 bytes) string across edge indices, present/absent
    // `find`, an empty needle, and true/false cases of every predicate, combining
    // them into a deterministic `i64` from `main` plus string-returning `substring`
    // exports the node runner decodes. Every export's WASM result must match the
    // interpreter bit-for-bit — including the char-vs-byte distinction.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_string_ops.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_string_ops.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Every function compiles to WASM (none is skipped/demoted to the interpreters).
    let emit_out = stdout(&emit);
    for name in [
        "sub_af",
        "sub_e",
        "sub_full",
        "sub_empty",
        "find_present",
        "find_absent",
        "find_empty",
        "contains_true",
        "contains_false",
        "starts_true",
        "starts_false",
        "ends_true",
        "ends_false",
        "bool_to_i64",
        "main",
    ] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile to WASM, got: {emit_out}"
        );
    }

    // Interpreter ground truth for `main` (the joined deterministic total).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "11");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM string-ops execution parity");
        return;
    }

    // Instantiate under node, call each export, and decode every `substring` record
    // straight out of `memory` (char count at +0, byte count at +4, bytes at +8).
    // The decoded text and headers must match the interpreters' `builtin_substring`
    // — critically, `substring("café", 3, 4)` is the multi-byte `é` (char_len 1,
    // byte_len 2), proving the char->byte mapping.
    let runner = std::env::temp_dir().join("lullaby_wasm_string_ops_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const u8=new Uint8Array(e.memory.buffer);\
           function dec(ptr){{const cl=dv.getInt32(ptr,true);const bl=dv.getInt32(ptr+4,true);\
             const s=Buffer.from(u8.slice(ptr+8,ptr+8+bl)).toString();return s+'/'+cl+'/'+bl;}}\
           const lines=[\
             'sub_af='+dec(e.sub_af()),\
             'sub_e='+dec(e.sub_e()),\
             'sub_full='+dec(e.sub_full()),\
             'sub_empty='+dec(e.sub_empty()),\
             'find_present='+e.find_present().toString(),\
             'find_absent='+e.find_absent().toString(),\
             'find_empty='+e.find_empty().toString(),\
             'contains_true='+e.contains_true().toString(),\
             'contains_false='+e.contains_false().toString(),\
             'starts_true='+e.starts_true().toString(),\
             'starts_false='+e.starts_false().toString(),\
             'ends_true='+e.ends_true().toString(),\
             'ends_false='+e.ends_false().toString(),\
             'main='+e.main().toString()\
           ];\
           process.stdout.write(lines.join(';'));\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    // Char-indexed substring slices, including a multi-byte char (char_len != byte_len).
    assert!(out_text.contains("sub_af=af/2/2"), "{out_text}");
    assert!(out_text.contains("sub_e=\u{e9}/1/2"), "{out_text}");
    assert!(out_text.contains("sub_full=caf\u{e9}/4/5"), "{out_text}");
    assert!(out_text.contains("sub_empty=/0/0"), "{out_text}");
    // `find` returns a CHAR index (present), -1 (absent), 0 (empty needle).
    assert!(out_text.contains("find_present=2"), "{out_text}");
    assert!(out_text.contains("find_absent=-1"), "{out_text}");
    assert!(out_text.contains("find_empty=0"), "{out_text}");
    // Byte-exact predicates, true and false cases.
    assert!(out_text.contains("contains_true=1"), "{out_text}");
    assert!(out_text.contains("contains_false=0"), "{out_text}");
    assert!(out_text.contains("starts_true=1"), "{out_text}");
    assert!(out_text.contains("starts_false=0"), "{out_text}");
    assert!(out_text.contains("ends_true=1"), "{out_text}");
    assert!(out_text.contains("ends_false=0"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
}

#[test]
pub(crate) fn wasm_to_string_execution_parity_with_node() {
    // `to_string(x)` compiles to WASM for integer/bool/char/byte/string arguments,
    // building `[char_len][byte_len][utf8]` records identical to the interpreters'
    // `Value::Display`. Floats are DEFERRED (no float `to_string` appears in the
    // fixture). The fixture exercises signed/unsigned/`i64::MIN`/`u64::MAX`/zero
    // integers, fixed-width kinds, `bool`, `byte`, ASCII + multi-byte `char`, and
    // the string identity, returning a deterministic joined `i64` length from
    // `main` plus per-type `string`-returning exports the node runner decodes.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_to_string.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_to_string.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // Every function compiles to WASM (none is skipped/demoted to the interpreters).
    let emit_out = stdout(&emit);
    for name in [
        "i64_text",
        "i64_min_text",
        "u64_text",
        "fixed_text",
        "bool_text",
        "byte_text",
        "char_of",
        "char_text",
        "string_id",
        "main",
    ] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` to compile to WASM, got: {emit_out}"
        );
    }

    // Interpreter ground truth for `main` (the joined char count of the bundle).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "78");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM to_string execution parity");
        return;
    }

    // Instantiate under node, call each string-returning export, and decode its
    // record straight out of `memory` (char count at +0, byte count at +4, bytes at
    // +8). The decoded text must match the interpreters' `to_string` bit-for-bit,
    // including `i64::MIN`, `u64::MAX`, a byte magnitude passed as a parameter, and
    // a 2-byte UTF-8 char (char_len = 1, byte_len = 2).
    let runner = std::env::temp_dir().join("lullaby_wasm_to_string_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           const e=r.instance.exports;\
           const dv=new DataView(e.memory.buffer);\
           const u8=new Uint8Array(e.memory.buffer);\
           const dec=(ptr)=>{{\
             const cl=dv.getInt32(ptr,true);\
             const bl=dv.getInt32(ptr+4,true);\
             const s=Buffer.from(u8.slice(ptr+8,ptr+8+bl)).toString();\
             return s+'/'+cl+'/'+bl;\
           }};\
           const lines=[\
             'i64='+dec(e.i64_text()),\
             'i64min='+dec(e.i64_min_text()),\
             'u64='+dec(e.u64_text()),\
             'fixed='+dec(e.fixed_text()),\
             'bool='+dec(e.bool_text()),\
             'byte='+dec(e.byte_text(200)),\
             'char='+dec(e.char_of(233)),\
             'chars='+dec(e.char_text()),\
             'sid='+dec(e.string_id()),\
             'main='+e.main().toString()\
           ];\
           process.stdout.write(lines.join(';'));\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    // Signed decimal with a negative and zero; all ASCII, so char_len == byte_len.
    assert!(out_text.contains("i64=42,0,-7/7/7"), "{out_text}");
    // `i64::MIN` prints its full negative magnitude (20 chars incl. the `-`).
    assert!(
        out_text.contains("i64min=-9223372036854775808/20/20"),
        "{out_text}"
    );
    // `to_u64(0 - 1)` is `u64::MAX` — the unsigned magnitude, not `-1`.
    assert!(
        out_text.contains("u64=18446744073709551615/20/20"),
        "{out_text}"
    );
    // `i8` wraps to -128; `u32` prints its magnitude.
    assert!(
        out_text.contains("fixed=-128|4000000000/15/15"),
        "{out_text}"
    );
    assert!(out_text.contains("bool=true,false/10/10"), "{out_text}");
    // `byte(200)` passed via the parameter prints decimal 200.
    assert!(out_text.contains("byte=200/3/3"), "{out_text}");
    // A 2-byte UTF-8 scalar (é = U+00E9): one char, two bytes.
    assert!(out_text.contains("char=é/1/2"), "{out_text}");
    // ASCII + 2-byte char: two chars, three bytes.
    assert!(out_text.contains("chars=Aé/2/3"), "{out_text}");
    // `to_string(string)` is the identity record.
    assert!(out_text.contains("sid=kept/4/4"), "{out_text}");
    // Whole-program `main` matches the interpreter.
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "{out_text}"
    );
}

#[test]
pub(crate) fn wasm_value_if_execution_parity_with_node() {
    // A value-producing tail `if`/`elif`/`else` (each branch yields the function's
    // result value) now compiles to WASM: the `if` emits a typed block so the
    // branch value is left on the stack. Previously such functions were skipped.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_value_if.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_value_if.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let emit_out = stdout(&emit);
    for name in ["sign_of", "abs_or_zero", "main"] {
        assert!(
            emit_out.contains(&format!("compiled {name}")),
            "expected `{name}` (value-producing `if`) to compile to WASM, got: {emit_out}"
        );
    }

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "145");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM value-if execution parity");
        return;
    }
    let runner = std::env::temp_dir().join("lullaby_wasm_value_if_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(out_text.contains("main=145"), "{out_text}");
}

#[test]
pub(crate) fn wasm_aggregate_args_execution_parity_with_node() {
    // Aggregates across call boundaries: a `main -> i64` that passes a struct to a
    // function reading its fields, receives a struct another function returns, and
    // takes+returns a fixed array — plus a value-semantics probe where a callee
    // mutates its struct/array PARAMETER and the caller's copy stays unchanged.
    // Every aggregate argument is deep-copied at the call site, so the WASM result
    // must equal the interpreter's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_aggregate_args.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_aggregate_args.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Every function — the struct/array takers and returners, the value-semantics
    // mutator, and `main` — compiles to WASM (none skipped).
    let listing = stdout(&emit);
    for name in [
        "sum_point",
        "make_point",
        "first_of",
        "bump",
        "mutate_point",
        "main",
    ] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // Interpreter ground truth for `main` (which drives every case).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "150");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM aggregate-args execution parity");
        return;
    }

    // Instantiate and compare `main()` to the interpreter. The value-semantics of
    // the deep copies are baked into `main`: `arr_untouched` (1, not 101) proves
    // `bump` did not mutate the caller's array, and `caller_unchanged` (11, not
    // 1998) proves `mutate_point` did not mutate the caller's struct.
    let runner = std::env::temp_dir().join("lullaby_wasm_aggregate_args_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM aggregate-args `main` must equal the interpreter (value semantics): {out_text}"
    );
}

#[test]
pub(crate) fn wasm_nested_aggregate_args_execution_parity_with_node() {
    // The recursive deep-copy path: aggregates nested inside aggregates crossing
    // call boundaries — a struct holding a struct, and an array of arrays. When a
    // callee mutates a nested field/element of its parameter, the caller's copy
    // must be untouched, which requires the copy-on-pass to recurse into nested
    // mutable aggregates. `main` returns the interpreter-checked total.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_aggregate_nested.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_aggregate_nested.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let listing = stdout(&emit);
    for name in ["outer_total", "wreck", "rows_sum", "wreck_rows", "main"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // Interpreter ground truth.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "32");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM nested-aggregate execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_aggregate_nested_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM nested-aggregate `main` must equal the interpreter (recursive value semantics): {out_text}"
    );
}

#[test]
pub(crate) fn wasm_fixed_width_integers_execution_parity_with_node() {
    // The fixed-width integer step: three fixtures whose `main` returns `i64` but
    // whose bodies exercise the width-normalized operations (wrapping arithmetic,
    // signedness-correct comparison/division, bitwise/shift, `~`, and the
    // `to_<T>`/`to_i64` conversions). Each compiles to WASM now, and each exported
    // `main` must equal the interpreter's ground truth bit-for-bit.
    let cases: [(&str, &str); 4] = [
        ("run_int_widths", "2147483649"),
        ("run_int_widths_wide", "7"),
        ("run_bitwise_widths", "410"),
        // `i64::MIN / -1` must wrap to `i64::MIN` (result 7) rather than trap the
        // WASM `i64.div_s`, on both the plain-i64 and fixed-width signed paths.
        ("run_div_overflow", "7"),
    ];

    // Emit each module and confirm `main` compiled (not skipped).
    let mut wasm_paths: Vec<(String, std::path::PathBuf, String)> = Vec::new();
    for (name, expected) in cases {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_wasm_{name}.wasm"));
        let emit = lullaby()
            .args([
                "wasm",
                "--verbose",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{}: {}", name, stderr(&emit));
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: `main` should compile to WASM, got: {}",
            stdout(&emit)
        );

        // Interpreter ground truth.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}: {}", name, stderr(&run));
        assert_eq!(stdout(&run).trim(), expected, "{name} interpreter result");

        wasm_paths.push((name.to_string(), out, expected.to_string()));
    }

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM fixed-width execution parity");
        return;
    }

    // A runner that instantiates each module and prints `name=main()`. `main`
    // returns `i64`, which is a BigInt in JS.
    for (name, out, expected) in &wasm_paths {
        let runner = std::env::temp_dir().join(format!("lullaby_wasm_{name}_runner.js"));
        let js = format!(
            "const fs=require('fs');\
             const bytes=fs.readFileSync({wasm:?});\
             const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
             WebAssembly.instantiate(bytes,imports).then(r=>{{\
               process.stdout.write('main='+r.instance.exports.main().toString());\
             }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
            wasm = out.to_str().expect("out path")
        );
        std::fs::write(&runner, js).expect("write runner");

        let node = Command::new("node")
            .arg(runner.to_str().expect("runner path"))
            .output()
            .expect("run node");
        assert!(
            node.status.success(),
            "{name} node failed: {}",
            String::from_utf8_lossy(&node.stderr)
        );
        let out_text = String::from_utf8_lossy(&node.stdout);
        assert!(
            out_text.contains(&format!("main={expected}")),
            "{name}: WASM `main` must equal the interpreter ({expected}), got: {out_text}"
        );
    }
}

#[test]
pub(crate) fn wasm_float_execution_parity_with_node() {
    // The float step: two fixtures whose `main` returns `i64` but whose bodies
    // exercise `f32`/`f64` arithmetic, comparisons, and the `to_f32`/`to_f64`
    // conversions. Each compiles to WASM now (single-precision `f32.*` ops keep
    // f32 bit-identical to the interpreter), and each exported `main` must equal
    // the interpreter's ground truth.
    let cases: [(&str, &str); 2] = [("run_f32", "3"), ("native_floats", "9")];

    let mut wasm_paths: Vec<(String, std::path::PathBuf, String)> = Vec::new();
    for (name, expected) in cases {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_wasm_{name}.wasm"));
        let emit = lullaby()
            .args([
                "wasm",
                "--verbose",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{}: {}", name, stderr(&emit));
        assert!(
            stdout(&emit).contains("compiled main"),
            "{name}: `main` should compile to WASM, got: {}",
            stdout(&emit)
        );

        // Interpreter ground truth.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}: {}", name, stderr(&run));
        assert_eq!(stdout(&run).trim(), expected, "{name} interpreter result");

        wasm_paths.push((name.to_string(), out, expected.to_string()));
    }

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM float execution parity");
        return;
    }

    // A runner that instantiates each module and prints `main=main()`. `main`
    // returns `i64`, which is a BigInt in JS.
    for (name, out, expected) in &wasm_paths {
        let runner = std::env::temp_dir().join(format!("lullaby_wasm_{name}_runner.js"));
        let js = format!(
            "const fs=require('fs');\
             const bytes=fs.readFileSync({wasm:?});\
             const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
             WebAssembly.instantiate(bytes,imports).then(r=>{{\
               process.stdout.write('main='+r.instance.exports.main().toString());\
             }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
            wasm = out.to_str().expect("out path")
        );
        std::fs::write(&runner, js).expect("write runner");

        let node = Command::new("node")
            .arg(runner.to_str().expect("runner path"))
            .output()
            .expect("run node");
        assert!(
            node.status.success(),
            "{name} node failed: {}",
            String::from_utf8_lossy(&node.stderr)
        );
        let out_text = String::from_utf8_lossy(&node.stdout);
        assert!(
            out_text.contains(&format!("main={expected}")),
            "{name}: WASM `main` must equal the interpreter ({expected}), got: {out_text}"
        );
    }
}

#[test]
pub(crate) fn wasm_enum_match_execution_parity_with_node() {
    // The enum + match step: a program whose `main` returns `i64` but whose body
    // exercises enum construction and `match` over the built-in `option<i64>`,
    // `result<i64, i64>` (scalar payloads), and a small user enum with a scalar
    // payload plus a wildcard arm, including a call returning `option<i64>` that
    // the caller matches. Each function compiles to WASM now (tag-based enum
    // records in linear memory), and the exported `main` must equal the
    // interpreter's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_enum_match.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_enum_match.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Every function (including the enum-payload matchers and constructors) must
    // COMPILE to WASM, not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["unwrap_or", "divide", "describe", "area", "pick", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "144");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM enum-match execution parity");
        return;
    }

    // Instantiate the module (no-op host imports) and call the exported `main`,
    // which threads every enum construction and match through linear memory.
    let runner = std::env::temp_dir().join("lullaby_wasm_enum_match_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM enum+match `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_list_build_execution_parity_with_node() {
    // The growable `list<T>` step: a program that builds a scalar-element list via
    // `list_new`/`push` (crossing the initial capacity to trigger a grow+copy),
    // reads it with `get`/`len`, replaces an element with `set`, and drops the last
    // with `pop`. Each function compiles to WASM now (a `[len][cap][slots]` block
    // in linear memory), and the exported `main` must equal the interpreter's
    // ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_build.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_build.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Both functions must COMPILE to WASM (the list ops lower to linear memory),
    // not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["build", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "5879");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list-build execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_build_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM list-build `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_list_value_semantics_execution_parity_with_node() {
    // The list value-semantics step: assigning a list to another binding shares an
    // `i32` pointer, but every mutating op (`push`/`set`) deep-copies first and a
    // list crossing a call boundary is deep-copied, so mutating one binding is
    // never observable through another. `main` probes an aliased binding, a
    // push-derived list, a set-derived list, and a callee that pushes to its
    // parameter; the WASM result must equal the interpreter's.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_value_semantics.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_value_semantics.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let verbose = stdout(&emit);
    for func in ["mutate", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "334211");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list value-semantics parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_value_semantics_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM list value-semantics `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_list_struct_and_nested_and_map_struct_execution_parity_with_node() {
    // Mutable-heap collection ELEMENTS/VALUES: a `list<struct>` (push structs, read a
    // field, `set` an element), a `list<list<i64>>` (one level of mutable nesting,
    // summed through nested `get`s), and a `map<i64, struct>` (`map_set`/`map_get`
    // returning `option<struct>`/`map_len`). CRUCIALLY it includes a value-semantics
    // probe: `get(ps, 2)` returns a struct that is mutated (`.x`/`.y` set to 1000/
    // 2000), then `get(ps, 2)` again must still read the ORIGINAL element — proving
    // `get` returns a deep copy (the interpreters' `values[i].clone()`), so the
    // mutable-aggregate element deep-copy matches the interpreters bit-for-bit. Each
    // function compiles to WASM (the collection's element/value deep-copy recurses
    // into the struct/nested list), and the exported `main` must equal the
    // interpreter's ground truth.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_struct.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_struct.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Every function must COMPILE to WASM (the mutable-heap element/value deep-copy
    // recursion lowers to linear memory), not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in [
        "point_sum",
        "grow_probe",
        "build_points",
        "nested_sum",
        "build_nested",
        "map_point_value",
        "build_map",
        "main",
    ] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "503411108");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list<struct>/map<K,struct> parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_struct_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM list<struct>/nested/map<K,struct> `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_map_build_execution_parity_with_node() {
    // The growable `map<K, V>` step: a program that builds a scalar-key,
    // scalar-value map via `map_new`/`map_set` (inserting several keys plus an
    // in-place update, and crossing the initial capacity to trigger a grow+copy),
    // reads it with `map_get` (matching the returned `option<V>`), `map_has`, and
    // `map_len`. Each function compiles to WASM now (a `[len][cap][(k,v) pairs]`
    // block in linear memory), and the exported `main` must equal the
    // interpreter's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_map_build.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_map_build.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // All functions must COMPILE to WASM (the map ops lower to linear memory),
    // not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["build", "lookup", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "5999509");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM map-build execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_map_build_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM map-build `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_map_value_semantics_execution_parity_with_node() {
    // The map value-semantics step: assigning a map to another binding shares an
    // `i32` pointer, but every mutating op (`map_set`) deep-copies first and a map
    // crossing a call boundary is deep-copied, so mutating one binding is never
    // observable through another. `main` probes an aliased binding, an insert-
    // derived map, an update-derived map, and a callee that inserts into its
    // parameter; the WASM result must equal the interpreter's.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_map_value_semantics.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_map_value_semantics.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let verbose = stdout(&emit);
    for func in ["probe", "value_of", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let interp_main = stdout(&run).trim().to_string();
    assert_eq!(interp_main, "2231100");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM map value-semantics parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_map_value_semantics_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM map value-semantics `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_map_string_key_execution_parity_with_node() {
    // The `map<string, V>` step: a `string`-KEYED map compiles now. The lookup
    // compares keys by CONTENT (equal `byte_len` and identical UTF-8 bytes), not
    // pointer identity, exactly like the interpreters' `Value` equality — so a key
    // built by concatenation (`"a" + "b"`, a fresh string object) is the SAME key
    // as a separately-built literal `"ab"`. The fixture builds a `map<string, i64>`
    // and a `map<string, string>`, sets keys via concatenated/`to_string` strings,
    // updates an existing key (proving content equality overwrites, not appends),
    // and reads with `map_get`/`map_has`/`map_len`. The exported `main` must equal
    // the interpreters' ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_map_string_key.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_map_string_key.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Every function must COMPILE to WASM (the string-keyed map ops lower to linear
    // memory with a content-equality scan), not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in ["build_scores", "score", "build_labels", "label_len", "main"] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`, cross-checked across all three backends
    // (AST/IR/bytecode) so the WASM result is compared to a value every interpreter
    // agrees on.
    let mut interp_main = String::new();
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}", stderr(&run));
        let got = stdout(&run).trim().to_string();
        if interp_main.is_empty() {
            interp_main = got;
        } else {
            assert_eq!(
                got, interp_main,
                "`{backend}` interpreter disagreed on map<string, _> ground truth"
            );
        }
    }
    assert_eq!(interp_main, "325634");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM map string-key execution parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_map_string_key_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM map string-key `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_list_string_and_map_string_execution_parity_with_node() {
    // The `string`-element/value step: a `list<string>` (built with `push` of
    // literal, concatenated, and `to_string` strings, read with `get`/`len`, and
    // passed to helpers) and a `map<i64, string>` (built with `map_set`, read with
    // `map_get` matching the returned `option<string>`, plus `map_has`/`map_len`).
    // A `string` element/value is an `i32` pointer stored in one slot exactly like
    // a scalar and — because strings are immutable — is SHARED (not deep-recursed)
    // on the value-semantic deep copy. `grow_probe` pushes to its list parameter
    // and `main` re-reads the caller's list length to prove the caller is
    // unaffected. All functions must compile to WASM and the exported `main` must
    // equal each interpreter backend's ground truth bit-for-bit.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_list_string.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_list_string.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // Every function must COMPILE to WASM (list<string>/map<i64,string> lower to
    // linear memory), not skip to the interpreters.
    let verbose = stdout(&emit);
    for func in [
        "total_len",
        "grow_probe",
        "build_words",
        "build_names",
        "name_len",
        "main",
    ] {
        assert!(
            verbose.contains(&format!("compiled {func}")),
            "`{func}` should compile to WASM (not skip), got: {verbose}"
        );
    }

    // Interpreter ground truth for `main`, identical on all three interpreter
    // backends (AST/IR/bytecode).
    let mut interp_main = String::new();
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{}", stderr(&run));
        let result = stdout(&run).trim().to_string();
        if interp_main.is_empty() {
            interp_main = result.clone();
        }
        assert_eq!(
            result, interp_main,
            "backend `{backend}` must match the other interpreter backends"
        );
    }
    assert_eq!(interp_main, "13444740");

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM list<string>/map<string> parity");
        return;
    }

    let runner = std::env::temp_dir().join("lullaby_wasm_list_string_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const imports={{env:{{log_i64:()=>{{}},console_log:()=>{{}},dom_set_text:()=>{{}}}}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           process.stdout.write('main='+r.instance.exports.main().toString());\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");
    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    assert!(
        out_text.contains(&format!("main={interp_main}")),
        "WASM list<string>/map<string> `main` must equal the interpreter ({interp_main}), got: {out_text}"
    );
}

#[test]
pub(crate) fn wasm_js_dom_interop_execution_parity_with_node() {
    // The JS/DOM interop step: a program whose exported function calls the
    // `console_log(s)` and `dom_set_text(id, text)` host imports with computed
    // strings. The generated JS harness supplies `env.console_log` and
    // `env.dom_set_text`, decodes each (ptr, len) string out of `memory`, and
    // captures them; the captured strings must equal what the interpreter prints,
    // and the exported `main` must equal the interpreter's `main`.
    let fixture = workspace_root().join("tests/fixtures/valid/wasm_interop.lby");
    let out = std::env::temp_dir().join("lullaby_wasm_interop.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));

    // The emitted module exports `memory` and seeds the interop string literals.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );
    assert!(
        contains_subslice(&bytes, b"console_log") && contains_subslice(&bytes, b"dom_set_text"),
        "module imports the JS/DOM host functions"
    );

    // Interpreter ground truth. `main` calls `ui()` (which logs two console lines
    // and two dom lines) then returns 22, printed as the final line. Split the
    // side-effect lines from the return value.
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let mut interp_lines: Vec<String> = stdout(&run)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let interp_return = interp_lines.pop();
    // console_log prints the string; dom_set_text prints `id=text`.
    assert_eq!(
        interp_lines,
        vec!["ready", "idle", "status=ready", "count=42"]
    );
    assert_eq!(interp_return.as_deref(), Some("22"));

    if !node_available() {
        eprintln!("node not found on PATH; skipping WASM JS/DOM interop execution parity");
        return;
    }

    // The harness decodes each string from the `(ptr, len)` host operands — `ptr`
    // points directly at the first UTF-8 byte and `len` is the byte length — so
    // it slices `[ptr, ptr + len)`, captures console/dom calls, and prints the
    // whole-program `main`.
    let runner = std::env::temp_dir().join("lullaby_wasm_interop_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const logs=[];const doms=[];let mem;\
         const dec=(ptr,len)=>Buffer.from(new Uint8Array(mem.buffer).slice(ptr,ptr+len)).toString();\
         const imports={{env:{{\
           log_i64:()=>{{}},\
           console_log:(p,l)=>logs.push(dec(p,l)),\
           dom_set_text:(ip,il,tp,tl)=>doms.push(dec(ip,il)+'='+dec(tp,tl))\
         }}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           mem=r.instance.exports.memory;\
           const main=r.instance.exports.main().toString();\
           process.stdout.write('logs='+logs.join('|')+';doms='+doms.join('|')+';main='+main);\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    // The captured `console_log` sequence equals the interpreter's stdout lines.
    assert!(out_text.contains("logs=ready|idle"), "{out_text}");
    // The captured `dom_set_text` `id=text` sequence equals the interpreter's.
    assert!(
        out_text.contains("doms=status=ready|count=42"),
        "{out_text}"
    );
    // Whole-program `main` matches the interpreter.
    assert!(out_text.contains("main=22"), "{out_text}");
}

// -- Full-stack web demo (WASM frontend + Lullaby HTTP backend, shared module) -

/// Every file of the full-stack example checks: the shared domain module, the
/// WASM frontend, the HTTP backend, and the copied `http` framework module.
#[test]
pub(crate) fn fullstack_example_files_check() {
    let dir = workspace_root().join("examples/valid/fullstack");
    for file in ["shared.lby", "frontend.lby", "backend.lby", "http.lby"] {
        let output = lullaby()
            .args(["check", dir.join(file).to_str().expect("file path")])
            .output()
            .expect("run cli");
        assert!(output.status.success(), "{file}: {output:?}");
    }
}

/// The frontend compiles to a real `.wasm` module (shared module included), and
/// — when `node` is present — instantiating it with capturing
/// `env.console_log` / `env.dom_set_text` imports renders the shared labels and
/// the exported `main` returns the summed shared priority score. The interpreter
/// is the ground truth for both.
#[test]
pub(crate) fn fullstack_frontend_wasm_matches_shared_logic() {
    let fixture = workspace_root().join("examples/valid/fullstack/frontend.lby");
    let out = std::env::temp_dir().join("lullaby_fullstack_frontend.wasm");
    let emit = lullaby()
        .args([
            "wasm",
            "--verbose",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    // The frontend entry and the imported shared logic all compiled.
    let listing = stdout(&emit);
    for name in ["main", "render", "classify", "priority_score"] {
        assert!(
            listing.contains(&format!("compiled {name}")),
            "expected `{name}` compiled: {listing}"
        );
    }

    // Valid WASM: the `\0asm` magic header plus the exported memory and the two
    // JS/DOM host imports the shared frontend uses.
    let bytes = std::fs::read(&out).expect("read wasm");
    assert!(bytes.starts_with(b"\0asm"), "wasm magic header");
    assert!(
        contains_subslice(&bytes, b"memory"),
        "module exports `memory`"
    );
    assert!(
        contains_subslice(&bytes, b"console_log") && contains_subslice(&bytes, b"dom_set_text"),
        "module imports the JS/DOM host functions"
    );

    // Interpreter ground truth: two console/dom lines per rendered task, then the
    // summed shared priority score (quick=1 + detailed=3 + empty=0 = 4).
    let run = lullaby()
        .args(["run", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(run.status.success(), "{}", stderr(&run));
    let mut interp_lines: Vec<String> = stdout(&run)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    let interp_return = interp_lines.pop();
    assert_eq!(
        interp_lines,
        vec![
            "quick",
            "task_a=quick",
            "detailed",
            "task_b=detailed",
            "empty",
            "task_c=empty",
        ]
    );
    assert_eq!(interp_return.as_deref(), Some("4"));

    if !node_available() {
        eprintln!("node not found on PATH; skipping full-stack frontend WASM parity");
        return;
    }

    // Instantiate under node with capturing host imports and assert the rendered
    // shared labels and the exported score match the interpreter.
    let runner = std::env::temp_dir().join("lullaby_fullstack_frontend_runner.js");
    let js = format!(
        "const fs=require('fs');\
         const bytes=fs.readFileSync({wasm:?});\
         const logs=[];const doms=[];let mem;\
         const dec=(ptr,len)=>Buffer.from(new Uint8Array(mem.buffer).slice(ptr,ptr+len)).toString();\
         const imports={{env:{{\
           log_i64:()=>{{}},\
           console_log:(p,l)=>logs.push(dec(p,l)),\
           dom_set_text:(ip,il,tp,tl)=>doms.push(dec(ip,il)+'='+dec(tp,tl))\
         }}}};\
         WebAssembly.instantiate(bytes,imports).then(r=>{{\
           mem=r.instance.exports.memory;\
           const main=r.instance.exports.main().toString();\
           process.stdout.write('logs='+logs.join('|')+';doms='+doms.join('|')+';main='+main);\
         }}).catch(err=>{{console.error('FAIL:'+err.message);process.exit(1)}});",
        wasm = out.to_str().expect("out path")
    );
    std::fs::write(&runner, js).expect("write runner");

    let node = Command::new("node")
        .arg(runner.to_str().expect("runner path"))
        .output()
        .expect("run node");
    assert!(
        node.status.success(),
        "node failed: {}",
        String::from_utf8_lossy(&node.stderr)
    );
    let out_text = String::from_utf8_lossy(&node.stdout);
    // The shared classification labels rendered to the console and the DOM.
    assert!(out_text.contains("logs=quick|detailed|empty"), "{out_text}");
    assert!(
        out_text.contains("doms=task_a=quick|task_b=detailed|task_c=empty"),
        "{out_text}"
    );
    // The summed shared priority score matches the interpreter.
    assert!(out_text.contains("main=4"), "{out_text}");
}

/// Drive the full-stack backend as a real HTTP client on all three backends and
/// assert the `/classify` body comes from the shared domain module (the same
/// label/score the frontend renders for the sample title).
#[test]
pub(crate) fn fullstack_shared_logic_round_trip() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn request(port: u16, path: &str) -> String {
        let mut stream = None;
        for _ in 0..100 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        let mut stream = stream.expect("connect to lullaby backend");
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).expect("client write");
        stream.flush().expect("client flush");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("client read to EOF");
        response
    }

    let backend_path = workspace_root().join("examples/valid/fullstack/backend.lby");

    for backend in ["ast", "ir", "bytecode"] {
        let port = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("probe bind");
            probe.local_addr().expect("addr").port()
        };

        // Serve two requests: the shared `/classify` route and an unknown path.
        let path = backend_path.clone();
        let port_arg = port.to_string();
        let server = std::thread::spawn(move || {
            lullaby()
                .args([
                    "run",
                    "--backend",
                    backend,
                    path.to_str().expect("backend path"),
                    &port_arg,
                    "2",
                ])
                .output()
                .expect("run cli")
        });

        // The shared route: 200 with the classification body for the sample title
        // "Write the design document" (detailed, score 3, valid), computed by the
        // shared module — the same values the WASM frontend renders.
        let classify = request(port, "/classify");
        let status_line = classify.lines().next().unwrap_or_default();
        assert_eq!(
            status_line, "HTTP/1.1 200 OK",
            "{backend} status line for /classify: {classify:?}"
        );
        assert!(
            classify.contains("label=detailed"),
            "{backend} shared label for /classify: {classify:?}"
        );
        assert!(
            classify.contains("score=3"),
            "{backend} shared score for /classify: {classify:?}"
        );
        assert!(
            classify.contains("valid=true"),
            "{backend} shared validity for /classify: {classify:?}"
        );
        assert!(
            classify.contains("title=Write the design document"),
            "{backend} sample title for /classify: {classify:?}"
        );

        // Unknown route still 404s through the shared router seed.
        let missing = request(port, "/does-not-exist");
        let missing_status = missing.lines().next().unwrap_or_default();
        assert_eq!(
            missing_status, "HTTP/1.1 404 Not Found",
            "{backend} status line for unknown path: {missing:?}"
        );

        let output = server.join().expect("server thread");
        assert!(
            output.status.success(),
            "{backend} lullaby backend: {output:?}"
        );
    }
}

/// Direct PE is the DEFAULT for eligible native builds. A plain `lullaby native
/// -o out.exe file.lby` (NO `--freestanding`) on a COFF program with a `main` and
/// no C-runtime import must write the runnable `.exe` in-house and skip the
/// external linker (`rust-lld`) entirely. This test needs neither `rust-lld` nor
/// `kernel32.lib` — that is the whole point — so it runs unconditionally across a
/// scalar, string/heap, aggregate, and control-flow fixture: emit each with the
/// default command, assert the direct-PE notice and the *absence* of an
/// intermediate object file (proof no linker ran), run the produced `.exe`, and
/// assert its exit code equals the interpreter's `main` result (mod 256).
#[test]
pub(crate) fn native_default_direct_pe_runs_without_linker() {
    let cases = [
        ("native_scalars", 39_i64),
        ("native_strings", 11),
        ("native_aggregates", 43),
        ("native_control_flow", 31),
    ];
    for (name, expected) in cases {
        let fixture = workspace_root().join(format!("tests/fixtures/valid/{name}.lby"));
        let out = std::env::temp_dir().join(format!("lullaby_default_direct_pe_{name}.exe"));
        let obj = out.with_extension("obj");
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&obj);

        // The DEFAULT native command: no `--freestanding`, no `--debug`.
        let emit = lullaby()
            .args([
                "native",
                "-o",
                out.to_str().expect("out path"),
                fixture.to_str().expect("fixture path"),
            ])
            .output()
            .expect("run cli");
        assert!(emit.status.success(), "{name}: {}", stderr(&emit));
        let listing = stdout(&emit);
        assert!(
            listing.contains("direct PE, no linker"),
            "{name}: default native build must take the direct-PE path: {listing}"
        );
        assert!(
            out.is_file(),
            "{name}: expected a direct-PE exe at {}",
            out.display()
        );
        // The direct path never invokes the linker, so it writes no object file.
        // Its absence is the proof that `rust-lld` was not part of this build.
        assert!(
            !obj.is_file(),
            "{name}: direct-PE default path must not write an object file"
        );

        // A real PE image begins with the DOS `MZ` magic.
        let bytes = std::fs::read(&out).expect("read direct pe");
        assert_eq!(&bytes[0..2], b"MZ", "{name}: PE image DOS magic");

        // Interpreter ground truth for `main`.
        let run = lullaby()
            .args(["run", fixture.to_str().expect("fixture path")])
            .output()
            .expect("run cli");
        assert!(run.status.success(), "{name}: {}", stderr(&run));
        let interp: i64 = stdout(&run).trim().parse().expect("interpreter i64");
        assert_eq!(interp, expected, "{name}: fixture main computes {expected}");

        // Run the in-house `.exe` (no linker was involved) and compare exit codes.
        let exe = Command::new(&out).output().expect("run direct pe exe");
        let exit = exe.status.code().expect("native exit code");
        assert_eq!(
            exit,
            (interp.rem_euclid(256)) as i32,
            "{name}: direct-PE exit code must equal the interpreter result (mod 256)"
        );
    }
}

/// A `--debug` native build must NOT take the direct-PE path: the CodeView
/// `.debug$S`/PDB source-line info lives only in the object + linker path, so a
/// debug build always emits an object file and reports the CodeView table. This
/// asserts the debug guard on the default-direct-PE change holds — no `rust-lld`
/// or `kernel32.lib` is required to observe it (the emit + object write happen
/// before any link attempt).
#[test]
pub(crate) fn native_debug_keeps_object_and_skips_direct_pe() {
    let fixture = workspace_root().join("tests/fixtures/valid/native_scalars.lby");
    let out = std::env::temp_dir().join("lullaby_debug_no_direct_pe.exe");
    let obj = out.with_extension("obj");
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&obj);

    let emit = lullaby()
        .args([
            "native",
            "--debug",
            "-o",
            out.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    let listing = stdout(&emit);
    // Debug build stays on the object + linker path.
    assert!(
        !listing.contains("direct PE, no linker"),
        "--debug must not take the direct-PE fast path: {listing}"
    );
    assert!(
        listing.contains("native object:") && listing.contains("CodeView"),
        "--debug must emit an object with a CodeView source-line table: {listing}"
    );
    assert!(
        obj.is_file(),
        "--debug must write an object file for the linker path"
    );
}
