use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub token_type: TokenType,
    pub lexeme: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenType {
    // Keywords
    And,
    Break,
    Continue,
    Def,
    Elif,
    Else,
    For,
    If,
    In,
    Lambda, // lambda
    Load,
    Not,
    Or,
    Pass,
    Return,

    // Literals
    Identifier(String),
    Integer(i64),
    Float(f64),
    String(String),
    True,
    False,
    None,

    // Operators
    Plus,        // +
    Minus,       // -
    Star,        // *
    Slash,       // /
    DoubleSlash, // //
    Percent,     // %
    DoubleStar,  // **
    Pipe,        // |  (bitwise-or / dict-union)
    PipeAssign,  // |=
    Caret,       // ^  (bitwise-xor)

    // Comparison
    Equal,        // ==
    NotEqual,     // !=
    Less,         // <
    LessEqual,    // <=
    Greater,      // >
    GreaterEqual, // >=

    // Assignment
    Assign,        // =
    PlusAssign,    // +=
    MinusAssign,   // -=
    StarAssign,    // *=
    SlashAssign,   // /=
    PercentAssign, // %=

    // Delimiters
    LeftParen,    // (
    RightParen,   // )
    LeftBracket,  // [
    RightBracket, // ]
    LeftBrace,    // {
    RightBrace,   // }
    Comma,        // ,
    Colon,        // :
    Dot,          // .

    // Special
    Newline,
    Indent,
    Dedent,
    Eof,
}

impl Token {
    pub fn new(token_type: TokenType, lexeme: String, line: usize, column: usize) -> Self {
        Self {
            token_type,
            lexeme,
            line,
            column,
        }
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{:?} '{}' at {}:{}",
            self.token_type, self.lexeme, self.line, self.column
        )
    }
}

impl TokenType {
    pub fn keyword(s: &str) -> Option<TokenType> {
        match s {
            "and" => Some(TokenType::And),
            "break" => Some(TokenType::Break),
            "continue" => Some(TokenType::Continue),
            "def" => Some(TokenType::Def),
            "elif" => Some(TokenType::Elif),
            "else" => Some(TokenType::Else),
            "for" => Some(TokenType::For),
            "if" => Some(TokenType::If),
            "in" => Some(TokenType::In),
            "lambda" => Some(TokenType::Lambda),
            "load" => Some(TokenType::Load),
            "not" => Some(TokenType::Not),
            "or" => Some(TokenType::Or),
            "pass" => Some(TokenType::Pass),
            "return" => Some(TokenType::Return),
            "True" => Some(TokenType::True),
            "False" => Some(TokenType::False),
            "None" => Some(TokenType::None),
            _ => None,
        }
    }
}
