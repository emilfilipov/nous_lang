//! Hover and go-to-definition resolution for the LSP server.
//!
//! Both features reuse the existing frontend: the document is lexed, parsed, and
//! (for hover) semantically validated so we can read the checked metadata the
//! checker already computed — inferred expression types (`expression_types`) and
//! function signatures (`signatures`). We never re-run inference here; we resolve
//! the identifier under the cursor against the parsed AST and that metadata.
//!
//! Positions are 0-based (LSP). Lullaby spans are 1-based single points whose
//! column marks the first character of the identifier/keyword token that starts
//! there (see the parser: `Variable`, `Call`, and declaration spans all point at
//! the token start). We match a cursor to an identifier by checking the cursor
//! lands within `[start_column, start_column + name.len())` on the token's line.

use lullaby_lexer::lex;
use lullaby_parser::{Function, Program, Stmt, TypeRef, parse};
use lullaby_semantics::{CheckedProgram, validate};
use serde_json::{Value, json};

/// The identifier under the cursor: its text plus the 0-based line and the
/// 0-based start column of the word it belongs to.
pub(crate) struct WordAt {
    pub(crate) name: String,
    pub(crate) line: usize,
    pub(crate) start: usize,
}

/// Compute the hover result for a position, or `None` when there is nothing to
/// show (whitespace, an unknown identifier, or a document that does not parse).
///
/// Returns an LSP `Hover` value:
/// `{ "contents": { "kind": "markdown", "value": .. } }`.
pub fn hover(text: &str, line: usize, character: usize) -> Option<Value> {
    let word = word_at(text, line, character)?;
    let checked = check(text)?;
    let program = &checked.program;

    // 1. A declaration of this name (function / struct / enum) — the most useful
    //    hover for a definition site or a call.
    if let Some(markdown) = declaration_hover(program, &word.name) {
        return Some(hover_value(markdown));
    }

    // 2. A known builtin: a short description. Checked before locals because a
    //    builtin *call* expression is also recorded in `expression_types` (with
    //    its return type), and the description is the more useful hover.
    if let Some(desc) = builtin_description(&word.name) {
        return Some(hover_value(format!(
            "```lullaby\n{}\n```\n\n{}",
            word.name, desc
        )));
    }

    // 3. A local or parameter: use the inferred type the checker recorded for the
    //    identifier expression at this exact position, else the declared type.
    if let Some(markdown) = local_hover(&checked, &word) {
        return Some(hover_value(markdown));
    }

    None
}

/// Compute the go-to-definition `Location` for a position, or `None` when the
/// identifier does not resolve to a definition in this document.
///
/// Returns an LSP `Location`: `{ "uri": .., "range": .. }`.
pub fn definition(text: &str, uri: &str, line: usize, character: usize) -> Option<Value> {
    let word = word_at(text, line, character)?;
    let tokens = lex(text).ok()?;
    let program = parse(&tokens).ok()?;

    // A top-level declaration (function / struct / enum / alias) with this name.
    if let Some(decl_line) = declaration_line(&program, &word.name) {
        let range = name_range_on_line(text, decl_line, &word.name)
            .unwrap_or_else(|| point_range(decl_line, 0));
        return Some(json!({ "uri": uri, "range": range }));
    }

    // Otherwise, a local `let` binding or parameter visible from the cursor: find
    // the enclosing function, then the binding of this name.
    let func = enclosing_function(&program, word.line)?;
    if let Some(decl_line) = let_binding_line(func, &word.name) {
        let range = name_range_on_line(text, decl_line, &word.name)
            .unwrap_or_else(|| point_range(decl_line, 0));
        return Some(json!({ "uri": uri, "range": range }));
    }
    if func.params.iter().any(|p| p.name == word.name) {
        // Parameters have no per-parameter span; point at the parameter name on
        // the function's signature line (the function span's line, 0-based).
        let decl_line = func.span.line.saturating_sub(1);
        let range = name_range_on_line(text, decl_line, &word.name)
            .unwrap_or_else(|| point_range(decl_line, 0));
        return Some(json!({ "uri": uri, "range": range }));
    }

    None
}

