use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub statements: Vec<Stmt>,
}

impl Program {
    pub fn new() -> Self {
        Self {
            statements: Vec::new(),
        }
    }

    /// #39 graph-RSS census: total statement-node count, recursing into
    /// nested bodies (function defs, if/elif/else, for). A coarse but
    /// robust proxy for parsed-AST weight that avoids a fragile full
    /// `Expr`-tree walk (Stmt has a fixed, small variant set). Used by the
    /// peak live-Program census to size how much AST is live mid-load.
    pub fn stmt_node_count(&self) -> usize {
        count_stmts(&self.statements)
    }
}

/// Recursively count statements, descending into the nested `Vec<Stmt>`
/// bodies that the block-bearing variants carry.
pub fn count_stmts(stmts: &[Stmt]) -> usize {
    let mut n = 0;
    for s in stmts {
        n += 1;
        match s {
            Stmt::FunctionDef { body, .. } => n += count_stmts(body),
            Stmt::If {
                then_body,
                elif_parts,
                else_body,
                ..
            } => {
                n += count_stmts(then_body);
                for (_cond, body) in elif_parts {
                    n += count_stmts(body);
                }
                if let Some(body) = else_body {
                    n += count_stmts(body);
                }
            }
            Stmt::For { body, .. } => n += count_stmts(body),
            _ => {}
        }
    }
    n
}

