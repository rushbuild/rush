// Rush Build - Build Query Language
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build query language for graph introspection and analysis.
//!
//! Provides a powerful query system for analyzing build graphs, finding
//! dependencies, and understanding build structure. Inspired by Bazel query
//! but with enhanced capabilities and better performance.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

/// Build target representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Target label (e.g., "//foo/bar:baz").
    pub label: String,

    /// Target kind (e.g., "rust_library", "py_binary").
    pub kind: String,

    /// Direct dependencies.
    pub deps: Vec<String>,

    /// Target attributes.
    pub attributes: HashMap<String, String>,
}

impl Target {
    /// Create a new target.
    pub fn new(label: String, kind: String) -> Self {
        Target {
            label,
            kind,
            deps: Vec::new(),
            attributes: HashMap::new(),
        }
    }

    /// Add a dependency.
    pub fn add_dep(&mut self, dep: String) {
        if !self.deps.contains(&dep) {
            self.deps.push(dep);
        }
    }

    /// Set an attribute.
    pub fn set_attr(&mut self, key: String, value: String) {
        self.attributes.insert(key, value);
    }
}

/// Query engine for build graphs.
pub struct QueryEngine {
    /// All targets in the build graph.
    targets: HashMap<String, Target>,

    /// Reverse dependency map.
    rdeps: HashMap<String, Vec<String>>,
}

impl QueryEngine {
    /// Create a new query engine.
    pub fn new() -> Self {
        QueryEngine {
            targets: HashMap::new(),
            rdeps: HashMap::new(),
        }
    }

    /// Add a target to the query engine.
    pub fn add_target(&mut self, target: Target) {
        let label = target.label.clone();

        // Update reverse dependencies
        for dep in &target.deps {
            self.rdeps
                .entry(dep.clone())
                .or_default()
                .push(label.clone());
        }

        self.targets.insert(label, target);
    }

    /// Execute a query and return matching targets.
    pub fn query(&self, query: &str) -> Result<Vec<String>> {
        let expr = QueryExpr::parse(query)?;
        self.eval_expr(&expr)
    }

    /// Evaluate a query expression.
    fn eval_expr(&self, expr: &QueryExpr) -> Result<Vec<String>> {
        match expr {
            QueryExpr::Label(label) => {
                if self.targets.contains_key(label) {
                    Ok(vec![label.clone()])
                } else {
                    Ok(Vec::new())
                }
            }

            QueryExpr::Pattern(pattern) => self.pattern_match(pattern),

            QueryExpr::Kind(kind) => {
                let results: Vec<_> = self
                    .targets
                    .values()
                    .filter(|t| &t.kind == kind)
                    .map(|t| t.label.clone())
                    .collect();
                Ok(results)
            }

            QueryExpr::Deps(target_expr, depth) => {
                let targets = self.eval_expr(target_expr)?;
                self.find_deps(&targets, *depth)
            }

            QueryExpr::Rdeps(target_expr, depth) => {
                let targets = self.eval_expr(target_expr)?;
                self.find_rdeps(&targets, *depth)
            }

            QueryExpr::AllPaths(from_expr, to_expr) => {
                let from_targets = self.eval_expr(from_expr)?;
                let to_targets = self.eval_expr(to_expr)?;
                self.find_all_paths(&from_targets, &to_targets)
            }

            QueryExpr::SomePath(from_expr, to_expr) => {
                let from_targets = self.eval_expr(from_expr)?;
                let to_targets = self.eval_expr(to_expr)?;
                self.find_some_path(&from_targets, &to_targets)
            }

            QueryExpr::Attr(attr_name, attr_value) => {
                let results: Vec<_> = self
                    .targets
                    .values()
                    .filter(|t| {
                        t.attributes
                            .get(attr_name)
                            .map(|v| v == attr_value)
                            .unwrap_or(false)
                    })
                    .map(|t| t.label.clone())
                    .collect();
                Ok(results)
            }

            QueryExpr::Intersect(left, right) => {
                let left_results: HashSet<_> = self.eval_expr(left)?.into_iter().collect();
                let right_results: HashSet<_> = self.eval_expr(right)?.into_iter().collect();
                Ok(left_results.intersection(&right_results).cloned().collect())
            }

            QueryExpr::Union(left, right) => {
                let mut results: HashSet<_> = self.eval_expr(left)?.into_iter().collect();
                results.extend(self.eval_expr(right)?);
                Ok(results.into_iter().collect())
            }

            QueryExpr::Except(left, right) => {
                let left_results: HashSet<_> = self.eval_expr(left)?.into_iter().collect();
                let right_results: HashSet<_> = self.eval_expr(right)?.into_iter().collect();
                Ok(left_results.difference(&right_results).cloned().collect())
            }

            QueryExpr::Filter(pattern, inner_expr) => {
                let results = self.eval_expr(inner_expr)?;
                Ok(results
                    .into_iter()
                    .filter(|label| self.matches_pattern(label, pattern))
                    .collect())
            }
        }
    }