/// Run lex -> parse -> validate and return the checked program, or `None` if any
/// stage fails (hover then simply shows nothing).
fn check(text: &str) -> Option<CheckedProgram> {
    let tokens = lex(text).ok()?;
    let program = parse(&tokens).ok()?;
    validate(&program).ok()
}

/// Wrap markdown in an LSP `Hover` value.
pub(crate) fn hover_value(markdown: String) -> Value {
    json!({ "contents": { "kind": "markdown", "value": markdown } })
}

/// The identifier at a 0-based `(line, character)` position, or `None` if the
/// position is not on a word character. The returned `start` is the 0-based
/// column of the first character of the word.
pub(crate) fn word_at(text: &str, line: usize, character: usize) -> Option<WordAt> {
    let source_line = text.lines().nth(line)?;
    let chars: Vec<char> = source_line.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    if character > chars.len() {
        return None;
    }
    // Determine the index of the character the cursor is "on". Prefer the char to
    // the right; if that is not a word char, try the char to the left (LSP places
    // the caret between characters, so end-of-word still resolves).
    let on = if character < chars.len() && is_word(chars[character]) {
        character
    } else if character > 0 && is_word(chars[character - 1]) {
        character - 1
    } else {
        return None;
    };
    let mut start = on;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    let mut end = on;
    while end + 1 < chars.len() && is_word(chars[end + 1]) {
        end += 1;
    }
    let name: String = chars[start..=end].iter().collect();
    Some(WordAt { name, line, start })
}

/// Hover markdown for a top-level declaration named `name`, if any.
pub(crate) fn declaration_hover(program: &Program, name: &str) -> Option<String> {
    if let Some(func) = program.functions.iter().find(|f| f.name == name) {
        return Some(format!(
            "```lullaby\n{}\n```",
            function_signature_text(func)
        ));
    }
    if let Some(decl) = program.structs.iter().find(|s| s.name == name) {
        let fields = decl
            .fields
            .iter()
            .map(|f| format!("    {} {}", f.name, f.ty.name))
            .collect::<Vec<_>>()
            .join("\n");
        return Some(format!("```lullaby\nstruct {}\n{}\n```", decl.name, fields));
    }
    if let Some(decl) = program.enums.iter().find(|e| e.name == name) {
        let variants = decl
            .variants
            .iter()
            .map(|v| {
                if v.payload.is_empty() {
                    format!("    {}", v.name)
                } else {
                    let payload = v
                        .payload
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("    {} {}", v.name, payload)
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Some(format!("```lullaby\nenum {}\n{}\n```", decl.name, variants));
    }
    None
}

/// Render a function's signature as a single `fn NAME p1 T1 ... -> Ret` line.
pub(crate) fn function_signature_text(func: &Function) -> String {
    let mut out = String::from("fn ");
    out.push_str(&func.name);
    for param in &func.params {
        out.push(' ');
        out.push_str(&param.name);
        out.push(' ');
        out.push_str(&param.ty.name);
    }
    out.push_str(" -> ");
    out.push_str(&func.return_type.name);
    out
}

/// Hover markdown for a local or parameter under the cursor: the inferred type
/// the checker recorded for the identifier expression at this exact span, or the
/// declared parameter/local type as a fallback.
fn local_hover(checked: &CheckedProgram, word: &WordAt) -> Option<String> {
    // The identifier expression span is 1-based; our word start is 0-based.
    let span_line = word.line + 1;
    let span_col = word.start + 1;
    if let Some(entry) = checked
        .info
        .expression_types
        .iter()
        .find(|e| e.span.line == span_line && e.span.column == span_col)
    {
        return Some(format!("```lullaby\n{} {}\n```", word.name, entry.ty.name));
    }

    // Fall back to a declared parameter type in the enclosing function.
    let func = enclosing_function(&checked.program, word.line)?;
    if let Some(param) = func.params.iter().find(|p| p.name == word.name) {
        return Some(format!("```lullaby\n{} {}\n```", param.name, param.ty.name));
    }
    // A `let` with a declared type.
    if let Some(ty) = declared_let_type(func, &word.name) {
        return Some(format!("```lullaby\n{} {}\n```", word.name, ty.name));
    }
    None
}

/// The declared type of a `let NAME TYPE = ..` binding in `func`, if present.
fn declared_let_type<'a>(func: &'a Function, name: &str) -> Option<&'a TypeRef> {
    fn walk<'a>(stmts: &'a [Stmt], name: &str) -> Option<&'a TypeRef> {
        for stmt in stmts {
            if let Stmt::Let {
                name: n,
                ty: Some(ty),
                ..
            } = stmt
                && n == name
            {
                return Some(ty);
            }
            if let Some(ty) = walk(stmt_body(stmt), name) {
                return Some(ty);
            }
        }
        None
    }
    walk(&func.body, name)
}

