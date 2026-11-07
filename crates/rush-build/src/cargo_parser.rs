// Rush Build - Cargo.toml Parser
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Parse Cargo.toml files to extract Rust dependencies.

use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed Cargo.toml manifest.
#[derive(Debug, Clone)]
pub struct CargoManifest {
    /// Package name.
    pub name: String,

    /// Package version.
    pub version: String,

    /// Edition (2015, 2018, 2021).
    pub edition: String,

    /// Dependencies.
    pub dependencies: HashMap<String, CargoDependency>,

    /// Dev dependencies (for tests).
    pub dev_dependencies: HashMap<String, CargoDependency>,

    /// Build dependencies.
    pub build_dependencies: HashMap<String, CargoDependency>,

    /// Library target.
    pub lib: Option<CargoTarget>,

    /// Binary targets.
    pub bins: Vec<CargoTarget>,

    /// Test targets.
    pub tests: Vec<CargoTarget>,
}

/// Cargo dependency specification.
#[derive(Debug, Clone)]
pub struct CargoDependency {
    /// Dependency name.
    pub name: String,

    /// Version requirement (e.g., "1.0", "^1.2", ">=0.3").
    pub version: Option<String>,

    /// Path to local dependency.
    pub path: Option<PathBuf>,

    /// Git repository URL.
    pub git: Option<String>,

    /// Features to enable.
    pub features: Vec<String>,

    /// Whether this is optional.
    pub optional: bool,
}

/// Cargo target (lib, bin, test, etc.).
#[derive(Debug, Clone)]
pub struct CargoTarget {
    /// Target name.
    pub name: String,

    /// Path to main source file.
    pub path: PathBuf,

    /// Crate type (lib, bin, etc.).
    pub crate_type: Option<String>,
}

impl CargoManifest {
    /// Parse a Cargo.toml file.
    pub fn parse(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Self::parse_from_str(&content, path.parent().unwrap_or(Path::new(".")))
    }

    /// Parse Cargo.toml content from string.
    pub fn parse_from_str(content: &str, manifest_dir: &Path) -> Result<Self> {
        let toml: toml::Value = toml::from_str(content)?;

        // Extract package metadata
        let package = toml
            .get("package")
            .ok_or_else(|| anyhow::anyhow!("Missing [package] section"))?;

        let name = package
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing package.name"))?
            .to_string();

        let version = package
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.1.0")
            .to_string();

        let edition = package
            .get("edition")
            .and_then(|v| v.as_str())
            .unwrap_or("2021")
            .to_string();

        // Parse dependencies
        let dependencies = Self::parse_dependencies(&toml, "dependencies", manifest_dir);
        let dev_dependencies = Self::parse_dependencies(&toml, "dev-dependencies", manifest_dir);
        let build_dependencies =
            Self::parse_dependencies(&toml, "build-dependencies", manifest_dir);

        // Parse library target
        let lib = if let Some(lib_table) = toml.get("lib") {
            Some(Self::parse_target(lib_table, &name, "lib", manifest_dir)?)
        } else {
            // Default lib.rs if it exists
            let lib_path = manifest_dir.join("src/lib.rs");
            if lib_path.exists() {
                Some(CargoTarget {
                    name: name.clone(),
                    path: lib_path,
                    crate_type: Some("lib".to_string()),
                })
            } else {
                None
            }
        };

        // Parse binary targets
        let bins = if let Some(bins_array) = toml.get("bin").and_then(|v| v.as_array()) {
            bins_array
                .iter()
                .filter_map(|bin| Self::parse_target(bin, &name, "bin", manifest_dir).ok())
                .collect()
        } else {
            // Check for default src/main.rs
            let main_path = manifest_dir.join("src/main.rs");
            if main_path.exists() {
                vec![CargoTarget {
                    name: name.clone(),
                    path: main_path,
                    crate_type: Some("bin".to_string()),
                }]
            } else {
                Vec::new()
            }
        };

        // Parse test targets
        let tests = if let Some(tests_array) = toml.get("test").and_then(|v| v.as_array()) {
            tests_array
                .iter()
                .filter_map(|test| Self::parse_target(test, &name, "test", manifest_dir).ok())
                .collect()
        } else {
            Vec::new()
        };

        Ok(CargoManifest {
            name,
            version,
            edition,
            dependencies,
            dev_dependencies,
            build_dependencies,
            lib,
            bins,
            tests,
        })
    }

    /// Parse dependencies from a TOML table.
    fn parse_dependencies(
        toml: &toml::Value,
        section: &str,
        manifest_dir: &Path,
    ) -> HashMap<String, CargoDependency> {
        let mut deps = HashMap::new();

        if let Some(deps_table) = toml.get(section).and_then(|v| v.as_table()) {
            for (name, value) in deps_table {
                if let Ok(dep) = Self::parse_dependency(name, value, manifest_dir) {
                    deps.insert(name.clone(), dep);
                }
            }
        }

        deps
    }

