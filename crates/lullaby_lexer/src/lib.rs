use std::path::Path;

pub use lullaby_diagnostics::Span;

pub const CANONICAL_EXTENSION: &str = "lby";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub span: Span,
}

impl Diagnostic {
    pub fn new(code: &'static str, message: impl Into<String>, span: Span) -> Self {
        Self {
            code,
            message: message.into(),
            span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Identifier(String),
    Keyword(Keyword),
    Number(String),
    String(String),
    Char(char),
    Symbol(String),
    Arrow,
    Newline,
    Indent,
    Dedent,
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    Fn,
    Return,
    If,
    Elif,
    Else,
    For,
    From,
    To,
    By,
    In,
    While,
    Loop,
    Break,
    Continue,
    Let,
    Const,
    And,
    Or,
    Not,
    True,
    False,
    Void,
    Module,
    Import,
    Pub,
    Package,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Interface,
    Class,
    Match,
    Switch,
    Try,
    Catch,
    Throw,
    Async,
    Await,
    Coroutine,
    Unsafe,
    Region,
    Alias,
    Extern,
    Export,
    Asm,
    /// `actor` — introduces a concurrent actor declaration (`actor Name` with a
    /// `state` section, an optional `init`, and `on <handler>` message handlers).
    Actor,
    /// `tell` — a fire-and-forget message send `tell handle.handler(args)` that
    /// enqueues onto an actor's mailbox and returns `void`.
    Tell,
    /// `ask` — a request-reply message send `ask handle.handler(args)` that
    /// enqueues a request carrying a one-shot reply slot and evaluates to a
    /// `Future<R>` (`R` is the handler's `-> R` reply type); `await` resolves the
    /// future to the reply value.
    Ask,
    /// `join_all` — a `Future<T>` combinator `join_all EXPR` that waits for every
    /// future in a `list<Future<T>>` to resolve and yields a `list<T>` of the
    /// results in input order (actor stage 5).
    JoinAll,
    /// `select` — a `Future<T>` combinator `select EXPR` that waits for the first
    /// future in a `list<Future<T>>` to resolve and yields a `Selected<T>`
    /// (`index i64`, `value T`); lowest input index wins a tie (actor stage 5).
    Select,
    /// `no-runtime` — the module-level freestanding-tier directive. When it is the
    /// first non-comment line of a `.lby` file, the module is compiled in the
    /// freestanding (`no-runtime`) tier: the compiler rejects any construct that
    /// requires the safe-tier runtime (a growable heap allocation, an actor/`spawn`/
    /// `tell`, a heap closure, an `rc`/`ref` handle, …). This is the one hyphenated
    /// Lullaby keyword; the lexer recognizes the exact contiguous spelling
    /// `no-runtime` as a single token (a bare `no` and a bare `runtime` remain
    /// ordinary identifiers).
    NoRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}

/// A source comment captured as trivia during lexing. Comments are not tokens
/// (they never reach the parser), but the formatter re-emits them so that
/// `lullaby fmt` is comment-preserving.
///
/// `line` is the 1-based source line the comment sits on. `trailing` is `true`
/// when code precedes the `#` on that line (an inline comment such as
/// `let x i64 = 5  # note`) and `false` for a comment that occupies its own line
/// (a full-line comment attached to the statement that follows it). `indent` is
/// the leading indentation width (spaces, with a tab counted as four) of a
/// full-line comment, so the formatter can place it at the matching block depth;
/// it is `0` for a trailing comment. `text` is the comment verbatim from `#` to
/// the end of the line, with trailing whitespace stripped; it is re-emitted
/// exactly, so the formatter never rewrites a comment's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub line: usize,
    pub trailing: bool,
    pub indent: usize,
    pub text: String,
}

pub fn validate_source_path(path: &Path) -> Result<(), Diagnostic> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some(CANONICAL_EXTENSION) => Ok(()),
        Some(ext) => Err(Diagnostic::new(
            "L0001",
            format!("unsupported source extension '.{ext}', expected '.{CANONICAL_EXTENSION}'"),
            Span::new(1, 1),
        )),
        None => Err(Diagnostic::new(
            "L0001",
            format!("source file has no extension, expected '.{CANONICAL_EXTENSION}'"),
            Span::new(1, 1),
        )),
    }
}

