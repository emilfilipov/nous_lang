//! Project- and module-aware analysis for the language server.
//!
//! The single-document pipeline in [`crate::diagnostics`] and [`crate::analysis`]
//! only ever sees the open buffer, so any project that spans multiple files via
//! `import` gets wrong or missing results: a `pub` symbol defined in an imported
//! module looks "undefined", and go-to-definition/hover can never cross a file
//! boundary. This module closes that gap by running the *same* module loader the
//! CLI uses (`lullaby_loader`) over the file's project, so diagnostics, hover,
//! and go-to-definition reflect the merged program.
//!
//! It is deliberately conservative about when it engages the loader. A file that
//! neither uses `import` nor lives inside a `lullaby.json` project is analyzed by
//! the unchanged single-document path, so a lone file behaves exactly as before.
//! The editor's live buffers are threaded through the loader as a
//! [`SourceOverlay`], so unsaved edits (in the open file or any other open file)
//! are what get analyzed — not stale on-disk bytes. Every fallible step degrades
//! to single-document analysis rather than failing, so the server never panics on
//! a mid-edit buffer, a missing import, or a malformed manifest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lullaby_diagnostics::DiagnosticReport;
use lullaby_lexer::lex;
use lullaby_loader::loader::{self, SourceOverlay, overlay_key};
use lullaby_loader::manifest;
use lullaby_parser::{Program, parse};
use lullaby_semantics::{SemanticDiagnostic, validate};
use serde_json::{Value, json};

use crate::{analysis, completion, diagnostics};

/// Compute the diagnostics to publish for the document `uri` (current text
/// `text`), consulting every open document in `documents` for cross-file
/// resolution.
///
/// When the file uses `import` or belongs to a project, diagnostics reflect the
/// merged program (imported symbols resolve, and loader diagnostics
/// `L0391`/`L0392`/`L0393`/`L0397` for this file surface); the open file's own
/// lex/parse/semantic errors are still reported at their real positions. A lone
/// file with no imports and no project is analyzed exactly as the single-document
/// pipeline did.
pub fn diagnostics(uri: &str, text: &str, documents: &HashMap<String, String>) -> Vec<Value> {
    let Some(path) = uri_to_path(uri) else {
        return diagnostics::compute(text);
    };

    // The open buffer's own lex/parse errors are authoritative and correctly
    // positioned. Report them directly (and do not run the loader, which would
    // only re-derive the same entry-file failure).
    let program = match diagnostics::lex_parse_lsp(text) {
        Ok(program) => program,
        Err(items) => return items,
    };

    let search_dirs = discover_search_dirs(&path);
    if program.imports.is_empty() && search_dirs.is_none() {
        // A lone file with no imports and no project: unchanged behavior.
        return diagnostics::compute(text);
    }

    let overlay = build_overlay(documents);
    let dirs = search_dirs.unwrap_or_default();
    match loader::load_program_in_project_with_overlay(&path, &dirs, &overlay) {
        // Loader errors (missing/cyclic imports, cross-module visibility,
        // duplicate names, or a lex/parse error in an imported file) carry the
        // owning file's path; keep only the ones for this document.
        Err(reports) => reports
            .into_iter()
            .filter(|report| report_belongs_to(report, &path))
            .map(|report| diagnostics::report_to_lsp(text, &report))
            .collect(),
        Ok(loaded) => match validate(&loaded.program) {
            Ok(_) => Vec::new(),
            Err(semantic) => semantic
                .into_iter()
                .filter(|diag| semantic_belongs(diag, &program, text))
                .map(|diag| diagnostics::semantic_diag_to_lsp(text, diag))
                .collect(),
        },
    }
}

/// Resolve a hover for a position, crossing module boundaries when the buffer's
/// own resolution finds nothing. Returns `null` when there is nothing to show.
pub fn hover(
    uri: &str,
    text: &str,
    line: usize,
    character: usize,
    documents: &HashMap<String, String>,
) -> Value {
    if let Some(value) = analysis::hover(text, line, character) {
        return value;
    }
    cross_file_hover(uri, text, line, character, documents).unwrap_or(Value::Null)
}

