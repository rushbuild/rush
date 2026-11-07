// Rush Build - Aspects
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build aspects for cross-cutting concerns.
//!
//! Aspects allow you to apply additional processing to targets during builds,
//! enabling features like code coverage, sanitizers, profiling, and custom
//! instrumentation without modifying build rules.

use anyhow::Result;
use rush_action::Action;
use rush_parse::Rule;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Aspect that can be applied to targets during builds.
pub trait Aspect: Send + Sync {
    /// Aspect name.
    fn name(&self) -> &str;

    /// Check if this aspect applies to a given rule.
    fn applies_to(&self, rule: &Rule) -> bool;

    /// Transform an action before execution.
    ///
    /// Can modify command, environment, inputs, outputs, etc.
    fn transform_action(&self, action: &mut Action) -> Result<()>;

    /// Called after action completes.
    ///
    /// Can collect results, generate reports, etc.
    fn post_action(&self, action: &Action, result: &rush_exec::ActionResult) -> Result<()> {
        let _ = (action, result);
        Ok(())
    }
}

/// Code coverage aspect.
///
/// Instruments binaries for coverage collection.
pub struct CoverageAspect {
    /// Coverage output directory.
    output_dir: std::path::PathBuf,

    /// Coverage format (lcov, html, etc.).
    _format: CoverageFormat,
}

/// Coverage output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverageFormat {
    /// LCOV format.
    Lcov,
    /// HTML report.
    Html,
    /// Cobertura XML.
    Cobertura,
}

impl CoverageAspect {
    /// Create a new coverage aspect.
    pub fn new(output_dir: std::path::PathBuf, format: CoverageFormat) -> Self {
        CoverageAspect {
            output_dir,
            _format: format,
        }
    }
}

impl Aspect for CoverageAspect {
    fn name(&self) -> &str {
        "coverage"
    }

    fn applies_to(&self, rule: &Rule) -> bool {
        // Apply to test and binary rules
        matches!(
            rule.rule_type.as_str(),
            "cc_test" | "cc_binary" | "rust_test" | "rust_binary"
        )
    }

    fn transform_action(&self, action: &mut Action) -> Result<()> {
        // Add coverage flags to compiler
        let command = &mut action.command;

        // For C/C++
        let exe_str = command.executable.to_string_lossy();
        if exe_str.ends_with("g++") || exe_str.ends_with("gcc") {
            command.args.push("--coverage".to_string());
            action.env.insert(
                "GCOV_PREFIX".to_string(),
                self.output_dir.display().to_string(),
            );
        }
        // For Rust
        else if exe_str.ends_with("rustc") {
            command.args.push("-C".to_string());
            command.args.push("instrument-coverage".to_string());
            action.env.insert(
                "LLVM_PROFILE_FILE".to_string(),
                format!("{}/coverage-%p.profraw", self.output_dir.display()),
            );
        }

        Ok(())
    }

    fn post_action(&self, _action: &Action, _result: &rush_exec::ActionResult) -> Result<()> {
        // Could generate coverage reports here
        Ok(())
    }
}

/// Sanitizer aspect.
///
/// Adds sanitizer instrumentation (ASAN, TSAN, UBSAN).
pub struct SanitizerAspect {
    /// Sanitizer type.
    sanitizer: SanitizerType,
}

/// Sanitizer types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SanitizerType {
    /// Address sanitizer (detects memory errors).
    Address,
    /// Thread sanitizer (detects data races).
    Thread,
    /// Undefined behavior sanitizer.
    UndefinedBehavior,
    /// Memory sanitizer (detects uninitialized reads).
    Memory,
}

impl SanitizerAspect {
    /// Create a new sanitizer aspect.
    pub fn new(sanitizer: SanitizerType) -> Self {
        SanitizerAspect { sanitizer }
    }

    fn sanitizer_flag(&self) -> &str {
        match self.sanitizer {
            SanitizerType::Address => "address",
            SanitizerType::Thread => "thread",
            SanitizerType::UndefinedBehavior => "undefined",
            SanitizerType::Memory => "memory",
        }
    }
}

impl Aspect for SanitizerAspect {
    fn name(&self) -> &str {
        "sanitizer"
    }

    fn applies_to(&self, rule: &Rule) -> bool {
        // Apply to all binary/test rules
        matches!(
            rule.rule_type.as_str(),
            "cc_test" | "cc_binary" | "rust_test" | "rust_binary"
        )
    }

    fn transform_action(&self, action: &mut Action) -> Result<()> {
        let command = &mut action.command;
        let flag = format!("-fsanitize={}", self.sanitizer_flag());
        let exe_str = command.executable.to_string_lossy();

        // For C/C++
        if exe_str.ends_with("g++")
            || exe_str.ends_with("gcc")
            || exe_str.ends_with("clang++")
            || exe_str.ends_with("clang")
        {
            command.args.push(flag.clone());
            command.args.push("-g".to_string()); // Debug symbols for better reports
        }
        // For Rust
        else if exe_str.ends_with("rustc") {
            command.args.push("-Z".to_string());
            command
                .args
                .push(format!("sanitizer={}", self.sanitizer_flag()));
        }

        Ok(())
    }
}

/// Profiling aspect.
///
/// Adds profiling instrumentation for performance analysis.
pub struct ProfilingAspect {
    /// Profiling output directory.
    _output_dir: std::path::PathBuf,

    /// Profiling mode.
    mode: ProfilingMode,
}

/// Profiling modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfilingMode {
    /// CPU profiling with gprof.
    Gprof,
    /// Performance counters with perf.
    Perf,
    /// Flame graph generation.
    FlameGraph,
}

