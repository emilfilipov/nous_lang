//! A minimal Language Server Protocol (LSP) server for Lullaby.
//!
//! The server speaks JSON-RPC 2.0 over stdio with `Content-Length`-framed
//! messages (see the `transport` module). The request-handling logic lives in
//! the pure [`handle_message`] function so it can be driven directly from tests
//! without any real stdio; [`run_stdio`] is a thin read/write loop around it.
//!
//! Supported features:
//! - Lifecycle: `initialize`, `initialized`, `shutdown`, `exit`.
//! - Document sync: full-text `textDocument/didOpen`, `didChange`, `didClose`
//!   held in an in-memory map.
//! - Diagnostics: on open/change the existing lex -> parse -> semantic pipeline
//!   runs over the document and the results are published as
//!   `textDocument/publishDiagnostics`. When the document uses `import` or is
//!   part of a `lullaby.json` project, the shared module loader runs so
//!   diagnostics reflect the merged program (imported symbols resolve, and the
//!   loader's cross-module diagnostics surface) — see the `project` module.
//! - Formatting: `textDocument/formatting` returns a single full-document
//!   `TextEdit` produced by the canonical formatter, or no edits when the
//!   document does not parse.
//! - Hover: `textDocument/hover` returns a signature for a function, a type for
//!   a local/parameter, a declaration for a struct/enum, or a short builtin
//!   description, resolved from the reused parser + checked metadata.
//! - Go-to-definition: `textDocument/definition` resolves the identifier under
//!   the cursor to its declaration `Location` (function/struct/enum/alias, or a
//!   local `let`/parameter binding).

use std::collections::HashMap;

use serde_json::{Value, json};

mod analysis;
mod completion;
mod diagnostics;
mod project;
mod transport;

pub use transport::run_stdio;

/// A single outbound JSON-RPC message the server wants to send to the client.
///
/// A `Response` carries an `id` (matching a request); a `Notification` is
/// unsolicited (e.g. published diagnostics). The [`Message::into_json`] method
/// renders the full JSON-RPC envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A response to a request with the given `id`, carrying either a `result`
    /// or an `error` object.
    Response { id: Value, payload: ResponsePayload },
    /// A server-initiated notification (no `id`) with a method and params.
    Notification { method: String, params: Value },
}

/// The body of a JSON-RPC response: either a success `result` or an `error`.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponsePayload {
    Result(Value),
    Error { code: i64, message: String },
}

impl Message {
    /// A success response to `id` with `result`.
    fn result(id: Value, result: Value) -> Self {
        Message::Response {
            id,
            payload: ResponsePayload::Result(result),
        }
    }

    /// A notification with `method` and `params`.
    fn notification(method: impl Into<String>, params: Value) -> Self {
        Message::Notification {
            method: method.into(),
            params,
        }
    }

    /// Render this message as a complete JSON-RPC 2.0 envelope value.
    pub fn into_json(self) -> Value {
        match self {
            Message::Response { id, payload } => match payload {
                ResponsePayload::Result(result) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": result,
                }),
                ResponsePayload::Error { code, message } => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message },
                }),
            },
            Message::Notification { method, params } => json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
        }
    }
}

/// In-memory server state: the open documents and lifecycle flags.
#[derive(Debug, Default)]
pub struct ServerState {
    /// Open documents keyed by URI, holding the full current text.
    documents: HashMap<String, String>,
    /// Set once the client sends `shutdown`; `exit` then terminates the loop.
    shutdown_requested: bool,
    /// Set once `exit` is received so the stdio loop stops.
    exit: bool,
}

impl ServerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current text of an open document, if any. Exposed for tests.
    pub fn document(&self, uri: &str) -> Option<&str> {
        self.documents.get(uri).map(String::as_str)
    }

    /// Whether `exit` has been received and the stdio loop should stop.
    pub fn should_exit(&self) -> bool {
        self.exit
    }
}