pub fn lex(source: &str) -> Result<Vec<Token>, Vec<Diagnostic>> {
    lex_with_comments(source).map(|(tokens, _comments)| tokens)
}

/// Lex `source` and additionally return the comment trivia (full-line and
/// trailing `#` comments), in source order. The token stream is identical to
/// [`lex`]; comments are collected on the side so a comment-preserving consumer
/// (the formatter) can re-emit them while every other stage keeps ignoring them.
pub fn lex_with_comments(source: &str) -> Result<(Vec<Token>, Vec<Comment>), Vec<Diagnostic>> {
    let mut lexer = Lexer::new(source);
    lexer.lex();
    if lexer.diagnostics.is_empty() {
        Ok((lexer.tokens, lexer.comments))
    } else {
        Err(lexer.diagnostics)
    }
}

struct Lexer<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    comments: Vec<Comment>,
    diagnostics: Vec<Diagnostic>,
    indent_stack: Vec<usize>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            tokens: Vec::new(),
            comments: Vec::new(),
            diagnostics: Vec::new(),
            indent_stack: vec![0],
        }
    }

    fn lex(&mut self) {
        for (line_index, raw_line) in self.source.lines().enumerate() {
            let line_number = line_index + 1;
            let line = raw_line.trim_end_matches('\r');

            let trimmed = line.trim_start();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with('#') {
                // A full-line comment: it occupies its own line and attaches to
                // the statement that follows it. Store it verbatim (right-trimmed)
                // so the formatter re-emits it unchanged.
                self.comments.push(Comment {
                    line: line_number,
                    trailing: false,
                    indent: count_indent(line),
                    text: trimmed.trim_end().to_string(),
                });
                continue;
            }

            let indent = count_indent(line);
            self.emit_indent_changes(indent, line_number);
            self.lex_line(line.trim_start(), line_number, indent + 1);
            self.tokens.push(Token::new(
                TokenKind::Newline,
                Span::new(line_number, line.len() + 1),
            ));
        }

        while self.indent_stack.len() > 1 {
            self.indent_stack.pop();
            self.tokens.push(Token::new(
                TokenKind::Dedent,
                Span::new(self.source.lines().count() + 1, 1),
            ));
        }

        self.tokens.push(Token::new(
            TokenKind::Eof,
            Span::new(self.source.lines().count() + 1, 1),
        ));
    }

    fn emit_indent_changes(&mut self, indent: usize, line: usize) {
        let current = *self
            .indent_stack
            .last()
            .expect("indent stack is never empty");
        if indent > current {
            self.indent_stack.push(indent);
            self.tokens
                .push(Token::new(TokenKind::Indent, Span::new(line, 1)));
            return;
        }

        while indent
            < *self
                .indent_stack
                .last()
                .expect("indent stack is never empty")
        {
            self.indent_stack.pop();
            self.tokens
                .push(Token::new(TokenKind::Dedent, Span::new(line, 1)));
        }

        if indent
            != *self
                .indent_stack
                .last()
                .expect("indent stack is never empty")
        {
            self.diagnostics.push(Diagnostic::new(
                "L0101",
                "indentation does not match any active block",
                Span::new(line, 1),
            ));
        }
    }

    fn lex_line(&mut self, line: &str, line_number: usize, base_column: usize) {
        let chars: Vec<char> = line.chars().collect();
        let mut index = 0;
        // Depth of open `[` on this line. A `;` is a forbidden statement
        // terminator at the top level (`L0103`), but inside brackets it is the
        // separator of the repeat/fill array literal `[value; count]`, so there
        // it is lexed as an ordinary symbol. Brackets never span lines, so a
        // per-line counter is sufficient.
        let mut bracket_depth: u32 = 0;

        while index < chars.len() {
            let ch = chars[index];
            let column = base_column + index;

            if ch.is_whitespace() {
                index += 1;
                continue;
            }

            if ch == '#' {
                // A trailing (inline) comment: code precedes the `#` on this
                // line, so it stays attached to that line. Everything from `#` to
                // end of line is the comment (right-trimmed); `#` inside a string
                // literal was already consumed by `lex_string`, so this is always
                // a real comment.
                self.comments.push(Comment {
                    line: line_number,
                    trailing: true,
                    indent: 0,
                    text: chars[index..]
                        .iter()
                        .collect::<String>()
                        .trim_end()
                        .to_string(),
                });
                break;
            }

            if matches!(ch, '{' | '}') {
                self.diagnostics.push(Diagnostic::new(
                    "L0102",
                    "curly braces are not block delimiters in Lullaby",
                    Span::new(line_number, column),
                ));
                index += 1;
                continue;
            }

            if ch == ';' {
                if bracket_depth > 0 {
                    // Fill-literal separator, e.g. `[0; 512]`. Emit it as a plain
                    // symbol; the parser accepts it only in the array-literal
                    // position and rejects a stray `;` there.
                    self.tokens.push(Token::new(
                        TokenKind::Symbol(";".to_string()),
                        Span::new(line_number, column),
                    ));
                    index += 1;
                    continue;
                }
                self.diagnostics.push(Diagnostic::new(
                    "L0103",
                    "semicolons do not terminate Lullaby statements",
                    Span::new(line_number, column),
                ));
                index += 1;
                continue;
            }

            if ch == '"' {
                index = self.lex_string(&chars, index, line_number, column);
                continue;
            }

            if ch == '\'' {
                index = self.lex_char(&chars, index, line_number, column);
                continue;
            }

            if ch.is_ascii_digit() {
                let start = index;
                // `_` is accepted as a digit separator inside the literal (e.g.
                // `1_000_000`); the parser validates its placement and strips it.
                while index < chars.len()
                    && (chars[index].is_ascii_alphanumeric()
                        || chars[index] == '.'
                        || chars[index] == '_')
                {
                    index += 1;
                }
                self.tokens.push(Token::new(
                    TokenKind::Number(chars[start..index].iter().collect()),
                    Span::new(line_number, column),
                ));
                continue;
            }

            if is_identifier_start(ch) {
                let start = index;
                while index < chars.len() && is_identifier_continue(chars[index]) {
                    index += 1;
                }
                let text: String = chars[start..index].iter().collect();
                // `no-runtime` is the one hyphenated keyword. When the identifier
                // `no` is immediately followed by the exact contiguous spelling
                // `-runtime` (bounded by a non-identifier char), lex the whole run
                // as a single `NoRuntime` keyword token rather than `no` `-`
                // `runtime`. A bare `no` or `runtime`, or a `no - runtime`
                // subtraction with surrounding spaces, is unaffected.
                if text == "no" && starts_with_bounded(&chars, index, "-runtime") {
                    index += "-runtime".len();
                    self.tokens.push(Token::new(
                        TokenKind::Keyword(Keyword::NoRuntime),
                        Span::new(line_number, column),
                    ));
                    continue;
                }
                let kind = keyword(&text)
                    .map(TokenKind::Keyword)
                    .unwrap_or(TokenKind::Identifier(text));
                self.tokens
                    .push(Token::new(kind, Span::new(line_number, column)));
                continue;
            }

            if ch == '-' && chars.get(index + 1) == Some(&'>') {
                self.tokens
                    .push(Token::new(TokenKind::Arrow, Span::new(line_number, column)));
                index += 2;
                continue;
            }

            let mut symbol = ch.to_string();
            if let Some(next) = chars.get(index + 1) {
                let pair = format!("{ch}{next}");
                // `<<`/`>>` are the bitwise shift operators; they must be lexed
                // as single two-char tokens so they never collide with the `<`/`>`
                // comparison symbols.
                if matches!(
                    pair.as_str(),
                    "==" | "!=" | "<=" | ">=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<" | ">>"
                ) {
                    symbol = pair;
                    index += 2;
                    self.tokens.push(Token::new(
                        TokenKind::Symbol(symbol),
                        Span::new(line_number, column),
                    ));
                    continue;
                }
            }

            if symbol == "[" {
                bracket_depth += 1;
            } else if symbol == "]" {
                bracket_depth = bracket_depth.saturating_sub(1);
            }
            self.tokens.push(Token::new(
                TokenKind::Symbol(symbol),
                Span::new(line_number, column),
            ));
            index += 1;
        }
    }

    fn lex_string(
        &mut self,
        chars: &[char],
        start: usize,
        line_number: usize,
        column: usize,
    ) -> usize {
        let mut index = start + 1;
        let mut value = String::new();
        while index < chars.len() {
            match chars[index] {
                '"' => {
                    self.tokens.push(Token::new(
                        TokenKind::String(value),
                        Span::new(line_number, column),
                    ));
                    return index + 1;
                }
                ch => value.push(ch),
            }
            index += 1;
        }

        self.diagnostics.push(Diagnostic::new(
            "L0104",
            "unterminated string literal",
            Span::new(line_number, column),
        ));
        index
    }

    /// Lex a single-quoted char literal `'c'` holding exactly one Unicode scalar.
    /// An empty (`''`), multi-character (`'ab'`), or unterminated literal is an
    /// `L0105` diagnostic. Returns the index just past the closing quote (or the
    /// scanned extent on error) so lexing can continue.
    fn lex_char(
        &mut self,
        chars: &[char],
        start: usize,
        line_number: usize,
        column: usize,
    ) -> usize {
        let mut index = start + 1;
        let mut value: Vec<char> = Vec::new();
        while index < chars.len() {
            if chars[index] == '\'' {
                if value.len() == 1 {
                    self.tokens.push(Token::new(
                        TokenKind::Char(value[0]),
                        Span::new(line_number, column),
                    ));
                } else {
                    self.diagnostics.push(Diagnostic::new(
                        "L0105",
                        "char literal must contain exactly one character",
                        Span::new(line_number, column),
                    ));
                }
                return index + 1;
            }
            value.push(chars[index]);
            index += 1;
        }

        self.diagnostics.push(Diagnostic::new(
            "L0105",
            "unterminated char literal",
            Span::new(line_number, column),
        ));
        index
    }
}