    /// Pattern matching for target labels.
    fn pattern_match(&self, pattern: &str) -> Result<Vec<String>> {
        let regex = self.pattern_to_regex(pattern);

        let results: Vec<_> = self
            .targets
            .keys()
            .filter(|label| regex.is_match(label))
            .cloned()
            .collect();

        Ok(results)
    }

    /// Convert glob pattern to regex.
    fn pattern_to_regex(&self, pattern: &str) -> regex::Regex {
        let pattern = pattern
            .replace(".", "\\.")
            .replace("*", ".*")
            .replace("?", ".");

        regex::Regex::new(&format!("^{}$", pattern))
            .unwrap_or_else(|_| regex::Regex::new("^$").unwrap())
    }

    /// Check if label matches pattern.
    fn matches_pattern(&self, label: &str, pattern: &str) -> bool {
        self.pattern_to_regex(pattern).is_match(label)
    }

    /// Find all dependencies up to specified depth.
    fn find_deps(&self, targets: &[String], depth: Option<usize>) -> Result<Vec<String>> {
        let mut result = HashSet::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        for target in targets {
            queue.push_back((target.clone(), 0));
        }

        while let Some((target, current_depth)) = queue.pop_front() {
            if visited.contains(&target) {
                continue;
            }

            if let Some(max_depth) = depth {
                if current_depth >= max_depth {
                    continue;
                }
            }

            visited.insert(target.clone());

            if let Some(t) = self.targets.get(&target) {
                for dep in &t.deps {
                    result.insert(dep.clone());
                    queue.push_back((dep.clone(), current_depth + 1));
                }
            }
        }

        Ok(result.into_iter().collect())
    }

    /// Find all reverse dependencies up to specified depth.
    fn find_rdeps(&self, targets: &[String], depth: Option<usize>) -> Result<Vec<String>> {
        let mut result = HashSet::new();
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();

        for target in targets {
            queue.push_back((target.clone(), 0));
        }

        while let Some((target, current_depth)) = queue.pop_front() {
            if visited.contains(&target) {
                continue;
            }

            if let Some(max_depth) = depth {
                if current_depth >= max_depth {
                    continue;
                }
            }

            visited.insert(target.clone());

            if let Some(rdeps) = self.rdeps.get(&target) {
                for rdep in rdeps {
                    result.insert(rdep.clone());
                    queue.push_back((rdep.clone(), current_depth + 1));
                }
            }
        }

        Ok(result.into_iter().collect())
    }

    /// Find all paths between two sets of targets.
    fn find_all_paths(&self, from: &[String], to: &[String]) -> Result<Vec<String>> {
        let to_set: HashSet<_> = to.iter().cloned().collect();
        let mut result = HashSet::new();

        for start in from {
            self.find_paths_dfs(start, &to_set, &mut HashSet::new(), &mut result);
        }

        Ok(result.into_iter().collect())
    }

