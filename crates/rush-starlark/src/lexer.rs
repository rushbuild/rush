use crate::token::{Token, TokenType};
use std::collections::VecDeque;

pub struct Lexer {
    input: Vec<char>,
    position: usize,
    line: usize,
    column: usize,
    indent_stack: Vec<usize>,
    pending_tokens: VecDeque<Token>,
    at_line_start: bool,
    nesting_level: usize, // Track parentheses/brackets/braces nesting
}

impl Lexer {
    pub fn new(input: &str) -> Self {
        Self {
            input: input.chars().collect(),
            position: 0,
            line: 1,
            column: 1,
            indent_stack: vec![0],
            pending_tokens: VecDeque::new(),
            at_line_start: true,
            nesting_level: 0,
        }
    }

    pub fn next_token(&mut self) -> Token {
        // Return pending tokens first (for DEDENT sequences)
        if let Some(token) = self.pending_tokens.pop_front() {
            return token;
        }

        self.skip_whitespace_and_comments();

        // Handle indentation at line start
        if self.at_line_start {
            return self.handle_indentation();
        }

        if self.is_at_end() {
            return self.handle_eof();
        }

        let ch = self.current_char();
        let line = self.line;
        let column = self.column;

        let token_type = match ch {
            '\n' => {
                self.advance();
                self.line += 1;
                self.column = 1;
                self.at_line_start = true;
                return self.next_token(); // Skip newlines, handle indentation on next call
            }
            '(' => {
                self.advance();
                self.nesting_level += 1;
                TokenType::LeftParen
            }
            ')' => {
                self.advance();
                if self.nesting_level > 0 {
                    self.nesting_level -= 1;
                }
                TokenType::RightParen
            }
            '[' => {
                self.advance();
                self.nesting_level += 1;
                TokenType::LeftBracket
            }
            ']' => {
                self.advance();
                if self.nesting_level > 0 {
                    self.nesting_level -= 1;
                }
                TokenType::RightBracket
            }
            '{' => {
                self.advance();
                self.nesting_level += 1;
                TokenType::LeftBrace
            }
            '}' => {
                self.advance();
                if self.nesting_level > 0 {
                    self.nesting_level -= 1;
                }
                TokenType::RightBrace
            }
            ',' => {
                self.advance();
                TokenType::Comma
            }
            ':' => {
                self.advance();
                TokenType::Colon
            }
            '.' => {
                self.advance();
                TokenType::Dot
            }
            '+' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::PlusAssign
                } else {
                    TokenType::Plus
                }
            }
            '-' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::MinusAssign
                } else {
                    TokenType::Minus
                }
            }
            '*' => {
                self.advance();
                if self.current_char() == '*' {
                    self.advance();
                    TokenType::DoubleStar
                } else if self.current_char() == '=' {
                    self.advance();
                    TokenType::StarAssign
                } else {
                    TokenType::Star
                }
            }
            '/' => {
                self.advance();
                if self.current_char() == '/' {
                    self.advance();
                    TokenType::DoubleSlash
                } else if self.current_char() == '=' {
                    self.advance();
                    TokenType::SlashAssign
                } else {
                    TokenType::Slash
                }
            }
            '%' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::PercentAssign
                } else {
                    TokenType::Percent
                }
            }
            // Starlark `|` (bitwise-or for ints; dict-union for dicts, Bazel 7+)
            // and `|=`. Modern MODULE.bazel/.bzl (e.g. protobuf's load chain) use
            // dict-union; without this the lexer panicked on '|'.
            '|' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::PipeAssign
                } else {
                    TokenType::Pipe
                }
            }
            '=' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::Equal
                } else {
                    TokenType::Assign
                }
            }
            '!' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::NotEqual
                } else {
                    panic!("Unexpected character '!' at {}:{}", line, column);
                }
            }
            '<' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::LessEqual
                } else {
                    TokenType::Less
                }
            }
            '>' => {
                self.advance();
                if self.current_char() == '=' {
                    self.advance();
                    TokenType::GreaterEqual
                } else {
                    TokenType::Greater
                }
            }
            '"' | '\'' => return self.read_string(ch),
            '\\' => {
                // Line continuation: backslash followed by newline
                self.advance();
                if self.current_char() == '\n' {
                    self.advance();
                    self.line += 1;
                    self.column = 0;
                    // Skip the continuation and get the next token
                    return self.next_token();
                } else {
                    panic!(
                        "Unexpected character '\\' at {}:{} (only valid before newline)",
                        line, column
                    );
                }
            }
            _ if ch.is_ascii_digit() => return self.read_number(),
            _ if ch.is_alphabetic() || ch == '_' => {
                // Starlark string prefixes: r"..." (raw), b"..." (bytes),
                // rb"..."/br"..." — generated external BUILD files use
                // r\"\"\"...\"\"\" heredoc commands. Bytes strings lex as plain
                // strings (rush models bytes as strings).
                if matches!(ch, 'r' | 'R' | 'b' | 'B') {
                    let n1 = self.peek_ahead(1);
                    if matches!(n1, Some('"') | Some('\'')) {
                        let raw = matches!(ch, 'r' | 'R');
                        self.advance();
                        let q = self.current_char();
                        return self.read_string_mode(q, raw);
                    }
                    if matches!(ch, 'r' | 'R') && matches!(n1, Some('b') | Some('B'))
                        || matches!(ch, 'b' | 'B') && matches!(n1, Some('r') | Some('R'))
                    {
                        if matches!(self.peek_ahead(2), Some('"') | Some('\'')) {
                            self.advance();
                            self.advance();
                            let q = self.current_char();
                            return self.read_string_mode(q, true);
                        }
                    }
                }
                return self.read_identifier();
            }
            _ => panic!("Unexpected character '{}' at {}:{}", ch, line, column),
        };

        Token::new(token_type, String::new(), line, column)
    }

    fn handle_indentation(&mut self) -> Token {
        self.at_line_start = false;

        // Count leading spaces
        let mut indent_level = 0;
        while !self.is_at_end() && self.current_char() == ' ' {
            indent_level += 1;
            self.advance();
        }

        // A whitespace-only final line cannot begin a new block. Treat EOF
        // after leading spaces exactly like EOF at column one so the lexer
        // emits only any outstanding DEDENT tokens followed by EOF.
        if self.is_at_end() {
            return self.handle_eof();
        }

        // Skip blank lines
        if self.current_char() == '\n' {
            self.advance();
            self.line += 1;
            self.column = 1;
            self.at_line_start = true;
            return self.next_token();
        }

        // Skip indentation tracking when inside parentheses/brackets/braces
        if self.nesting_level > 0 {
            return self.next_token();
        }

        let current_indent = *self.indent_stack.last().unwrap();

        if indent_level > current_indent {
            // INDENT
            self.indent_stack.push(indent_level);
            return Token::new(TokenType::Indent, String::new(), self.line, 1);
        } else if indent_level < current_indent {
            // DEDENT (possibly multiple)
            let mut dedent_count = 0;
            while !self.indent_stack.is_empty() && *self.indent_stack.last().unwrap() > indent_level
            {
                self.indent_stack.pop();
                dedent_count += 1;
            }

            if self.indent_stack.is_empty() || *self.indent_stack.last().unwrap() != indent_level {
                panic!("Indentation error at line {}", self.line);
            }

            // Return first DEDENT, queue the rest
            for _ in 1..dedent_count {
                self.pending_tokens.push_back(Token::new(
                    TokenType::Dedent,
                    String::new(),
                    self.line,
                    1,
                ));
            }

            return Token::new(TokenType::Dedent, String::new(), self.line, 1);
        }

        // Same indentation level, continue parsing
        self.next_token()
    }

    fn handle_eof(&mut self) -> Token {
        // Emit DEDENT tokens for remaining indent levels
        if self.indent_stack.len() > 1 {
            self.indent_stack.pop();
            self.pending_tokens.push_back(Token::new(
                TokenType::Eof,
                String::new(),
                self.line,
                self.column,
            ));
            return Token::new(TokenType::Dedent, String::new(), self.line, self.column);
        }

        Token::new(TokenType::Eof, String::new(), self.line, self.column)
    }

    fn read_identifier(&mut self) -> Token {
        let start_line = self.line;
        let start_column = self.column;
        let mut result = String::new();

        while !self.is_at_end()
            && (self.current_char().is_alphanumeric() || self.current_char() == '_')
        {
            result.push(self.current_char());
            self.advance();
        }

        let token_type =
            TokenType::keyword(&result).unwrap_or_else(|| TokenType::Identifier(result.clone()));

        Token::new(token_type, result, start_line, start_column)
    }

    fn read_number(&mut self) -> Token {
        let start_line = self.line;
        let start_column = self.column;
        let mut result = String::new();
        let mut is_float = false;

        while !self.is_at_end()
            && (self.current_char().is_ascii_digit() || self.current_char() == '.')
        {
            if self.current_char() == '.' {
                if is_float {
                    break; // Second dot, stop parsing
                }
                is_float = true;
            }
            result.push(self.current_char());
            self.advance();
        }

        let token_type = if is_float {
            TokenType::Float(result.parse().unwrap())
        } else {
            TokenType::Integer(result.parse().unwrap())
        };

        Token::new(token_type, result, start_line, start_column)
    }

    fn read_string(&mut self, quote: char) -> Token {
        self.read_string_mode(quote, false)
    }

    /// String lexing with an optional RAW mode (Starlark r"..."/rb"..."):
    /// in raw mode backslash is a literal character, never an escape.
    fn read_string_mode(&mut self, quote: char, raw: bool) -> Token {
        let start_line = self.line;
        let start_column = self.column;
        self.advance(); // Skip opening quote

        let mut result = String::new();

        // Check for triple-quoted strings
        let is_triple =
            !self.is_at_end() && self.current_char() == quote && self.peek_ahead(1) == Some(quote);

        if is_triple {
            self.advance(); // Second quote
            self.advance(); // Third quote

            // Read until closing triple quotes
            while !self.is_at_end() {
                if self.current_char() == quote
                    && self.peek_ahead(1) == Some(quote)
                    && self.peek_ahead(2) == Some(quote)
                {
                    self.advance();
                    self.advance();
                    self.advance();
                    break;
                }

                if self.current_char() == '\\' && !raw {
                    // Handle escape sequences in triple-quoted strings
                    self.advance();
                    if !self.is_at_end() {
                        let escaped = match self.current_char() {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '\\' => '\\',
                            '\'' => '\'',
                            '"' => '"',
                            '\n' => {
                                // Backslash-newline in triple-quoted string: line continuation
                                self.line += 1;
                                self.column = 0;
                                self.advance();
                                continue; // Skip adding to result
                            }
                            c => {
                                // Unknown escape: keep backslash and character
                                result.push('\\');
                                c
                            }
                        };
                        result.push(escaped);
                        self.advance();
                    }
                } else {
                    if self.current_char() == '\n' {
                        self.line += 1;
                        self.column = 0;
                    }

                    result.push(self.current_char());
                    self.advance();
                }
            }
        } else {
            // Regular string
            while !self.is_at_end() && self.current_char() != quote {
                if self.current_char() == '\\' && !raw {
                    self.advance();
                    if !self.is_at_end() {
                        let escaped = match self.current_char() {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            '\\' => '\\',
                            '\'' => '\'',
                            '"' => '"',
                            c => c,
                        };
                        result.push(escaped);
                        self.advance();
                    }
                } else {
                    result.push(self.current_char());
                    self.advance();
                }
            }

            if !self.is_at_end() {
                self.advance(); // Skip closing quote
            }
        }

        Token::new(
            TokenType::String(result.clone()),
            result,
            start_line,
            start_column,
        )
    }

    fn skip_whitespace_and_comments(&mut self) {
        while !self.is_at_end() {
            match self.current_char() {
                ' ' | '\t' | '\r' if !self.at_line_start => {
                    self.advance();
                }
                '#' => {
                    // Skip comment until end of line
                    while !self.is_at_end() && self.current_char() != '\n' {
                        self.advance();
                    }
                }
                _ => break,
            }
        }
    }

    fn current_char(&self) -> char {
        self.input[self.position]
    }

    fn peek_ahead(&self, n: usize) -> Option<char> {
        if self.position + n < self.input.len() {
            Some(self.input[self.position + n])
        } else {
            None
        }
    }

    fn advance(&mut self) {
        if !self.is_at_end() {
            self.position += 1;
            self.column += 1;
        }
    }

    fn is_at_end(&self) -> bool {
        self.position >= self.input.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(input: &str) -> Vec<Token> {
        let mut lexer = Lexer::new(input);
        let mut tokens = Vec::new();
        loop {
            let token = lexer.next_token();
            let is_eof = matches!(token.token_type, TokenType::Eof);
            tokens.push(token);
            if is_eof {
                break;
            }
        }
        tokens
    }

    #[test]
    fn test_basic_tokens() {
        let tokens = tokenize("x = 42");
        assert_eq!(tokens[0].token_type, TokenType::Identifier("x".to_string()));
        assert_eq!(tokens[1].token_type, TokenType::Assign);
        assert_eq!(tokens[2].token_type, TokenType::Integer(42));
    }

    #[test]
    fn test_operators() {
        let tokens = tokenize("+ - * / // ** % == != < <= > >=");
        let expected = vec![
            TokenType::Plus,
            TokenType::Minus,
            TokenType::Star,
            TokenType::Slash,
            TokenType::DoubleSlash,
            TokenType::DoubleStar,
            TokenType::Percent,
            TokenType::Equal,
            TokenType::NotEqual,
            TokenType::Less,
            TokenType::LessEqual,
            TokenType::Greater,
            TokenType::GreaterEqual,
        ];
        for (i, expected_type) in expected.iter().enumerate() {
            assert_eq!(&tokens[i].token_type, expected_type);
        }
    }

    #[test]
    fn test_string_literals() {
        let tokens = tokenize(r#""hello" 'world'"#);
        assert_eq!(tokens[0].token_type, TokenType::String("hello".to_string()));
        assert_eq!(tokens[1].token_type, TokenType::String("world".to_string()));
    }

    #[test]
    fn test_keywords() {
        let tokens = tokenize("def if else for return");
        assert_eq!(tokens[0].token_type, TokenType::Def);
        assert_eq!(tokens[1].token_type, TokenType::If);
        assert_eq!(tokens[2].token_type, TokenType::Else);
        assert_eq!(tokens[3].token_type, TokenType::For);
        assert_eq!(tokens[4].token_type, TokenType::Return);
    }

    #[test]
    fn test_indentation() {
        let source = r#"
def foo():
    x = 1
    y = 2
"#;
        let tokens = tokenize(source);
        // Should have INDENT after def foo():, and DEDENT at end
        let has_indent = tokens
            .iter()
            .any(|t| matches!(t.token_type, TokenType::Indent));
        let has_dedent = tokens
            .iter()
            .any(|t| matches!(t.token_type, TokenType::Dedent));
        assert!(has_indent);
        assert!(has_dedent);
    }

    #[test]
    fn test_trailing_whitespace_does_not_indent() {
        let tokens = tokenize("x = 1\n            ");
        assert!(!tokens
            .iter()
            .any(|token| matches!(token.token_type, TokenType::Indent | TokenType::Dedent)));
        assert!(matches!(
            tokens.last().map(|token| &token.token_type),
            Some(TokenType::Eof)
        ));
    }
}
