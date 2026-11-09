use crate::ast::{BinOp, BindingPattern, CallExpr, Expr, Program, Stmt, UnaryOp};
use crate::lexer::Lexer;
use crate::token::{Token, TokenType};
use anyhow::{anyhow, Result};

pub struct Parser {
    lexer: Lexer,
    current: Token,
    peek: Token,
    /// Track delimiter depth (parens, brackets, braces) to ignore INDENT/DEDENT inside delimiters
    delimiter_depth: usize,
    /// Track the line number of the last consumed token.
    /// Used to distinguish ternary `if` (same line) from statement `if` (different line).
    last_consumed_line: usize,
}

impl Parser {
    pub fn new(mut lexer: Lexer) -> Self {
        let current = lexer.next_token();
        let peek = lexer.next_token();
        let last_consumed_line = current.line;
        Self {
            lexer,
            current,
            peek,
            delimiter_depth: 0,
            last_consumed_line,
        }
    }

    pub fn parse(&mut self) -> Result<Program> {
        let mut program = Program::new();

        while !matches!(self.current.token_type, TokenType::Eof) {
            program.statements.push(self.parse_statement()?);
        }

        Ok(program)
    }

    fn parse_statement(&mut self) -> Result<Stmt> {
        match &self.current.token_type {
            TokenType::Def => self.parse_function_def(),
            TokenType::If => self.parse_if_statement(),
            TokenType::For => self.parse_for_statement(),
            TokenType::Return => self.parse_return_statement(),
            TokenType::Break => {
                self.advance();
                Ok(Stmt::Break)
            }
            TokenType::Continue => {
                self.advance();
                Ok(Stmt::Continue)
            }
            TokenType::Pass => {
                self.advance();
                Ok(Stmt::Pass)
            }
            TokenType::Load => {
                // Skip load statements for now - they're handled separately
                self.parse_load_statement()
            }
            TokenType::Identifier(_) => {
                // Could be assignment or expression statement
                // We need to parse the expression first to handle cases like:
                // - x = 1 (simple assignment)
                // - x, y = 1, 2 (tuple unpacking)
                // - x[0] = 1 (indexed assignment)
                // - x.attr = 1 (attribute assignment - not yet supported)
                // - x() (function call - expression statement)

                // Parse the left side (could be identifier, index, tuple of identifiers, etc.)
                let expr = self.parse_postfix_expression()?;

                // Check if this is an assignment
                if matches!(
                    self.current.token_type,
                    TokenType::Assign
                        | TokenType::PlusAssign
                        | TokenType::MinusAssign
                        | TokenType::StarAssign
                        | TokenType::SlashAssign
                        | TokenType::PercentAssign
                        | TokenType::PipeAssign
                        | TokenType::Comma // For tuple unpacking: a, b = ...
                ) {
                    self.parse_assignment_with_lhs(expr)
                } else {
                    Ok(Stmt::Expr(Box::new(expr)))
                }
            }
            TokenType::Indent | TokenType::Dedent => {
                self.advance();
                self.parse_statement()
            }
            _ => {
                // Parse as expression statement
                // Check if it's a list comprehension - those can appear at statement level
                let expr = self.parse_expression()?;

                // Check if this could be a tuple unpacking assignment like (a, b) = ...
                // This handles cases where a statement starts with ( instead of an identifier
                if matches!(
                    self.current.token_type,
                    TokenType::Assign
                        | TokenType::PlusAssign
                        | TokenType::MinusAssign
                        | TokenType::StarAssign
                        | TokenType::SlashAssign
                        | TokenType::PercentAssign
                        | TokenType::PipeAssign
                        | TokenType::Comma
                ) {
                    return self.parse_assignment_with_lhs(expr);
                }

                // Check if it's a list comprehension - if so, expand to for loop
                if let Expr::ListComp {
                    element,
                    for_clauses,
                } = expr
                {
                    // Statement-level list comprehension - expand to for loop(s)
                    // [element for vars in iterable if condition]
                    // becomes: for vars in iterable: if condition: element

                    // Build the loop from innermost to outermost
                    // For simplicity, we only handle single for clause (most common)
                    if for_clauses.len() == 1 {
                        let (vars, iterable, condition) = &for_clauses[0];
                        let body = if let Some(cond) = condition {
                            // Has condition: for x in items: if cond: element
                            vec![Stmt::If {
                                condition: Box::new((**cond).clone()),
                                then_body: vec![Stmt::Expr(element)],
                                elif_parts: vec![],
                                else_body: None,
                            }]
                        } else {
                            // No condition: for x in items: element
                            vec![Stmt::Expr(element)]
                        };

                        Ok(Stmt::For {
                            vars: BindingPattern::from_identifiers(vars.clone()),
                            iterable: Box::new((**iterable).clone()),
                            body,
                        })
                    } else {
                        // TODO: Handle nested comprehensions
                        // For now, just treat as expression statement (will be a metadata-only)
                        Ok(Stmt::Expr(Box::new(Expr::ListComp {
                            element,
                            for_clauses,
                        })))
                    }
                } else {
                    Ok(Stmt::Expr(Box::new(expr)))
                }
            }
        }
    }