    /// DFS to find all paths.
    fn find_paths_dfs(
        &self,
        current: &str,
        targets: &HashSet<String>,
        visited: &mut HashSet<String>,
        result: &mut HashSet<String>,
    ) {
        if visited.contains(current) {
            return;
        }

        visited.insert(current.to_string());
        result.insert(current.to_string());

        if targets.contains(current) {
            return;
        }

        if let Some(target) = self.targets.get(current) {
            for dep in &target.deps {
                self.find_paths_dfs(dep, targets, visited, result);
            }
        }
    }

    /// Find at least one path between two sets of targets.
    fn find_some_path(&self, from: &[String], to: &[String]) -> Result<Vec<String>> {
        let to_set: HashSet<_> = to.iter().cloned().collect();

        for start in from {
            let mut visited = HashSet::new();
            let mut path = Vec::new();

            if self.find_path_dfs(start, &to_set, &mut visited, &mut path) {
                return Ok(path);
            }
        }

        Ok(Vec::new())
    }

    /// DFS to find a single path.
    fn find_path_dfs(
        &self,
        current: &str,
        targets: &HashSet<String>,
        visited: &mut HashSet<String>,
        path: &mut Vec<String>,
    ) -> bool {
        if visited.contains(current) {
            return false;
        }

        visited.insert(current.to_string());
        path.push(current.to_string());

        if targets.contains(current) {
            return true;
        }

        if let Some(target) = self.targets.get(current) {
            for dep in &target.deps {
                if self.find_path_dfs(dep, targets, visited, path) {
                    return true;
                }
            }
        }

        path.pop();
        false
    }

    /// Get all targets.
    pub fn all_targets(&self) -> Vec<String> {
        self.targets.keys().cloned().collect()
    }

    /// Get target count.
    pub fn target_count(&self) -> usize {
        self.targets.len()
    }
}

impl Default for QueryEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Query expression AST.
#[derive(Debug, Clone)]
pub enum QueryExpr {
    /// Exact label match.
    Label(String),

    /// Pattern match (with wildcards).
    Pattern(String),

    /// Match by target kind.
    Kind(String),

    /// Find dependencies (with optional depth limit).
    Deps(Box<QueryExpr>, Option<usize>),

    /// Find reverse dependencies.
    Rdeps(Box<QueryExpr>, Option<usize>),

    /// Find all paths between two sets.
    AllPaths(Box<QueryExpr>, Box<QueryExpr>),

    /// Find some path between two sets.
    SomePath(Box<QueryExpr>, Box<QueryExpr>),

    /// Match by attribute value.
    Attr(String, String),

    /// Intersection of two queries.
    Intersect(Box<QueryExpr>, Box<QueryExpr>),

    /// Union of two queries.
    Union(Box<QueryExpr>, Box<QueryExpr>),

    /// Difference of two queries.
    Except(Box<QueryExpr>, Box<QueryExpr>),

    /// Filter results by pattern.
    Filter(String, Box<QueryExpr>),
}

impl QueryExpr {
    /// Parse a query string into an expression.
    pub fn parse(query: &str) -> Result<Self> {
        QueryParser::new(query).parse()
    }
}

impl fmt::Display for QueryExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryExpr::Label(l) => write!(f, "{}", l),
            QueryExpr::Pattern(p) => write!(f, "pattern({})", p),
            QueryExpr::Kind(k) => write!(f, "kind({})", k),
            QueryExpr::Deps(e, depth) => {
                if let Some(d) = depth {
                    write!(f, "deps({}, {})", e, d)
                } else {
                    write!(f, "deps({})", e)
                }
            }
            QueryExpr::Rdeps(e, depth) => {
                if let Some(d) = depth {
                    write!(f, "rdeps({}, {})", e, d)
                } else {
                    write!(f, "rdeps({})", e)
                }
            }
            QueryExpr::AllPaths(from, to) => write!(f, "allpaths({}, {})", from, to),
            QueryExpr::SomePath(from, to) => write!(f, "somepath({}, {})", from, to),
            QueryExpr::Attr(k, v) => write!(f, "attr({}, {})", k, v),
            QueryExpr::Intersect(l, r) => write!(f, "({} ^ {})", l, r),
            QueryExpr::Union(l, r) => write!(f, "({} + {})", l, r),
            QueryExpr::Except(l, r) => write!(f, "({} - {})", l, r),
            QueryExpr::Filter(p, e) => write!(f, "filter({}, {})", p, e),
        }
    }
}