    /// Parse a single dependency.
    fn parse_dependency(
        name: &str,
        value: &toml::Value,
        manifest_dir: &Path,
    ) -> Result<CargoDependency> {
        match value {
            // Simple version: "dep = "1.0""
            toml::Value::String(version) => Ok(CargoDependency {
                name: name.to_string(),
                version: Some(version.clone()),
                path: None,
                git: None,
                features: Vec::new(),
                optional: false,
            }),
            // Complex: dep = { version = "1.0", features = ["foo"] }
            toml::Value::Table(table) => {
                let version = table
                    .get("version")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                let path = table
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| manifest_dir.join(p));

                let git = table.get("git").and_then(|v| v.as_str()).map(String::from);

                let features = table
                    .get("features")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                let optional = table
                    .get("optional")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                Ok(CargoDependency {
                    name: name.to_string(),
                    version,
                    path,
                    git,
                    features,
                    optional,
                })
            }
            _ => anyhow::bail!("Invalid dependency specification for {}", name),
        }
    }

    /// Parse a target (lib, bin, test).
    fn parse_target(
        value: &toml::Value,
        default_name: &str,
        target_type: &str,
        manifest_dir: &Path,
    ) -> Result<CargoTarget> {
        let table = value
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("Target must be a table"))?;

        let name = table
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(default_name)
            .to_string();

        let path_str = table
            .get("path")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| {
                // Default paths
                match target_type {
                    "lib" => Some("src/lib.rs".to_string()),
                    "bin" => Some("src/main.rs".to_string()),
                    "test" => Some(format!("tests/{}.rs", name)),
                    _ => None,
                }
            })
            .ok_or_else(|| anyhow::anyhow!("Missing path for target"))?;

        let path = manifest_dir.join(path_str);

        let crate_type = table
            .get("crate-type")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(CargoTarget {
            name,
            path,
            crate_type,
        })
    }

    /// Convert to Rush BUILD file rules.
    pub fn to_build_rules(&self) -> Vec<String> {
        let mut rules = Vec::new();

        // Generate rust_library if lib exists
        if let Some(lib) = &self.lib {
            let deps: Vec<String> = self
                .dependencies
                .keys()
                .map(|dep| format!("\"//external:{}\"", dep))
                .collect();

            let deps_str = if deps.is_empty() {
                String::new()
            } else {
                format!("\n    deps = [{}],", deps.join(", "))
            };

            rules.push(format!(
                r#"rust_library(
    name = "{}",
    srcs = ["{}"],
    edition = "{}",{}
)"#,
                lib.name,
                lib.path.display(),
                self.edition,
                deps_str
            ));
        }

        // Generate rust_binary for each bin
        for bin in &self.bins {
            let mut deps: Vec<String> = self
                .dependencies
                .keys()
                .map(|dep| format!("\"//external:{}\"", dep))
                .collect();

            // Add lib dependency if it exists
            if self.lib.is_some() {
                deps.insert(0, format!("\":{}\"", self.name));
            }

            let deps_str = if deps.is_empty() {
                String::new()
            } else {
                format!("\n    deps = [{}],", deps.join(", "))
            };

            rules.push(format!(
                r#"rust_binary(
    name = "{}",
    srcs = ["{}"],
    edition = "{}",{}
)"#,
                bin.name,
                bin.path.display(),
                self.edition,
                deps_str
            ));
        }

        // Generate rust_test for each test
        for test in &self.tests {
            let mut deps: Vec<String> = self
                .dev_dependencies
                .keys()
                .chain(self.dependencies.keys())
                .map(|dep| format!("\"//external:{}\"", dep))
                .collect();

            if self.lib.is_some() {
                deps.insert(0, format!("\":{}\"", self.name));
            }

            let deps_str = if deps.is_empty() {
                String::new()
            } else {
                format!("\n    deps = [{}],", deps.join(", "))
            };

            rules.push(format!(
                r#"rust_test(
    name = "{}",
    srcs = ["{}"],
    edition = "{}",{}
)"#,
                test.name,
                test.path.display(),
                self.edition,
                deps_str
            ));
        }

        rules
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_cargo() {
        let toml = r#"
[package]
name = "myapp"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
tokio = { version = "1.0", features = ["full"] }
"#;

        let manifest = CargoManifest::parse_from_str(toml, Path::new(".")).unwrap();
        assert_eq!(manifest.name, "myapp");
        assert_eq!(manifest.edition, "2021");
        assert_eq!(manifest.dependencies.len(), 2);

        let serde = &manifest.dependencies["serde"];
        assert_eq!(serde.version, Some("1.0".to_string()));

        let tokio = &manifest.dependencies["tokio"];
        assert!(tokio.features.contains(&"full".to_string()));
    }

    #[test]
    fn test_to_build_rules() {
        let toml = r#"
[package]
name = "mylib"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = "1.0"
"#;

        let manifest = CargoManifest::parse_from_str(toml, Path::new(".")).unwrap();
        let rules = manifest.to_build_rules();

        // Should generate a rust_library rule
        assert!(!rules.is_empty());
        assert!(rules[0].contains("rust_library"));
        assert!(rules[0].contains("mylib"));
    }
}