/// The 0-based source line of the `let NAME` binding for `name` in `func`, if any.
fn let_binding_line(func: &Function, name: &str) -> Option<usize> {
    fn walk(stmts: &[Stmt], name: &str) -> Option<usize> {
        for stmt in stmts {
            if let Stmt::Let { name: n, span, .. } = stmt
                && n == name
            {
                return Some(span.line.saturating_sub(1));
            }
            if let Some(line) = walk(stmt_body(stmt), name) {
                return Some(line);
            }
        }
        None
    }
    walk(&func.body, name)
}

/// The nested statement bodies of a block-bearing statement, for recursive
/// traversal. Returns an empty slice for leaf statements. This is intentionally
/// conservative — it covers the common nesting (`while`/`for`/`loop`/`unsafe`/
/// `try` bodies); `if`/`match` arm bodies are not descended into, which only
/// means a `let` inside a conditional falls back to the function level.
pub(crate) fn stmt_body(stmt: &Stmt) -> &[Stmt] {
    match stmt {
        Stmt::While { body, .. }
        | Stmt::For { body, .. }
        | Stmt::Loop { body, .. }
        | Stmt::Unsafe { body, .. }
        | Stmt::Try { body, .. } => body,
        _ => &[],
    }
}

/// The function whose body/signature encloses the 0-based `line`, if any. Chooses
/// the function whose span line is the greatest one at or before `line` (source
/// order), so a cursor inside a body resolves to that body's function.
pub(crate) fn enclosing_function(program: &Program, line: usize) -> Option<&Function> {
    let target = line + 1; // spans are 1-based
    program
        .functions
        .iter()
        .filter(|f| f.span.line <= target)
        .max_by_key(|f| f.span.line)
}

/// The 0-based declaration line of a top-level declaration named `name`.
pub(crate) fn declaration_line(program: &Program, name: &str) -> Option<usize> {
    if let Some(func) = program.functions.iter().find(|f| f.name == name) {
        return Some(func.span.line.saturating_sub(1));
    }
    if let Some(decl) = program.structs.iter().find(|s| s.name == name) {
        return Some(decl.span.line.saturating_sub(1));
    }
    if let Some(decl) = program.enums.iter().find(|e| e.name == name) {
        return Some(decl.span.line.saturating_sub(1));
    }
    if let Some(decl) = program.aliases.iter().find(|a| a.name == name) {
        return Some(decl.span.line.saturating_sub(1));
    }
    None
}

/// A 0-based LSP range covering `name` on `line` in `text`, found by searching
/// the line text for the identifier as a whole word. `None` if not found.
pub(crate) fn name_range_on_line(text: &str, line: usize, name: &str) -> Option<Value> {
    let source_line = text.lines().nth(line)?;
    let chars: Vec<char> = source_line.chars().collect();
    let needle: Vec<char> = name.chars().collect();
    if needle.is_empty() {
        return None;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        if chars[i..i + needle.len()] == needle[..] {
            let before_ok = i == 0 || !is_word(chars[i - 1]);
            let after_ok = i + needle.len() >= chars.len() || !is_word(chars[i + needle.len()]);
            if before_ok && after_ok {
                return Some(json!({
                    "start": { "line": line, "character": i },
                    "end": { "line": line, "character": i + needle.len() },
                }));
            }
        }
        i += 1;
    }
    None
}

