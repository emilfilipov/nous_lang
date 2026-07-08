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
    While,
    Loop,
    Break,
    Continue,
    Let,
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
    let mut lexer = Lexer::new(source);
    lexer.lex();
    if lexer.diagnostics.is_empty() {
        Ok(lexer.tokens)
    } else {
        Err(lexer.diagnostics)
    }
}

struct Lexer<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    indent_stack: Vec<usize>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
            indent_stack: vec![0],
        }
    }

    fn lex(&mut self) {
        for (line_index, raw_line) in self.source.lines().enumerate() {
            let line_number = line_index + 1;
            let line = raw_line.trim_end_matches('\r');

            if line.trim().is_empty() || line.trim_start().starts_with('#') {
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

        while index < chars.len() {
            let ch = chars[index];
            let column = base_column + index;

            if ch.is_whitespace() {
                index += 1;
                continue;
            }

            if ch == '#' {
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
                    "==" | "!=" | "<=" | ">=" | "+=" | "-=" | "*=" | "/=" | "<<" | ">>"
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
        "while" => Keyword::While,
        "loop" => Keyword::Loop,
        "break" => Keyword::Break,
        "continue" => Keyword::Continue,
        "let" => Keyword::Let,
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
    fn validates_canonical_extension() {
        assert!(validate_source_path(Path::new("main.lby")).is_ok());
        assert!(validate_source_path(Path::new("main.txt")).is_err());
        assert!(validate_source_path(Path::new("main")).is_err());
        // The retired `.lullaby` extension is no longer accepted.
        assert!(validate_source_path(Path::new("main.lullaby")).is_err());
    }
}
