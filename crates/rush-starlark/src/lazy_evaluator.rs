//! Lazy .bzl loading: Work-first build orchestration
//!
//! This module implements lazy evaluation of BUILD files and .bzl dependencies,
//! enabling builds to start compilation immediately rather than loading all .bzl
//! files eagerly upfront.
//!
//! ## The Cocktail Party Analogy
//!
//! BUILD files with .bzl loads are like rooms in a mansion during a cocktail party:
//! - Each .bzl file = a room in the house
//! - Each thread = a cocktail server with limited cups (memory)
//! - Goal: Serve drinks efficiently without visiting every room first
//!
//! **Wrong Approach (Eager Loading)**:
//! 1. Walk through entire mansion checking every room
//! 2. Load all 25 rooms worth of inventory (25 GB)
//! 3. THEN start serving drinks
//!
//! **Right Approach (Lazy Loading)**:
//! 1. Start in first room (BUILD file)
//! 2. See what people want (target's source files)
//! 3. Start serving immediately (spawn compiles)
//! 4. Only visit next room when:
//!    - All servers are busy (cores saturated)
//!    - OR we need something from that room (dependency)

use crate::ast::{BindingPattern, Expr, Program, Stmt};
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Evaluation mode for BUILD files
#[derive(Debug, Clone)]
pub enum EvaluatorMode {
    /// Eager: Evaluate all statements including all load() calls upfront
    /// This is the current (WRONG) behavior that loads 25 GB for TensorFlow
    Eager,

    /// Lazy: Only evaluate what's needed for the requested target
    /// - Parse entire AST but don't evaluate yet
    /// - Find target definition in AST
    /// - Extract immediate work (source files to compile)
    /// - Load .bzl files only when:
    ///   a) Target directly uses symbols from them
    ///   b) Dependencies need symbols from them
    ///   c) All cores idle and we need more work
    Lazy {
        /// The target we're trying to build (e.g., "build_pip_package")
        requested_target: String,
    },
}

/// Analysis result from lazy BUILD file parsing
///
/// This struct contains everything we can extract from a BUILD file
/// WITHOUT loading any .bzl files.
#[derive(Debug)]
pub struct LazyBuildAnalysis {
    /// The target we're building
    pub target: String,

    /// Immediate actions we can execute right now
    /// These are source files we can start compiling immediately
    /// Example: cc_binary(srcs=["a.cc", "b.cc"]) → 2 compile actions
    pub immediate_actions: Vec<ImmediateAction>,

    /// Symbols needed from .bzl files (load only when required)
    /// Map: bzl_path → list of symbols needed from that file
    /// Example: {"//build_defs.bzl": ["cc_library", "cc_binary"]}
    pub required_bzl_symbols: HashMap<PathBuf, Vec<String>>,

    /// Dependencies to resolve lazily
    /// Example: deps = [":libfoo", "//third_party:bar"]
    pub pending_deps: Vec<String>,

    /// Raw target definition (for later evaluation if needed)
    pub target_definition: Option<TargetDefinition>,
}

/// An action we can execute immediately without loading .bzl files
#[derive(Debug, Clone)]
pub struct ImmediateAction {
    /// Type of action (compile, link, etc.)
    pub action_type: ActionType,

    /// Input files (source files to compile)
    pub inputs: Vec<PathBuf>,

    /// Expected output file
    pub output: PathBuf,

    /// Additional flags/options extracted from BUILD file
    pub options: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionType {
    /// Compile a single source file
    Compile,
    /// Link object files into binary/library
    Link,
    /// Copy files
    Copy,
    /// Generate files (genrule)
    Generate,
}

/// Raw target definition from BUILD file AST
#[derive(Debug, Clone)]
pub struct TargetDefinition {
    /// Target name (e.g., "build_pip_package")
    pub name: String,

    /// Rule type (e.g., "cc_binary", "py_library")
    pub rule_type: String,

