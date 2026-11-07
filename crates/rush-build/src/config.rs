// Rush Build - Build Configuration System
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build configuration system.
//!
//! Manages build configurations, compilation modes, platform settings,
//! and flag inheritance. Supports .rushrc files for project-specific
//! configuration and allows target-specific overrides.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Build configuration manager.
pub struct ConfigManager {
    /// Active configuration.
    active_config: BuildConfiguration,

    /// Named configurations.
    configs: HashMap<String, BuildConfiguration>,

    /// User configuration from .rushrc.
    user_config: Option<UserConfig>,

    /// Platform detection cache.
    platform_cache: Option<Platform>,
}

/// Complete build configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfiguration {
    /// Configuration name.
    pub name: String,

    /// Compilation mode.
    pub mode: CompilationMode,

    /// Target platform.
    pub platform: Platform,

    /// C/C++ flags.
    pub cflags: Vec<String>,
    pub cxxflags: Vec<String>,
    pub ldflags: Vec<String>,

    /// Rust flags.
    pub rustflags: Vec<String>,

    /// Go flags.
    pub goflags: Vec<String>,

    /// Python flags.
    pub pythonflags: Vec<String>,

    /// Environment variables.
    pub env: HashMap<String, String>,

    /// Feature flags.
    pub features: HashMap<String, bool>,

    /// Optimization level (0-3, s, z).
    pub opt_level: OptLevel,

    /// Debug info level.
    pub debug_level: DebugLevel,

    /// Link-time optimization.
    pub lto: LtoMode,

    /// Code generation options.
    pub codegen: CodegenOptions,

    /// Target-specific overrides.
    pub target_overrides: HashMap<String, TargetConfig>,
}

/// Compilation mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CompilationMode {
    /// Debug build (no optimization, full debug info).
    Debug,

    /// Release build (full optimization, no debug info).
    Release,

    /// Release with debug info.
    RelWithDebInfo,

    /// Minimum size release.
    MinSizeRel,

    /// Fast build (some optimization, some debug info).
    FastBuild,

    /// Custom mode.
    Custom(String),
}

/// Platform specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Platform {
    /// Operating system.
    pub os: OsType,

    /// CPU architecture.
    pub arch: ArchType,

    /// ABI/environment.
    pub abi: String,

    /// Vendor.
    pub vendor: String,

    /// Full target triple (e.g., "x86_64-unknown-linux-gnu").
    pub triple: String,
}

/// Operating system type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum OsType {
    Linux,
    MacOS,
    Windows,
    FreeBSD,
    OpenBSD,
    NetBSD,
    Android,
    IOS,
    Custom(String),
}

/// Architecture type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ArchType {
    X86_64,
    Aarch64,
    Arm,
    Armv7,
    I686,
    Mips,
    Mips64,
    PowerPC,
    PowerPC64,
    Riscv64,
    S390x,
    Wasm32,
    Custom(String),
}

/// Optimization level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OptLevel {
    /// No optimization.
    O0,
    /// Basic optimization.
    O1,
    /// Moderate optimization.
    O2,
    /// Aggressive optimization.
    O3,
    /// Optimize for size.
    Os,
    /// Aggressively optimize for size.
    Oz,
}

/// Debug information level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DebugLevel {
    /// No debug info.
    None,
    /// Line tables only.
    LineTablesOnly,
    /// Full debug info.
    Full,
    /// Full debug info with extra checks.
    FullWithChecks,
}

/// Link-time optimization mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LtoMode {
    /// No LTO.
    None,
    /// Thin LTO.
    Thin,
    /// Full LTO.
    Full,
}

/// Code generation options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodegenOptions {
    /// Position independent code.
    pub pic: bool,

    /// Position independent executable.
    pub pie: bool,

    /// Stack protector.
    pub stack_protector: StackProtectorMode,

    /// Control flow integrity.
    pub cfi: bool,

    /// SafeStack.
    pub safestack: bool,

    /// Shadow call stack.
    pub shadow_call_stack: bool,

    /// Sanitizers.
    pub sanitizers: Vec<Sanitizer>,

    /// CPU-specific tuning.
    pub cpu: Option<String>,

    /// Target features (e.g., "+avx2,-sse").
    pub features: Vec<String>,
}

/// Stack protector mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StackProtectorMode {
    None,
    Basic,
    Strong,
    All,
}

/// Sanitizer type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Sanitizer {
    Address,
    Thread,
    Memory,
    Leak,
    UndefinedBehavior,
    DataFlow,
}

/// Target-specific configuration override.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetConfig {
    /// Additional flags for this target.
    pub flags: Vec<String>,

    /// Environment variable overrides.
    pub env: HashMap<String, String>,

    /// Feature toggles.
    pub features: HashMap<String, bool>,
}

