//! Code-completion for the LSP server (`textDocument/completion`).
//!
//! Completion is intentionally simple and robust rather than context-sensitive:
//! it offers the union of
//!
//! 1. the Lullaby **keyword** set,
//! 2. the **in-file top-level declarations** (functions, structs, enums, type
//!    aliases, traits, constants) parsed from the current buffer, and
//! 3. the **in-scope locals and parameters** of the function enclosing the
//!    cursor.
//!
//! Imported `pub` symbols reachable through the file's `import`s are added by the
//! module-aware layer in [`crate::project`], which reuses
//! [`public_declaration_items`].
//!
//! The one hard requirement is graceful degradation: a mid-edit buffer that does
//! not lex/parse must never panic and must still yield keyword completions. So
//! keywords are produced unconditionally, and the declaration/local passes only
//! run when the buffer parses. Member (`.`) completion is deliberately out of
//! scope for this increment (see `documents/lsp_design.md`).

use std::collections::HashSet;

use lullaby_lexer::lex;
use lullaby_parser::{Function, Program, Stmt, parse};
use serde_json::{Value, json};

use crate::analysis;

// LSP `CompletionItemKind` numeric codes (see the LSP specification). Only the
// kinds Lullaby can produce are named here.
const KIND_FUNCTION: i64 = 3;
const KIND_CLASS: i64 = 7; // used for a type alias (a named type)
const KIND_INTERFACE: i64 = 8; // used for a trait
const KIND_VARIABLE: i64 = 6; // a local `let` or a parameter
const KIND_ENUM: i64 = 13;
const KIND_KEYWORD: i64 = 14;
const KIND_CONSTANT: i64 = 21;
const KIND_STRUCT: i64 = 22;

/// The Lullaby keyword set, transcribed from the lexer's keyword table
/// (`keyword` in `crates/lullaby_lexer/src/lib.rs`). The lexer owns no public
/// enumeration of its keywords, so the list is mirrored here and pinned to the
/// lexer by the `keyword_list_matches_the_lexer` test, which asserts every entry
/// still lexes to a single `Keyword` token — a keyword renamed or removed
/// upstream fails the build rather than silently drifting.
pub(crate) const KEYWORDS: &[&str] = &[
    "fn",
    "return",
    "if",
    "elif",
    "else",
    "for",
    "from",
    "to",
    "by",
    "in",
    "while",
    "loop",
    "break",
    "continue",
    "let",
    "const",
    "and",
    "or",
    "not",
    "true",
    "false",
    "void",
    "module",
    "import",
    "pub",
    "package",
    "struct",
    "enum",
    "union",
    "trait",
    "impl",
    "interface",
    "class",
    "match",
    "switch",
    "try",
    "catch",
    "throw",
    "async",
    "await",
    "coroutine",
    "unsafe",
    "region",
    "alias",
    "extern",
    "export",
    "asm",
];

/// Build the completion items for a document's `text`, with `line` being the
/// 0-based cursor line when known (used only to find the enclosing function for
/// local/parameter completion).
///
/// Always includes the keyword set. When the buffer lexes and parses it also
/// adds the in-file top-level declarations and the enclosing function's locals
/// and parameters. An unparseable buffer degrades to keywords only and never
/// panics. Labels are de-duplicated (the first occurrence wins).
pub(crate) fn completion_items(text: &str, line: Option<usize>) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    // Declarations and in-scope locals are only available when the buffer parses.
    if let Some(program) = parse_program(text) {
        if let Some(line) = line
            && let Some(func) = analysis::enclosing_function(&program, line)
        {
            for item in local_items(func) {
                push_unique(&mut items, &mut seen, item);
            }
        }
        for item in declaration_items(&program) {
            push_unique(&mut items, &mut seen, item);
        }
    }

    // Keywords are always offered, even for a buffer that does not parse.
    for keyword in KEYWORDS {
        push_unique(&mut items, &mut seen, keyword_item(keyword));
    }

    items
}

/// Completion items for every top-level declaration in `program`, regardless of
/// visibility (the current file sees all of its own declarations).
pub(crate) fn declaration_items(program: &Program) -> Vec<Value> {
    collect_declaration_items(program, false)
}