    fn parse_function_def(&mut self) -> Result<Stmt> {
        self.expect(TokenType::Def)?;

        let name = self.expect_identifier()?;

        self.expect(TokenType::LeftParen)?;
        let mut params = Vec::new();
        let mut varargs_param = None;
        let mut kwargs_param = None;

        while !matches!(self.current.token_type, TokenType::RightParen) {
            // Check for **kwargs
            if matches!(self.current.token_type, TokenType::DoubleStar) {
                self.advance();
                kwargs_param = Some(self.expect_identifier()?);

                // **kwargs must be the last parameter
                if matches!(self.current.token_type, TokenType::Comma) {
                    self.advance();
                }
                break;
            }

            // Check for *args or bare * (keyword-only argument separator)
            if matches!(self.current.token_type, TokenType::Star) {
                self.advance();

                // Check if this is a bare star (*, kwarg) - keyword-only argument separator
                if matches!(self.current.token_type, TokenType::Comma) {
                    // Bare star: all following arguments must be keyword-only
                    // Just skip the comma and continue parsing (no varargs param)
                    self.advance();
                    continue;
                } else {
                    // This is *args
                    varargs_param = Some(self.expect_identifier()?);

                    // After *args, only **kwargs can follow
                    if matches!(self.current.token_type, TokenType::Comma) {
                        self.advance();
                    }
                    continue;
                }
            }

            // Parse parameter name
            let param_name = self.expect_identifier()?;

            // Check for default value
            let default_value = if matches!(self.current.token_type, TokenType::Assign) {
                self.advance();
                Some(self.parse_expression()?)
            } else {
                None
            };

            params.push((param_name, default_value));

            if matches!(self.current.token_type, TokenType::Comma) {
                self.advance();
            }
        }
        self.expect(TokenType::RightParen)?;

        self.expect(TokenType::Colon)?;
        self.expect(TokenType::Indent)?;

        let mut body = Vec::new();
        while !matches!(self.current.token_type, TokenType::Dedent | TokenType::Eof) {
            body.push(self.parse_statement()?);
        }

        if matches!(self.current.token_type, TokenType::Dedent) {
            self.advance();
        }

        Ok(Stmt::FunctionDef {
            name,
            params,
            varargs_param,
            kwargs_param,
            body,
        })
    }

    fn parse_if_statement(&mut self) -> Result<Stmt> {
        self.expect(TokenType::If)?;

        let condition = self.parse_expression()?;
        self.expect(TokenType::Colon)?;
        self.expect(TokenType::Indent)?;

        let mut then_body = Vec::new();
        while !matches!(
            self.current.token_type,
            TokenType::Dedent | TokenType::Elif | TokenType::Else | TokenType::Eof
        ) {
            then_body.push(self.parse_statement()?);
        }

        if matches!(self.current.token_type, TokenType::Dedent) {
            self.advance();
        }

        let mut elif_parts = Vec::new();
        while matches!(self.current.token_type, TokenType::Elif) {
            self.advance();
            let elif_cond = self.parse_expression()?;
            self.expect(TokenType::Colon)?;
            self.expect(TokenType::Indent)?;

            let mut elif_body = Vec::new();
            while !matches!(
                self.current.token_type,
                TokenType::Dedent | TokenType::Elif | TokenType::Else | TokenType::Eof
            ) {
                elif_body.push(self.parse_statement()?);
            }

            if matches!(self.current.token_type, TokenType::Dedent) {
                self.advance();
            }

            elif_parts.push((Box::new(elif_cond), elif_body));
        }

        let else_body = if matches!(self.current.token_type, TokenType::Else) {
            self.advance();
            self.expect(TokenType::Colon)?;
            self.expect(TokenType::Indent)?;

            let mut body = Vec::new();
            while !matches!(self.current.token_type, TokenType::Dedent | TokenType::Eof) {
                body.push(self.parse_statement()?);
            }

            if matches!(self.current.token_type, TokenType::Dedent) {
                self.advance();
            }

            Some(body)
        } else {
            None
        };

        Ok(Stmt::If {
            condition: Box::new(condition),
            then_body,
            elif_parts,
            else_body,
        })
    }

    fn parse_for_statement(&mut self) -> Result<Stmt> {
        self.expect(TokenType::For)?;

        let vars = self.parse_binding_pattern_sequence()?;

        self.expect(TokenType::In)?;

        let iterable = self.parse_expression()?;
        self.expect(TokenType::Colon)?;
        self.expect(TokenType::Indent)?;

        let mut body = Vec::new();
        while !matches!(self.current.token_type, TokenType::Dedent | TokenType::Eof) {
            body.push(self.parse_statement()?);
        }

        if matches!(self.current.token_type, TokenType::Dedent) {
            self.advance();
        }

        Ok(Stmt::For {
            vars,
            iterable: Box::new(iterable),
            body,
        })
    }

    fn parse_binding_pattern_sequence(&mut self) -> Result<BindingPattern> {
        let mut patterns = vec![self.parse_binding_pattern()?];

        while matches!(self.current.token_type, TokenType::Comma) {
            self.advance();
            if matches!(
                self.current.token_type,
                TokenType::In | TokenType::RightParen | TokenType::RightBracket
            ) {
                break;
            }
            patterns.push(self.parse_binding_pattern()?);
        }

        if patterns.len() == 1 {
            Ok(patterns.remove(0))
        } else {
            Ok(BindingPattern::Tuple(patterns))
        }
    }

    fn parse_binding_pattern(&mut self) -> Result<BindingPattern> {
        if matches!(self.current.token_type, TokenType::LeftParen) {
            self.advance();
            let pattern = self.parse_binding_pattern_sequence()?;
            self.expect(TokenType::RightParen)?;
            Ok(pattern)
        } else if matches!(self.current.token_type, TokenType::LeftBracket) {
            // Starlark list-pattern loop variables:
            // `for [a, b] in ...` (rules_python's whl_library_alias.bzl).
            // Unlike grouping parens, a list pattern ALWAYS unpacks, even
            // with a single element (`for [a] in [[1]]` binds a=1).
            self.advance();
            let pattern = self.parse_binding_pattern_sequence()?;
            self.expect(TokenType::RightBracket)?;
            Ok(match pattern {
                p @ BindingPattern::Tuple(_) => p,
                single => BindingPattern::Tuple(vec![single]),
            })
        } else {
            Ok(BindingPattern::Identifier(self.expect_identifier()?))
        }
    }

    fn parse_return_statement(&mut self) -> Result<Stmt> {
        self.expect(TokenType::Return)?;

        if matches!(
            self.current.token_type,
            TokenType::Eof | TokenType::Dedent | TokenType::Indent
        ) {
            Ok(Stmt::Return(None))
        } else {
            // Use parse_tuple_expression to support "return a, b" syntax
            let expr = self.parse_tuple_expression()?;
            Ok(Stmt::Return(Some(Box::new(expr))))
        }
    }

