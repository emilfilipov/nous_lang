use std::path::Path;

pub const CANONICAL_EXTENSION: &str = "nl";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub line: usize,
    pub column: usize,
}

impl Span {
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }
}

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
            "N0001",
            format!("unsupported source extension '.{ext}', expected '.{CANONICAL_EXTENSION}'"),
            Span::new(1, 1),
        )),
        None => Err(Diagnostic::new(
            "N0001",
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
                "N0101",
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
                    "N0102",
                    "curly braces are not block delimiters in Nous Lang",
                    Span::new(line_number, column),
                ));
                index += 1;
                continue;
            }

            if ch == ';' {
                self.diagnostics.push(Diagnostic::new(
                    "N0103",
                    "semicolons do not terminate Nous Lang statements",
                    Span::new(line_number, column),
                ));
                index += 1;
                continue;
            }

            if ch == '"' {
                index = self.lex_string(&chars, index, line_number, column);
                continue;
            }

            if ch.is_ascii_digit() {
                let start = index;
                while index < chars.len()
                    && (chars[index].is_ascii_alphanumeric() || chars[index] == '.')
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
                if matches!(
                    pair.as_str(),
                    "==" | "!=" | "<=" | ">=" | "+=" | "-=" | "*=" | "/="
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
            "N0104",
            "unterminated string literal",
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
        assert_eq!(diagnostics[0].code, "N0102");
        assert_eq!(diagnostics[1].code, "N0103");
    }

    #[test]
    fn validates_canonical_extension() {
        assert!(validate_source_path(Path::new("main.nl")).is_ok());
        assert!(validate_source_path(Path::new("main.nous")).is_err());
    }
}