/// Simple query parser.
struct QueryParser {
    input: String,
    pos: usize,
}

impl QueryParser {
    fn new(input: &str) -> Self {
        QueryParser {
            input: input.to_string(),
            pos: 0,
        }
    }

    fn parse(&mut self) -> Result<QueryExpr> {
        self.skip_whitespace();

        if self.peek_str("deps(") {
            self.parse_deps()
        } else if self.peek_str("rdeps(") {
            self.parse_rdeps()
        } else if self.peek_str("allpaths(") {
            self.parse_allpaths()
        } else if self.peek_str("somepath(") {
            self.parse_somepath()
        } else if self.peek_str("kind(") {
            self.parse_kind()
        } else if self.peek_str("attr(") {
            self.parse_attr()
        } else if self.peek_str("filter(") {
            self.parse_filter()
        } else if self.input[self.pos..].contains('*') || self.input[self.pos..].contains('?') {
            Ok(QueryExpr::Pattern(
                self.input[self.pos..].trim().to_string(),
            ))
        } else {
            Ok(QueryExpr::Label(self.input[self.pos..].trim().to_string()))
        }
    }

    fn parse_deps(&mut self) -> Result<QueryExpr> {
        self.consume("deps(")?;
        let target_expr = Box::new(self.parse_inner()?);

        self.skip_whitespace();
        let depth = if self.peek_char(',') {
            self.consume(",")?;
            self.skip_whitespace();
            Some(self.parse_number()?)
        } else {
            None
        };

        self.consume(")")?;
        Ok(QueryExpr::Deps(target_expr, depth))
    }

    fn parse_rdeps(&mut self) -> Result<QueryExpr> {
        self.consume("rdeps(")?;
        let target_expr = Box::new(self.parse_inner()?);

        self.skip_whitespace();
        let depth = if self.peek_char(',') {
            self.consume(",")?;
            self.skip_whitespace();
            Some(self.parse_number()?)
        } else {
            None
        };

        self.consume(")")?;
        Ok(QueryExpr::Rdeps(target_expr, depth))
    }

    fn parse_allpaths(&mut self) -> Result<QueryExpr> {
        self.consume("allpaths(")?;
        let from = Box::new(self.parse_inner()?);
        self.consume(",")?;
        let to = Box::new(self.parse_inner()?);
        self.consume(")")?;
        Ok(QueryExpr::AllPaths(from, to))
    }

    fn parse_somepath(&mut self) -> Result<QueryExpr> {
        self.consume("somepath(")?;
        let from = Box::new(self.parse_inner()?);
        self.consume(",")?;
        let to = Box::new(self.parse_inner()?);
        self.consume(")")?;
        Ok(QueryExpr::SomePath(from, to))
    }

    fn parse_kind(&mut self) -> Result<QueryExpr> {
        self.consume("kind(")?;
        let kind = self.parse_string()?;
        self.consume(")")?;
        Ok(QueryExpr::Kind(kind))
    }

    fn parse_attr(&mut self) -> Result<QueryExpr> {
        self.consume("attr(")?;
        let key = self.parse_string()?;
        self.consume(",")?;
        let value = self.parse_string()?;
        self.consume(")")?;
        Ok(QueryExpr::Attr(key, value))
    }

    fn parse_filter(&mut self) -> Result<QueryExpr> {
        self.consume("filter(")?;
        let pattern = self.parse_string()?;
        self.consume(",")?;
        let expr = Box::new(self.parse_inner()?);
        self.consume(")")?;
        Ok(QueryExpr::Filter(pattern, expr))
    }