/// Handle one decoded JSON-RPC message and return every message to send back.
///
/// This is the pure core of the server: it mutates `state` and returns the
/// outbound messages (responses and/or notifications) rather than writing to
/// any stream, so tests can drive the whole protocol without stdio.
///
/// `id` is `Some` for a request (which must get a response) and `None` for a
/// notification (which must not).
pub fn handle_message(
    state: &mut ServerState,
    method: &str,
    id: Option<Value>,
    params: Value,
) -> Vec<Message> {
    match method {
        "initialize" => match id {
            Some(id) => vec![Message::result(id, initialize_result())],
            None => Vec::new(),
        },
        "initialized" => Vec::new(),
        "shutdown" => {
            state.shutdown_requested = true;
            match id {
                Some(id) => vec![Message::result(id, Value::Null)],
                None => Vec::new(),
            }
        }
        "exit" => {
            state.exit = true;
            Vec::new()
        }
        "textDocument/didOpen" => handle_did_open(state, &params),
        "textDocument/didChange" => handle_did_change(state, &params),
        "textDocument/didClose" => handle_did_close(state, &params),
        "textDocument/formatting" => match id {
            Some(id) => vec![Message::result(id, handle_formatting(state, &params))],
            None => Vec::new(),
        },
        "textDocument/hover" => match id {
            Some(id) => vec![Message::result(id, handle_hover(state, &params))],
            None => Vec::new(),
        },
        "textDocument/definition" => match id {
            Some(id) => vec![Message::result(id, handle_definition(state, &params))],
            None => Vec::new(),
        },
        "textDocument/completion" => match id {
            Some(id) => vec![Message::result(id, handle_completion(state, &params))],
            None => Vec::new(),
        },
        // Unknown request: reply with a "method not found" error so the client
        // is not left waiting. Unknown notifications are ignored.
        _ => match id {
            Some(id) => vec![Message::Response {
                id,
                payload: ResponsePayload::Error {
                    code: -32601,
                    message: format!("method not found: {method}"),
                },
            }],
            None => Vec::new(),
        },
    }
}

