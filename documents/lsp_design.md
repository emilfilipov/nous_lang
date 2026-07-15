# Lullaby Language Server (LSP) Design

The `lullaby_lsp` crate implements a minimal [Language Server Protocol](https://microsoft.github.io/language-server-protocol/) server for `.lby` source files. It is additive editor tooling: it reuses the existing frontend (lexer, parser, semantic analyzer) and the canonical formatter without touching the interpreters, IR, native code generation, or the WebAssembly backend. Cross-backend parity is unaffected.

The server is exposed through the `lullaby lsp` CLI subcommand, which runs the stdio read/write loop. It uses the Rust standard library plus `serde_json` only; no third-party LSP framework, async runtime, or protocol crate is added.

## Transport And Framing

The server speaks JSON-RPC 2.0 over stdin/stdout using the LSP base-protocol framing:

```
Content-Length: <N>\r\n
\r\n
<N bytes of UTF-8 JSON>
```

`crates/lullaby_lsp/src/transport.rs` reads header lines until a blank line, parses the `Content-Length`, reads exactly that many body bytes, and decodes the JSON with `serde_json`. Outbound messages are written with the same framing and the stream is flushed after each write. The loop terminates when the client sends `exit` or closes stdin.

## Request-Handling Core

All protocol behavior lives in the pure function

```rust
pub fn handle_message(
    state: &mut ServerState,
    method: &str,
    id: Option<Value>,
    params: Value,
) -> Vec<Message>
```

It mutates the in-memory `ServerState` and returns the outbound messages (responses and notifications) instead of writing to any stream. This makes the whole protocol testable without real stdio: tests build `params` as `serde_json` values, call `handle_message`, and assert on the returned `Message`s. The stdio loop (`serve`/`run_stdio`) is a thin wrapper that decodes a request, calls `handle_message`, and writes each returned message back.

`id` is `Some` for a request (which must receive a response) and `None` for a notification (which must not).

## Capabilities

The `initialize` response advertises:

- `textDocumentSync = 1` (full): the client sends the entire document text on every change.
- `documentFormattingProvider = true`.
- `hoverProvider = true`.
- `definitionProvider = true`.
- `completionProvider = { resolveProvider: false }` (no resolve step, no trigger characters).

References and other providers are intentionally not advertised.

## Lifecycle

| Method | Behavior |
| --- | --- |
| `initialize` | Returns capabilities (including completion) and `serverInfo`. |
| `initialized` | Acknowledged (no-op notification). |
| `shutdown` | Marks the server as shutdown-requested and returns a null result. |
| `exit` | Sets the exit flag so the stdio loop stops. |

## Document Sync

Open documents are held in a `HashMap<String, String>` keyed by URI:

- `textDocument/didOpen` stores the text and publishes diagnostics.
- `textDocument/didChange` replaces the text with the last content change's full `text` (full sync) and republishes diagnostics.
- `textDocument/didClose` drops the document and publishes an empty diagnostics set to clear any markers.

## Diagnostics

On open and change the server runs the same pipeline as `lullaby check`: lex, then parse, then `lullaby_semantics::validate`. It reports whichever stage first produces errors, which matches the command-line behavior (a single failing phase at a time). Each Lullaby diagnostic carries a stable code (for example `L0307`), a message, and a source span.

Lullaby spans are single 1-based `line`/`column` points. They are converted to 0-based LSP ranges. Because a span has no length, the end is widened to cover the identifier/number/keyword token that starts at that column (scanning the document line for word characters); when the position is not on a word character the range falls back to a single character. Each diagnostic is published as an LSP `Diagnostic` with `severity = 1` (Error), `source = "lullaby"`, the Lullaby `code`, and the message, via a `textDocument/publishDiagnostics` notification.

## Module And Project Awareness

`crates/lullaby_lsp/src/project.rs` makes the server work correctly for multi-file projects, which the single-document pipeline could not: a `pub` symbol defined in an imported module used to read as "undefined", and hover/go-to-definition could never leave the open buffer. The server now runs the **same** module resolution the CLI uses, from the shared `lullaby_loader` crate (extracted from the CLI precisely so the server can depend on it — the CLI depends on `lullaby_lsp`, so the loader could not stay in the CLI without a dependency cycle).

The path is engaged conservatively. A document is analyzed module-aware only when it uses `import` **or** lives inside a `lullaby.json` project (found by walking up from the file's directory for a manifest and resolving it into the project's `src` search directories). A lone file with no imports and no project falls through to the unchanged single-document pipeline, so single-file behavior is byte-for-byte as before.

- **Open-buffer overlay.** Every open document is supplied to the loader as a `SourceOverlay` (`overlay_key(path) -> live text`), so the editor's current, possibly-unsaved buffers are analyzed instead of stale on-disk bytes. The loader reads a module from the overlay when present and from disk otherwise; the CLI never passes an overlay, so its on-disk behavior is unchanged.
- **Diagnostics.** The open buffer's own lex/parse errors are reported directly from the buffer (authoritative positions, no loader run). Otherwise the loader runs over the file's project: loader diagnostics (`L0391` no-shadowing, `L0392` visibility, `L0393` cycle, `L0397` missing module) are filtered to those whose source path is this file; then `validate` runs over the merged program, and semantic diagnostics are attributed back to the open file by their enclosing-function name (top-level names are globally unique across a merged project under `L0391`), falling back to whether the span lands within the open file's own extent for the rare function-less diagnostic. A symbol defined in an imported module therefore no longer reads as undefined, while a genuine error in the open file is still reported at its real position.
- **Hover and go-to-definition.** When buffer-local resolution finds nothing, the loader's per-module views (`LoadedProgram.modules`, each carrying a module's name/path/source/AST) are searched for a declaration of the identifier. Hover renders that declaration's signature from the other file; go-to-definition returns a `Location` whose URI is the other file's `file://` URI (built by percent-encoding the path) and whose range covers the declaration's name.