/// Completion items for the **`pub`** top-level declarations in `program`. Used
/// for imported modules, where only exported symbols are reachable.
pub(crate) fn public_declaration_items(program: &Program) -> Vec<Value> {
    collect_declaration_items(program, true)
}

/// Shared body of [`declaration_items`] / [`public_declaration_items`]. When
/// `public_only` is set, non-`pub` declarations are skipped.
fn collect_declaration_items(program: &Program, public_only: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for func in &program.functions {
        if public_only && !func.is_public {
            continue;
        }
        out.push(item(
            &func.name,
            KIND_FUNCTION,
            analysis::function_signature_text(func),
        ));
    }
    for decl in &program.structs {
        if public_only && !decl.is_public {
            continue;
        }
        out.push(item(
            &decl.name,
            KIND_STRUCT,
            format!("struct {}", decl.name),
        ));
    }
    for decl in &program.enums {
        if public_only && !decl.is_public {
            continue;
        }
        out.push(item(&decl.name, KIND_ENUM, format!("enum {}", decl.name)));
    }
    for decl in &program.aliases {
        if public_only && !decl.is_public {
            continue;
        }
        out.push(item(
            &decl.name,
            KIND_CLASS,
            format!("alias {} = {}", decl.name, decl.target.name),
        ));
    }
    for decl in &program.traits {
        if public_only && !decl.is_public {
            continue;
        }
        out.push(item(
            &decl.name,
            KIND_INTERFACE,
            format!("trait {}", decl.name),
        ));
    }
    for decl in &program.consts {
        if public_only && !decl.is_public {
            continue;
        }
        out.push(item(
            &decl.name,
            KIND_CONSTANT,
            format!("const {} {}", decl.name, decl.ty.name),
        ));
    }
    out
}

/// Completion items for the parameters and `let` locals of `func`, rendered as
/// `NAME TYPE` (the declared type when present).
fn local_items(func: &Function) -> Vec<Value> {
    let mut out = Vec::new();
    for param in &func.params {
        out.push(item(
            &param.name,
            KIND_VARIABLE,
            format!("{} {}", param.name, param.ty.name),
        ));
    }
    collect_let_items(&func.body, &mut out);
    out
}

/// Recursively collect `let` bindings from a statement list (descending the same
/// block-bearing statements [`analysis::stmt_body`] exposes) as `Variable`
/// completion items.
fn collect_let_items(stmts: &[Stmt], out: &mut Vec<Value>) {
    for stmt in stmts {
        if let Stmt::Let { name, ty, .. } = stmt {
            let detail = match ty {
                Some(ty) => format!("{} {}", name, ty.name),
                None => format!("let {name}"),
            };
            out.push(item(name, KIND_VARIABLE, detail));
        }
        collect_let_items(analysis::stmt_body(stmt), out);
    }
}

/// A keyword completion item.
fn keyword_item(keyword: &str) -> Value {
    item(keyword, KIND_KEYWORD, "keyword".to_string())
}

/// Build one LSP `CompletionItem` JSON value.
fn item(label: &str, kind: i64, detail: String) -> Value {
    json!({ "label": label, "kind": kind, "detail": detail })
}

/// Push `item` onto `items` unless an item with the same `label` was already
/// added (the first occurrence wins).
fn push_unique(items: &mut Vec<Value>, seen: &mut HashSet<String>, item: Value) {
    if let Some(label) = item["label"].as_str()
        && seen.insert(label.to_string())
    {
        items.push(item);
    }
}