/// Resolve go-to-definition for a position, crossing module boundaries when the
/// buffer's own resolution finds nothing. Returns `null` when it does not
/// resolve.
pub fn definition(
    uri: &str,
    text: &str,
    line: usize,
    character: usize,
    documents: &HashMap<String, String>,
) -> Value {
    if let Some(value) = analysis::definition(text, uri, line, character) {
        return value;
    }
    cross_file_definition(uri, text, line, character, documents).unwrap_or(Value::Null)
}

/// Compute the completion items for the document `uri`, with `line` being the
/// 0-based cursor line when known. Always offers keywords and the buffer's own
/// in-scope declarations/locals (via [`completion::completion_items`]); when the
/// file is module-aware, it additionally offers the `pub` symbols reachable
/// through its imports, resolved by the same loader as diagnostics/hover.
///
/// Every step degrades gracefully: an unparseable buffer still yields keywords,
/// and a failed project load simply omits the imported symbols.
pub fn completion(
    uri: &str,
    text: &str,
    line: Option<usize>,
    documents: &HashMap<String, String>,
) -> Value {
    let mut items = completion::completion_items(text, line);
    let mut seen: std::collections::HashSet<String> = items
        .iter()
        .filter_map(|item| item["label"].as_str().map(str::to_string))
        .collect();

    if let Some(path) = uri_to_path(uri)
        && let Some(loaded) = load_project(&path, text, documents)
    {
        let entry = overlay_key(&path);
        for module in &loaded.modules {
            // Skip the entry file itself: its declarations are already offered by
            // the buffer-local pass above.
            if overlay_key(&module.path) == entry {
                continue;
            }
            for item in completion::public_declaration_items(&module.program) {
                if let Some(label) = item["label"].as_str()
                    && seen.insert(label.to_string())
                {
                    items.push(item);
                }
            }
        }
    }

    Value::Array(items)
}

/// Hover for an identifier that resolves to a `pub` declaration in an imported
/// module: renders that declaration's signature from the other file.
fn cross_file_hover(
    uri: &str,
    text: &str,
    line: usize,
    character: usize,
    documents: &HashMap<String, String>,
) -> Option<Value> {
    let path = uri_to_path(uri)?;
    let word = analysis::word_at(text, line, character)?;
    let loaded = load_project(&path, text, documents)?;
    let entry = overlay_key(&path);
    for module in &loaded.modules {
        if overlay_key(&module.path) == entry {
            continue;
        }
        if let Some(markdown) = analysis::declaration_hover(&module.program, &word.name) {
            return Some(analysis::hover_value(markdown));
        }
    }
    None
}

/// Go-to-definition for an identifier that resolves to a declaration in an
/// imported module: returns a `Location` pointing into the other file.
fn cross_file_definition(
    uri: &str,
    text: &str,
    line: usize,
    character: usize,
    documents: &HashMap<String, String>,
) -> Option<Value> {
    let path = uri_to_path(uri)?;
    let word = analysis::word_at(text, line, character)?;
    let loaded = load_project(&path, text, documents)?;
    let entry = overlay_key(&path);
    for module in &loaded.modules {
        if overlay_key(&module.path) == entry {
            continue;
        }
        if let Some(decl_line) = analysis::declaration_line(&module.program, &word.name) {
            let range = analysis::name_range_on_line(&module.source, decl_line, &word.name)
                .unwrap_or_else(|| analysis::point_range(decl_line, 0));
            return Some(json!({ "uri": path_to_uri(&module.path), "range": range }));
        }
    }
    None
}

/// Run the module loader for `path`'s project, or `None` when the file is not
/// module-aware (no imports and no project) or the load fails for any reason.
fn load_project(
    path: &Path,
    text: &str,
    documents: &HashMap<String, String>,
) -> Option<loader::LoadedProgram> {
    let has_imports = lex(text)
        .ok()
        .and_then(|tokens| parse(&tokens).ok())
        .is_some_and(|program| !program.imports.is_empty());
    let search_dirs = discover_search_dirs(path);
    if !has_imports && search_dirs.is_none() {
        return None;
    }
    let overlay = build_overlay(documents);
    let dirs = search_dirs.unwrap_or_default();
    loader::load_program_in_project_with_overlay(path, &dirs, &overlay).ok()
}