    /// Raw attributes as expressions (not yet evaluated)
    pub attributes: HashMap<String, Expr>,
}

impl LazyBuildAnalysis {
    /// Analyze a BUILD file and extract work for the requested target
    /// WITHOUT loading any .bzl files yet
    ///
    /// This is the entry point for lazy evaluation.
    pub fn analyze(_build_path: &Path, target: &str, ast: &Program) -> Result<Self> {
        let mut analyzer = BuildAnalyzer::new(target.to_string());

        // 1. Find the target definition in the AST
        let target_def = analyzer.find_target_in_ast(ast, target)?;

        // 2. Extract immediate work from the target (srcs, hdrs, etc.)
        let immediate_actions = analyzer.extract_immediate_work(&target_def)?;

        // 3. Note what .bzl symbols are referenced (but don't load yet)
        let required_bzl_symbols = analyzer.extract_required_bzl_symbols(ast)?;

        // 4. Extract dependencies for later lazy resolution
        let pending_deps = analyzer.extract_deps(&target_def)?;

        Ok(Self {
            target: target.to_string(),
            immediate_actions,
            required_bzl_symbols,
            pending_deps,
            target_definition: Some(target_def),
        })
    }
}

/// Internal analyzer for extracting information from BUILD AST
struct BuildAnalyzer {
    #[allow(dead_code)]
    target: String,
    /// Track which .bzl symbols are referenced
    #[allow(dead_code)]
    referenced_symbols: HashSet<String>,
    /// Map symbol to .bzl file it comes from
    symbol_to_bzl: HashMap<String, PathBuf>,
}

impl BuildAnalyzer {
    fn new(target: String) -> Self {
        Self {
            target,
            referenced_symbols: HashSet::new(),
            symbol_to_bzl: HashMap::new(),
        }
    }

    /// Find target definition in BUILD file AST
    ///
    /// Searches for a function call where:
    /// - Function name is any rule (cc_binary, py_library, etc.)
    /// - Has a name="target" argument
    fn find_target_in_ast(&mut self, program: &Program, target: &str) -> Result<TargetDefinition> {
        for stmt in &program.statements {
            if let Some(target_def) = self.check_stmt_for_target(stmt, target)? {
                return Ok(target_def);
            }
        }

        Err(anyhow!("Target '{}' not found in BUILD file", target))
    }