Every fallible step degrades to single-document analysis rather than failing: a non-`file://` URI (e.g. an unsaved `untitled:` buffer), a malformed manifest, a missing import, or a mid-edit buffer never panics the server.

## Formatting

`textDocument/formatting` looks up the stored document text and runs the canonical formatter (`lullaby_parser::format_program`) after a successful lex+parse. It returns a single full-document `TextEdit` whose range spans the entire current document. If the document does not parse, or is already canonical, it returns no edits.

## Hover And Go-To-Definition

`crates/lullaby_lsp/src/analysis.rs` resolves the identifier under the cursor for
`textDocument/hover` and `textDocument/definition`. Both reuse the frontend
rather than re-implementing any analysis:

- The cursor position (0-based) is mapped to the whole word it lands on by
  scanning the document line for word characters. A position on whitespace or a
  non-identifier token resolves to nothing.
- The document is lexed and parsed; hover additionally runs
  `lullaby_semantics::validate` to obtain the `CheckedProgram`. When any stage
  fails, hover/definition return `null` (diagnostics still cover the errors).

Hover picks the first match, in order:

1. A top-level declaration with that name — a function's `fn NAME p T ... -> Ret`
   signature, or a `struct`/`enum` declaration rendered from the AST.
2. A known builtin — a short description (checked before locals because a builtin
   *call* expression is also recorded in the checker's inferred-type table, and
   the description is the more useful hover).
3. A local or parameter — the inferred type the checker recorded for the
   identifier expression at that exact 1-based span
   (`SemanticInfo::expression_types`), or the declared parameter/`let` type as a
   fallback.

This relies on the parser giving `Variable`/`Call`/declaration expressions a span
that points at the identifier's first character, so a 0-based cursor column maps
to a 1-based span column by `+1`. No new semantics accessor was needed: the
checked metadata (`signatures`, `expression_types`) and `Signature` fields are
already `pub`.

Go-to-definition returns a `Location` in the same document:

- A top-level declaration (function/struct/enum/alias) resolves to a range over
  its name on the declaration line (found by searching that line for the name, so
  `pub`/`async` prefixes do not shift the column).
- Otherwise the enclosing function is found (the last function whose span line is
  at or before the cursor), then a `let` binding of that name resolves to the
  `let` line, and a parameter resolves to the function's signature line. Local
  resolution descends `while`/`for`/`loop`/`unsafe`/`try` bodies.

## Completion

`crates/lullaby_lsp/src/completion.rs` handles `textDocument/completion`. It is a
deliberately simple, robust first implementation — no context-sensitive ranking,
no snippet expansion, and no member/`.`-completion (see "Deferred Features"). It
returns a plain `CompletionItem[]` (each with a `label`, an integer `kind`, and a
`detail` string), built from the union of three sources, with labels
de-duplicated (first occurrence wins):