/// User configuration from .rushrc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    /// Default compilation mode.
    pub default_mode: Option<String>,

    /// Default platform.
    pub default_platform: Option<String>,

    /// Global flags.
    pub global_flags: HashMap<String, Vec<String>>,

    /// Named configurations.
    pub configurations: HashMap<String, BuildConfiguration>,

    /// Aliases.
    pub aliases: HashMap<String, String>,

    /// Memory limit configuration.
    pub memory_limit: Option<MemoryLimitConfig>,
}

/// Memory limit configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLimitConfig {
    /// Memory threshold in GB. Build will abort if exceeded.
    pub threshold_gb: u64,

    /// Check interval in seconds.
    pub check_interval_secs: u64,

    /// Enable memory guard.
    pub enabled: bool,
}

impl ConfigManager {
    /// Create new configuration manager.
    pub fn new() -> Self {
        let platform = Platform::detect();
        let default_config = BuildConfiguration::default_for_platform(&platform);

        let mut configs = HashMap::new();
        configs.insert("default".to_string(), default_config.clone());

        ConfigManager {
            active_config: default_config,
            configs,
            user_config: None,
            platform_cache: Some(platform),
        }
    }

    /// Load user configuration from .rushrc.
    pub fn load_user_config<P: AsRef<Path>>(&mut self, workspace_root: P) -> Result<()> {
        let rushrc_path = workspace_root.as_ref().join(".rushrc");

        if !rushrc_path.exists() {
            return Ok(()); // No user config is fine
        }

        let content = std::fs::read_to_string(&rushrc_path).context("Failed to read .rushrc")?;

        let user_config: UserConfig =
            toml::from_str(&content).context("Failed to parse .rushrc")?;

        // Merge user configurations
        for (name, config) in &user_config.configurations {
            self.configs.insert(name.clone(), config.clone());
        }

        self.user_config = Some(user_config);

        Ok(())
    }

    /// Set active configuration.
    pub fn set_active(&mut self, name: &str) -> Result<()> {
        let config = self
            .configs
            .get(name)
            .context(format!("Configuration not found: {}", name))?
            .clone();

        self.active_config = config;
        Ok(())
    }

    /// Get active configuration.
    pub fn active(&self) -> &BuildConfiguration {
        &self.active_config
    }

    /// Register a configuration.
    pub fn register(&mut self, name: String, config: BuildConfiguration) {
        self.configs.insert(name, config);
    }

    /// Create configuration for compilation mode.
    pub fn for_mode(&self, mode: CompilationMode) -> BuildConfiguration {
        let platform = self.platform_cache.clone().unwrap_or_else(Platform::detect);
        BuildConfiguration::for_mode(mode, platform)
    }

    /// Get configuration for specific target.
    pub fn for_target(&self, target: &str) -> BuildConfiguration {
        let mut config = self.active_config.clone();

        // Apply target-specific overrides
        if let Some(override_cfg) = config.target_overrides.get(target).cloned() {
            config.apply_override(&override_cfg);
        }

        config
    }

    /// Detect host platform.
    pub fn detect_platform() -> Platform {
        Platform::detect()
    }

    /// List available configurations.
    pub fn list_configs(&self) -> Vec<String> {
        self.configs.keys().cloned().collect()
    }

    /// Create cross-compilation config.
    pub fn for_cross_compilation(&self, target_triple: &str) -> Result<BuildConfiguration> {
        let target_platform = Platform::from_triple(target_triple)?;
        let mut config = self.active_config.clone();
        config.platform = target_platform;
        Ok(config)
    }
}

impl Default for ConfigManager {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildConfiguration {
    /// Create default configuration for platform.
    pub fn default_for_platform(platform: &Platform) -> Self {
        BuildConfiguration {
            name: "default".to_string(),
            mode: CompilationMode::FastBuild,
            platform: platform.clone(),
            cflags: vec![],
            cxxflags: vec![],
            ldflags: vec![],
            rustflags: vec![],
            goflags: vec![],
            pythonflags: vec![],
            env: HashMap::new(),
            features: HashMap::new(),
            opt_level: OptLevel::O2,
            debug_level: DebugLevel::LineTablesOnly,
            lto: LtoMode::None,
            codegen: CodegenOptions::default(),
            target_overrides: HashMap::new(),
        }
    }

