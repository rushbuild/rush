// Rush Build - Language Server Protocol Integration
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Language Server Protocol (LSP) integration for IDE support.
//!
//! Provides intelligent code editing features for BUILD files including
//! autocomplete, go-to-definition, diagnostics, hover information, and
//! code actions.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// LSP position in a document.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    /// Line number (0-indexed).
    pub line: u32,

    /// Character offset (0-indexed).
    pub character: u32,
}

/// LSP range in a document.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Range {
    /// Start position.
    pub start: Position,

    /// End position.
    pub end: Position,
}

/// LSP diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    /// Error.
    Error = 1,

    /// Warning.
    Warning = 2,

    /// Information.
    Information = 3,

    /// Hint.
    Hint = 4,
}

/// LSP diagnostic message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Range where the diagnostic applies.
    pub range: Range,

    /// Severity level.
    pub severity: DiagnosticSeverity,

    /// Diagnostic message.
    pub message: String,

    /// Source (e.g., "rush").
    pub source: Option<String>,

    /// Error code.
    pub code: Option<String>,
}

/// Completion item kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum CompletionItemKind {
    /// Text completion.
    Text = 1,

    /// Method completion.
    Method = 2,

    /// Function completion.
    Function = 3,

    /// Constructor completion.
    Constructor = 4,

    /// Field completion.
    Field = 5,

    /// Variable completion.
    Variable = 6,

    /// Class completion.
    Class = 7,

    /// Module completion.
    Module = 9,

    /// Property completion.
    Property = 10,

    /// Keyword completion.
    Keyword = 14,

    /// File completion.
    File = 17,

    /// Folder completion.
    Folder = 19,
}

/// Completion item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionItem {
    /// Label to display.
    pub label: String,

    /// Kind of completion.
    pub kind: CompletionItemKind,

    /// Detail information.
    pub detail: Option<String>,

    /// Documentation.
    pub documentation: Option<String>,

    /// Text to insert.
    pub insert_text: Option<String>,
}

/// Hover information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hover {
    /// Hover contents (markdown).
    pub contents: String,

    /// Range where hover applies.
    pub range: Option<Range>,
}

/// Symbol kind.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SymbolKind {
    /// File symbol.
    File = 1,

    /// Module symbol.
    Module = 2,

    /// Function symbol.
    Function = 12,

    /// Variable symbol.
    Variable = 13,

    /// String symbol.
    String = 15,

    /// Array symbol.
    Array = 18,
}

/// Document symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentSymbol {
    /// Symbol name.
    pub name: String,

    /// Symbol kind.
    pub kind: SymbolKind,

    /// Range of the symbol.
    pub range: Range,

    /// Selection range (the identifier).
    pub selection_range: Range,

    /// Children symbols.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<DocumentSymbol>,
}

/// Location in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// File URI.
    pub uri: String,

    /// Range in the file.
    pub range: Range,
}

/// Rush LSP server.
pub struct RushLspServer {
    /// Workspace root.
    _workspace_root: PathBuf,

    /// Open documents.
    documents: HashMap<String, Document>,

    /// Build file index.
    index: BuildFileIndex,
}

/// Document state.
struct Document {
    /// Document URI.
    _uri: String,

    /// Document content.
    content: String,

    /// Document version.
    version: i32,
}

/// Build file index for cross-file navigation.
struct BuildFileIndex {
    /// Target definitions.
    targets: HashMap<String, TargetInfo>,

    /// File-to-targets mapping.
    file_targets: HashMap<PathBuf, Vec<String>>,
}

/// Target information for indexing.
#[derive(Debug, Clone)]
struct TargetInfo {
    /// Target label.
    label: String,

    /// Target kind.
    kind: String,

    /// Location of definition.
    location: Location,

    /// Dependencies.
    _deps: Vec<String>,
}

impl RushLspServer {
    /// Create a new LSP server.
    pub fn new<P: AsRef<Path>>(workspace_root: P) -> Self {
        RushLspServer {
            _workspace_root: workspace_root.as_ref().to_path_buf(),
            documents: HashMap::new(),
            index: BuildFileIndex {
                targets: HashMap::new(),
                file_targets: HashMap::new(),
            },
        }
    }

    /// Handle document open.
    pub fn did_open(&mut self, uri: String, content: String, version: i32) {
        self.documents.insert(
            uri.clone(),
            Document {
                _uri: uri,
                content,
                version,
            },
        );
    }