    fn parse_load_statement(&mut self) -> Result<Stmt> {
        self.expect(TokenType::Load)?;
        self.expect(TokenType::LeftParen)?;

        let file = match &self.current.token_type {
            TokenType::String(s) => {
                let file = s.clone();
                self.advance();
                file
            }
            _ => return Err(anyhow!("Expected string in load statement")),
        };

        let mut symbols = Vec::new();
        while matches!(self.current.token_type, TokenType::Comma) {
            self.advance();

            // Check if this is a keyword argument (alias = "original")
            if let TokenType::Identifier(local_name) = &self.current.token_type {
                let local_name = local_name.clone();
                self.advance();

                // Check for '=' to confirm this is an alias
                if matches!(self.current.token_type, TokenType::Assign) {
                    self.advance();

                    // Parse the original name (must be a string)
                    if let TokenType::String(original_name) = &self.current.token_type {
                        let original_name = original_name.clone();
                        symbols.push((local_name, original_name));
                        self.advance();
                    } else {
                        return Err(anyhow!("Expected string after '=' in load statement"));
                    }
                } else {
                    return Err(anyhow!("Expected '=' after identifier in load statement"));
                }
            } else if let TokenType::String(s) = &self.current.token_type {
                // Positional argument: no alias, use same name for both
                let name = s.clone();
                symbols.push((name.clone(), name));
                self.advance();
            }
        }

        self.expect(TokenType::RightParen)?;

        Ok(Stmt::Load { file, symbols })
    }