/// Whether a loader diagnostic belongs to the file at `path`. Loader reports
/// always carry the owning file's path; a report without one is kept
/// (defensively) so a genuine error is never silently dropped.
fn report_belongs_to(report: &DiagnosticReport, path: &Path) -> bool {
    match &report.source_path {
        Some(source) => overlay_key(Path::new(source)) == overlay_key(path),
        None => true,
    }
}

/// Whether a semantic diagnostic on the merged program belongs to the open file.
///
/// Top-level declaration names are globally unique across a merged project (the
/// loader's `L0391` no-shadowing rule), so a diagnostic that names its enclosing
/// function is attributed exactly by that name. The rare function-less diagnostic
/// is attributed by whether its span falls within the open file's own extent.
fn semantic_belongs(diag: &SemanticDiagnostic, buffer: &Program, text: &str) -> bool {
    if let Some(function) = &diag.function {
        return buffer.functions.iter().any(|f| &f.name == function);
    }
    match diag.span {
        Some(span) => {
            let first = first_declaration_line(buffer);
            let last = text.lines().count();
            first.is_some_and(|start| span.line >= start && span.line <= last.max(start))
        }
        None => true,
    }
}

/// The 1-based line of the earliest top-level declaration in `program`, if any.
fn first_declaration_line(program: &Program) -> Option<usize> {
    let mut lines: Vec<usize> = Vec::new();
    lines.extend(program.functions.iter().map(|item| item.span.line));
    lines.extend(program.structs.iter().map(|item| item.span.line));
    lines.extend(program.enums.iter().map(|item| item.span.line));
    lines.extend(program.aliases.iter().map(|item| item.span.line));
    lines.extend(program.consts.iter().map(|item| item.span.line));
    lines.into_iter().min()
}

/// Build a [`SourceOverlay`] from every open document so the loader analyzes the
/// editor's live buffers instead of on-disk bytes.
fn build_overlay(documents: &HashMap<String, String>) -> SourceOverlay {
    let mut overlay = SourceOverlay::new();
    for (uri, text) in documents {
        if let Some(path) = uri_to_path(uri) {
            overlay.insert(overlay_key(&path), text.clone());
        }
    }
    overlay
}

/// Walk up from the file's directory looking for a `lullaby.json`; if found,
/// resolve it into the project's ordered source search directories. Returns
/// `None` when the file is not inside a project, or the nearest manifest is
/// malformed (degrade to sibling-only import resolution).
fn discover_search_dirs(path: &Path) -> Option<Vec<PathBuf>> {
    let mut dir = path.parent();
    while let Some(current) = dir {
        if let Some((root, manifest_path)) = manifest::manifest_path_for(current) {
            return manifest::load_manifest(&root, &manifest_path)
                .ok()
                .map(|resolved| resolved.search_dirs);
        }
        dir = current.parent();
    }
    None
}

/// Convert a `file://` URI to a filesystem path, or `None` for a non-file URI
/// (e.g. an `untitled:` buffer that has never been saved).
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Skip an (empty, for local files) authority up to the path's leading slash.
    let path_part = &rest[rest.find('/')?..];
    let decoded = percent_decode(path_part);
    let normalized = strip_leading_slash_for_drive(&decoded);
    if normalized.is_empty() {
        return None;
    }
    Some(PathBuf::from(normalized))
}

/// Convert a filesystem path to a `file://` URI with percent-encoding, so a
/// returned definition `Location` names the target file the way the client sent
/// it.
fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let with_slash = if text.starts_with('/') {
        text
    } else {
        format!("/{text}")
    };
    format!("file://{}", percent_encode_path(&with_slash))
}

/// Drop the leading slash a `file://` URI puts before a Windows drive letter, so
/// `/C:/x` becomes `C:/x`. Unix paths (`/home/...`) are returned unchanged.
fn strip_leading_slash_for_drive(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        path[1..].to_string()
    } else {
        path.to_string()
    }
}

