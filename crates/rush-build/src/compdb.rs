// Rush Compilation Database
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Compilation database (compile_commands.json) generation.
//!
//! Generates a compilation database compatible with clangd and other
//! C++ language servers for IDE integration.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// A single compilation command entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilationEntry {
    /// Working directory for the compilation.
    pub directory: String,

    /// The source file being compiled.
    pub file: String,

    /// The compilation command (arguments array preferred over command string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<String>>,

    /// Alternative: full command string (less preferred than arguments).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Output file path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

impl CompilationEntry {
    /// Create a new compilation entry from arguments.
    pub fn new(
        directory: PathBuf,
        file: PathBuf,
        arguments: Vec<String>,
        output: Option<PathBuf>,
    ) -> Self {
        CompilationEntry {
            directory: directory.display().to_string(),
            file: file.display().to_string(),
            arguments: Some(arguments),
            command: None,
            output: output.map(|p| p.display().to_string()),
        }
    }

    /// Create from a command string (fallback).
    pub fn from_command(
        directory: PathBuf,
        file: PathBuf,
        command: String,
        output: Option<PathBuf>,
    ) -> Self {
        CompilationEntry {
            directory: directory.display().to_string(),
            file: file.display().to_string(),
            arguments: None,
            command: Some(command),
            output: output.map(|p| p.display().to_string()),
        }
    }
}

/// Compilation database builder.
#[derive(Debug, Clone)]
pub struct CompilationDatabase {
    /// All compilation entries.
    entries: HashMap<String, CompilationEntry>,

    /// Workspace root directory.
    workspace_root: PathBuf,
}

impl CompilationDatabase {
    /// Create a new compilation database.
    pub fn new(workspace_root: PathBuf) -> Self {
        CompilationDatabase {
            entries: HashMap::new(),
            workspace_root,
        }
    }

    /// Add a compilation command.
    pub fn add_entry(&mut self, entry: CompilationEntry) {
        // Use file path as key to deduplicate
        self.entries.insert(entry.file.clone(), entry);
    }

    /// Add a C++ compilation command.
    pub fn add_cpp_compile(
        &mut self,
        source_file: PathBuf,
        compiler: &str,
        args: Vec<String>,
        output_file: Option<PathBuf>,
    ) {
        let mut full_args = vec![compiler.to_string()];
        full_args.extend(args);

        let entry = CompilationEntry::new(
            self.workspace_root.clone(),
            source_file,
            full_args,
            output_file,
        );

        self.add_entry(entry);
    }

    /// Get number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if database is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write compilation database to file.
    pub fn write_to_file(&self, path: &Path) -> Result<()> {
        let mut entries: Vec<&CompilationEntry> = self.entries.values().collect();

        // Sort by file path for deterministic output
        entries.sort_by(|a, b| a.file.cmp(&b.file));

        let json = serde_json::to_string_pretty(&entries)?;

        let mut file = File::create(path)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;

        Ok(())
    }

    /// Get all entries (for inspection).
    pub fn entries(&self) -> Vec<CompilationEntry> {
        let mut entries: Vec<_> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| a.file.cmp(&b.file));
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_compilation_entry() {
        let entry = CompilationEntry::new(
            PathBuf::from("/workspace"),
            PathBuf::from("src/main.cc"),
            vec![
                "g++".to_string(),
                "-c".to_string(),
                "src/main.cc".to_string(),
                "-o".to_string(),
                "main.o".to_string(),
            ],
            Some(PathBuf::from("main.o")),
        );

        assert_eq!(entry.directory, "/workspace");
        assert_eq!(entry.file, "src/main.cc");
        assert!(entry.arguments.is_some());
        assert_eq!(entry.arguments.unwrap().len(), 5);
    }

    #[test]
    fn test_compilation_database() -> Result<()> {
        let temp = TempDir::new()?;
        let mut db = CompilationDatabase::new(temp.path().to_path_buf());

        db.add_cpp_compile(
            temp.path().join("main.cc"),
            "g++",
            vec!["-c".to_string(), "main.cc".to_string()],
            Some(temp.path().join("main.o")),
        );

        assert_eq!(db.len(), 1);

        let output_path = temp.path().join("compile_commands.json");
        db.write_to_file(&output_path)?;

        assert!(output_path.exists());

        // Verify JSON is valid
        let content = std::fs::read_to_string(&output_path)?;
        let parsed: Vec<CompilationEntry> = serde_json::from_str(&content)?;
        assert_eq!(parsed.len(), 1);

        Ok(())
    }
}