/// The server capabilities advertised in the `initialize` response.
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            // 1 = full document sync: the client sends the whole text on change.
            "textDocumentSync": 1,
            "documentFormattingProvider": true,
            "hoverProvider": true,
            "definitionProvider": true,
            // Completion offers keywords, in-scope declarations/locals, and
            // imported `pub` symbols. No resolve step and no trigger characters
            // (member/`.` completion is deferred).
            "completionProvider": {
                "resolveProvider": false,
            },
        },
        "serverInfo": {
            "name": "lullaby-lsp",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// `textDocument/didOpen`: store the text and publish diagnostics.
fn handle_did_open(state: &mut ServerState, params: &Value) -> Vec<Message> {
    let doc = &params["textDocument"];
    let Some(uri) = doc["uri"].as_str() else {
        return Vec::new();
    };
    let text = doc["text"].as_str().unwrap_or_default().to_string();
    let uri = uri.to_string();
    state.documents.insert(uri.clone(), text);
    vec![publish_diagnostics(state, &uri)]
}

/// `textDocument/didChange` (full sync): replace the text and republish.
fn handle_did_change(state: &mut ServerState, params: &Value) -> Vec<Message> {
    let Some(uri) = params["textDocument"]["uri"].as_str() else {
        return Vec::new();
    };
    // Full-text sync (textDocumentSync = 1): the last content change carries the
    // entire new document text.
    let Some(text) = params["contentChanges"]
        .as_array()
        .and_then(|changes| changes.last())
        .and_then(|change| change["text"].as_str())
    else {
        return Vec::new();
    };
    let uri = uri.to_string();
    let text = text.to_string();
    state.documents.insert(uri.clone(), text);
    vec![publish_diagnostics(state, &uri)]
}

/// `textDocument/didClose`: drop the document and clear its diagnostics.
fn handle_did_close(state: &mut ServerState, params: &Value) -> Vec<Message> {
    let Some(uri) = params["textDocument"]["uri"].as_str() else {
        return Vec::new();
    };
    state.documents.remove(uri);
    // Publish an empty diagnostics set so any markers for this file are cleared.
    vec![Message::notification(
        "textDocument/publishDiagnostics",
        json!({ "uri": uri, "diagnostics": [] }),
    )]
}

/// Run the (module-aware) pipeline for `uri` and build a `publishDiagnostics`
/// notification. Cross-file resolution consults every open document in `state`.
fn publish_diagnostics(state: &ServerState, uri: &str) -> Message {
    let text = state.documents.get(uri).map(String::as_str).unwrap_or("");
    let items = project::diagnostics(uri, text, &state.documents);
    Message::notification(
        "textDocument/publishDiagnostics",
        json!({ "uri": uri, "diagnostics": items }),
    )
}

/// `textDocument/formatting`: return a single full-document `TextEdit`, or an
/// empty edit list when the document is not stored or does not parse.
fn handle_formatting(state: &ServerState, params: &Value) -> Value {
    let Some(uri) = params["textDocument"]["uri"].as_str() else {
        return Value::Array(Vec::new());
    };
    let Some(text) = state.documents.get(uri) else {
        return Value::Array(Vec::new());
    };
    match diagnostics::format_source(text) {
        Some(formatted) if formatted != *text => {
            json!([{
                "range": full_document_range(text),
                "newText": formatted,
            }])
        }
        // Already canonical or not parseable: no edits.
        _ => Value::Array(Vec::new()),
    }
}

/// `textDocument/hover`: resolve the identifier under the cursor and return an
/// LSP `Hover`, or `null` when there is nothing to show (whitespace, an unknown
/// identifier, or a document that does not parse).
fn handle_hover(state: &ServerState, params: &Value) -> Value {
    let Some(uri) = params["textDocument"]["uri"].as_str() else {
        return Value::Null;
    };
    let Some((text, line, character)) = position_of(state, params) else {
        return Value::Null;
    };
    project::hover(uri, text, line, character, &state.documents)
}

/// `textDocument/definition`: resolve the identifier under the cursor to its
/// definition `Location`, or `null` when it does not resolve.
fn handle_definition(state: &ServerState, params: &Value) -> Value {
    let Some(uri) = params["textDocument"]["uri"].as_str() else {
        return Value::Null;
    };
    let Some((text, line, character)) = position_of(state, params) else {
        return Value::Null;
    };
    project::definition(uri, text, line, character, &state.documents)
}

/// `textDocument/completion`: return the completion items for the cursor
/// position as an LSP `CompletionItem[]`. Offers keywords, the buffer's in-scope
/// declarations and locals, and imported `pub` symbols. Returns an empty array
/// when the document is not open; never errors on an unparseable buffer.
fn handle_completion(state: &ServerState, params: &Value) -> Value {
    let Some(uri) = params["textDocument"]["uri"].as_str() else {
        return Value::Array(Vec::new());
    };
    let Some(text) = state.documents.get(uri) else {
        return Value::Array(Vec::new());
    };
    let line = params["position"]["line"]
        .as_u64()
        .map(|line| line as usize);
    project::completion(uri, text, line, &state.documents)
}

/// Look up the open document text plus the 0-based `(line, character)` for a
/// `TextDocumentPositionParams`, or `None` when the document is not open or the
/// position is missing.
fn position_of<'a>(state: &'a ServerState, params: &Value) -> Option<(&'a str, usize, usize)> {
    let uri = params["textDocument"]["uri"].as_str()?;
    let text = state.documents.get(uri)?;
    let line = params["position"]["line"].as_u64()? as usize;
    let character = params["position"]["character"].as_u64()? as usize;
    Some((text.as_str(), line, character))
}