    /// Create configuration for specific mode.
    pub fn for_mode(mode: CompilationMode, platform: Platform) -> Self {
        let (opt_level, debug_level, lto) = match &mode {
            CompilationMode::Debug => (OptLevel::O0, DebugLevel::Full, LtoMode::None),
            CompilationMode::Release => (OptLevel::O3, DebugLevel::None, LtoMode::Thin),
            CompilationMode::RelWithDebInfo => (OptLevel::O2, DebugLevel::Full, LtoMode::None),
            CompilationMode::MinSizeRel => (OptLevel::Os, DebugLevel::None, LtoMode::Full),
            CompilationMode::FastBuild => (OptLevel::O1, DebugLevel::LineTablesOnly, LtoMode::None),
            CompilationMode::Custom(_) => (OptLevel::O2, DebugLevel::LineTablesOnly, LtoMode::None),
        };

        BuildConfiguration {
            name: format!("{:?}", mode).to_lowercase(),
            mode,
            platform,
            cflags: vec![],
            cxxflags: vec![],
            ldflags: vec![],
            rustflags: vec![],
            goflags: vec![],
            pythonflags: vec![],
            env: HashMap::new(),
            features: HashMap::new(),
            opt_level,
            debug_level,
            lto,
            codegen: CodegenOptions::default(),
            target_overrides: HashMap::new(),
        }
    }

    /// Apply target-specific override.
    fn apply_override(&mut self, override_cfg: &TargetConfig) {
        // Merge flags
        self.cflags.extend(override_cfg.flags.clone());

        // Merge environment
        for (key, value) in &override_cfg.env {
            self.env.insert(key.clone(), value.clone());
        }

        // Merge features
        for (key, value) in &override_cfg.features {
            self.features.insert(key.clone(), *value);
        }
    }

    /// Get compiler flags as strings.
    pub fn get_compiler_flags(&self, lang: &str) -> Vec<String> {
        let mut flags = vec![
            format!("-O{}", self.opt_level.as_flag()),
            self.debug_level.as_flag(),
        ];

        match lang {
            "c" => flags.extend(self.cflags.clone()),
            "c++" => flags.extend(self.cxxflags.clone()),
            "rust" => flags.extend(self.rustflags.clone()),
            "go" => flags.extend(self.goflags.clone()),
            _ => {}
        }

        if self.codegen.pic {
            flags.push("-fPIC".to_string());
        }

        flags
    }
}

impl Platform {
    /// Detect current platform.
    pub fn detect() -> Self {
        let os = if cfg!(target_os = "linux") {
            OsType::Linux
        } else if cfg!(target_os = "macos") {
            OsType::MacOS
        } else if cfg!(target_os = "windows") {
            OsType::Windows
        } else if cfg!(target_os = "freebsd") {
            OsType::FreeBSD
        } else {
            OsType::Custom(std::env::consts::OS.to_string())
        };

        let arch = if cfg!(target_arch = "x86_64") {
            ArchType::X86_64
        } else if cfg!(target_arch = "aarch64") {
            ArchType::Aarch64
        } else if cfg!(target_arch = "arm") {
            ArchType::Arm
        } else {
            ArchType::Custom(std::env::consts::ARCH.to_string())
        };

        let triple = format!("{}-{}-{}", arch.as_str(), "unknown", os.as_str());

        Platform {
            os,
            arch,
            abi: "gnu".to_string(),
            vendor: "unknown".to_string(),
            triple,
        }
    }

    /// Parse platform from target triple.
    pub fn from_triple(triple: &str) -> Result<Self> {
        let parts: Vec<&str> = triple.split('-').collect();

        if parts.len() < 3 {
            anyhow::bail!("Invalid target triple: {}", triple);
        }

        let arch = ArchType::from_str(parts[0])?;
        let vendor = parts[1].to_string();
        let os = OsType::from_str(parts[2])?;
        let abi = parts.get(3).unwrap_or(&"").to_string();

        Ok(Platform {
            os,
            arch,
            abi,
            vendor,
            triple: triple.to_string(),
        })
    }
}

impl OsType {
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "linux" => OsType::Linux,
            "darwin" | "macos" => OsType::MacOS,
            "windows" => OsType::Windows,
            "freebsd" => OsType::FreeBSD,
            _ => OsType::Custom(s.to_string()),
        })
    }

    fn as_str(&self) -> &str {
        match self {
            OsType::Linux => "linux",
            OsType::MacOS => "darwin",
            OsType::Windows => "windows",
            OsType::FreeBSD => "freebsd",
            OsType::OpenBSD => "openbsd",
            OsType::NetBSD => "netbsd",
            OsType::Android => "android",
            OsType::IOS => "ios",
            OsType::Custom(s) => s,
        }
    }
}

