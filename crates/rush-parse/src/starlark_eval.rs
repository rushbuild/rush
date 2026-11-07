// Starlark evaluation for BUILD files
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{BuildFile, Rule, Value};

// Type alias for complex list comprehension parse result
type ListCompResult = Option<(usize, String, String, String, String)>;

// Lazy-compiled regexes for performance
static RULE_PATTERN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)^([a-z_]+)\s*\(\s*name\s*=\s*"([^"]+)""#).unwrap());

static MACRO_PATTERN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)^def\s+([a-z_]+)\s*\(([^)]*)\):"#).unwrap());

static LOAD_PATTERN_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?m)^load\s*\(\s*"([^"]+)""#).unwrap());

static MACRO_RULE_PATTERN_RE: Lazy<Regex> = Lazy::new(|| {
    // Match rules in macro bodies where name can be a parameter or string literal
    Regex::new(r#"(?m)([a-z_]+)\s*\(\s*(?:name\s*=\s*([a-z_]+|"[^"]+")|$)"#).unwrap()
});

/// Starlark evaluator for BUILD files
pub struct StarlarkEvaluator {
    /// Workspace root directory
    workspace_root: PathBuf,

    /// Cache of parsed BUILD files (bounded LRU to prevent OOM)
    build_cache: Arc<Mutex<lru::LruCache<PathBuf, Arc<BuildFile>>>>,

    /// Cache of loaded .bzl modules (bounded LRU to prevent OOM)
    bzl_cache: Arc<Mutex<lru::LruCache<PathBuf, HashMap<String, MacroDefinition>>>>,

    /// Current BUILD file being parsed (for glob expansion)
    current_build_dir: std::cell::RefCell<Option<PathBuf>>,
}

/// Represents a macro definition from a .bzl file
#[derive(Debug, Clone)]
pub struct MacroDefinition {
    /// Name of the macro
    pub name: String,

    /// Parameters the macro accepts
    pub params: Vec<String>,

    /// Macro body (for now, simplified)
    pub body: String,
}

impl StarlarkEvaluator {
    /// Create a new Starlark evaluator
    pub fn new(workspace_root: PathBuf) -> Self {
        // Default: 200 BUILD files + 100 .bzl files (reasonable for most projects)
        // Large projects like TensorFlow will benefit from LRU eviction
        let build_capacity = NonZeroUsize::new(200).unwrap();
        let bzl_capacity = NonZeroUsize::new(100).unwrap();

        StarlarkEvaluator {
            workspace_root,
            build_cache: Arc::new(Mutex::new(lru::LruCache::new(build_capacity))),
            bzl_cache: Arc::new(Mutex::new(lru::LruCache::new(bzl_capacity))),
            current_build_dir: std::cell::RefCell::new(None),
        }
    }

    /// Parse and evaluate a BUILD file
    pub fn eval_build_file(&self, build_path: &Path) -> Result<Arc<BuildFile>> {
        // Check cache first (LRU cache)
        {
            let mut cache = self.build_cache.lock();
            if let Some(cached) = cache.get(&build_path.to_path_buf()) {
                return Ok(Arc::clone(cached));
            }
        }

        let content = std::fs::read_to_string(build_path)
            .with_context(|| format!("Failed to read BUILD file: {}", build_path.display()))?;

        // Set current BUILD directory for glob() expansion
        if let Some(parent) = build_path.parent() {
            *self.current_build_dir.borrow_mut() = Some(parent.to_path_buf());
        }

        // Parse BUILD file content
        let rules = self.parse_build_content(build_path, &content)?;

        // Clear current BUILD directory
        *self.current_build_dir.borrow_mut() = None;

        crate::validate_unique_rule_names(&rules, build_path)?;

        let build_file = Arc::new(BuildFile {
            path: build_path.to_path_buf(),
            rules,
        });

        // Cache the result (LRU may evict old entries)
        {
            let mut cache = self.build_cache.lock();
            cache.put(build_path.to_path_buf(), Arc::clone(&build_file));
        }

        Ok(build_file)
    }

    /// Parse BUILD file content and extract rules
    fn parse_build_content(&self, build_path: &Path, content: &str) -> Result<Vec<Rule>> {
        // First, process load() statements
        self.process_load_statements(build_path, content)?;

        let mut rules = Vec::new();

        // Process list comprehensions first
        let list_comp_rules = self.process_list_comprehensions(content)?;
        rules.extend(list_comp_rules);

        // Simple regex-based parsing for now
        // This will handle most common BUILD patterns

        // Match cc_library(...), rust_library(...), etc.
        for cap in RULE_PATTERN_RE.captures_iter(content) {
            let rule_type_str = cap.get(1).unwrap().as_str();
            let name_str = cap.get(2).unwrap().as_str();

            // Extract the full rule body
            if let Some(rule_body) = self.extract_rule_body(content, cap.get(0).unwrap().start()) {
                let attributes = self.parse_rule_attributes(&rule_body)?;

                // Check if this is a macro call
                if let Some(macro_def) = self.find_macro(rule_type_str) {
                    // Expand macro into rules
                    let expanded = self.expand_macro(&macro_def, name_str, &attributes)?;
                    rules.extend(expanded);
                } else {
                    // Regular rule
                    let mut rule = Rule {
                        name: name_str.to_string(),
                        rule_type: rule_type_str.to_string(),
                        attributes,
                    };

                    // Translate Envoy macros to standard rules
                    rule = crate::envoy_compat::translate_envoy_macro(&rule);

                    rules.push(rule);
                }
            }
        }

        Ok(rules)
    }

    /// Process list comprehensions and expand them into rules
    fn process_list_comprehensions(&self, content: &str) -> Result<Vec<Rule>> {
        let mut rules = Vec::new();

        // Find list comprehensions by looking for patterns like "[ ... for ... in ... ]"
        let mut pos = 0;
        let content_chars: Vec<char> = content.chars().collect();

        while pos < content_chars.len() {
            // Skip to next '['
            while pos < content_chars.len() && content_chars[pos] != '[' {
                pos += 1;
            }

            if pos >= content_chars.len() {
                break;
            }

            // Try to parse as list comprehension
            if let Some((comp_end, rule_type, loop_var, iterable, template)) =
                self.try_parse_list_comp(content, pos)?
            {
                // Evaluate the iterable
                let items = self.evaluate_iterable(&iterable)?;

                // Generate rules for each item
                for item in items {
                    let expanded =
                        self.expand_list_comp_template(&template, &loop_var, &item, &rule_type)?;
                    rules.push(expanded);
                }

                pos = comp_end + 1;
            } else {
                pos += 1;
            }
        }

        Ok(rules)
    }

    /// Try to parse a list comprehension starting at position pos
    fn try_parse_list_comp(&self, content: &str, start: usize) -> Result<ListCompResult> {
        let chars: Vec<char> = content.chars().collect();
        if chars[start] != '[' {
            return Ok(None);
        }

        let mut pos = start + 1;

        // Skip whitespace
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }

        // Look for a rule name (letters and underscore)
        let rule_start = pos;
        while pos < chars.len() && (chars[pos].is_alphanumeric() || chars[pos] == '_') {
            pos += 1;
        }

        if pos == rule_start {
            return Ok(None);
        }

        let rule_type: String = chars[rule_start..pos].iter().collect();

        // Skip whitespace
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }

        // Should see '(' for rule call
        if pos >= chars.len() || chars[pos] != '(' {
            return Ok(None);
        }

        // Extract the rule template (everything until matching ')')
        let template_start = pos;
        let mut depth = 0;
        while pos < chars.len() {
            if chars[pos] == '(' {
                depth += 1;
            } else if chars[pos] == ')' {
                depth -= 1;
                if depth == 0 {
                    pos += 1;
                    break;
                }
            }
            pos += 1;
        }

        let template: String = chars[template_start..pos].iter().collect();

        // Skip whitespace
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }

        // Look for "for"
        if pos + 3 > chars.len() || &chars[pos..pos + 3].iter().collect::<String>() != "for" {
            return Ok(None);
        }
        pos += 3;

        // Skip whitespace
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }

        // Get loop variable
        let var_start = pos;
        while pos < chars.len() && (chars[pos].is_alphanumeric() || chars[pos] == '_') {
            pos += 1;
        }

        let loop_var: String = chars[var_start..pos].iter().collect();

        // Skip whitespace
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }

        // Look for "in"
        if pos + 2 > chars.len() || &chars[pos..pos + 2].iter().collect::<String>() != "in" {
            return Ok(None);
        }
        pos += 2;

        // Skip whitespace
        while pos < chars.len() && chars[pos].is_whitespace() {
            pos += 1;
        }

        // Get iterable (everything until ']')
        let iter_start = pos;
        let mut bracket_depth = 0;
        while pos < chars.len() {
            if chars[pos] == '[' {
                bracket_depth += 1;
            } else if chars[pos] == ']' {
                if bracket_depth == 0 {
                    break;
                }
                bracket_depth -= 1;
            }
            pos += 1;
        }

        let iterable: String = chars[iter_start..pos].iter().collect();

        if pos >= chars.len() || chars[pos] != ']' {
            return Ok(None);
        }

        Ok(Some((pos, rule_type, loop_var, iterable, template)))
    }

    /// Evaluate an iterable expression
    fn evaluate_iterable(&self, expr: &str) -> Result<Vec<String>> {
        let expr = expr.trim();

        // Handle simple list literals: ["a", "b", "c"]
        if expr.starts_with('[') && expr.ends_with(']') {
            let inner = &expr[1..expr.len() - 1];
            return Ok(inner
                .split(',')
                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                .filter(|s| !s.is_empty())
                .collect());
        }

        // Handle dict.keys() pattern
        if expr.ends_with(".keys()") {
            // For now, return empty list - would need to evaluate the dict
            // This requires evaluating variables defined earlier in the file
            return Ok(Vec::new());
        }

        // Handle range() function
        if expr.starts_with("range(") {
            // For now, return empty list - would need to parse range args
            return Ok(Vec::new());
        }

        // Unknown iterable - return empty for now
        Ok(Vec::new())
    }

    /// Expand a list comprehension template with a specific item value
    fn expand_list_comp_template(
        &self,
        template: &str,
        loop_var: &str,
        item_value: &str,
        rule_type: &str,
    ) -> Result<Rule> {
        // Substitute the loop variable in the template
        let mut expanded = template.to_string();

        // Replace "{...}".format(var) patterns with the expanded value
        // Match patterns like "{}_target".format(item) or "{}".format(item)
        let format_pattern = format!(
            r#""([^"]*)?\{{}}([^"]*)".format\({}\)"#,
            regex::escape(loop_var)
        );
        let format_re = Regex::new(&format_pattern).unwrap();

        expanded = format_re
            .replace_all(&expanded, |caps: &regex::Captures| {
                let prefix = caps.get(1).map_or("", |m| m.as_str());
                let suffix = caps.get(2).map_or("", |m| m.as_str());
                format!(r#""{}{}{}""#, prefix, item_value, suffix)
            })
            .to_string();

        // Also handle {var} patterns inside strings (for manual replacement if needed)
        let var_in_braces = format!(r#"\{{{}\}}"#, regex::escape(loop_var));
        let var_re = Regex::new(&var_in_braces).unwrap();
        expanded = var_re.replace_all(&expanded, item_value).to_string();

        // Handle Python % operator patterns: "%s" % (var,) or "%s" % var
        // Match patterns like "prefix_%s_suffix" % (item,) or "text %s" % item
        let percent_tuple_pattern = format!(
            r#""([^"]*%s[^"]*)" % \(\s*{}\s*,?\s*\)"#,
            regex::escape(loop_var)
        );
        let percent_tuple_re = Regex::new(&percent_tuple_pattern).unwrap();

        expanded = percent_tuple_re
            .replace_all(&expanded, |caps: &regex::Captures| {
                let template_str = caps.get(1).map_or("", |m| m.as_str());
                // Replace %s with the actual value
                let result = template_str.replace("%s", item_value);
                format!(r#""{}""#, result)
            })
            .to_string();

        // Also handle direct form (without parentheses): "text %s" % var
        let percent_direct_pattern = format!(r#""([^"]*%s[^"]*)" % {}"#, regex::escape(loop_var));
        let percent_direct_re = Regex::new(&percent_direct_pattern).unwrap();

        expanded = percent_direct_re
            .replace_all(&expanded, |caps: &regex::Captures| {
                let template_str = caps.get(1).map_or("", |m| m.as_str());
                // Replace %s with the actual value
                let result = template_str.replace("%s", item_value);
                format!(r#""{}""#, result)
            })
            .to_string();

        // Parse the expanded template
        let attributes = self.parse_rule_attributes(&expanded)?;

        // Extract name from attributes
        let name = if let Some(Value::String(n)) = attributes.get("name") {
            n.clone()
        } else {
            item_value.to_string()
        };

        Ok(Rule {
            name,
            rule_type: rule_type.to_string(),
            attributes,
        })
    }

    /// Find a macro definition by name
    fn find_macro(&self, name: &str) -> Option<MacroDefinition> {
        let cache = self.bzl_cache.lock();
        // LRU cache doesn't support iter(), so we need to iterate using iter()
        for (_path, macros) in cache.iter() {
            if let Some(macro_def) = macros.get(name) {
                return Some(macro_def.clone());
            }
        }
        None
    }

    /// Expand a macro call into rules
    fn expand_macro(
        &self,
        macro_def: &MacroDefinition,
        name: &str,
        attributes: &HashMap<String, Value>,
    ) -> Result<Vec<Rule>> {
        // Substitute macro parameters in the body
        let mut expanded_body = macro_def.body.clone();

        // Build a parameter substitution map
        let mut param_map = HashMap::new();
        param_map.insert("name".to_string(), format!("\"{}\"", name));

        // Add other attributes as parameters
        for (key, value) in attributes {
            if let Value::String(s) = value {
                param_map.insert(key.clone(), format!("\"{}\"", s));
            } else if let Value::List(items) = value {
                let list_str = items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => format!("\"{}\"", s),
                        _ => "".to_string(),
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                param_map.insert(key.clone(), format!("[{}]", list_str));
            }
        }

        // Perform parameter substitution
        for (param_name, param_value) in &param_map {
            // Replace parameter references only on the right side of assignments
            // Match patterns like "key = param_name" but not "param_name = value"
            let pattern = format!(r"=\s*{}\b", regex::escape(param_name));
            let replacement = format!("= {}", param_value);
            let re = Regex::new(&pattern).unwrap();
            expanded_body = re
                .replace_all(&expanded_body, replacement.as_str())
                .to_string();
        }

        let mut rules = Vec::new();

        // Look for rule calls in the expanded body
        for cap in MACRO_RULE_PATTERN_RE.captures_iter(&expanded_body) {
            let rule_type_str = cap.get(1).unwrap().as_str();

            // Extract rule body from macro
            if let Some(rule_body) =
                self.extract_rule_body(&expanded_body, cap.get(0).unwrap().start())
            {
                // Parse attributes from the expanded rule
                let macro_attributes = self.parse_rule_attributes(&rule_body)?;

                // Get the name from the attributes
                let rule_name = if let Some(Value::String(n)) = macro_attributes.get("name") {
                    n.clone()
                } else {
                    name.to_string()
                };

                let rule = Rule {
                    name: rule_name,
                    rule_type: rule_type_str.to_string(),
                    attributes: macro_attributes,
                };

                rules.push(rule);
            }
        }

        // If no rules found in macro body, create a placeholder rule
        if rules.is_empty() {
            rules.push(Rule {
                name: name.to_string(),
                rule_type: "macro_expanded".to_string(),
                attributes: attributes.clone(),
            });
        }

        Ok(rules)
    }

    /// Process load() statements and load .bzl files
    fn process_load_statements(&self, build_path: &Path, content: &str) -> Result<()> {
        for cap in LOAD_PATTERN_RE.captures_iter(content) {
            let bzl_path_str = cap.get(1).unwrap().as_str();

            // Skip external repository loads (start with @) for now
            // These would need to be resolved through external_deps/MODULE.bazel
            if bzl_path_str.starts_with('@') {
                continue;
            }

            // Resolve the .bzl file path
            let bzl_path = self.resolve_bzl_path(build_path, bzl_path_str)?;

            // Load the .bzl file if not already cached
            let needs_load = {
                let cache = self.bzl_cache.lock();
                !cache.contains(&bzl_path)
            };

            if needs_load {
                // Try to load the .bzl file, but don't fail if it doesn't exist
                match self.load_bzl_file(&bzl_path) {
                    Ok(macros) => {
                        let mut cache = self.bzl_cache.lock();
                        cache.put(bzl_path.clone(), macros);
                    }
                    Err(_) => {
                        // Skip .bzl files that can't be loaded
                        // Insert empty map to avoid retrying
                        let mut cache = self.bzl_cache.lock();
                        cache.put(bzl_path.clone(), HashMap::new());
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve a .bzl file path from a load() statement
    fn resolve_bzl_path(&self, build_path: &Path, bzl_path_str: &str) -> Result<PathBuf> {
        if let Some(stripped) = bzl_path_str.strip_prefix("//") {
            // Workspace-relative path
            let rel_path = stripped;
            // Replace : with / for package-relative files
            let rel_path = rel_path.replace(':', "/");
            // Strip leading / if present (e.g., //:file.bzl becomes /file.bzl after replace)
            let rel_path = rel_path.trim_start_matches('/');
            Ok(self.workspace_root.join(rel_path))
        } else if let Some(rel_path) = bzl_path_str.strip_prefix(':') {
            // Package-relative path
            let package_dir = build_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("BUILD file has no parent directory"))?;
            Ok(package_dir.join(rel_path))
        } else {
            // Relative path
            let package_dir = build_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("BUILD file has no parent directory"))?;
            Ok(package_dir.join(bzl_path_str))
        }
    }

    /// Extract rule body from BUILD content
    fn extract_rule_body(&self, content: &str, start_pos: usize) -> Option<String> {
        let remaining = &content[start_pos..];

        // Find matching parentheses
        let mut depth = 0;
        let mut body = String::new();
        let mut started = false;

        for ch in remaining.chars() {
            if ch == '(' {
                depth += 1;
                started = true;
            } else if ch == ')' {
                depth -= 1;
                if depth == 0 && started {
                    body.push(ch);
                    break;
                }
            }
            if started {
                body.push(ch);
            }
        }

        if depth == 0 && !body.is_empty() {
            Some(body)
        } else {
            None
        }
    }

    /// Parse rule attributes from rule body
    fn parse_rule_attributes(&self, body: &str) -> Result<HashMap<String, Value>> {
        let mut attributes = HashMap::new();

        // Parse key = value pairs with proper bracket matching
        let chars = body.chars().peekable();
        let mut current_key = String::new();
        let mut current_value = String::new();
        let mut in_key = true;
        let mut paren_depth = 0;
        let mut bracket_depth = 0;
        let mut in_string = false;
        let mut string_char = '\0';

        for ch in chars {
            if in_string {
                current_value.push(ch);
                if ch == string_char && current_value.chars().rev().nth(1) != Some('\\') {
                    in_string = false;
                }
                continue;
            }

            match ch {
                '"' | '\'' => {
                    current_value.push(ch);
                    in_string = true;
                    string_char = ch;
                }
                '=' if in_key && paren_depth == 0 && bracket_depth == 0 => {
                    in_key = false;
                }
                '(' => {
                    if !in_key {
                        current_value.push(ch);
                        paren_depth += 1;
                    }
                }
                ')' => {
                    if !in_key && paren_depth > 0 {
                        current_value.push(ch);
                        paren_depth -= 1;
                    }
                }
                '[' => {
                    if !in_key {
                        current_value.push(ch);
                        bracket_depth += 1;
                    }
                }
                ']' => {
                    if !in_key && bracket_depth > 0 {
                        current_value.push(ch);
                        bracket_depth -= 1;
                    }
                }
                ',' if paren_depth == 0 && bracket_depth == 0 && !in_key => {
                    // End of attribute
                    let key = current_key.trim().to_string();
                    let value_str = current_value.trim();
                    if !key.is_empty() && !value_str.is_empty() {
                        let value = self.parse_value(value_str)?;
                        attributes.insert(key, value);
                    }
                    current_key.clear();
                    current_value.clear();
                    in_key = true;
                }
                '\n' if paren_depth == 0 && bracket_depth == 0 && !in_key => {
                    // End of attribute (newline)
                    let key = current_key.trim().to_string();
                    let value_str = current_value.trim();
                    if !key.is_empty() && !value_str.is_empty() {
                        let value = self.parse_value(value_str)?;
                        attributes.insert(key, value);
                    }
                    current_key.clear();
                    current_value.clear();
                    in_key = true;
                }
                _ => {
                    if in_key {
                        if ch.is_alphanumeric() || ch == '_' {
                            current_key.push(ch);
                        }
                    } else {
                        current_value.push(ch);
                    }
                }
            }
        }

        // Handle last attribute
        if !current_key.is_empty() && !current_value.is_empty() {
            let key = current_key.trim().to_string();
            let value_str = current_value.trim();
            let value = self.parse_value(value_str)?;
            attributes.insert(key, value);
        }

        Ok(attributes)
    }

    /// Parse a value (string, list, bool)
    fn parse_value(&self, value_str: &str) -> Result<Value> {
        let trimmed = value_str.trim();

        if trimmed == "True" || trimmed == "true" {
            return Ok(Value::Bool(true));
        } else if trimmed == "False" || trimmed == "false" {
            return Ok(Value::Bool(false));
        }

        // Check for select() function call
        if trimmed.starts_with("select(") && trimmed.ends_with(')') {
            return self.parse_select(trimmed);
        }

        // Check for glob() function call
        if trimmed.starts_with("glob(") && trimmed.ends_with(')') {
            return self.expand_glob(trimmed);
        }

        // Check for list
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let list_content = &trimmed[1..trimmed.len() - 1];
            let items: Vec<Value> = list_content
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| {
                    // Remove quotes if present
                    let cleaned = s.trim_matches('"').trim_matches('\'');
                    Value::String(cleaned.to_string())
                })
                .collect();
            return Ok(Value::List(items));
        }

        // Default to string (remove quotes)
        let cleaned = trimmed.trim_matches('"').trim_matches('\'').to_string();
        Ok(Value::String(cleaned))
    }

    /// Parse select() expression
    fn parse_select(&self, select_expr: &str) -> Result<Value> {
        // Extract the dictionary from select({...})
        let inner = &select_expr[7..select_expr.len() - 1]; // Remove "select(" and ")"

        let trimmed = inner.trim();
        if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
            // For import compatibility, if we can't parse select(), return empty list
            // This allows BUILD files to import even if select() is complex
            eprintln!("   ⚠️  Warning: Could not parse select() expression, using empty default");
            return Ok(Value::List(vec![]));
        }

        let dict_content = &trimmed[1..trimmed.len() - 1];

        // Parse key: value pairs
        let mut conditions = HashMap::new();
        let chars = dict_content.chars().peekable();
        let mut current_key = String::new();
        let mut current_value = String::new();
        let mut in_key = true;
        let mut in_string = false;
        let mut string_char = '\0';
        let mut bracket_depth = 0;
        let mut brace_depth = 0;

        for ch in chars {
            if in_string {
                if in_key {
                    current_key.push(ch);
                } else {
                    current_value.push(ch);
                }
                if ch == string_char
                    && current_key.chars().rev().nth(1) != Some('\\')
                    && current_value.chars().rev().nth(1) != Some('\\')
                {
                    in_string = false;
                }
                continue;
            }

            match ch {
                '"' | '\'' => {
                    if in_key {
                        current_key.push(ch);
                    } else {
                        current_value.push(ch);
                    }
                    in_string = true;
                    string_char = ch;
                }
                ':' if bracket_depth == 0 && brace_depth == 0 && in_key => {
                    in_key = false;
                }
                '[' => {
                    if !in_key {
                        current_value.push(ch);
                        bracket_depth += 1;
                    }
                }
                ']' => {
                    if !in_key && bracket_depth > 0 {
                        current_value.push(ch);
                        bracket_depth -= 1;
                    }
                }
                '{' => {
                    if !in_key {
                        current_value.push(ch);
                        brace_depth += 1;
                    }
                }
                '}' => {
                    if !in_key && brace_depth > 0 {
                        current_value.push(ch);
                        brace_depth -= 1;
                    }
                }
                ',' if bracket_depth == 0 && brace_depth == 0 && !in_key => {
                    // End of key-value pair
                    let key = current_key
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .to_string();
                    let value_str = current_value.trim();
                    if !key.is_empty() && !value_str.is_empty() {
                        let value = self.parse_value(value_str)?;
                        conditions.insert(key, value);
                    }
                    current_key.clear();
                    current_value.clear();
                    in_key = true;
                }
                _ => {
                    if in_key {
                        current_key.push(ch);
                    } else {
                        current_value.push(ch);
                    }
                }
            }
        }

        // Handle last pair
        if !current_key.is_empty() {
            let key = current_key
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            let value_str = current_value.trim();
            if !value_str.is_empty() {
                // Use a fallback if value parsing fails
                match self.parse_value(value_str) {
                    Ok(value) => {
                        conditions.insert(key, value);
                    }
                    Err(_) => {
                        eprintln!(
                            "   ⚠️  Warning: Could not parse select() value for key '{}', skipping",
                            key
                        );
                    }
                }
            }
        }

        // If we successfully parsed at least one condition, return it
        // Otherwise return the default value or empty list
        if !conditions.is_empty() {
            Ok(Value::Select(conditions))
        } else {
            eprintln!("   ⚠️  Warning: select() parsed no conditions, using empty list");
            Ok(Value::List(vec![]))
        }
    }

    /// Expand glob() function to list of files
    fn expand_glob(&self, glob_expr: &str) -> Result<Value> {
        // Extract patterns from glob(["pattern1", "pattern2"], exclude=["excl"])
        // For now, handle simple case: glob(["*.cc"])

        let inner = &glob_expr[5..glob_expr.len() - 1]; // Remove "glob(" and ")"

        // Parse the patterns list
        let patterns = if let Some(list_start) = inner.find('[') {
            if let Some(list_end) = inner[list_start..].find(']') {
                let list_content = &inner[list_start + 1..list_start + list_end];
                list_content
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        // Parse exclude parameter if present
        let excludes = if let Some(excl_pos) = inner.find("exclude") {
            if let Some(list_start) = inner[excl_pos..].find('[') {
                let abs_start = excl_pos + list_start;
                if let Some(list_end) = inner[abs_start..].find(']') {
                    let list_content = &inner[abs_start + 1..abs_start + list_end];
                    list_content
                        .split(',')
                        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                }
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        // Expand patterns to actual files
        let files = self.match_glob_patterns(&patterns, &excludes)?;

        Ok(Value::List(files.into_iter().map(Value::String).collect()))
    }

    /// Match glob patterns against filesystem
    fn match_glob_patterns(&self, patterns: &[String], excludes: &[String]) -> Result<Vec<String>> {
        use glob::glob as glob_fn;

        let mut matched_files = Vec::new();

        // Get the current BUILD file directory
        let base_dir = self
            .current_build_dir
            .borrow()
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No BUILD file context for glob"))?
            .clone();

        for pattern in patterns {
            // Convert pattern to absolute path relative to BUILD file directory
            let full_pattern = base_dir.join(pattern);
            let pattern_str = full_pattern
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Invalid glob pattern path"))?;

            // Use glob crate to match files
            for entry in glob_fn(pattern_str)? {
                let path = entry?;

                // Convert to relative path from BUILD file directory
                let rel_path = path
                    .strip_prefix(&base_dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                // Check if excluded
                let is_excluded = excludes.iter().any(|excl| {
                    // Simple pattern matching for excludes
                    if excl.contains('*') {
                        // Basic wildcard matching
                        let parts: Vec<&str> = excl.split('*').collect();
                        if parts.len() == 2 {
                            rel_path.starts_with(parts[0]) && rel_path.ends_with(parts[1])
                        } else {
                            false
                        }
                    } else {
                        rel_path == *excl
                    }
                });

                if !is_excluded {
                    matched_files.push(rel_path);
                }
            }
        }

        // Sort for deterministic output
        matched_files.sort();
        Ok(matched_files)
    }

    /// Load and process a .bzl file
    pub fn load_bzl_file(&self, bzl_path: &Path) -> Result<HashMap<String, MacroDefinition>> {
        // Check cache first (LRU cache)
        {
            let mut cache = self.bzl_cache.lock();
            if let Some(cached) = cache.get(&bzl_path.to_path_buf()) {
                return Ok(cached.clone());
            }
        }

        let content = std::fs::read_to_string(bzl_path)
            .with_context(|| format!("Failed to read .bzl file: {}", bzl_path.display()))?;

        let macros = self.parse_bzl_macros(&content)?;

        // Cache the result (LRU may evict old entries)
        {
            let mut cache = self.bzl_cache.lock();
            cache.put(bzl_path.to_path_buf(), macros.clone());
        }

        Ok(macros)
    }

    /// Parse macro definitions from .bzl file
    fn parse_bzl_macros(&self, content: &str) -> Result<HashMap<String, MacroDefinition>> {
        let mut macros = HashMap::new();

        // Match def macro_name(params):
        for cap in MACRO_PATTERN_RE.captures_iter(content) {
            let name = cap.get(1).unwrap().as_str().to_string();
            let params_str = cap.get(2).unwrap().as_str();

            let params: Vec<String> = params_str
                .split(',')
                .map(|s| s.trim().split('=').next().unwrap_or("").trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            // Extract macro body
            let macro_start = cap.get(0).unwrap().end();
            let body = self.extract_python_function_body(content, macro_start);

            macros.insert(name.clone(), MacroDefinition { name, params, body });
        }

        Ok(macros)
    }

    /// Extract Python function body based on indentation
    fn extract_python_function_body(&self, content: &str, start_pos: usize) -> String {
        let remaining = &content[start_pos..];
        let mut body = String::new();
        let mut base_indent = None;

        for line in remaining.lines() {
            // Skip empty lines at start
            if body.is_empty() && line.trim().is_empty() {
                continue;
            }

            // Determine base indentation from first non-empty line
            if base_indent.is_none() && !line.trim().is_empty() {
                let indent = line.len() - line.trim_start().len();
                base_indent = Some(indent);
            }

            // Check if we've dedented back (end of function)
            if let Some(base) = base_indent {
                let current_indent = line.len() - line.trim_start().len();
                if !line.trim().is_empty() && current_indent < base {
                    break;
                }
            }

            body.push_str(line);
            body.push('\n');
        }

        body
    }
}

// Re-export BuildFile to make it cloneable
impl Clone for BuildFile {
    fn clone(&self) -> Self {
        BuildFile {
            path: self.path.clone(),
            rules: self
                .rules
                .iter()
                .map(|r| Rule {
                    name: r.name.clone(),
                    rule_type: r.rule_type.clone(),
                    attributes: r.attributes.clone(),
                })
                .collect(),
        }
    }
}

impl Clone for Value {
    fn clone(&self) -> Self {
        match self {
            Value::String(s) => Value::String(s.clone()),
            Value::Int(value) => Value::Int(*value),
            Value::List(l) => Value::List(l.clone()),
            Value::Dict(values) => Value::Dict(values.clone()),
            Value::Bool(b) => Value::Bool(*b),
            Value::None => Value::None,
            Value::Select(conditions) => Value::Select(conditions.clone()),
            Value::Configurable { op, left, right } => Value::Configurable {
                op: *op,
                left: left.clone(),
                right: right.clone(),
            },
        }
    }
}