/// Decode `%XX` escapes in a URI path component. Invalid escapes are left as-is.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a path for a `file://` URI, keeping unreserved characters and
/// the path separator `/` and escaping everything else (e.g. `:` -> `%3A`).
fn percent_encode_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// The numeric value of a single hex digit byte, or `None` if it is not one.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A unique on-disk directory for a multi-file test, cleaned up on drop.
    struct TempProject {
        dir: PathBuf,
    }

    impl TempProject {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "lullaby_lsp_project_{}_{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self { dir }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.dir.join(name);
            fs::write(&path, contents).expect("write file");
            path
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Build an open-documents map from `(uri, text)` pairs.
    fn documents(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(uri, text)| ((*uri).to_string(), (*text).to_string()))
            .collect()
    }

    const MODULE_B: &str = "pub fn square x i64 -> i64\n    return x * x\n";
    const MAIN_A: &str = "import math\n\nfn main -> i64\n    return square(3)\n";

    #[test]
    fn imported_symbol_is_not_flagged_undefined() {
        let project = TempProject::new();
        let a_path = project.write("main.lby", MAIN_A);
        project.write("math.lby", MODULE_B);
        let uri = path_to_uri(&a_path);
        // Only the entry is "open"; the imported module is read from disk.
        let docs = documents(&[(&uri, MAIN_A)]);

        let diags = diagnostics(&uri, MAIN_A, &docs);
        assert!(
            diags.is_empty(),
            "imported `square` must resolve, got {diags:?}"
        );
    }

    #[test]
    fn genuine_error_in_entry_is_reported_at_the_right_range() {
        let project = TempProject::new();
        // `let flag bool = 1` on line 3 (0-based) is a real type mismatch; the
        // imported `square` on line 4 must NOT be flagged undefined.
        let main = "import math\n\nfn main -> i64\n    let flag bool = 1\n    return square(3)\n";
        let a_path = project.write("main.lby", main);
        project.write("math.lby", MODULE_B);
        let uri = path_to_uri(&a_path);
        let docs = documents(&[(&uri, main)]);

        let diags = diagnostics(&uri, main, &docs);
        assert!(!diags.is_empty(), "the type mismatch must be reported");
        assert!(
            diags
                .iter()
                .all(|d| !d["message"].as_str().unwrap_or("").contains("square")),
            "`square` is imported and must not be reported, got {diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d["range"]["start"]["line"] == serde_json::json!(3)),
            "the mismatch is on line 3 (0-based), got {diags:?}"
        );
    }

    #[test]
    fn cross_module_definition_jumps_to_the_other_file() {
        let project = TempProject::new();
        let a_path = project.write("main.lby", MAIN_A);
        project.write("math.lby", MODULE_B);
        let uri = path_to_uri(&a_path);
        let docs = documents(&[(&uri, MAIN_A)]);

        // `square` inside the call on line 3 (0-based), char 13.
        let loc = definition(&uri, MAIN_A, 3, 13, &docs);
        assert_ne!(loc, Value::Null, "definition should resolve across files");
        assert!(
            loc["uri"].as_str().unwrap_or("").ends_with("math.lby"),
            "definition should point at math.lby, got {loc:?}"
        );
        // `square` is declared on line 0 of math.lby.
        assert_eq!(loc["range"]["start"]["line"], serde_json::json!(0));
    }

    #[test]
    fn cross_module_hover_shows_the_other_files_signature() {
        let project = TempProject::new();
        let a_path = project.write("main.lby", MAIN_A);
        project.write("math.lby", MODULE_B);
        let uri = path_to_uri(&a_path);
        let docs = documents(&[(&uri, MAIN_A)]);

        let value = hover(&uri, MAIN_A, 3, 13, &docs);
        let contents = value["contents"]["value"].as_str().unwrap_or_default();
        assert!(
            contents.contains("fn square x i64 -> i64"),
            "hover should show the imported signature, got {contents:?}"
        );
    }

    #[test]
    fn completion_includes_imported_pub_symbol() {
        let project = TempProject::new();
        let a_path = project.write("main.lby", MAIN_A);
        project.write("math.lby", MODULE_B);
        let uri = path_to_uri(&a_path);
        let docs = documents(&[(&uri, MAIN_A)]);

        let value = completion(&uri, MAIN_A, Some(3), &docs);
        let items = value.as_array().expect("completion array");
        let square = items
            .iter()
            .find(|item| item["label"] == json!("square"))
            .expect("imported `square` should be offered");
        // `square` is a function (kind 3), and its detail is the imported signature.
        assert_eq!(square["kind"], json!(3));
        assert!(
            square["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("fn square x i64 -> i64"),
            "got {square:?}"
        );
        // Keywords are still present alongside the imported symbol.
        assert!(items.iter().any(|item| item["label"] == json!("fn")));
    }

    #[test]
    fn completion_on_lone_unparseable_file_still_offers_keywords() {
        let project = TempProject::new();
        // A forbidden brace: never parses, and the file has no imports/project.
        let bad = "fn main -> i64 {\n";
        let path = project.write("lone.lby", bad);
        let uri = path_to_uri(&path);
        let docs = documents(&[(&uri, bad)]);

        let value = completion(&uri, bad, Some(0), &docs);
        let items = value.as_array().expect("completion array");
        assert!(items.iter().any(|item| item["label"] == json!("fn")));
    }

    #[test]
    fn single_file_no_import_matches_single_document_pipeline() {
        // A lone file with no imports and no project must behave exactly as the
        // unchanged single-document pipeline, for both a valid and invalid file.
        let project = TempProject::new();

        let valid = "fn main -> i64\n    return 0\n";
        let valid_path = project.write("valid.lby", valid);
        let valid_uri = path_to_uri(&valid_path);
        let docs = documents(&[(&valid_uri, valid)]);
        assert_eq!(
            diagnostics(&valid_uri, valid, &docs),
            diagnostics::compute(valid)
        );

        let invalid = "fn main -> i64\n    let value bool = 1\n    return 0\n";
        let invalid_path = project.write("invalid.lby", invalid);
        let invalid_uri = path_to_uri(&invalid_path);
        let docs = documents(&[(&invalid_uri, invalid)]);
        let got = diagnostics(&invalid_uri, invalid, &docs);
        assert_eq!(got, diagnostics::compute(invalid));
        assert!(
            !got.is_empty(),
            "the single-file mismatch is still reported"
        );
    }

    #[test]
    fn unsaved_buffer_edit_is_analyzed_over_disk() {
        // The on-disk entry is broken, but the editor's live buffer is valid: the
        // overlay must make diagnostics reflect the buffer, not stale disk bytes.
        let project = TempProject::new();
        let a_path = project.write(
            "main.lby",
            "import math\n\nfn main -> i64\n    return nope(3)\n",
        );
        project.write("math.lby", MODULE_B);
        let uri = path_to_uri(&a_path);
        let docs = documents(&[(&uri, MAIN_A)]);

        let diags = diagnostics(&uri, MAIN_A, &docs);
        assert!(diags.is_empty(), "the live buffer is valid; got {diags:?}");
    }

    #[test]
    fn uri_round_trips_a_unix_path() {
        let uri = "file:///home/user/pkg/a.lby";
        let path = uri_to_path(uri).expect("path");
        assert_eq!(path, PathBuf::from("/home/user/pkg/a.lby"));
        assert_eq!(path_to_uri(&path), uri);
    }

    #[test]
    fn uri_decodes_windows_drive_and_percent_escapes() {
        let uri = "file:///C%3A/Users/dev/a%20b.lby";
        let path = uri_to_path(uri).expect("path");
        assert_eq!(path, PathBuf::from("C:/Users/dev/a b.lby"));
        // Re-encoding produces the canonical escaped form.
        assert_eq!(path_to_uri(&path), "file:///C%3A/Users/dev/a%20b.lby");
    }

    #[test]
    fn non_file_uri_has_no_path() {
        assert!(uri_to_path("untitled:Untitled-1").is_none());
    }
}