/// Lex and parse `text`, returning the [`Program`] or `None` if either stage
/// fails (completion then degrades to keywords only).
fn parse_program(text: &str) -> Option<Program> {
    let tokens = lex(text).ok()?;
    parse(&tokens).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lullaby_lexer::TokenKind;

    /// Every entry in [`KEYWORDS`] must still lex to exactly one `Keyword` token.
    /// This pins the mirrored list to the lexer's own keyword table so a rename
    /// or removal upstream fails here instead of silently offering a stale word.
    #[test]
    fn keyword_list_matches_the_lexer() {
        for keyword in KEYWORDS {
            let tokens = lex(keyword).expect("keyword lexes");
            let kinds: Vec<&TokenKind> = tokens
                .iter()
                .map(|t| &t.kind)
                .filter(|k| !matches!(k, TokenKind::Newline | TokenKind::Eof))
                .collect();
            assert!(
                matches!(kinds.as_slice(), [TokenKind::Keyword(_)]),
                "`{keyword}` should lex to a single Keyword token, got {kinds:?}"
            );
        }
    }

    fn labels(items: &[Value]) -> Vec<String> {
        items
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect()
    }

    fn find<'a>(items: &'a [Value], label: &str) -> Option<&'a Value> {
        items.iter().find(|i| i["label"] == json!(label))
    }

    const PROG: &str = "\
struct Point
    x i64
    y i64

enum Color
    Red
    Green

alias Int = i64

const LIMIT i64 = 10

fn add a i64 b i64 -> i64
    let sum i64 = a + b
    return sum
";

    #[test]
    fn offers_keywords_including_control_forms() {
        let items = completion_items(PROG, None);
        let labels = labels(&items);
        for expected in [
            "fn", "let", "struct", "enum", "match", "if", "for", "import", "pub",
        ] {
            assert!(labels.contains(&expected.to_string()), "missing {expected}");
        }
        // Keywords carry the Keyword kind (14).
        assert_eq!(find(&items, "fn").unwrap()["kind"], json!(KIND_KEYWORD));
    }

    #[test]
    fn offers_in_file_declarations_with_the_right_kind() {
        let items = completion_items(PROG, None);
        assert_eq!(find(&items, "add").unwrap()["kind"], json!(KIND_FUNCTION));
        assert!(
            find(&items, "add").unwrap()["detail"]
                .as_str()
                .unwrap()
                .contains("fn add a i64 b i64 -> i64")
        );
        assert_eq!(find(&items, "Point").unwrap()["kind"], json!(KIND_STRUCT));
        assert_eq!(find(&items, "Color").unwrap()["kind"], json!(KIND_ENUM));
        assert_eq!(find(&items, "Int").unwrap()["kind"], json!(KIND_CLASS));
        assert_eq!(find(&items, "LIMIT").unwrap()["kind"], json!(KIND_CONSTANT));
    }

    #[test]
    fn offers_locals_and_parameters_inside_the_enclosing_function() {
        // The `let sum` line is line 15 (0-based); the function starts on line 13.
        let items = completion_items(PROG, Some(15));
        assert_eq!(find(&items, "a").unwrap()["kind"], json!(KIND_VARIABLE));
        assert_eq!(find(&items, "b").unwrap()["kind"], json!(KIND_VARIABLE));
        assert_eq!(find(&items, "sum").unwrap()["kind"], json!(KIND_VARIABLE));
        assert!(
            find(&items, "sum").unwrap()["detail"]
                .as_str()
                .unwrap()
                .contains("sum i64")
        );
    }

    #[test]
    fn unparseable_buffer_still_offers_keywords_without_panicking() {
        // A brace is a forbidden delimiter, so this never parses.
        let bad = "fn main -> i64 {\n    let x = \n";
        let items = completion_items(bad, Some(1));
        let labels = labels(&items);
        assert!(labels.contains(&"fn".to_string()));
        assert!(labels.contains(&"return".to_string()));
        // No declarations are offered because the buffer did not parse.
        assert!(find(&items, "main").is_none());
    }

    #[test]
    fn labels_are_deduplicated() {
        let items = completion_items(PROG, Some(15));
        let mut labels = labels(&items);
        let before = labels.len();
        labels.sort();
        labels.dedup();
        assert_eq!(before, labels.len(), "labels should be unique");
    }

    #[test]
    fn public_declaration_items_filters_to_pub() {
        let src = "pub fn exported -> i64\n    return 0\n\nfn hidden -> i64\n    return 0\n";
        let tokens = lex(src).unwrap();
        let program = parse(&tokens).unwrap();
        let items = public_declaration_items(&program);
        assert!(
            find(&items, "exported").is_some(),
            "pub fn should be offered"
        );
        assert!(
            find(&items, "hidden").is_none(),
            "private fn is not exported"
        );
    }
}