impl Default for Program {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BindingPattern {
    Identifier(String),
    Tuple(Vec<BindingPattern>),
}

impl BindingPattern {
    pub fn from_identifiers(names: Vec<String>) -> Self {
        if names.len() == 1 {
            Self::Identifier(names.into_iter().next().unwrap())
        } else {
            Self::Tuple(names.into_iter().map(Self::Identifier).collect())
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// Expression statement (e.g., function call)
    Expr(Box<Expr>),

    /// Assignment: x = value or a, [b, c] = value (recursive unpacking)
    Assign {
        target: BindingPattern,
        value: Box<Expr>,
    },

    /// Augmented assignment: x += value
    AugAssign {
        target: String,
        op: BinOp,
        value: Box<Expr>,
    },

    /// Index assignment: obj[key] = value
    IndexAssign {
        object: Box<Expr>,
        index: Box<Expr>,
        value: Box<Expr>,
    },

    /// Augmented index assignment: obj[key] += value
    AugIndexAssign {
        object: Box<Expr>,
        index: Box<Expr>,
        op: BinOp,
        value: Box<Expr>,
    },

    /// Function definition
    FunctionDef {
        name: String,
        params: Vec<(String, Option<Expr>)>, // (name, default_value)
        varargs_param: Option<String>,       // *args parameter
        kwargs_param: Option<String>,        // **kwargs parameter
        body: Vec<Stmt>,
    },

    /// If statement with optional elif/else
    If {
        condition: Box<Expr>,
        then_body: Vec<Stmt>,
        elif_parts: Vec<(Box<Expr>, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
    },

    /// For loop (supports recursively nested tuple unpacking)
    For {
        vars: BindingPattern,
        iterable: Box<Expr>,
        body: Vec<Stmt>,
    },

    /// Return statement
    Return(Option<Box<Expr>>),

    /// Break statement
    Break,

    /// Continue statement
    Continue,

    /// Pass statement (metadata-only)
    Pass,

    /// Load statement: load("file.bzl", "symbol1", alias = "symbol2")
    ///
    /// Symbols are stored as (local_name, original_name) tuples:
    /// - load("f.bzl", "foo") → ("foo", "foo")
    /// - load("f.bzl", bar = "foo") → ("bar", "foo")
    Load {
        file: String,
        symbols: Vec<(String, String)>, // (local_name, original_name_in_bzl)
    },
}

/// Function call expression fields (boxed to reduce Expr enum size)
#[derive(Debug, Clone, PartialEq)]
pub struct CallExpr {
    pub function: Box<Expr>,
    pub args: Vec<Expr>,
    pub args_unpack: Vec<Expr>,
    pub kwargs: Vec<(String, Expr)>,
    pub kwargs_unpack: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Integer literal
    Int(i64),

    /// Float literal
    Float(f64),

    /// String literal
    String(String),

    /// Boolean literal
    Bool(bool),

    /// None literal
    None,

    /// Identifier (variable reference)
    Ident(String),

    /// List literal: [1, 2, 3]
    List(Vec<Expr>),

    /// Dict literal: {"key": value, **other_dict}
    /// Entries are (key, value) pairs, or (None, dict) for **dict unpacking
    Dict(Vec<(Option<Expr>, Expr)>),

    /// Tuple: (1, 2, 3)
    Tuple(Vec<Expr>),

    /// Binary operation: a + b
    BinOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },

    /// Unary operation: -x, not x
    UnaryOp { op: UnaryOp, operand: Box<Expr> },

    /// Function call: foo(a, b, *args_list, c=1, **kwargs_dict)
    Call(Box<CallExpr>),

    /// Attribute access: obj.attr
    Attribute { object: Box<Expr>, attr: String },

    /// Index access: obj[index]
    Index { object: Box<Expr>, index: Box<Expr> },

    /// Slice: obj[start:end:step]
    Slice {
        object: Box<Expr>,
        start: Option<Box<Expr>>,
        end: Option<Box<Expr>>,
        step: Option<Box<Expr>>,
    },

    /// List comprehension: [x for x in items if condition] or [x for a in b for x in a]
    /// for_clauses contains (vars, iterable, optional_condition) tuples
    /// vars is a Vec to support tuple unpacking: [x for (a, b, c) in items]
    ListComp {
        element: Box<Expr>,
        for_clauses: Box<Vec<ForClause>>,
    },

    /// Dict comprehension: {k: v for k, v in items if condition}
    /// Supports multiple for clauses and tuple unpacking
    /// for_clauses contains (vars, iterable, optional_condition) tuples
    DictComp {
        key: Box<Expr>,
        value: Box<Expr>,
        for_clauses: Box<Vec<ForClause>>,
    },

    /// Conditional expression: a if cond else b
    IfExpr {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },

    /// Lambda expression: lambda x, *args, **kwargs: x
    Lambda {
        params: Vec<(String, Option<Box<Expr>>)>, // (name, optional_default)
        varargs_param: Option<String>,
        kwargs_param: Option<String>,
        body: Box<Expr>,
    },
}

/// Type alias for comprehension for-clauses: (vars, iterable, optional_condition)
/// vars is Vec<String> to support tuple unpacking: [x for (a, b, c) in items]
pub type ForClause = (Vec<String>, Box<Expr>, Option<Box<Expr>>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,

    // Comparison
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,

    // Logical
    And,
    Or,

    // Membership
    In,
    NotIn,

    // Bitwise
    BitAnd,
    BitOr,
    BitXor,
    LeftShift,
    RightShift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
}

impl fmt::Display for Expr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Expr::Int(n) => write!(f, "{}", n),
            Expr::Float(n) => write!(f, "{}", n),
            Expr::String(s) => write!(f, "\"{}\"", s),
            Expr::Bool(b) => write!(f, "{}", if *b { "True" } else { "False" }),
            Expr::None => write!(f, "None"),
            Expr::Ident(name) => write!(f, "{}", name),
            Expr::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Expr::Dict(pairs) => {
                write!(f, "{{")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match k {
                        Some(key) => write!(f, "{}: {}", key, v)?,
                        None => write!(f, "**{}", v)?,
                    }
                }
                write!(f, "}}")
            }
            Expr::BinOp { left, op, right } => {
                write!(f, "({} {:?} {})", left, op, right)
            }
            Expr::Call(call) => {
                write!(f, "{}(", call.function)?;
                let mut first = true;
                for arg in &call.args {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", arg)?;
                    first = false;
                }
                for list_expr in &call.args_unpack {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "*{}", list_expr)?;
                    first = false;
                }
                for (k, v) in &call.kwargs {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}={}", k, v)?;
                    first = false;
                }
                for dict_expr in &call.kwargs_unpack {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "**{}", dict_expr)?;
                    first = false;
                }
                write!(f, ")")
            }
            _ => write!(f, "<expr>"),
        }
    }
}