- **Keywords.** The Lullaby keyword set, mirrored from the lexer's keyword table
  (there is no public enumeration of it) as `completion::KEYWORDS` and pinned to
  the lexer by the `keyword_list_matches_the_lexer` test, which asserts every
  entry still lexes to a single `Keyword` token so an upstream rename/removal
  fails the build. Kind `Keyword` (14).
- **In-file top-level declarations.** Functions (kind `Function`, detail is the
  `fn NAME p T ... -> Ret` signature), structs (`Struct`), enums (`Enum`), type
  aliases (`Class`, detail `alias NAME = TARGET`), traits (`Interface`), and
  constants (`Constant`, detail `const NAME type`) parsed from the current
  buffer. **In-scope locals/parameters** of the function enclosing the cursor are
  also offered (kind `Variable`, detail `NAME type`), reusing the same
  enclosing-function and block-descent helpers as hover/go-to-definition.
- **Imported `pub` symbols.** When the file is module-aware (uses `import` or
  lives in a `lullaby.json` project), `crate::project::completion` runs the same
  `lullaby_loader` machinery as diagnostics/hover over the file's project and
  adds the `pub` declarations of every non-entry module, so symbols reachable
  through the file's imports complete with the imported signature as their
  detail.

Robustness is the one hard requirement: keywords are produced unconditionally, so
a mid-edit buffer that does not lex/parse still yields keyword completions and
never panics; the declaration/local passes and the project load each degrade to
nothing on failure rather than erroring.

## Testing

`crates/lullaby_lsp` carries unit tests that drive `handle_message` directly (no stdio):

- `initialize` advertises the expected capabilities.
- `didOpen` with an invalid program publishes a `publishDiagnostics` notification with at least one diagnostic whose code is `L`-prefixed and whose range is 0-based and well-formed.
- `didOpen` with a valid `fn main -> i64` returning a literal publishes zero diagnostics.
- `didChange` updates the stored text and republishes.
- `didClose` drops the document and clears diagnostics.
- `formatting` returns exactly one full-document `TextEdit` for a parseable-but-unformatted document and no edits for an unparseable one.
- `completion` at a top-level position offers the keyword set; it offers an in-file `fn`/`struct`/`enum`/`const` with the correct `CompletionItemKind`; a two-file project offers an imported `pub` symbol with its signature detail; and an unparseable buffer still returns keywords without panicking. The keyword list is pinned to the lexer by `keyword_list_matches_the_lexer`, and `public_declaration_items` is unit-tested to filter to `pub`.
- `hover` over a function name returns its signature; over a typed local returns its type; over whitespace or an unknown identifier returns `null`.
- `definition` on a call jumps to the function declaration's name range; on a local jumps to its `let` line; on an unresolved position returns `null`.

`project.rs` additionally carries filesystem-backed tests over a real two-file project (`main.lby` importing a `pub` symbol from `math.lby`): an imported symbol is not flagged undefined; a genuine type error in the entry is still reported at the correct 0-based range while the imported symbol is not; cross-module go-to-definition points at the other file's URI and declaration line; cross-module hover shows the imported signature; an unsaved-buffer overlay is analyzed in preference to broken on-disk bytes; and a lone no-import file produces byte-identical diagnostics to the single-document pipeline (regression guard). URI/path round-trips (Unix and percent-encoded Windows drive) and non-`file://` rejection are unit-tested. The `lullaby_loader` crate tests the overlay precedence and per-module exposure directly.

The transport module additionally tests the framed read/write loop end to end over in-memory byte buffers (initialize -> didOpen -> shutdown -> exit).

## Deferred Features

The following are intentionally out of scope for this increment and can be layered on later without changing the transport or the `handle_message` shape:

- Signature help; member/`.`-completion (field/method completion after a `.`); context-sensitive completion ranking and snippet expansion. (Keyword + declaration + local + imported-symbol completion is now supported; see the "Completion" section above.)
- References, document symbols, workspace symbols. (Hover and go-to-definition are now supported; see the section above.)
- Incremental (range) document sync.
- Code actions / quick fixes (for example applying the formatter or diagnostic-directed edits).

Module- and project-aware analysis (imports and `lullaby.json` search directories) is now supported — see "Module And Project Awareness" above.
- Reporting more than the first failing phase's diagnostics at once.