/// A zero-width LSP range at `(line, character)`.
pub(crate) fn point_range(line: usize, character: usize) -> Value {
    json!({
        "start": { "line": line, "character": character },
        "end": { "line": line, "character": character },
    })
}

/// A short human description for a well-known builtin, or `None`.
fn builtin_description(name: &str) -> Option<&'static str> {
    let desc = match name {
        "print" => "Write a string to standard output without a trailing newline.",
        "println" => "Write a string followed by a newline to standard output.",
        "warn" => "Write a string followed by a newline to standard error.",
        "assert" => "Abort with a runtime error when the boolean argument is false.",
        "len" => "The length of a string, array, or list.",
        "some" => "Wrap a value in an `option<T>` present case.",
        "ok" => "Wrap a value in the success case of a `result<T, E>`.",
        "err" => "Wrap a value in the error case of a `result<T, E>`.",
        "list_new" => "Create a new empty growable `list<T>`.",
        "map_new" => "Create a new empty `map<K, V>`.",
        "flush" => "Flush buffered standard output.",
        _ => return None,
    };
    Some(desc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROG: &str = "\
fn add a i64 b i64 -> i64
    return a + b

fn main -> i64
    let total i64 = add(1, 2)
    return total
";

    #[test]
    fn hover_on_function_name_shows_signature() {
        // `add` on line 4 (0-based), inside the call `add(1, 2)`.
        let value = hover(PROG, 4, 20).expect("hover result");
        let text = value["contents"]["value"].as_str().unwrap();
        assert!(text.contains("fn add a i64 b i64 -> i64"), "got {text}");
    }

    #[test]
    fn hover_on_typed_local_shows_type() {
        // `total` on the `return total` line (line 5, 0-based).
        let value = hover(PROG, 5, 11).expect("hover result");
        let text = value["contents"]["value"].as_str().unwrap();
        assert!(text.contains("total i64"), "got {text}");
    }

    #[test]
    fn hover_on_builtin_shows_description() {
        let src = "fn main -> i64\n    println(\"hi\")\n    return 0\n";
        let value = hover(src, 1, 4).expect("hover result");
        let text = value["contents"]["value"].as_str().unwrap();
        assert!(text.to_lowercase().contains("newline"), "got {text}");
    }

    #[test]
    fn hover_on_whitespace_is_none() {
        // Line 2 is blank.
        assert!(hover(PROG, 2, 0).is_none());
    }

    #[test]
    fn hover_on_keyword_is_none() {
        // A `return` keyword is not an identifier we resolve.
        let src = "fn main -> i64\n    return 0\n";
        assert!(hover(src, 1, 4).is_none());
    }

    #[test]
    fn definition_on_call_jumps_to_function() {
        let loc = definition(PROG, "file:///a.lby", 4, 20).expect("definition");
        assert_eq!(loc["uri"], json!("file:///a.lby"));
        // `add` is declared on line 0.
        assert_eq!(loc["range"]["start"]["line"], json!(0));
        // The range covers the name `add` after `fn `.
        assert_eq!(loc["range"]["start"]["character"], json!(3));
        assert_eq!(loc["range"]["end"]["character"], json!(6));
    }

    #[test]
    fn definition_on_local_jumps_to_let() {
        // `total` in `return total` (line 5) jumps to its `let` on line 4.
        let loc = definition(PROG, "file:///a.lby", 5, 11).expect("definition");
        assert_eq!(loc["range"]["start"]["line"], json!(4));
    }

    #[test]
    fn definition_on_param_jumps_to_signature() {
        // `a` in `return a + b` (line 1) jumps to the signature line 0.
        let loc = definition(PROG, "file:///a.lby", 1, 11).expect("definition");
        assert_eq!(loc["range"]["start"]["line"], json!(0));
    }

    #[test]
    fn definition_on_whitespace_is_none() {
        assert!(definition(PROG, "file:///a.lby", 2, 0).is_none());
    }
}