impl ArchType {
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "x86_64" | "amd64" => ArchType::X86_64,
            "aarch64" | "arm64" => ArchType::Aarch64,
            "arm" => ArchType::Arm,
            "armv7" => ArchType::Armv7,
            "i686" | "x86" => ArchType::I686,
            _ => ArchType::Custom(s.to_string()),
        })
    }

    fn as_str(&self) -> &str {
        match self {
            ArchType::X86_64 => "x86_64",
            ArchType::Aarch64 => "aarch64",
            ArchType::Arm => "arm",
            ArchType::Armv7 => "armv7",
            ArchType::I686 => "i686",
            ArchType::Mips => "mips",
            ArchType::Mips64 => "mips64",
            ArchType::PowerPC => "powerpc",
            ArchType::PowerPC64 => "powerpc64",
            ArchType::Riscv64 => "riscv64",
            ArchType::S390x => "s390x",
            ArchType::Wasm32 => "wasm32",
            ArchType::Custom(s) => s,
        }
    }
}

impl OptLevel {
    fn as_flag(&self) -> String {
        match self {
            OptLevel::O0 => "0".to_string(),
            OptLevel::O1 => "1".to_string(),
            OptLevel::O2 => "2".to_string(),
            OptLevel::O3 => "3".to_string(),
            OptLevel::Os => "s".to_string(),
            OptLevel::Oz => "z".to_string(),
        }
    }
}

impl DebugLevel {
    fn as_flag(&self) -> String {
        match self {
            DebugLevel::None => "-g0".to_string(),
            DebugLevel::LineTablesOnly => "-gline-tables-only".to_string(),
            DebugLevel::Full => "-g".to_string(),
            DebugLevel::FullWithChecks => "-g3".to_string(),
        }
    }
}

impl Default for CodegenOptions {
    fn default() -> Self {
        CodegenOptions {
            pic: true,
            pie: true,
            stack_protector: StackProtectorMode::Strong,
            cfi: false,
            safestack: false,
            shadow_call_stack: false,
            sanitizers: vec![],
            cpu: None,
            features: vec![],
        }
    }
}

/// Configuration builder for fluent API.
pub struct ConfigBuilder {
    config: BuildConfiguration,
}

impl ConfigBuilder {
    /// Start building a configuration.
    pub fn new(name: impl Into<String>) -> Self {
        let platform = Platform::detect();
        ConfigBuilder {
            config: BuildConfiguration {
                name: name.into(),
                mode: CompilationMode::FastBuild,
                platform,
                cflags: vec![],
                cxxflags: vec![],
                ldflags: vec![],
                rustflags: vec![],
                goflags: vec![],
                pythonflags: vec![],
                env: HashMap::new(),
                features: HashMap::new(),
                opt_level: OptLevel::O2,
                debug_level: DebugLevel::LineTablesOnly,
                lto: LtoMode::None,
                codegen: CodegenOptions::default(),
                target_overrides: HashMap::new(),
            },
        }
    }

    /// Set compilation mode.
    pub fn mode(mut self, mode: CompilationMode) -> Self {
        self.config.mode = mode;
        self
    }

    /// Set optimization level.
    pub fn opt_level(mut self, level: OptLevel) -> Self {
        self.config.opt_level = level;
        self
    }

    /// Set debug level.
    pub fn debug_level(mut self, level: DebugLevel) -> Self {
        self.config.debug_level = level;
        self
    }

    /// Add C flags.
    pub fn cflags(mut self, flags: Vec<String>) -> Self {
        self.config.cflags.extend(flags);
        self
    }

    /// Add C++ flags.
    pub fn cxxflags(mut self, flags: Vec<String>) -> Self {
        self.config.cxxflags.extend(flags);
        self
    }

    /// Set LTO mode.
    pub fn lto(mut self, mode: LtoMode) -> Self {
        self.config.lto = mode;
        self
    }

    /// Build the configuration.
    pub fn build(self) -> BuildConfiguration {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_detection() {
        let platform = Platform::detect();
        assert!(!platform.triple.is_empty());
    }

    #[test]
    fn test_platform_parsing() {
        let platform = Platform::from_triple("x86_64-unknown-linux-gnu").unwrap();
        assert_eq!(platform.arch, ArchType::X86_64);
        assert_eq!(platform.os, OsType::Linux);
    }

    #[test]
    fn test_config_manager() {
        let mut manager = ConfigManager::new();

        let debug_config = manager.for_mode(CompilationMode::Debug);
        assert_eq!(debug_config.mode, CompilationMode::Debug);

        manager.register("custom".to_string(), debug_config);
        assert!(manager.configs.contains_key("custom"));
    }

    #[test]
    fn test_config_builder() {
        let config = ConfigBuilder::new("test")
            .mode(CompilationMode::Release)
            .opt_level(OptLevel::O3)
            .lto(LtoMode::Full)
            .build();

        assert_eq!(config.mode, CompilationMode::Release);
        assert!(matches!(config.opt_level, OptLevel::O3));
    }
}