    fn parse_assignment_with_lhs(&mut self, lhs: Expr) -> Result<Stmt> {
        // Handle different types of left-hand sides
        match &lhs {
            Expr::Ident(name) => {
                // Simple identifier or tuple unpacking
                let mut targets = vec![name.clone()];

                // Check for tuple unpacking (multiple targets separated by commas)
                while matches!(self.current.token_type, TokenType::Comma) {
                    self.advance(); // consume comma
                    let next_target = self.expect_identifier()?;
                    targets.push(next_target);
                }

                match &self.current.token_type {
                    TokenType::Assign => {
                        self.advance();
                        let value = self.parse_tuple_expression()?;
                        Ok(Stmt::Assign {
                            target: BindingPattern::from_identifiers(targets),
                            value: Box::new(value),
                        })
                    }
                    TokenType::PlusAssign => {
                        if targets.len() > 1 {
                            return Err(anyhow!(
                                "Augmented assignment does not support tuple unpacking"
                            ));
                        }
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugAssign {
                            target: name.clone(),
                            op: BinOp::Add,
                            value: Box::new(value),
                        })
                    }
                    TokenType::MinusAssign => {
                        if targets.len() > 1 {
                            return Err(anyhow!(
                                "Augmented assignment does not support tuple unpacking"
                            ));
                        }
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugAssign {
                            target: name.clone(),
                            op: BinOp::Sub,
                            value: Box::new(value),
                        })
                    }
                    TokenType::StarAssign => {
                        if targets.len() > 1 {
                            return Err(anyhow!(
                                "Augmented assignment does not support tuple unpacking"
                            ));
                        }
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugAssign {
                            target: name.clone(),
                            op: BinOp::Mul,
                            value: Box::new(value),
                        })
                    }
                    TokenType::SlashAssign => {
                        if targets.len() > 1 {
                            return Err(anyhow!(
                                "Augmented assignment does not support tuple unpacking"
                            ));
                        }
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugAssign {
                            target: name.clone(),
                            op: BinOp::Div,
                            value: Box::new(value),
                        })
                    }
                    TokenType::PercentAssign => {
                        if targets.len() > 1 {
                            return Err(anyhow!(
                                "Augmented assignment does not support tuple unpacking"
                            ));
                        }
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugAssign {
                            target: name.clone(),
                            op: BinOp::Mod,
                            value: Box::new(value),
                        })
                    }
                    TokenType::PipeAssign => {
                        if targets.len() > 1 {
                            return Err(anyhow!(
                                "Augmented assignment does not support tuple unpacking"
                            ));
                        }
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugAssign {
                            target: name.clone(),
                            op: BinOp::BitOr,
                            value: Box::new(value),
                        })
                    }
                    _ => Err(anyhow!("Expected assignment operator")),
                }
            }
            Expr::Index { object, index } => {
                // Indexed assignment: obj[key] = value or obj[key] += value
                match &self.current.token_type {
                    TokenType::Assign => {
                        self.advance();
                        let value = self.parse_tuple_expression()?;
                        Ok(Stmt::IndexAssign {
                            object: Box::new((**object).clone()),
                            index: Box::new((**index).clone()),
                            value: Box::new(value),
                        })
                    }
                    TokenType::PlusAssign => {
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugIndexAssign {
                            object: Box::new((**object).clone()),
                            index: Box::new((**index).clone()),
                            op: BinOp::Add,
                            value: Box::new(value),
                        })
                    }
                    TokenType::MinusAssign => {
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugIndexAssign {
                            object: Box::new((**object).clone()),
                            index: Box::new((**index).clone()),
                            op: BinOp::Sub,
                            value: Box::new(value),
                        })
                    }
                    TokenType::StarAssign => {
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugIndexAssign {
                            object: Box::new((**object).clone()),
                            index: Box::new((**index).clone()),
                            op: BinOp::Mul,
                            value: Box::new(value),
                        })
                    }
                    TokenType::SlashAssign => {
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugIndexAssign {
                            object: Box::new((**object).clone()),
                            index: Box::new((**index).clone()),
                            op: BinOp::Div,
                            value: Box::new(value),
                        })
                    }
                    TokenType::PercentAssign => {
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugIndexAssign {
                            object: Box::new((**object).clone()),
                            index: Box::new((**index).clone()),
                            op: BinOp::Mod,
                            value: Box::new(value),
                        })
                    }
                    TokenType::PipeAssign => {
                        self.advance();
                        let value = self.parse_expression()?;
                        Ok(Stmt::AugIndexAssign {
                            object: Box::new((**object).clone()),
                            index: Box::new((**index).clone()),
                            op: BinOp::BitOr,
                            value: Box::new(value),
                        })
                    }
                    _ => Err(anyhow!(
                        "Expected assignment operator after indexed expression"
                    )),
                }
            }
            Expr::Tuple(_) | Expr::List(_) => {
                let target = Self::binding_pattern_from_expr(&lhs)?;

                // Only regular assignment is supported for destructuring.
                if matches!(self.current.token_type, TokenType::Assign) {
                    self.advance();
                    let value = self.parse_tuple_expression()?;
                    Ok(Stmt::Assign {
                        target,
                        value: Box::new(value),
                    })
                } else {
                    Err(anyhow!(
                        "Augmented assignment does not support tuple unpacking"
                    ))
                }
            }
            _ => Err(anyhow!("Invalid left-hand side for assignment: {:?}", lhs)),
        }
    }

    fn binding_pattern_from_expr(expr: &Expr) -> Result<BindingPattern> {
        match expr {
            Expr::Ident(name) => Ok(BindingPattern::Identifier(name.clone())),
            Expr::Tuple(elements) | Expr::List(elements) => elements
                .iter()
                .map(Self::binding_pattern_from_expr)
                .collect::<Result<Vec<_>>>()
                .map(BindingPattern::Tuple),
            _ => Err(anyhow!(
                "Destructuring assignment requires identifiers or nested sequences, got: {:?}",
                expr
            )),
        }
    }

    fn parse_expression(&mut self) -> Result<Expr> {
        self.parse_ternary_expression()
    }

    /// Parse expression that may be a tuple (for assignment values)
    fn parse_tuple_expression(&mut self) -> Result<Expr> {
        // Parse first expression
        let first = self.parse_ternary_expression()?;

        // Check for comma - creates implicit tuple
        if matches!(self.current.token_type, TokenType::Comma) {
            let mut elements = vec![first];

            while matches!(self.current.token_type, TokenType::Comma) {
                self.advance(); // consume comma

                // Allow trailing comma
                if matches!(self.current.token_type, TokenType::Newline | TokenType::Eof) {
                    break;
                }

                elements.push(self.parse_ternary_expression()?);
            }

            Ok(Expr::Tuple(elements))
        } else {
            Ok(first)
        }
    }

    fn parse_ternary_expression(&mut self) -> Result<Expr> {
        let expr = self.parse_or_expression()?;

        // After parsing the expression, check for conditional expression: expr if condition else else_expr
        // The key challenge is distinguishing between:
        //   1. `[...] if cond else []` - ternary expression (if on same line as ])
        //   2. `[...]\nif cond:` - new if statement (if on different line)
        //
        // We use `self.last_consumed_line` which tracks the line of the most recently
        // consumed token. This lets us know if `if` is on the same line as the last
        // part of the expression (e.g., the `]` of a list).
        //
        // For a multi-line expression like `[...\n...\n] if cond else []`:
        // - Expression ends at line N (where `]` is)
        // - `if` is also on line N
        // - last_consumed_line = N (the `]` line)
        // - if_line = N
        // - Same line, so it's a ternary
        //
        // For `x = 5\nif x > 0:\n  pass`:
        // - Expression ends at line M (where `5` is)
        // - `if` is on line M+1
        // - last_consumed_line = M
        // - if_line = M+1
        // - Different lines, so NOT a ternary
        if matches!(self.current.token_type, TokenType::If) {
            let if_line = self.current.line;

            // Inside delimiters, always allow multi-line ternaries
            if self.delimiter_depth > 0 {
                self.advance(); // consume 'if'
                let condition = self.parse_or_expression()?;
                self.expect(TokenType::Else)?;
                let else_expr = self.parse_ternary_expression()?;

                return Ok(Expr::IfExpr {
                    condition: Box::new(condition),
                    then_expr: Box::new(expr),
                    else_expr: Box::new(else_expr),
                });
            }

            // At statement level (delimiter_depth == 0):
            // Only parse as ternary if `if` is on the same line as the last token of the expression.
            if if_line == self.last_consumed_line {
                self.advance(); // consume 'if'
                let condition = self.parse_or_expression()?;
                self.expect(TokenType::Else)?;
                let else_expr = self.parse_ternary_expression()?;

                return Ok(Expr::IfExpr {
                    condition: Box::new(condition),
                    then_expr: Box::new(expr),
                    else_expr: Box::new(else_expr),
                });
            }
            // else: `if` is on different line, don't parse as ternary
            // Just return the expression and let the caller handle the `if` as a statement
        }

        Ok(expr)
    }

    fn parse_or_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_and_expression()?;

        while matches!(self.current.token_type, TokenType::Or) {
            self.advance();
            let right = self.parse_and_expression()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::Or,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_and_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_not_expression()?;

        while matches!(self.current.token_type, TokenType::And) {
            self.advance();
            let right = self.parse_not_expression()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::And,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_not_expression(&mut self) -> Result<Expr> {
        if matches!(self.current.token_type, TokenType::Not) {
            self.advance();
            let operand = self.parse_not_expression()?;
            Ok(Expr::UnaryOp {
                op: UnaryOp::Not,
                operand: Box::new(operand),
            })
        } else {
            self.parse_comparison_expression()
        }
    }

    fn parse_comparison_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_bitor_expression()?;

        while let Some(op) = self.match_comparison_op() {
            // Advance once for most operators, twice for "not in"
            self.advance();
            if matches!(op, BinOp::NotIn) {
                self.advance(); // consume the "in" part of "not in"
            }
            let right = self.parse_bitor_expression()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    /// `|` (bitwise-or for ints; dict-union for dicts). Precedence sits between
    /// comparison (looser) and bitwise XOR (tighter).
    fn parse_bitor_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_bitxor_expression()?;

        while matches!(self.current.token_type, TokenType::Pipe) {
            self.advance();
            let right = self.parse_bitxor_expression()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::BitOr,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_bitxor_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_additive_expression()?;

        while matches!(self.current.token_type, TokenType::Caret) {
            self.advance();
            let right = self.parse_additive_expression()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::BitXor,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_additive_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_multiplicative_expression()?;

        while matches!(self.current.token_type, TokenType::Plus | TokenType::Minus) {
            let op = match self.current.token_type {
                TokenType::Plus => BinOp::Add,
                TokenType::Minus => BinOp::Sub,
                _ => unreachable!(),
            };
            self.advance();
            let right = self.parse_multiplicative_expression()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_multiplicative_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_power_expression()?;

        while matches!(
            self.current.token_type,
            TokenType::Star | TokenType::Slash | TokenType::DoubleSlash | TokenType::Percent
        ) {
            let op = match self.current.token_type {
                TokenType::Star => BinOp::Mul,
                TokenType::Slash => BinOp::Div,
                TokenType::DoubleSlash => BinOp::FloorDiv,
                TokenType::Percent => BinOp::Mod,
                _ => unreachable!(),
            };
            self.advance();
            let right = self.parse_power_expression()?;
            left = Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_power_expression(&mut self) -> Result<Expr> {
        let mut left = self.parse_unary_expression()?;

        if matches!(self.current.token_type, TokenType::DoubleStar) {
            self.advance();
            let right = self.parse_power_expression()?; // Right-associative
            left = Expr::BinOp {
                left: Box::new(left),
                op: BinOp::Pow,
                right: Box::new(right),
            };
        }

        Ok(left)
    }

    fn parse_unary_expression(&mut self) -> Result<Expr> {
        if matches!(self.current.token_type, TokenType::Minus) {
            self.advance();
            let operand = self.parse_unary_expression()?;
            Ok(Expr::UnaryOp {
                op: UnaryOp::Neg,
                operand: Box::new(operand),
            })
        } else {
            self.parse_postfix_expression()
        }
    }

    fn parse_postfix_expression(&mut self) -> Result<Expr> {
        // Track the line where the expression starts and the last operation
        let start_line = self.current.line;
        let mut last_operation_line = start_line;
        let mut expr = self.parse_primary_expression()?;

        loop {
            // Check if we're on a different line than the last operation
            // Rules for continuing across newlines:
            // 1. If inside delimiters (parens/brackets/braces), always continue
            // 2. If on same line as last operation, continue for postfix operators
            // 3. If on different line AND not inside delimiters:
            //    - Only continue for `.` (unambiguous postfix operator)
            //    - Break for `[` or `(` (could start new expression)
            if self.current.line != last_operation_line && self.delimiter_depth == 0 {
                // On a different line from last operation, not inside delimiters
                // Only continue if it's a dot (unambiguous postfix)
                if !matches!(self.current.token_type, TokenType::Dot) {
                    break;
                }
            }

            match &self.current.token_type {
                TokenType::LeftParen => {
                    let op_line = self.current.line; // Capture line BEFORE parsing
                    expr = self.parse_call_expression(expr)?;
                    last_operation_line = op_line; // Use the line where the operation started
                }
                TokenType::LeftBracket => {
                    let op_line = self.current.line; // Capture line BEFORE parsing
                    expr = self.parse_index_expression(expr)?;
                    last_operation_line = op_line; // Use the line where the operation started
                }
                TokenType::Dot => {
                    let op_line = self.current.line; // Capture line BEFORE parsing
                    self.advance();
                    let attr = self.expect_identifier()?;
                    expr = Expr::Attribute {
                        object: Box::new(expr),
                        attr,
                    };
                    last_operation_line = op_line; // Use the line where the operation started
                }
                _ => {
                    break;
                }
            }
        }

        Ok(expr)
    }

    fn parse_call_expression(&mut self, function: Expr) -> Result<Expr> {
        self.expect(TokenType::LeftParen)?;

        let mut args = Vec::new();
        let mut args_unpack = Vec::new();
        let mut kwargs = Vec::new();
        let mut kwargs_unpack = Vec::new();

        while !matches!(
            self.current.token_type,
            TokenType::RightParen | TokenType::Eof
        ) {
            // Check for **kwargs unpacking
            if matches!(self.current.token_type, TokenType::DoubleStar) {
                self.advance(); // consume **
                let dict_expr = self.parse_expression()?;
                kwargs_unpack.push(dict_expr);
            }
            // Check for *args unpacking
            else if matches!(self.current.token_type, TokenType::Star) {
                self.advance(); // consume *
                let list_expr = self.parse_expression()?;
                args_unpack.push(list_expr);
            }
            // Check if this is a keyword argument
            else if matches!(self.current.token_type, TokenType::Identifier(_))
                && matches!(self.peek.token_type, TokenType::Assign)
            {
                let key = self.expect_identifier()?;
                self.expect(TokenType::Assign)?;
                let value = self.parse_expression()?;
                kwargs.push((key, value));
            } else {
                args.push(self.parse_expression()?);
            }

            if matches!(self.current.token_type, TokenType::Comma) {
                self.advance();
            }
        }

        self.expect(TokenType::RightParen)?;

        Ok(Expr::Call(Box::new(CallExpr {
            function: Box::new(function),
            args,
            args_unpack,
            kwargs,
            kwargs_unpack,
        })))
    }

    fn parse_index_expression(&mut self, object: Expr) -> Result<Expr> {
        self.expect(TokenType::LeftBracket)?;

        // Check for slice starting with colon: obj[:end]
        let start = if matches!(self.current.token_type, TokenType::Colon) {
            None
        } else if matches!(self.current.token_type, TokenType::RightBracket) {
            // Empty brackets not allowed
            return Err(anyhow!("Empty index/slice not allowed"));
        } else {
            Some(Box::new(self.parse_expression()?))
        };

        // Check if this is a slice (has colon)
        if matches!(self.current.token_type, TokenType::Colon) {
            self.advance(); // consume ':'

            // Parse end (optional: obj[start:])
            let end = if matches!(
                self.current.token_type,
                TokenType::Colon | TokenType::RightBracket
            ) {
                None
            } else {
                Some(Box::new(self.parse_expression()?))
            };

            // Parse step (optional: obj[start:end:step])
            let step = if matches!(self.current.token_type, TokenType::Colon) {
                self.advance(); // consume second ':'
                if matches!(self.current.token_type, TokenType::RightBracket) {
                    None
                } else {
                    Some(Box::new(self.parse_expression()?))
                }
            } else {
                None
            };

            self.expect(TokenType::RightBracket)?;

            Ok(Expr::Slice {
                object: Box::new(object),
                start,
                end,
                step,
            })
        } else {
            // Simple index: obj[index]
            self.expect(TokenType::RightBracket)?;

            Ok(Expr::Index {
                object: Box::new(object),
                index: start.unwrap(), // start must be Some if we didn't see ':'
            })
        }
    }

    fn parse_primary_expression(&mut self) -> Result<Expr> {
        match &self.current.token_type.clone() {
            TokenType::Integer(n) => {
                let n = *n;
                self.advance();
                Ok(Expr::Int(n))
            }
            TokenType::Float(f) => {
                let f = *f;
                self.advance();
                Ok(Expr::Float(f))
            }
            TokenType::String(s) => {
                let s = s.clone();
                self.advance();
                Ok(Expr::String(s))
            }
            TokenType::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            TokenType::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            TokenType::None => {
                self.advance();
                Ok(Expr::None)
            }
            TokenType::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Ok(Expr::Ident(name))
            }
            TokenType::Lambda => self.parse_lambda_expression(),
            TokenType::LeftBracket => self.parse_list_literal(),
            TokenType::LeftBrace => self.parse_dict_literal(),
            TokenType::LeftParen => {
                self.advance();

                // Empty tuple: ()
                if matches!(self.current.token_type, TokenType::RightParen) {
                    self.advance();
                    return Ok(Expr::Tuple(vec![]));
                }

                // Parse first element
                let first = self.parse_expression()?;

                // Check for comma (indicates tuple)
                if matches!(self.current.token_type, TokenType::Comma) {
                    self.advance();

                    let mut elements = vec![first];

                    // Parse remaining elements (allow trailing comma)
                    while !matches!(self.current.token_type, TokenType::RightParen) {
                        elements.push(self.parse_expression()?);

                        if matches!(self.current.token_type, TokenType::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }

                    self.expect(TokenType::RightParen)?;
                    Ok(Expr::Tuple(elements))
                } else {
                    // Just a parenthesized expression
                    self.expect(TokenType::RightParen)?;
                    Ok(first)
                }
            }
            _ => Err(anyhow!(
                "Unexpected token in expression: {:?} at {}:{}",
                self.current.token_type,
                self.current.line,
                self.current.column
            )),
        }
    }

    /// Parse lambda expression: lambda [params]: expr
    /// Examples:
    ///   lambda: None
    ///   lambda x: x + 1
    ///   lambda x, y: x + y
    ///   lambda x, y = 1: x + y
    fn parse_lambda_expression(&mut self) -> Result<Expr> {
        self.expect(TokenType::Lambda)?;

        let mut params = Vec::new();
        let mut varargs_param = None;
        let mut kwargs_param = None;

        // Parse parameters until we hit a colon
        while !matches!(self.current.token_type, TokenType::Colon) {
            if matches!(self.current.token_type, TokenType::DoubleStar) {
                self.advance();
                kwargs_param = Some(self.expect_identifier()?);
                if matches!(self.current.token_type, TokenType::Comma) {
                    self.advance();
                }
                break;
            }

            if matches!(self.current.token_type, TokenType::Star) {
                self.advance();
                if !matches!(self.current.token_type, TokenType::Comma) {
                    varargs_param = Some(self.expect_identifier()?);
                }
                if matches!(self.current.token_type, TokenType::Comma) {
                    self.advance();
                }
                continue;
            }

            // Parse parameter name
            let param_name = self.expect_identifier()?;

            // Check for default value
            let default_value = if matches!(self.current.token_type, TokenType::Assign) {
                self.advance();
                Some(Box::new(self.parse_expression()?))
            } else {
                None
            };

            params.push((param_name, default_value));

            // Expect comma or colon
            if matches!(self.current.token_type, TokenType::Comma) {
                self.advance();
            } else if !matches!(self.current.token_type, TokenType::Colon) {
                return Err(anyhow!(
                    "Expected ',' or ':' in lambda, got {:?} at {}:{}",
                    self.current.token_type,
                    self.current.line,
                    self.current.column
                ));
            }
        }

        self.expect(TokenType::Colon)?;

        // Parse the body expression
        let body = self.parse_expression()?;

        Ok(Expr::Lambda {
            params,
            varargs_param,
            kwargs_param,
            body: Box::new(body),
        })
    }

    fn parse_list_literal(&mut self) -> Result<Expr> {
        let _list_start_line = self.current.line;
        self.expect(TokenType::LeftBracket)?;

        // Empty list
        if matches!(self.current.token_type, TokenType::RightBracket) {
            self.advance();
            return Ok(Expr::List(vec![]));
        }

        // Parse first element
        let first_element = self.parse_expression()?;

        // Skip newlines if inside delimiters (should be depth 1 after opening '[')
        // This is needed for multi-line list comprehensions: [expr\n   for x in items]
        while self.delimiter_depth > 0 && matches!(self.current.token_type, TokenType::Newline) {
            self.advance();
        }

        // Check if this is a list comprehension
        if matches!(self.current.token_type, TokenType::For) {
            return self.parse_list_comprehension(first_element);
        }

        // Regular list literal
        let mut elements = vec![first_element];

        while matches!(self.current.token_type, TokenType::Comma) {
            self.advance();

            // Allow trailing comma
            if matches!(self.current.token_type, TokenType::RightBracket) {
                break;
            }

            elements.push(self.parse_expression()?);
        }

        self.expect(TokenType::RightBracket)?;

        Ok(Expr::List(elements))
    }

    fn parse_list_comprehension(&mut self, element: Expr) -> Result<Expr> {
        // We've already parsed the element expression
        // Current token should be 'for'

        let mut for_clauses = Vec::new();

        // Parse one or more for clauses (supports nested comprehensions)
        while matches!(self.current.token_type, TokenType::For) {
            self.advance(); // consume 'for'

            // Parse loop variables (supports tuple unpacking: for x, y, z in items)
            // Also supports optional parentheses: for (x, y, z) in items
            let has_parens = matches!(self.current.token_type, TokenType::LeftParen);
            if has_parens {
                self.advance(); // consume '('
            }

            let mut vars = vec![self.expect_identifier()?];

            while matches!(self.current.token_type, TokenType::Comma) {
                self.advance(); // consume comma
                vars.push(self.expect_identifier()?);
            }

            if has_parens {
                self.expect(TokenType::RightParen)?;
            }

            // Expect 'in'
            self.expect(TokenType::In)?;

            // Parse iterable expression
            // Use parse_or_expression() instead of parse_expression() to avoid
            // consuming a following 'if' as part of a ternary expression
            let iterable = self.parse_or_expression()?;

            // Check for optional 'if' condition
            let condition = if matches!(self.current.token_type, TokenType::If) {
                self.advance();
                // Also use parse_or_expression() for the condition to avoid nested ternary issues
                Some(self.parse_or_expression()?)
            } else {
                None
            };

            for_clauses.push((vars, Box::new(iterable), condition.map(Box::new)));
        }

        self.expect(TokenType::RightBracket)?;

        Ok(Expr::ListComp {
            element: Box::new(element),
            for_clauses: Box::new(for_clauses),
        })
    }

    fn parse_dict_literal(&mut self) -> Result<Expr> {
        self.expect(TokenType::LeftBrace)?;

        // Empty dict
        if matches!(self.current.token_type, TokenType::RightBrace) {
            self.advance();
            return Ok(Expr::Dict(vec![]));
        }

        // Check for **dict unpacking first
        if matches!(self.current.token_type, TokenType::DoubleStar) {
            self.advance(); // consume **
            let dict_expr = self.parse_expression()?;
            let mut pairs = vec![(None, dict_expr)];

            while matches!(self.current.token_type, TokenType::Comma) {
                self.advance();
                if matches!(self.current.token_type, TokenType::RightBrace) {
                    break;
                }
                if matches!(self.current.token_type, TokenType::DoubleStar) {
                    self.advance();
                    let dict_expr = self.parse_expression()?;
                    pairs.push((None, dict_expr));
                } else {
                    let key = self.parse_expression()?;
                    self.expect(TokenType::Colon)?;
                    let value = self.parse_expression()?;
                    pairs.push((Some(key), value));
                }
            }

            self.expect(TokenType::RightBrace)?;
            return Ok(Expr::Dict(pairs));
        }

        // Parse first key: value pair
        let key = self.parse_expression()?;
        self.expect(TokenType::Colon)?;
        let value = self.parse_expression()?;

        // Check if this is a dict comprehension
        if matches!(self.current.token_type, TokenType::For) {
            return self.parse_dict_comprehension(key, value);
        }

        // Regular dict literal
        let mut pairs = vec![(Some(key), value)];

        while matches!(self.current.token_type, TokenType::Comma) {
            self.advance();

            // Allow trailing comma
            if matches!(self.current.token_type, TokenType::RightBrace) {
                break;
            }

            // Check for **dict unpacking
            if matches!(self.current.token_type, TokenType::DoubleStar) {
                self.advance(); // consume **
                let dict_expr = self.parse_expression()?;
                pairs.push((None, dict_expr)); // None key means unpacking
            } else {
                let key = self.parse_expression()?;
                self.expect(TokenType::Colon)?;
                let value = self.parse_expression()?;
                pairs.push((Some(key), value));
            }
        }

        self.expect(TokenType::RightBrace)?;

        Ok(Expr::Dict(pairs))
    }

    fn parse_dict_comprehension(&mut self, key: Expr, value: Expr) -> Result<Expr> {
        // We've already parsed the key: value
        // Current token should be 'for'

        let mut for_clauses = Vec::new();

        // Parse one or more for clauses (supports nested comprehensions)
        while matches!(self.current.token_type, TokenType::For) {
            self.advance(); // consume 'for'

            // Parse loop variables (supports tuple unpacking: for x, y in items)
            let has_parens = matches!(self.current.token_type, TokenType::LeftParen);
            if has_parens {
                self.advance(); // consume '('
            }

            let mut vars = vec![self.expect_identifier()?];

            while matches!(self.current.token_type, TokenType::Comma) {
                self.advance(); // consume comma
                vars.push(self.expect_identifier()?);
            }

            if has_parens {
                self.expect(TokenType::RightParen)?;
            }

            // Expect 'in'
            self.expect(TokenType::In)?;

            // Parse iterable expression
            // Use parse_or_expression() to avoid consuming 'if' as ternary
            let iterable = self.parse_or_expression()?;

            // Check for optional 'if' condition
            let condition = if matches!(self.current.token_type, TokenType::If) {
                self.advance();
                Some(self.parse_or_expression()?)
            } else {
                None
            };

            for_clauses.push((vars, Box::new(iterable), condition.map(Box::new)));
        }

        self.expect(TokenType::RightBrace)?;

        Ok(Expr::DictComp {
            key: Box::new(key),
            value: Box::new(value),
            for_clauses: Box::new(for_clauses),
        })
    }

    fn match_comparison_op(&self) -> Option<BinOp> {
        // Check for "not in" (two-token operator)
        if matches!(self.current.token_type, TokenType::Not)
            && matches!(self.peek.token_type, TokenType::In)
        {
            return Some(BinOp::NotIn);
        }

        match self.current.token_type {
            TokenType::Equal => Some(BinOp::Eq),
            TokenType::NotEqual => Some(BinOp::NotEq),
            TokenType::Less => Some(BinOp::Lt),
            TokenType::LessEqual => Some(BinOp::LtE),
            TokenType::Greater => Some(BinOp::Gt),
            TokenType::GreaterEqual => Some(BinOp::GtE),
            TokenType::In => Some(BinOp::In),
            _ => None,
        }
    }

    fn expect(&mut self, expected: TokenType) -> Result<()> {
        if std::mem::discriminant(&self.current.token_type) == std::mem::discriminant(&expected) {
            self.advance();
            Ok(())
        } else {
            Err(anyhow!(
                "Expected {:?}, got {:?} at {}:{}",
                expected,
                self.current.token_type,
                self.current.line,
                self.current.column
            ))
        }
    }

    fn expect_identifier(&mut self) -> Result<String> {
        match &self.current.token_type {
            TokenType::Identifier(name) => {
                let name = name.clone();
                self.advance();
                Ok(name)
            }
            _ => Err(anyhow!(
                "Expected identifier, got {:?} at {}:{}",
                self.current.token_type,
                self.current.line,
                self.current.column
            )),
        }
    }

    fn advance(&mut self) {
        // Track the line of the token being consumed
        // This is used to distinguish ternary `if` from statement `if`
        self.last_consumed_line = self.current.line;

        // Track delimiter depth (parens, brackets, braces)
        match &self.current.token_type {
            TokenType::LeftParen | TokenType::LeftBracket | TokenType::LeftBrace => {
                self.delimiter_depth += 1
            }
            TokenType::RightParen | TokenType::RightBracket | TokenType::RightBrace => {
                self.delimiter_depth = self.delimiter_depth.saturating_sub(1)
            }
            _ => {}
        }

        self.current = self.peek.clone();
        self.peek = self.lexer.next_token();

        // Skip INDENT/DEDENT/NEWLINE tokens when inside delimiters
        // This allows multi-line expressions inside brackets, parens, braces
        while self.delimiter_depth > 0
            && matches!(
                self.current.token_type,
                TokenType::Indent | TokenType::Dedent | TokenType::Newline
            )
        {
            self.current = self.peek.clone();
            self.peek = self.lexer.next_token();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(source: &str) -> Result<Program> {
        let lexer = Lexer::new(source);
        let mut parser = Parser::new(lexer);
        parser.parse()
    }

    #[test]
    fn test_assignment() {
        let program = parse("x = 42").unwrap();
        assert_eq!(program.statements.len(), 1);
    }

    #[test]
    fn test_function_def() {
        let source = r#"
def add(a, b):
    return a + b
"#;
        let program = parse(source).unwrap();
        assert_eq!(program.statements.len(), 1);
    }

    #[test]
    fn test_if_statement() {
        let source = r#"
if x > 0:
    y = 1
else:
    y = 0
"#;
        let program = parse(source).unwrap();
        assert_eq!(program.statements.len(), 1);
    }

    #[test]
    fn test_function_call() {
        let program = parse("foo(1, 2, x=3)").unwrap();
        assert_eq!(program.statements.len(), 1);
    }

    #[test]
    fn test_list_literal() {
        let program = parse("[1, 2, 3]").unwrap();
        assert_eq!(program.statements.len(), 1);
    }

    #[test]
    fn test_dict_literal() {
        let program = parse(r#"{"a": 1, "b": 2}"#).unwrap();
        assert_eq!(program.statements.len(), 1);
    }

    #[test]
    fn test_tuple_return() {
        let source = r#"
def foo():
    return a, b
"#;
        let program = parse(source).unwrap();
        assert_eq!(program.statements.len(), 1);

        // Verify it's a function with a return statement
        if let Stmt::FunctionDef { body, .. } = &program.statements[0] {
            assert_eq!(body.len(), 1);
            if let Stmt::Return(Some(expr)) = &body[0] {
                // Should be a tuple expression
                matches!(&**expr, Expr::Tuple(_));
            } else {
                panic!("Expected return statement with tuple");
            }
        } else {
            panic!("Expected function definition");
        }
    }

    #[test]
    fn test_lambda_no_params() {
        // lambda with no parameters
        let program = parse("f = lambda: None").unwrap();
        assert_eq!(program.statements.len(), 1);
        if let Stmt::Assign { value, .. } = &program.statements[0] {
            assert!(matches!(value.as_ref(), Expr::Lambda { params, .. } if params.is_empty()));
        } else {
            panic!("Expected assignment");
        }
    }

    #[test]
    fn test_lambda_single_param() {
        // lambda with single parameter
        let program = parse("f = lambda x: x + 1").unwrap();
        assert_eq!(program.statements.len(), 1);
        if let Stmt::Assign { value, .. } = &program.statements[0] {
            assert!(matches!(value.as_ref(), Expr::Lambda { params, .. } if params.len() == 1));
        } else {
            panic!("Expected assignment");
        }
    }

    #[test]
    fn test_lambda_multiple_params() {
        // lambda with multiple parameters
        let program = parse("f = lambda x, y: x + y").unwrap();
        assert_eq!(program.statements.len(), 1);
        if let Stmt::Assign { value, .. } = &program.statements[0] {
            assert!(matches!(value.as_ref(), Expr::Lambda { params, .. } if params.len() == 2));
        } else {
            panic!("Expected assignment");
        }
    }

    #[test]
    fn test_lambda_with_default() {
        // lambda with default parameter value
        let program = parse("f = lambda x, y = 1: x + y").unwrap();
        assert_eq!(program.statements.len(), 1);
        if let Stmt::Assign { value, .. } = &program.statements[0] {
            if let Expr::Lambda { params, .. } = value.as_ref() {
                assert_eq!(params.len(), 2);
                assert!(params[0].1.is_none()); // x has no default
                assert!(params[1].1.is_some()); // y has default
            } else {
                panic!("Expected lambda");
            }
        } else {
            panic!("Expected assignment");
        }
    }

    #[test]
    fn test_lambda_in_dict() {
        // lambda as dict value (common pattern in Starlark)
        let program = parse(r#"d = {"get": lambda ctx: None}"#).unwrap();
        assert_eq!(program.statements.len(), 1);
    }
}
