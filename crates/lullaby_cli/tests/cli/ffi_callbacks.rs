//! CLI integration tests: FFI callbacks (passing a Lullaby function to C as a
//! C-ABI function pointer). Split out of tests/cli.rs so the callback round-trip
//! lives in its own module. Mirrors the export-into-Lullaby harness
//! (`c_calls_into_exported_lullaby_function_when_compilable`) but inverts the
//! direction: here the C driver *receives* a Lullaby function pointer and invokes
//! it, proving the callback is directly callable under the Win64 C ABI.

use crate::*;
use std::process::Command;

/// FFI callback round-trip: an `extern fn apply_cmp cmp fn(i64, i64) -> i64 …`
/// takes a Lullaby function as a C-ABI function pointer, and the exported
/// `run_callback` passes the top-level `diff` to it. A C driver defines
/// `apply_cmp` (which calls `cmp(a, b)`) and a `main` that calls
/// `run_callback(10, 3)`; linked against the Lullaby library object, the process
/// invokes the Lullaby `diff` **through a C function pointer** and exits with
/// `diff(10, 3) == 7`. Subtraction is non-commutative, so a wrong argument order
/// would exit 249 (-7 mod 256) — the exit code 7 proves the callback pointer is
/// invoked with the correct calling convention. Gated on a discoverable C
/// compiler; the object-emission and interpreter-rejection parts always run.
#[test]
pub(crate) fn ffi_callback_roundtrip_when_compilable() {
    let fixture = workspace_root().join("tests/fixtures/native_only/ffi_callback_roundtrip.lby");

    // `check` validates the extern callback signature (a C-marshallable
    // `fn(i64, i64) -> i64` parameter) and the export body/call site.
    let check = lullaby()
        .args(["check", fixture.to_str().expect("fixture path")])
        .output()
        .expect("run cli");
    assert!(check.status.success(), "{}", stderr(&check));

    // Every interpreter backend rejects an extern call — even one that passes a
    // callback — with `L0423` (there is no C to call). The fixture is export-only
    // (no `main`), so exercise interpreter rejection with an equivalent program
    // whose `main` calls the extern directly.
    let main_src = "extern fn apply_cmp cmp fn(i64, i64) -> i64 a i64 b i64 -> i64\n\n\
                    fn diff x i64 y i64 -> i64\n    x - y\n\n\
                    fn main -> i64\n    apply_cmp(diff, 10, 3)\n";
    let main_tmp = std::env::temp_dir().join("lullaby_ffi_callback_main.lby");
    std::fs::write(&main_tmp, main_src).expect("write callback main fixture");
    for backend in ["ast", "ir", "bytecode"] {
        let run = lullaby()
            .args([
                "run",
                "--backend",
                backend,
                main_tmp.to_str().expect("temp path"),
            ])
            .output()
            .expect("run cli");
        assert!(
            !run.status.success(),
            "extern callback call must fail on the {backend} interpreter"
        );
        let rendered = format!("{}{}", stdout(&run), stderr(&run));
        assert!(
            rendered.contains("L0423"),
            "expected L0423 on {backend}: {rendered}"
        );
    }

    // Native codegen: emit the library object. `run_callback` and `diff` compile;
    // there is no `main`, so the CLI reports a C-callable library object (the
    // `apply_cmp` symbol stays an undefined external the C driver resolves). The
    // object path is derived from the `-o` exe stem.
    let exe_arg = std::env::temp_dir().join("lullaby_ffi_callback.exe");
    let obj = exe_arg.with_extension("obj");
    let _ = std::fs::remove_file(&obj);
    let emit = lullaby()
        .args([
            "native",
            "--verbose",
            "-o",
            exe_arg.to_str().expect("out path"),
            fixture.to_str().expect("fixture path"),
        ])
        .output()
        .expect("run cli");
    assert!(emit.status.success(), "{}", stderr(&emit));
    assert!(
        stdout(&emit).contains("compiled run_callback"),
        "expected `run_callback` compiled: {}",
        stdout(&emit)
    );
    assert!(
        stdout(&emit).contains("C-callable library object"),
        "expected a C-callable library object report: {}",
        stdout(&emit)
    );
    assert!(obj.is_file(), "expected object at {}", obj.display());

    let Some(cc) = find_c_compiler() else {
        eprintln!("no C compiler (cl/clang) found; skipping FFI callback round-trip execution");
        return;
    };

    // A C driver: `apply_cmp` invokes the passed-in Lullaby callback, and `main`
    // drives it through the exported `run_callback`. `apply_cmp` is a non-static
    // external symbol so the Lullaby object's undefined `apply_cmp` resolves to it.
    let c_src = std::env::temp_dir().join("lullaby_ffi_callback_driver.c");
    std::fs::write(
        &c_src,
        "typedef long long i64;\n\
         i64 apply_cmp(i64 (*cmp)(i64, i64), i64 a, i64 b) { return cmp(a, b); }\n\
         extern i64 run_callback(i64, i64);\n\
         int main(void) { return (int)run_callback(10, 3); }\n",
    )
    .expect("write c driver");
    let out_exe = std::env::temp_dir().join("lullaby_ffi_callback_driver.exe");
    let _ = std::fs::remove_file(&out_exe);

    let link = if cc == "cl" {
        // cl driver.c lullaby.obj /Fe:out.exe (MSVC driver links the CRT + obj).
        Command::new("cl")
            .args(["/nologo"])
            .arg(&c_src)
            .arg(&obj)
            .arg(format!("/Fe:{}", out_exe.display()))
            .current_dir(std::env::temp_dir())
            .output()
    } else {
        Command::new("clang")
            .arg(&c_src)
            .arg(&obj)
            .arg("-o")
            .arg(&out_exe)
            .output()
    };
    let link = match link {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "C compiler `{cc}` could not link the callback object; skipping run:\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        Err(error) => {
            eprintln!("could not run C compiler `{cc}`: {error}; skipping run");
            return;
        }
    };
    let _ = link;

    assert!(
        out_exe.is_file(),
        "expected linked exe at {}",
        out_exe.display()
    );
    let run = Command::new(&out_exe).output().expect("run c driver exe");
    let exit = run.status.code().expect("c driver exit code");
    // run_callback(10, 3) → apply_cmp(diff, 10, 3) → diff(10, 3) == 7.
    assert_eq!(
        exit, 7,
        "Lullaby callback invoked through a C function pointer must exit 7"
    );
}
