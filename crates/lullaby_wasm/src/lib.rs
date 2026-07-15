//! WebAssembly entry point for the Lullaby browser playground.
//!
//! Compiles the interpreter pipeline (lex → parse → semantic check → AST run) to
//! `wasm32` and exposes a single `run(source)` that executes a `.lby` program
//! entirely in the browser and returns its captured output as JSON. No server,
//! no code execution off the user's machine.

use lullaby_lexer::{Diagnostic, lex};
use lullaby_parser::parse;
use lullaby_runtime::{clear_wasm_output, run_main, take_wasm_output};
use lullaby_semantics::{SemanticDiagnostic, validate};
use wasm_bindgen::prelude::*;

fn render_diagnostics(diags: &[Diagnostic]) -> String {
    diags
        .iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_semantic(diags: &[SemanticDiagnostic]) -> String {
    diags
        .iter()
        .map(|d| format!("{}: {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run a Lullaby program. Returns a JSON string:
/// `{"ok":true,"output":"…","result":"…"}` on success, or
/// `{"ok":false,"output":"…","error":"…"}` on a lex/parse/semantic/runtime error
/// (`output` still carries whatever the program printed before it failed).
#[wasm_bindgen]
pub fn run(source: &str) -> String {
    clear_wasm_output();
    let outcome = (|| -> Result<String, String> {
        let tokens = lex(source).map_err(|d| render_diagnostics(&d))?;
        let program = parse(&tokens).map_err(|d| render_diagnostics(&d))?;
        // Run the checked program: `validate` resolves aliases, writes back
        // inferred return types, and folds named compile-time constants into
        // literals, so the AST interpreter never sees an unresolved `const`.
        let checked = validate(&program).map_err(|d| render_semantic(&d))?;
        let value =
            run_main(&checked.program).map_err(|e| format!("{}: {}", e.code, e.message))?;
        Ok(value.to_string())
    })();
    let output = take_wasm_output();
    let json = match outcome {
        Ok(result) => serde_json::json!({ "ok": true, "output": output, "result": result }),
        Err(error) => serde_json::json!({ "ok": false, "output": output, "error": error }),
    };
    json.to_string()
}