    fn parse_inner(&mut self) -> Result<QueryExpr> {
        self.skip_whitespace();
        let start = self.pos;

        while self.pos < self.input.len() {
            let ch = self.input.chars().nth(self.pos).unwrap();
            if ch == ',' || ch == ')' {
                break;
            }
            self.pos += 1;
        }

        let inner = self.input[start..self.pos].trim();
        QueryExpr::parse(inner)
    }

    fn parse_string(&mut self) -> Result<String> {
        self.skip_whitespace();
        let start = self.pos;

        while self.pos < self.input.len() {
            let ch = self.input.chars().nth(self.pos).unwrap();
            if ch == ',' || ch == ')' {
                break;
            }
            self.pos += 1;
        }

        Ok(self.input[start..self.pos].trim().to_string())
    }

    fn parse_number(&mut self) -> Result<usize> {
        let s = self.parse_string()?;
        s.parse().context("Failed to parse number")
    }

    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() {
            let ch = self.input.chars().nth(self.pos).unwrap();
            if !ch.is_whitespace() {
                break;
            }
            self.pos += 1;
        }
    }

    fn peek_str(&self, s: &str) -> bool {
        self.input[self.pos..].starts_with(s)
    }

    fn peek_char(&self, c: char) -> bool {
        self.input.chars().nth(self.pos) == Some(c)
    }

    fn consume(&mut self, s: &str) -> Result<()> {
        if !self.peek_str(s) {
            anyhow::bail!("Expected '{}' at position {}", s, self.pos);
        }
        self.pos += s.len();
        self.skip_whitespace();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_engine() -> QueryEngine {
        let mut engine = QueryEngine::new();

        // Create a simple dependency graph
        let mut app = Target::new("//app:main".to_string(), "rust_binary".to_string());
        app.add_dep("//lib:core".to_string());
        app.add_dep("//lib:utils".to_string());

        let mut core = Target::new("//lib:core".to_string(), "rust_library".to_string());
        core.add_dep("//lib:base".to_string());

        let mut utils = Target::new("//lib:utils".to_string(), "rust_library".to_string());
        utils.add_dep("//lib:base".to_string());

        let base = Target::new("//lib:base".to_string(), "rust_library".to_string());

        engine.add_target(app);
        engine.add_target(core);
        engine.add_target(utils);
        engine.add_target(base);

        engine
    }

    #[test]
    fn test_label_query() {
        let engine = create_test_engine();
        let results = engine.query("//app:main").unwrap();
        assert_eq!(results, vec!["//app:main"]);
    }

    #[test]
    fn test_kind_query() {
        let engine = create_test_engine();
        let results = engine.query("kind(rust_library)").unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.contains(&"//lib:core".to_string()));
    }

    #[test]
    fn test_deps_query() {
        let engine = create_test_engine();
        let results = engine.query("deps(//app:main)").unwrap();

        // Should include all transitive deps
        assert!(results.contains(&"//lib:core".to_string()));
        assert!(results.contains(&"//lib:utils".to_string()));
        assert!(results.contains(&"//lib:base".to_string()));
    }

    #[test]
    fn test_deps_with_depth() {
        let engine = create_test_engine();
        let results = engine.query("deps(//app:main, 1)").unwrap();

        // Should only include direct deps
        assert_eq!(results.len(), 2);
        assert!(results.contains(&"//lib:core".to_string()));
        assert!(results.contains(&"//lib:utils".to_string()));
    }

    #[test]
    fn test_rdeps_query() {
        let engine = create_test_engine();
        let results = engine.query("rdeps(//lib:base)").unwrap();

        // base is used by core, utils, and transitively by app
        assert!(results.contains(&"//lib:core".to_string()));
        assert!(results.contains(&"//lib:utils".to_string()));
        assert!(results.contains(&"//app:main".to_string()));
    }
}