    /// Check if a statement defines our target
    fn check_stmt_for_target(
        &mut self,
        stmt: &Stmt,
        target: &str,
    ) -> Result<Option<TargetDefinition>> {
        match stmt {
            Stmt::Expr(expr) => self.check_expr_for_target(expr, target),
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                // Check both branches
                for s in then_body {
                    if let Some(def) = self.check_stmt_for_target(s, target)? {
                        return Ok(Some(def));
                    }
                }
                if let Some(else_stmts) = else_body {
                    for s in else_stmts {
                        if let Some(def) = self.check_stmt_for_target(s, target)? {
                            return Ok(Some(def));
                        }
                    }
                }
                Ok(None)
            }
            Stmt::For {
                vars,
                iterable,
                body,
            } => {
                // Handle list comprehensions: [rule(...) for var in list]
                // For targets defined in list comprehensions, we need to evaluate
                // the iterable and check each iteration

                // Only handle simple cases: single loop variable
                let BindingPattern::Identifier(loop_var) = vars else {
                    return Ok(None);
                };

                // Evaluate iterable (simple cases: list literals)
                let items = match self.evaluate_simple_list(iterable) {
                    Ok(items) => items,
                    Err(_) => return Ok(None), // Skip complex iterables
                };

                // Check each iteration
                for item in items {
                    // Create a simple binding context for this iteration
                    let mut bindings = HashMap::new();
                    bindings.insert(loop_var.clone(), item);

                    // Check each statement in the loop body
                    for stmt in body {
                        if let Some(def) =
                            self.check_stmt_for_target_with_bindings(stmt, target, &bindings)?
                        {
                            return Ok(Some(def));
                        }
                    }
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Check if an expression is a rule call defining our target
    fn check_expr_for_target(
        &mut self,
        expr: &Expr,
        target: &str,
    ) -> Result<Option<TargetDefinition>> {
        match expr {
            Expr::Call(call) => {
                // Extract rule name (function being called)
                let rule_type = match call.function.as_ref() {
                    Expr::Ident(name) => name.clone(),
                    _ => return Ok(None),
                };

                // Check if this call has name="our_target"
                let mut target_name = None;
                let mut raw_attributes = HashMap::new();

                for (key, value) in &call.kwargs {
                    if key == "name" {
                        if let Expr::String(name) = value {
                            target_name = Some(name.clone());
                        }
                    }
                    raw_attributes.insert(key.clone(), value.clone());
                }

                // Is this our target?
                if target_name.as_deref() == Some(target) {
                    // Evaluate attribute expressions, using the target's own name for self-references
                    let mut attributes = HashMap::new();
                    let mut self_bindings = HashMap::new();
                    self_bindings.insert("name".to_string(), target.to_string());

                    for (key, value) in raw_attributes {
                        // Try to evaluate the expression with self-bindings
                        let evaluated_expr = if let Some(evaluated) =
                            self.evaluate_expr_with_bindings(&value, &self_bindings)
                        {
                            // Convert evaluated string back to Expr::String
                            Expr::String(evaluated)
                        } else {
                            // Keep original expression if we can't evaluate it
                            value
                        };
                        attributes.insert(key, evaluated_expr);
                    }

                    return Ok(Some(TargetDefinition {
                        name: target.to_string(),
                        rule_type,
                        attributes,
                    }));
                }

                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Extract immediate work (compile actions) from target definition
    ///
    /// For cc_binary(srcs=["a.cc", "b.cc"]), we can immediately queue:
    /// - g++ -c a.cc -o a.o
    /// - g++ -c b.cc -o b.o
    fn extract_immediate_work(
        &self,
        target_def: &TargetDefinition,
    ) -> Result<Vec<ImmediateAction>> {
        let mut actions = Vec::new();

        // Look for "srcs" attribute
        if let Some(srcs_expr) = target_def.attributes.get("srcs") {
            let source_files = self.extract_file_list(srcs_expr)?;

            // For each source file, create a compile action
            for src in source_files {
                // Determine output path (e.g., "foo.cc" → "foo.o")
                let output = src.with_extension("o");

                actions.push(ImmediateAction {
                    action_type: ActionType::Compile,
                    inputs: vec![src],
                    output,
                    options: HashMap::new(),
                });
            }
        }

        Ok(actions)
    }

    /// Extract list of files from an expression (e.g., ["a.cc", "b.cc"])
    fn extract_file_list(&self, expr: &Expr) -> Result<Vec<PathBuf>> {
        match expr {
            Expr::List(items) => {
                let mut files = Vec::new();
                for item in items {
                    if let Expr::String(s) = item {
                        files.push(PathBuf::from(s));
                    }
                }
                Ok(files)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Extract which .bzl symbols are referenced in the BUILD file
    ///
    /// Scans load() statements to build a map of symbol → .bzl file
    fn extract_required_bzl_symbols(
        &mut self,
        program: &Program,
    ) -> Result<HashMap<PathBuf, Vec<String>>> {
        let mut bzl_to_symbols: HashMap<PathBuf, Vec<String>> = HashMap::new();

        for stmt in &program.statements {
            if let Stmt::Load { file, symbols } = stmt {
                // file is already a String
                let bzl_path = PathBuf::from(file);

                // Record which symbols come from this .bzl file
                let symbol_list: Vec<String> = symbols
                    .iter()
                    .map(|(local_name, _original_name)| local_name.clone())
                    .collect();

                // Build reverse mapping (symbol → bzl file)
                for symbol in &symbol_list {
                    self.symbol_to_bzl.insert(symbol.clone(), bzl_path.clone());
                }

                bzl_to_symbols.insert(bzl_path, symbol_list);
            }
        }

        Ok(bzl_to_symbols)
    }

    /// Extract dependencies from target definition
    fn extract_deps(&self, target_def: &TargetDefinition) -> Result<Vec<String>> {
        if let Some(Expr::List(items)) = target_def.attributes.get("deps") {
            let mut deps = Vec::new();
            for item in items {
                if let Expr::String(s) = item {
                    deps.push(s.clone());
                }
            }
            return Ok(deps);
        }

        Ok(Vec::new())
    }

    /// Evaluate simple list literals for loop iteration
    /// Only handles: [Expr::String("a"), Expr::String("b"), ...]
    fn evaluate_simple_list(&self, expr: &Expr) -> Result<Vec<String>> {
        match expr {
            Expr::List(items) => {
                let mut result = Vec::new();
                for item in items {
                    match item {
                        Expr::String(s) => result.push(s.clone()),
                        _ => return Err(anyhow!("Complex list item not supported")),
                    }
                }
                Ok(result)
            }
            _ => Err(anyhow!("Only list literals supported as iterables")),
        }
    }

    /// Check if a statement defines our target, with variable bindings
    fn check_stmt_for_target_with_bindings(
        &mut self,
        stmt: &Stmt,
        target: &str,
        bindings: &HashMap<String, String>,
    ) -> Result<Option<TargetDefinition>> {
        match stmt {
            Stmt::Expr(expr) => self.check_expr_for_target_with_bindings(expr, target, bindings),
            _ => Ok(None),
        }
    }

    /// Check if an expression is a rule call defining our target, with variable bindings
    fn check_expr_for_target_with_bindings(
        &mut self,
        expr: &Expr,
        target: &str,
        bindings: &HashMap<String, String>,
    ) -> Result<Option<TargetDefinition>> {
        match expr {
            Expr::Call(call) => {
                // Extract rule name (function being called)
                let rule_type = match call.function.as_ref() {
                    Expr::Ident(name) => name.clone(),
                    _ => return Ok(None),
                };

                // Check if this call has name="our_target"
                let mut target_name = None;
                let mut raw_attributes = HashMap::new();

                for (key, value) in &call.kwargs {
                    if key == "name" {
                        // Evaluate the name expression with bindings
                        if let Some(evaluated_name) =
                            self.evaluate_expr_with_bindings(value, bindings)
                        {
                            target_name = Some(evaluated_name);
                        }
                    }
                    raw_attributes.insert(key.clone(), value.clone());
                }

                // Is this our target?
                if target_name.as_deref() == Some(target) {
                    // Evaluate all attributes with current bindings + self-reference
                    let mut all_bindings = bindings.clone();
                    all_bindings.insert("name".to_string(), target.to_string());

                    let mut attributes = HashMap::new();
                    for (key, value) in raw_attributes {
                        // Try to evaluate the expression
                        let evaluated_expr = if let Some(evaluated) =
                            self.evaluate_expr_with_bindings(&value, &all_bindings)
                        {
                            Expr::String(evaluated)
                        } else {
                            value
                        };
                        attributes.insert(key, evaluated_expr);
                    }

                    return Ok(Some(TargetDefinition {
                        name: target.to_string(),
                        rule_type,
                        attributes,
                    }));
                }

                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// Evaluate a simple expression with variable bindings
    /// Handles: String literals, Ident (variable lookup), BinOp::Mod (Python % operator)
    fn evaluate_expr_with_bindings(
        &self,
        expr: &Expr,
        bindings: &HashMap<String, String>,
    ) -> Option<String> {
        match expr {
            Expr::String(s) => Some(s.clone()),
            Expr::Ident(name) => bindings.get(name).cloned(),
            Expr::BinOp { left, op, right } => {
                // Handle Python % operator: "format %s" % (var,)
                if let crate::ast::BinOp::Mod = op {
                    if let Expr::String(format_str) = left.as_ref() {
                        // Evaluate the right side
                        let value = match right.as_ref() {
                            // Direct variable: "%s" % var
                            Expr::Ident(name) => bindings.get(name).cloned(),
                            // Tuple with one element: "%s" % (var,)
                            Expr::Tuple(items) if items.len() == 1 => {
                                if let Expr::Ident(name) = &items[0] {
                                    bindings.get(name).cloned()
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };

                        if let Some(val) = value {
                            // Simple %s replacement
                            return Some(format_str.replace("%s", &val));
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;

    #[test]
    fn test_find_simple_target() {
        let source = r#"
cc_binary(
    name = "hello",
    srcs = ["hello.cc"],
)
"#;

        let lexer = crate::lexer::Lexer::new(source);
        let mut parser = Parser::new(lexer);
        let program = parser.parse().unwrap();

        let analysis = LazyBuildAnalysis::analyze(Path::new("BUILD"), "hello", &program).unwrap();

        assert_eq!(analysis.target, "hello");
        assert_eq!(analysis.immediate_actions.len(), 1);
        assert_eq!(
            analysis.immediate_actions[0].action_type,
            ActionType::Compile
        );
    }

    #[test]
    fn test_extract_bzl_symbols() {
        let source = r#"
load("//build_defs.bzl", "cc_library", "cc_binary")
load("//third_party.bzl", "external_dep")

cc_binary(
    name = "app",
    srcs = ["app.cc"],
)
"#;

        let lexer = crate::lexer::Lexer::new(source);
        let mut parser = Parser::new(lexer);
        let program = parser.parse().unwrap();

        let analysis = LazyBuildAnalysis::analyze(Path::new("BUILD"), "app", &program).unwrap();

        assert_eq!(analysis.required_bzl_symbols.len(), 2);
        assert!(analysis
            .required_bzl_symbols
            .contains_key(&PathBuf::from("//build_defs.bzl")));
    }
}