/// A 0-based LSP range covering the whole document, from (0,0) to just past the
/// last character. The end line/character are computed from the text so the
/// replacement covers every existing byte.
fn full_document_range(text: &str) -> Value {
    let mut line = 0usize;
    let mut character = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += 1;
        }
    }
    json!({
        "start": { "line": 0, "character": 0 },
        "end": { "line": line, "character": character },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple valid program: `fn main -> i64` returning a literal.
    const VALID: &str = "fn main -> i64\n    return 0\n";

    /// An invalid program: a `bool`-annotated local bound to an `i64` literal is
    /// a semantic type mismatch with a concrete source span.
    const INVALID: &str = "fn main -> i64\n    let value bool = 1\n    return 0\n";

    fn open(state: &mut ServerState, uri: &str, text: &str) -> Vec<Message> {
        handle_message(
            state,
            "textDocument/didOpen",
            None,
            json!({ "textDocument": { "uri": uri, "text": text } }),
        )
    }

    fn diagnostics_of(message: &Message) -> &Vec<Value> {
        match message {
            Message::Notification { method, params } => {
                assert_eq!(method, "textDocument/publishDiagnostics");
                params["diagnostics"].as_array().expect("diagnostics array")
            }
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    #[test]
    fn initialize_advertises_capabilities() {
        let mut state = ServerState::new();
        let out = handle_message(&mut state, "initialize", Some(json!(1)), json!({}));
        assert_eq!(out.len(), 1);
        let value = out[0].clone().into_json();
        let caps = &value["result"]["capabilities"];
        assert_eq!(caps["textDocumentSync"], json!(1));
        assert_eq!(caps["documentFormattingProvider"], json!(true));
    }

    #[test]
    fn did_open_invalid_program_publishes_diagnostic_with_sane_range() {
        let mut state = ServerState::new();
        let out = open(&mut state, "file:///a.lby", INVALID);
        assert_eq!(out.len(), 1);
        let diags = diagnostics_of(&out[0]);
        assert!(!diags.is_empty(), "expected at least one diagnostic");
        let first = &diags[0];
        // A stable Lullaby diagnostic code (e.g. "L0329") is carried through.
        assert!(
            first["code"].as_str().is_some_and(|c| c.starts_with('L')),
            "expected an L-prefixed code, got {:?}",
            first["code"]
        );
        // The range is 0-based and well-formed: start <= end, non-negative.
        let start = &first["range"]["start"];
        let end = &first["range"]["end"];
        let start_line = start["line"].as_u64().expect("start line");
        let start_char = start["character"].as_u64().expect("start char");
        let end_line = end["line"].as_u64().expect("end line");
        let end_char = end["character"].as_u64().expect("end char");
        assert!(start_line <= end_line);
        if start_line == end_line {
            assert!(start_char <= end_char);
        }
        // The document is two lines; the error is within the file.
        assert!(end_line <= 1, "range should stay within the document");
        let _ = start_char;
    }

    #[test]
    fn did_open_valid_program_publishes_no_diagnostics() {
        let mut state = ServerState::new();
        let out = open(&mut state, "file:///ok.lby", VALID);
        assert_eq!(out.len(), 1);
        let diags = diagnostics_of(&out[0]);
        assert!(
            diags.is_empty(),
            "expected zero diagnostics for a valid program, got {diags:?}"
        );
    }

    #[test]
    fn did_change_republishes_diagnostics_and_updates_text() {
        let mut state = ServerState::new();
        open(&mut state, "file:///a.lby", VALID);
        let out = handle_message(
            &mut state,
            "textDocument/didChange",
            None,
            json!({
                "textDocument": { "uri": "file:///a.lby" },
                "contentChanges": [ { "text": INVALID } ],
            }),
        );
        assert_eq!(state.document("file:///a.lby"), Some(INVALID));
        let diags = diagnostics_of(&out[0]);
        assert!(!diags.is_empty());
    }

    #[test]
    fn did_close_drops_document_and_clears_diagnostics() {
        let mut state = ServerState::new();
        open(&mut state, "file:///a.lby", VALID);
        let out = handle_message(
            &mut state,
            "textDocument/didClose",
            None,
            json!({ "textDocument": { "uri": "file:///a.lby" } }),
        );
        assert!(state.document("file:///a.lby").is_none());
        let diags = diagnostics_of(&out[0]);
        assert!(diags.is_empty());
    }

    #[test]
    fn formatting_returns_edit_for_unformatted_document() {
        let mut state = ServerState::new();
        // Extra blank lines / trailing whitespace make this non-canonical while
        // remaining parseable.
        let unformatted = "fn main -> i64\n\n\n    return 0\n";
        open(&mut state, "file:///f.lby", unformatted);
        let out = handle_message(
            &mut state,
            "textDocument/formatting",
            Some(json!(2)),
            json!({ "textDocument": { "uri": "file:///f.lby" } }),
        );
        assert_eq!(out.len(), 1);
        let value = out[0].clone().into_json();
        let edits = value["result"].as_array().expect("edit array");
        assert_eq!(edits.len(), 1, "expected exactly one full-document edit");
        let edit = &edits[0];
        assert!(edit["newText"].as_str().is_some());
        // Range starts at the very top of the document.
        assert_eq!(edit["range"]["start"]["line"], json!(0));
        assert_eq!(edit["range"]["start"]["character"], json!(0));
    }

    #[test]
    fn formatting_unparseable_document_returns_no_edits() {
        let mut state = ServerState::new();
        // A brace is a forbidden delimiter, so this never parses.
        let bad = "fn main -> i64 {\n    return 0\n}\n";
        open(&mut state, "file:///bad.lby", bad);
        let out = handle_message(
            &mut state,
            "textDocument/formatting",
            Some(json!(3)),
            json!({ "textDocument": { "uri": "file:///bad.lby" } }),
        );
        let value = out[0].clone().into_json();
        let edits = value["result"].as_array().expect("edit array");
        assert!(
            edits.is_empty(),
            "unparseable document should yield no edits"
        );
    }

    #[test]
    fn shutdown_then_exit_sets_flags() {
        let mut state = ServerState::new();
        let out = handle_message(&mut state, "shutdown", Some(json!(9)), json!({}));
        assert_eq!(out.len(), 1);
        assert!(!state.should_exit());
        let out = handle_message(&mut state, "exit", None, Value::Null);
        assert!(out.is_empty());
        assert!(state.should_exit());
    }

    #[test]
    fn unknown_request_returns_method_not_found() {
        let mut state = ServerState::new();
        let out = handle_message(
            &mut state,
            "textDocument/references",
            Some(json!(7)),
            json!({}),
        );
        let value = out[0].clone().into_json();
        assert_eq!(value["error"]["code"], json!(-32601));
    }

    #[test]
    fn initialize_advertises_hover_and_definition() {
        let mut state = ServerState::new();
        let out = handle_message(&mut state, "initialize", Some(json!(1)), json!({}));
        let value = out[0].clone().into_json();
        let caps = &value["result"]["capabilities"];
        assert_eq!(caps["hoverProvider"], json!(true));
        assert_eq!(caps["definitionProvider"], json!(true));
    }

    /// A two-function program used by the hover/definition request tests.
    const HOVER_PROG: &str = "\
fn add a i64 b i64 -> i64
    return a + b

fn main -> i64
    let total i64 = add(1, 2)
    return total
";

    fn position(uri: &str, line: u64, character: u64) -> Value {
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        })
    }

    #[test]
    fn hover_over_function_returns_signature() {
        let mut state = ServerState::new();
        open(&mut state, "file:///h.lby", HOVER_PROG);
        // `add` inside the call on line 4 (0-based).
        let out = handle_message(
            &mut state,
            "textDocument/hover",
            Some(json!(10)),
            position("file:///h.lby", 4, 20),
        );
        let value = out[0].clone().into_json();
        let contents = value["result"]["contents"]["value"]
            .as_str()
            .expect("hover markdown");
        assert!(
            contents.contains("fn add a i64 b i64 -> i64"),
            "got {contents}"
        );
    }

    #[test]
    fn hover_over_typed_local_returns_type() {
        let mut state = ServerState::new();
        open(&mut state, "file:///h.lby", HOVER_PROG);
        // `total` in `return total` on line 5 (0-based).
        let out = handle_message(
            &mut state,
            "textDocument/hover",
            Some(json!(11)),
            position("file:///h.lby", 5, 11),
        );
        let value = out[0].clone().into_json();
        let contents = value["result"]["contents"]["value"]
            .as_str()
            .expect("hover markdown");
        assert!(contents.contains("total i64"), "got {contents}");
    }

    #[test]
    fn hover_over_whitespace_returns_null() {
        let mut state = ServerState::new();
        open(&mut state, "file:///h.lby", HOVER_PROG);
        // Line 2 is blank.
        let out = handle_message(
            &mut state,
            "textDocument/hover",
            Some(json!(12)),
            position("file:///h.lby", 2, 0),
        );
        let value = out[0].clone().into_json();
        assert_eq!(value["result"], Value::Null);
    }

    #[test]
    fn definition_on_call_jumps_to_declaration() {
        let mut state = ServerState::new();
        open(&mut state, "file:///h.lby", HOVER_PROG);
        let out = handle_message(
            &mut state,
            "textDocument/definition",
            Some(json!(13)),
            position("file:///h.lby", 4, 20),
        );
        let value = out[0].clone().into_json();
        let loc = &value["result"];
        assert_eq!(loc["uri"], json!("file:///h.lby"));
        // `add` is declared on line 0, name at columns 3..6 (`fn add`).
        assert_eq!(loc["range"]["start"]["line"], json!(0));
        assert_eq!(loc["range"]["start"]["character"], json!(3));
        assert_eq!(loc["range"]["end"]["character"], json!(6));
    }

    #[test]
    fn definition_on_local_jumps_to_let() {
        let mut state = ServerState::new();
        open(&mut state, "file:///h.lby", HOVER_PROG);
        let out = handle_message(
            &mut state,
            "textDocument/definition",
            Some(json!(14)),
            position("file:///h.lby", 5, 11),
        );
        let value = out[0].clone().into_json();
        // The `let total` binding is on line 4 (0-based).
        assert_eq!(value["result"]["range"]["start"]["line"], json!(4));
    }

    #[test]
    fn initialize_advertises_completion() {
        let mut state = ServerState::new();
        let out = handle_message(&mut state, "initialize", Some(json!(1)), json!({}));
        let value = out[0].clone().into_json();
        let caps = &value["result"]["capabilities"];
        assert_eq!(caps["completionProvider"]["resolveProvider"], json!(false));
    }

    /// A program with a function, struct, enum, and const for completion tests.
    const COMPLETION_PROG: &str = "\
struct Widget
    size i64

enum State
    On
    Off

const MAX i64 = 5

fn build w i64 -> i64
    return w
";

    fn completion(uri: &str, line: u64, character: u64) -> Value {
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        })
    }

    fn completion_labels(value: &Value) -> Vec<String> {
        value["result"]
            .as_array()
            .expect("completion array")
            .iter()
            .map(|item| item["label"].as_str().unwrap().to_string())
            .collect()
    }

    fn completion_item<'a>(value: &'a Value, label: &str) -> Option<&'a Value> {
        value["result"]
            .as_array()
            .expect("completion array")
            .iter()
            .find(|item| item["label"] == json!(label))
    }

    #[test]
    fn completion_offers_keywords_at_top_level() {
        let mut state = ServerState::new();
        open(&mut state, "file:///c.lby", COMPLETION_PROG);
        let out = handle_message(
            &mut state,
            "textDocument/completion",
            Some(json!(20)),
            completion("file:///c.lby", 0, 0),
        );
        let value = out[0].clone().into_json();
        let labels = completion_labels(&value);
        for keyword in ["fn", "let", "struct", "enum", "import", "pub"] {
            assert!(labels.contains(&keyword.to_string()), "missing {keyword}");
        }
        // Keyword items carry the LSP Keyword kind (14).
        assert_eq!(completion_item(&value, "fn").unwrap()["kind"], json!(14));
    }

    #[test]
    fn completion_offers_in_file_declarations_with_kinds() {
        let mut state = ServerState::new();
        open(&mut state, "file:///c.lby", COMPLETION_PROG);
        let out = handle_message(
            &mut state,
            "textDocument/completion",
            Some(json!(21)),
            completion("file:///c.lby", 10, 4),
        );
        let value = out[0].clone().into_json();
        // fn = 3, struct = 22, enum = 13, constant = 21.
        assert_eq!(completion_item(&value, "build").unwrap()["kind"], json!(3));
        assert_eq!(
            completion_item(&value, "Widget").unwrap()["kind"],
            json!(22)
        );
        assert_eq!(completion_item(&value, "State").unwrap()["kind"], json!(13));
        assert_eq!(completion_item(&value, "MAX").unwrap()["kind"], json!(21));
    }

    #[test]
    fn completion_on_unparseable_buffer_still_returns_keywords() {
        let mut state = ServerState::new();
        // A brace is a forbidden delimiter; this never parses.
        let bad = "fn main -> i64 {\n    let x = \n";
        open(&mut state, "file:///bad.lby", bad);
        let out = handle_message(
            &mut state,
            "textDocument/completion",
            Some(json!(22)),
            completion("file:///bad.lby", 1, 8),
        );
        let value = out[0].clone().into_json();
        let labels = completion_labels(&value);
        assert!(labels.contains(&"fn".to_string()));
        assert!(labels.contains(&"return".to_string()));
    }

    #[test]
    fn definition_on_unknown_returns_null() {
        let mut state = ServerState::new();
        open(&mut state, "file:///h.lby", HOVER_PROG);
        let out = handle_message(
            &mut state,
            "textDocument/definition",
            Some(json!(15)),
            position("file:///h.lby", 2, 0),
        );
        let value = out[0].clone().into_json();
        assert_eq!(value["result"], Value::Null);
    }
}