/// True when `chars[at..]` begins with the exact characters of `pat` and the
/// character immediately after the match (if any) is not an identifier-continue
/// char. Used to recognize the hyphenated `no-runtime` keyword as a single token
/// without swallowing a longer identifier-like run (`no-runtimex` does not match).
fn starts_with_bounded(chars: &[char], at: usize, pat: &str) -> bool {
    let pat_chars: Vec<char> = pat.chars().collect();
    if chars.len() < at + pat_chars.len() {
        return false;
    }
    if chars[at..at + pat_chars.len()] != pat_chars[..] {
        return false;
    }
    !chars
        .get(at + pat_chars.len())
        .is_some_and(|next| is_identifier_continue(*next))
}

fn count_indent(line: &str) -> usize {
    line.chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .map(|ch| if ch == '\t' { 4 } else { 1 })
        .sum()
}

fn is_identifier_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_identifier_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn keyword(text: &str) -> Option<Keyword> {
    Some(match text {
        "fn" => Keyword::Fn,
        "return" => Keyword::Return,
        "if" => Keyword::If,
        "elif" => Keyword::Elif,
        "else" => Keyword::Else,
        "for" => Keyword::For,
        "from" => Keyword::From,
        "to" => Keyword::To,
        "by" => Keyword::By,
        "in" => Keyword::In,
        "while" => Keyword::While,
        "loop" => Keyword::Loop,
        "break" => Keyword::Break,
        "continue" => Keyword::Continue,
        "let" => Keyword::Let,
        "const" => Keyword::Const,
        "and" => Keyword::And,
        "or" => Keyword::Or,
        "not" => Keyword::Not,
        "true" => Keyword::True,
        "false" => Keyword::False,
        "void" => Keyword::Void,
        "module" => Keyword::Module,
        "import" => Keyword::Import,
        "pub" => Keyword::Pub,
        "package" => Keyword::Package,
        "struct" => Keyword::Struct,
        "enum" => Keyword::Enum,
        "union" => Keyword::Union,
        "trait" => Keyword::Trait,
        "impl" => Keyword::Impl,
        "interface" => Keyword::Interface,
        "class" => Keyword::Class,
        "match" => Keyword::Match,
        "switch" => Keyword::Switch,
        "try" => Keyword::Try,
        "catch" => Keyword::Catch,
        "throw" => Keyword::Throw,
        "async" => Keyword::Async,
        "await" => Keyword::Await,
        "coroutine" => Keyword::Coroutine,
        "unsafe" => Keyword::Unsafe,
        "region" => Keyword::Region,
        "alias" => Keyword::Alias,
        "extern" => Keyword::Extern,
        "export" => Keyword::Export,
        "asm" => Keyword::Asm,
        "actor" => Keyword::Actor,
        "tell" => Keyword::Tell,
        "ask" => Keyword::Ask,
        "join_all" => Keyword::JoinAll,
        "select" => Keyword::Select,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_indent_and_dedent_tokens() {
        let tokens = lex("fn main -> void\n    return\n").expect("valid source");
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Indent));
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Dedent));
    }

    #[test]
    fn rejects_braces_and_semicolons() {
        let diagnostics = lex("fn main {;\n").expect_err("invalid source");
        assert_eq!(diagnostics.len(), 2);
        assert_eq!(diagnostics[0].code, "L0102");
        assert_eq!(diagnostics[1].code, "L0103");
    }

    #[test]
    fn semicolon_inside_brackets_lexes_as_a_symbol() {
        // The fill-literal separator `[value; count]` is a `;` symbol token, not
        // an `L0103` error — but only inside brackets.
        let tokens = lex("fn main -> i64\n    let a array<i64, 4> = [0; 4]\n    a[0]\n")
            .expect("fill literal lexes cleanly");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Symbol(";".to_string())),
            "expected a `;` symbol token inside the fill literal"
        );
    }

    #[test]
    fn semicolon_at_statement_level_still_rejected() {
        // Outside brackets, `;` remains a forbidden statement terminator.
        let diagnostics = lex("fn main -> i64\n    let a = 1;\n    a\n").expect_err("stray `;`");
        assert!(diagnostics.iter().any(|d| d.code == "L0103"));
    }

    #[test]
    fn recognizes_planned_keywords_for_parser_rejection() {
        let tokens = lex("import math\nmodule demo\nstruct Point\ntry\ncatch\n").expect("lex");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::Import))
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::Module))
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::Struct))
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::Try))
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::Catch))
        );
    }

    #[test]
    fn recognizes_const_keyword() {
        let tokens = lex("const MAX i64 = 5\n").expect("lex");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::Const)),
            "expected a `const` keyword token, got {tokens:?}"
        );
    }

    #[test]
    fn number_with_digit_separators_is_one_token() {
        let tokens = lex("fn main -> i64\n    1_000_000\n").expect("lex");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Number("1_000_000".to_string())),
            "expected a single `1_000_000` number token, got {tokens:?}"
        );
    }

    #[test]
    fn lexes_bitwise_operator_tokens() {
        // `<<`/`>>` must lex as single two-char symbols, while `& | ^ ~` are
        // single-char symbols. `>>` must not be split into two `>` comparisons.
        let tokens = lex("fn main -> i64\n    1 & 2 | 3 ^ 4 << 5 >> 6\n").expect("lex");
        let symbols: Vec<&str> = tokens
            .iter()
            .filter_map(|token| match &token.kind {
                TokenKind::Symbol(sym) => Some(sym.as_str()),
                _ => None,
            })
            .collect();
        assert!(symbols.contains(&"&"), "missing & token in {symbols:?}");
        assert!(symbols.contains(&"|"), "missing | token in {symbols:?}");
        assert!(symbols.contains(&"^"), "missing ^ token in {symbols:?}");
        assert!(symbols.contains(&"<<"), "missing << token in {symbols:?}");
        assert!(symbols.contains(&">>"), "missing >> token in {symbols:?}");
        assert!(
            !symbols.contains(&">"),
            "`>>` must not be split into `>` tokens: {symbols:?}"
        );
    }

    #[test]
    fn lexes_bitwise_not_token() {
        let tokens = lex("fn main -> i64\n    ~5\n").expect("lex");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Symbol("~".to_string())),
            "expected a `~` symbol token, got {tokens:?}"
        );
    }

    #[test]
    fn lexes_no_runtime_directive_as_one_keyword() {
        // `no-runtime` at the top of a module lexes as a single NoRuntime keyword,
        // not as `no` `-` `runtime`.
        let tokens = lex("no-runtime\nfn main -> i64\n    0\n").expect("lex");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::NoRuntime)),
            "expected a NoRuntime keyword token, got {tokens:?}"
        );
        assert!(
            !tokens
                .iter()
                .any(|token| token.kind == TokenKind::Symbol("-".to_string())),
            "`no-runtime` must not be split into a `-` subtraction: {tokens:?}"
        );
    }

    #[test]
    fn bare_no_and_runtime_stay_identifiers() {
        // A bare `no` / `runtime`, and a spaced `no - runtime` subtraction, are
        // ordinary identifiers/operators — only the contiguous `no-runtime` is the
        // keyword.
        let tokens = lex("fn f no i64, runtime i64 -> i64\n    no - runtime\n").expect("lex");
        assert!(
            tokens
                .iter()
                .filter(|token| token.kind == TokenKind::Identifier("no".to_string()))
                .count()
                >= 1,
            "`no` should be an identifier: {tokens:?}"
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Symbol("-".to_string())),
            "spaced `no - runtime` should keep its `-`: {tokens:?}"
        );
        assert!(
            !tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword(Keyword::NoRuntime)),
            "spaced `no - runtime` must not lex as the NoRuntime keyword: {tokens:?}"
        );
    }

    #[test]
    fn lexes_char_literal() {
        let tokens = lex("fn main -> char\n    'a'\n").expect("valid source");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Char('a'))
        );
    }

    #[test]
    fn rejects_malformed_char_literal() {
        let diagnostics = lex("fn main -> char\n    'ab'\n").expect_err("invalid source");
        assert_eq!(diagnostics[0].code, "L0105");
    }

    #[test]
    fn captures_full_line_and_trailing_comments() {
        let source = "# header\nfn main -> i64\n    let x i64 = 5  # trailing\n    x\n";
        let (_tokens, comments) = lex_with_comments(source).expect("lex");
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].line, 1);
        assert!(!comments[0].trailing);
        assert_eq!(comments[0].text, "# header");
        assert_eq!(comments[1].line, 3);
        assert!(comments[1].trailing);
        assert_eq!(comments[1].text, "# trailing");
    }

    #[test]
    fn does_not_capture_hash_inside_string_literal() {
        // A `#` inside a string is part of the string, not a comment.
        let source = "fn main -> string\n    \"a # b\"\n";
        let (_tokens, comments) = lex_with_comments(source).expect("lex");
        assert!(comments.is_empty(), "unexpected comments: {comments:?}");
    }

    #[test]
    fn validates_canonical_extension() {
        assert!(validate_source_path(Path::new("main.lby")).is_ok());
        assert!(validate_source_path(Path::new("main.txt")).is_err());
        assert!(validate_source_path(Path::new("main")).is_err());
        // The retired `.lullaby` extension is no longer accepted.
        assert!(validate_source_path(Path::new("main.lullaby")).is_err());
    }
}