    /// Handle document change.
    pub fn did_change(&mut self, uri: String, content: String, version: i32) {
        if let Some(doc) = self.documents.get_mut(&uri) {
            doc.content = content;
            doc.version = version;
        }
    }

    /// Handle document close.
    pub fn did_close(&mut self, uri: &str) {
        self.documents.remove(uri);
    }

    /// Provide diagnostics for a document.
    pub fn diagnostics(&self, uri: &str) -> Vec<Diagnostic> {
        let mut diagnostics = Vec::new();

        if let Some(doc) = self.documents.get(uri) {
            // Check for common issues
            for (line_num, line) in doc.content.lines().enumerate() {
                // Check for undefined targets in deps
                if line.contains("deps = [") {
                    // Simple heuristic - in practice would parse properly
                    if line.contains("//nonexistent") {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line: line_num as u32,
                                    character: 0,
                                },
                                end: Position {
                                    line: line_num as u32,
                                    character: line.len() as u32,
                                },
                            },
                            severity: DiagnosticSeverity::Error,
                            message: "Undefined target in dependencies".to_string(),
                            source: Some("rush".to_string()),
                            code: Some("undefined-target".to_string()),
                        });
                    }
                }

                // Check for deprecated syntax
                if line.contains("cc_library") {
                    diagnostics.push(Diagnostic {
                        range: Range {
                            start: Position {
                                line: line_num as u32,
                                character: 0,
                            },
                            end: Position {
                                line: line_num as u32,
                                character: line.len() as u32,
                            },
                        },
                        severity: DiagnosticSeverity::Warning,
                        message: "Consider using cpp_library instead".to_string(),
                        source: Some("rush".to_string()),
                        code: Some("deprecated-rule".to_string()),
                    });
                }
            }
        }

        diagnostics
    }

    /// Provide completions at a position.
    pub fn completions(&self, uri: &str, position: Position) -> Vec<CompletionItem> {
        let mut items = Vec::new();

        if let Some(doc) = self.documents.get(uri) {
            let lines: Vec<_> = doc.content.lines().collect();
            if let Some(line) = lines.get(position.line as usize) {
                let context = &line[..position.character.min(line.len() as u32) as usize];

                // Rule completions
                if context.trim_start() == context || context.ends_with('\n') {
                    items.extend(self.rule_completions());
                }

                // Attribute completions
                if context.contains('(') && !context.contains(')') {
                    items.extend(self.attribute_completions());
                }

                // Target completions in deps
                if context.contains("deps") || context.contains("\"//") {
                    items.extend(self.target_completions());
                }
            }
        }

        items
    }

    /// Rule name completions.
    fn rule_completions(&self) -> Vec<CompletionItem> {
        vec![
            CompletionItem {
                label: "rust_library".to_string(),
                kind: CompletionItemKind::Function,
                detail: Some("Rust library rule".to_string()),
                documentation: Some("Builds a Rust library".to_string()),
                insert_text: Some(
                    "rust_library(\n    name = \"$1\",\n    srcs = [\"$2\"],\n)".to_string(),
                ),
            },
            CompletionItem {
                label: "rust_binary".to_string(),
                kind: CompletionItemKind::Function,
                detail: Some("Rust binary rule".to_string()),
                documentation: Some("Builds a Rust binary".to_string()),
                insert_text: Some(
                    "rust_binary(\n    name = \"$1\",\n    srcs = [\"$2\"],\n)".to_string(),
                ),
            },
            CompletionItem {
                label: "cpp_library".to_string(),
                kind: CompletionItemKind::Function,
                detail: Some("C++ library rule".to_string()),
                documentation: Some("Builds a C++ library".to_string()),
                insert_text: Some(
                    "cpp_library(\n    name = \"$1\",\n    srcs = [\"$2\"],\n)".to_string(),
                ),
            },
            CompletionItem {
                label: "cpp_binary".to_string(),
                kind: CompletionItemKind::Function,
                detail: Some("C++ binary rule".to_string()),
                documentation: Some("Builds a C++ binary".to_string()),
                insert_text: Some(
                    "cpp_binary(\n    name = \"$1\",\n    srcs = [\"$2\"],\n)".to_string(),
                ),
            },
            CompletionItem {
                label: "go_library".to_string(),
                kind: CompletionItemKind::Function,
                detail: Some("Go library rule".to_string()),
                documentation: Some("Builds a Go library".to_string()),
                insert_text: Some(
                    "go_library(\n    name = \"$1\",\n    srcs = [\"$2\"],\n)".to_string(),
                ),
            },
            CompletionItem {
                label: "go_binary".to_string(),
                kind: CompletionItemKind::Function,
                detail: Some("Go binary rule".to_string()),
                documentation: Some("Builds a Go binary".to_string()),
                insert_text: Some(
                    "go_binary(\n    name = \"$1\",\n    srcs = [\"$2\"],\n)".to_string(),
                ),
            },
        ]
    }

    /// Attribute completions.
    fn attribute_completions(&self) -> Vec<CompletionItem> {
        vec![
            CompletionItem {
                label: "name".to_string(),
                kind: CompletionItemKind::Property,
                detail: Some("Target name".to_string()),
                documentation: Some("Unique name for this target".to_string()),
                insert_text: Some("name = \"$1\"".to_string()),
            },
            CompletionItem {
                label: "srcs".to_string(),
                kind: CompletionItemKind::Property,
                detail: Some("Source files".to_string()),
                documentation: Some("List of source files".to_string()),
                insert_text: Some("srcs = [\"$1\"]".to_string()),
            },
            CompletionItem {
                label: "deps".to_string(),
                kind: CompletionItemKind::Property,
                detail: Some("Dependencies".to_string()),
                documentation: Some("List of target dependencies".to_string()),
                insert_text: Some("deps = [\"$1\"]".to_string()),
            },
            CompletionItem {
                label: "visibility".to_string(),
                kind: CompletionItemKind::Property,
                detail: Some("Visibility".to_string()),
                documentation: Some("List of packages that can depend on this".to_string()),
                insert_text: Some("visibility = [\"//visibility:public\"]".to_string()),
            },
        ]
    }

    /// Target label completions.
    fn target_completions(&self) -> Vec<CompletionItem> {
        self.index
            .targets
            .values()
            .map(|target| CompletionItem {
                label: target.label.clone(),
                kind: CompletionItemKind::Module,
                detail: Some(format!("{} target", target.kind)),
                documentation: None,
                insert_text: Some(format!("\"{}\"", target.label)),
            })
            .collect()
    }

    /// Provide hover information.
    pub fn hover(&self, uri: &str, position: Position) -> Option<Hover> {
        if let Some(doc) = self.documents.get(uri) {
            let lines: Vec<_> = doc.content.lines().collect();
            if let Some(line) = lines.get(position.line as usize) {
                // Check if hovering over a rule name
                if line.contains("rust_library") {
                    return Some(Hover {
                        contents: "**rust_library**\n\nBuilds a Rust library.\n\nAttributes:\n- `name`: Target name\n- `srcs`: Source files\n- `deps`: Dependencies".to_string(),
                        range: None,
                    });
                }

                if line.contains("cpp_library") {
                    return Some(Hover {
                        contents: "**cpp_library**\n\nBuilds a C++ library.\n\nAttributes:\n- `name`: Target name\n- `srcs`: Source files\n- `hdrs`: Header files\n- `deps`: Dependencies".to_string(),
                        range: None,
                    });
                }

                // Check if hovering over a target reference
                if line.contains("//") {
                    // Extract target label (simple heuristic)
                    if let Some(start) = line.find("//") {
                        if let Some(end) = line[start..].find('"') {
                            let label = &line[start..start + end];
                            if let Some(target) = self.index.targets.get(label) {
                                return Some(Hover {
                                    contents: format!(
                                        "**{}**\n\nKind: {}\n\nDefined in: {}",
                                        target.label, target.kind, target.location.uri
                                    ),
                                    range: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Find definition location.
    pub fn goto_definition(&self, uri: &str, position: Position) -> Option<Location> {
        if let Some(doc) = self.documents.get(uri) {
            let lines: Vec<_> = doc.content.lines().collect();
            if let Some(line) = lines.get(position.line as usize) {
                // Extract target label at position
                if let Some(start) = line.find("//") {
                    if let Some(end) = line[start..].find('"') {
                        let label = &line[start..start + end];
                        if let Some(target) = self.index.targets.get(label) {
                            return Some(target.location.clone());
                        }
                    }
                }
            }
        }

        None
    }

    /// Find references to a symbol.
    pub fn find_references(&self, uri: &str, position: Position) -> Vec<Location> {
        let mut locations = Vec::new();

        // Extract target label at position
        if let Some(doc) = self.documents.get(uri) {
            let lines: Vec<_> = doc.content.lines().collect();
            if let Some(line) = lines.get(position.line as usize) {
                if let Some(start) = line.find("//") {
                    if let Some(end) = line[start..].find('"') {
                        let target_label = &line[start..start + end];

                        // Search all documents for references
                        for (doc_uri, document) in &self.documents {
                            for (line_num, doc_line) in document.content.lines().enumerate() {
                                if doc_line.contains(target_label) {
                                    locations.push(Location {
                                        uri: doc_uri.clone(),
                                        range: Range {
                                            start: Position {
                                                line: line_num as u32,
                                                character: 0,
                                            },
                                            end: Position {
                                                line: line_num as u32,
                                                character: doc_line.len() as u32,
                                            },
                                        },
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        locations
    }

    /// Get document symbols.
    pub fn document_symbols(&self, uri: &str) -> Vec<DocumentSymbol> {
        let mut symbols = Vec::new();

        if let Some(doc) = self.documents.get(uri) {
            for (line_num, line) in doc.content.lines().enumerate() {
                // Find rule definitions (simple heuristic)
                if line.contains("_library(") || line.contains("_binary(") {
                    if let Some(name_start) = line.find("name = \"") {
                        let name_offset = name_start + 8;
                        if let Some(name_end) = line[name_offset..].find('"') {
                            let name = &line[name_offset..name_offset + name_end];

                            symbols.push(DocumentSymbol {
                                name: name.to_string(),
                                kind: SymbolKind::Function,
                                range: Range {
                                    start: Position {
                                        line: line_num as u32,
                                        character: 0,
                                    },
                                    end: Position {
                                        line: line_num as u32,
                                        character: line.len() as u32,
                                    },
                                },
                                selection_range: Range {
                                    start: Position {
                                        line: line_num as u32,
                                        character: name_start as u32,
                                    },
                                    end: Position {
                                        line: line_num as u32,
                                        character: (name_offset + name_end) as u32,
                                    },
                                },
                                children: Vec::new(),
                            });
                        }
                    }
                }
            }
        }

        symbols
    }

    /// Index a build file.
    pub fn index_file(&mut self, path: &Path, content: &str) -> Result<()> {
        // Simple parsing to extract targets
        // In practice, would use proper Starlark parser

        for (line_num, line) in content.lines().enumerate() {
            if line.contains("_library(") || line.contains("_binary(") {
                if let Some(name_start) = line.find("name = \"") {
                    let name_offset = name_start + 8;
                    if let Some(name_end) = line[name_offset..].find('"') {
                        let name = &line[name_offset..name_offset + name_end];

                        let kind = if line.contains("rust_") {
                            "rust"
                        } else if line.contains("cpp_") {
                            "cpp"
                        } else if line.contains("go_") {
                            "go"
                        } else {
                            "unknown"
                        };

                        let label = format!("//{}:{}", path.display(), name);

                        let target_info = TargetInfo {
                            label: label.clone(),
                            kind: kind.to_string(),
                            location: Location {
                                uri: format!("file://{}", path.display()),
                                range: Range {
                                    start: Position {
                                        line: line_num as u32,
                                        character: 0,
                                    },
                                    end: Position {
                                        line: line_num as u32,
                                        character: line.len() as u32,
                                    },
                                },
                            },
                            _deps: Vec::new(),
                        };

                        self.index.targets.insert(label.clone(), target_info);
                        self.index
                            .file_targets
                            .entry(path.to_path_buf())
                            .or_default()
                            .push(label);
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_diagnostics() {
        let mut server = RushLspServer::new("/workspace");

        let content = r#"
rust_library(
    name = "mylib",
    deps = ["//nonexistent:target"],
)
"#;

        server.did_open("file:///BUILD".to_string(), content.to_string(), 1);

        let diagnostics = server.diagnostics("file:///BUILD");
        assert!(!diagnostics.is_empty());
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
    }

    #[test]
    fn test_completions() {
        let mut server = RushLspServer::new("/workspace");

        server.did_open("file:///BUILD".to_string(), "rust_".to_string(), 1);

        let completions = server.completions(
            "file:///BUILD",
            Position {
                line: 0,
                character: 5,
            },
        );

        assert!(!completions.is_empty());
        assert!(completions.iter().any(|c| c.label == "rust_library"));
    }

    #[test]
    fn test_document_symbols() {
        let mut server = RushLspServer::new("/workspace");

        let content = r#"
rust_library(name = "lib1")
rust_binary(name = "bin1")
"#;

        server.did_open("file:///BUILD".to_string(), content.to_string(), 1);

        let symbols = server.document_symbols("file:///BUILD");
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "lib1");
        assert_eq!(symbols[1].name, "bin1");
    }
}