impl ProfilingAspect {
    /// Create a new profiling aspect.
    pub fn new(output_dir: std::path::PathBuf, mode: ProfilingMode) -> Self {
        ProfilingAspect {
            _output_dir: output_dir,
            mode,
        }
    }
}

impl Aspect for ProfilingAspect {
    fn name(&self) -> &str {
        "profiling"
    }

    fn applies_to(&self, rule: &Rule) -> bool {
        matches!(
            rule.rule_type.as_str(),
            "cc_binary" | "cc_test" | "rust_binary" | "rust_test"
        )
    }

    fn transform_action(&self, action: &mut Action) -> Result<()> {
        let command = &mut action.command;
        let exe_str = command.executable.to_string_lossy();

        match self.mode {
            ProfilingMode::Gprof => {
                if exe_str.ends_with("g++") || exe_str.ends_with("gcc") {
                    command.args.push("-pg".to_string());
                }
            }
            ProfilingMode::Perf | ProfilingMode::FlameGraph => {
                // Frame pointers for better stack traces
                if exe_str.ends_with("g++") || exe_str.ends_with("gcc") {
                    command.args.push("-fno-omit-frame-pointer".to_string());
                } else if exe_str.ends_with("rustc") {
                    command.args.push("-C".to_string());
                    command.args.push("force-frame-pointers=yes".to_string());
                }
            }
        }

        Ok(())
    }
}

/// Aspect manager for applying aspects to builds.
pub struct AspectManager {
    /// Registered aspects.
    aspects: Vec<Box<dyn Aspect>>,
}

impl AspectManager {
    /// Create a new aspect manager.
    pub fn new() -> Self {
        AspectManager {
            aspects: Vec::new(),
        }
    }

    /// Register an aspect.
    pub fn register<A: Aspect + 'static>(&mut self, aspect: A) {
        self.aspects.push(Box::new(aspect));
    }

    /// Apply aspects to an action.
    pub fn transform_action(&self, rule: &Rule, action: &mut Action) -> Result<()> {
        for aspect in &self.aspects {
            if aspect.applies_to(rule) {
                aspect.transform_action(action)?;
            }
        }
        Ok(())
    }

    /// Notify aspects of action completion.
    pub fn post_action(
        &self,
        rule: &Rule,
        action: &Action,
        result: &rush_exec::ActionResult,
    ) -> Result<()> {
        for aspect in &self.aspects {
            if aspect.applies_to(rule) {
                aspect.post_action(action, result)?;
            }
        }
        Ok(())
    }

    /// Get list of active aspects.
    pub fn active_aspects(&self) -> Vec<String> {
        self.aspects.iter().map(|a| a.name().to_string()).collect()
    }

    /// Get number of registered aspects.
    pub fn len(&self) -> usize {
        self.aspects.len()
    }

    /// Check if any aspects are registered.
    pub fn is_empty(&self) -> bool {
        self.aspects.is_empty()
    }
}

impl Default for AspectManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Custom aspect for dependency graph extraction.
///
/// Example of a user-defined aspect.
pub struct DependencyGraphAspect {
    /// Collected dependencies.
    dependencies: std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<String>>>>,
}

impl DependencyGraphAspect {
    /// Create a new dependency graph aspect.
    pub fn new() -> Self {
        DependencyGraphAspect {
            dependencies: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    /// Get collected dependencies.
    pub fn dependencies(&self) -> HashMap<String, Vec<String>> {
        self.dependencies.lock().unwrap().clone()
    }
}

impl Aspect for DependencyGraphAspect {
    fn name(&self) -> &str {
        "dependency_graph"
    }

    fn applies_to(&self, _rule: &Rule) -> bool {
        true // Apply to all rules
    }

    fn transform_action(&self, action: &mut Action) -> Result<()> {
        // Extract dependencies from action inputs
        let inputs: Vec<String> = action
            .inputs
            .iter()
            .map(|a| {
                let path = match a {
                    rush_action::Artifact::Source(p) => p,
                    rush_action::Artifact::Derived { output_path, .. } => output_path,
                };
                path.display().to_string()
            })
            .collect();

        let mut deps = self.dependencies.lock().unwrap();
        deps.insert(action.id.to_string(), inputs);

        Ok(())
    }
}

impl Default for DependencyGraphAspect {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::Command;
    use std::path::PathBuf;

    #[test]
    fn test_aspect_manager() {
        let mut manager = AspectManager::new();

        let aspect = SanitizerAspect::new(SanitizerType::Address);
        manager.register(aspect);

        assert_eq!(manager.len(), 1);
        assert!(!manager.is_empty());
    }

    #[test]
    fn test_sanitizer_aspect() {
        let aspect = SanitizerAspect::new(SanitizerType::Address);

        let inputs = vec![];
        let command = Command {
            working_dir: None,
            executable: PathBuf::from("/usr/bin/g++"),
            args: vec!["-c".to_string(), "test.cc".to_string()],
            execution_info: rush_action::ExecutionInfo::default(),
        };
        let env = HashMap::new();

        let mut action = Action {
            id: Action::compute_id(&inputs, &command, &env).unwrap(),
            inputs,
            outputs: vec![],
            command,
            env,
            mnemonic: "test".to_string(),
        };

        aspect.transform_action(&mut action).unwrap();

        let command = &action.command;
        assert!(command.args.contains(&"-fsanitize=address".to_string()));
        assert!(command.args.contains(&"-g".to_string()));
    }
}
