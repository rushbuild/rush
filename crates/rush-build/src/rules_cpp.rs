// Rush Build - C++ Build Rules
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! C++ build rules for compiling C++ code.
//!
//! Provides rules for building C++ libraries, binaries, and tests with
//! support for modern C++ standards, compiler flags, and dependency management.

use anyhow::Result;
use rush_action::{Action, Artifact, Command};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// C++ compiler type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CppCompiler {
    /// GCC compiler.
    Gcc,

    /// Clang compiler.
    Clang,

    /// MSVC compiler.
    Msvc,
}

impl CppCompiler {
    /// Get compiler executable name.
    pub fn executable(&self) -> &str {
        match self {
            CppCompiler::Gcc => "g++",
            CppCompiler::Clang => "clang++",
            CppCompiler::Msvc => "cl.exe",
        }
    }

    /// Get C compiler name (for mixed C/C++ projects).
    pub fn c_executable(&self) -> &str {
        match self {
            CppCompiler::Gcc => "gcc",
            CppCompiler::Clang => "clang",
            CppCompiler::Msvc => "cl.exe",
        }
    }
}

/// C++ standard version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CppStandard {
    /// C++11
    Cpp11,
    /// C++14
    Cpp14,
    /// C++17
    Cpp17,
    /// C++20
    Cpp20,
    /// C++23
    Cpp23,
}

impl CppStandard {
    /// Get compiler flag for this standard.
    pub fn flag(&self, compiler: CppCompiler) -> String {
        let std_name = match self {
            CppStandard::Cpp11 => "c++11",
            CppStandard::Cpp14 => "c++14",
            CppStandard::Cpp17 => "c++17",
            CppStandard::Cpp20 => "c++20",
            CppStandard::Cpp23 => "c++23",
        };

        match compiler {
            CppCompiler::Msvc => format!("/std:{}", std_name),
            _ => format!("-std={}", std_name),
        }
    }
}

/// C++ library linkage type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkageType {
    /// Static library (.a or .lib).
    Static,

    /// Shared library (.so, .dylib, or .dll).
    Shared,
}

/// C++ library rule attributes.
#[derive(Debug, Clone)]
pub struct CppLibraryAttrs {
    /// Library name.
    pub name: String,

    /// Source files.
    pub srcs: Vec<PathBuf>,

    /// Header files (public interface).
    pub hdrs: Vec<PathBuf>,

    /// Include directories.
    pub includes: Vec<PathBuf>,

    /// Dependencies.
    pub deps: Vec<String>,

    /// Compiler flags.
    pub copts: Vec<String>,

    /// Linker flags.
    pub linkopts: Vec<String>,

    /// Defines (-D flags).
    pub defines: Vec<String>,

    /// C++ standard.
    pub std: CppStandard,

    /// Linkage type.
    pub linkage: LinkageType,

    /// Visibility (alwayslink, etc.).
    pub visibility: Vec<String>,
}

impl CppLibraryAttrs {
    /// Create a new C++ library with default attributes.
    pub fn new(name: String) -> Self {
        CppLibraryAttrs {
            name,
            srcs: Vec::new(),
            hdrs: Vec::new(),
            includes: Vec::new(),
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            std: CppStandard::Cpp17,
            linkage: LinkageType::Static,
            visibility: Vec::new(),
        }
    }
}

/// C++ binary rule attributes.
#[derive(Debug, Clone)]
pub struct CppBinaryAttrs {
    /// Binary name.
    pub name: String,

    /// Source files.
    pub srcs: Vec<PathBuf>,

    /// Dependencies.
    pub deps: Vec<String>,

    /// Compiler flags.
    pub copts: Vec<String>,

    /// Linker flags.
    pub linkopts: Vec<String>,

    /// Defines.
    pub defines: Vec<String>,

    /// C++ standard.
    pub std: CppStandard,
}

impl CppBinaryAttrs {
    /// Create a new C++ binary with default attributes.
    pub fn new(name: String) -> Self {
        CppBinaryAttrs {
            name,
            srcs: Vec::new(),
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            std: CppStandard::Cpp17,
        }
    }
}

/// C++ build rules implementation.
pub struct CppRules {
    /// Compiler to use.
    compiler: CppCompiler,

    /// Output directory.
    output_dir: PathBuf,
}

impl CppRules {
    /// Create a new C++ rules instance.
    pub fn new(compiler: CppCompiler, output_dir: PathBuf) -> Self {
        CppRules {
            compiler,
            output_dir,
        }
    }

    /// Build a C++ library.
    pub fn build_library(&self, attrs: &CppLibraryAttrs) -> Result<Vec<Action>> {
        let mut actions = Vec::new();

        // Compile each source file to object file
        let mut object_files = Vec::new();
        for src in &attrs.srcs {
            let obj_path = self.output_dir.join(format!(
                "{}/{}.o",
                attrs.name,
                src.file_stem().unwrap().to_string_lossy()
            ));

            let action = self.compile_source(
                src,
                &obj_path,
                &attrs.includes,
                &attrs.copts,
                &attrs.defines,
                attrs.std,
            )?;

            actions.push(action);
            object_files.push(obj_path);
        }

        // Link object files into library
        let lib_path = match attrs.linkage {
            LinkageType::Static => self.output_dir.join(format!("lib{}.a", attrs.name)),
            LinkageType::Shared => {
                if cfg!(target_os = "macos") {
                    self.output_dir.join(format!("lib{}.dylib", attrs.name))
                } else if cfg!(target_os = "windows") {
                    self.output_dir.join(format!("{}.dll", attrs.name))
                } else {
                    self.output_dir.join(format!("lib{}.so", attrs.name))
                }
            }
        };

        let link_action =
            self.link_library(&object_files, &lib_path, &attrs.linkopts, attrs.linkage)?;

        actions.push(link_action);

        Ok(actions)
    }

    /// Build a C++ binary.
    pub fn build_binary(&self, attrs: &CppBinaryAttrs) -> Result<Vec<Action>> {
        let mut actions = Vec::new();

        // Compile each source file
        let mut object_files = Vec::new();
        for src in &attrs.srcs {
            let obj_path = self.output_dir.join(format!(
                "{}/{}.o",
                attrs.name,
                src.file_stem().unwrap().to_string_lossy()
            ));

            let action =
                self.compile_source(src, &obj_path, &[], &attrs.copts, &attrs.defines, attrs.std)?;

            actions.push(action);
            object_files.push(obj_path);
        }

        // Link into binary
        let bin_path = if cfg!(target_os = "windows") {
            self.output_dir.join(format!("{}.exe", attrs.name))
        } else {
            self.output_dir.join(&attrs.name)
        };

        let link_action = self.link_binary(&object_files, &bin_path, &attrs.linkopts)?;

        actions.push(link_action);

        Ok(actions)
    }

    /// Compile a single source file.
    ///
    /// Generates dependency files (.d) for automatic header dependency tracking,
    /// following Ninja's philosophy of correctness through compiler-generated deps.
    fn compile_source(
        &self,
        src: &Path,
        output: &Path,
        includes: &[PathBuf],
        copts: &[String],
        defines: &[String],
        std: CppStandard,
    ) -> Result<Action> {
        let mut args = Vec::new();

        // Add standard flag
        args.push(std.flag(self.compiler));

        // Add include directories
        for include in includes {
            match self.compiler {
                CppCompiler::Msvc => args.push(format!("/I{}", include.display())),
                _ => args.push(format!("-I{}", include.display())),
            }
        }

        // Add defines
        for define in defines {
            match self.compiler {
                CppCompiler::Msvc => args.push(format!("/D{}", define)),
                _ => args.push(format!("-D{}", define)),
            }
        }

        // Generate dependency file for header tracking (Ninja-style)
        // This enables automatic incremental rebuilds when headers change
        let dep_file = output.with_extension("d");
        match self.compiler {
            CppCompiler::Msvc => {
                // MSVC: /showIncludes outputs dependencies to stdout
                // Rush build system should parse this output
                // Future work: Implement /showIncludes parsing in executor
            }
            CppCompiler::Gcc | CppCompiler::Clang => {
                // GCC/Clang: Generate .d file with header dependencies
                // -MMD: Generate dependency file, exclude system headers
                // -MF: Specify dependency file location
                // -MT: Specify target name in dependency file
                args.push("-MMD".to_string());
                args.push("-MF".to_string());
                args.push(dep_file.display().to_string());
                args.push("-MT".to_string());
                args.push(output.display().to_string());
            }
        }

        // Add custom compiler flags
        args.extend(copts.iter().cloned());

        // Add output flag
        match self.compiler {
            CppCompiler::Msvc => {
                args.push("/c".to_string());
                args.push(format!("/Fo{}", output.display()));
            }
            _ => {
                args.push("-c".to_string());
                args.push("-o".to_string());
                args.push(output.display().to_string());
            }
        }

        // Add source file
        args.push(src.display().to_string());

        let command = Command {
            executable: self.compiler.executable().into(),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let inputs = vec![Artifact::Source(src.to_path_buf())];

        // Generate outputs: object file + dependency file
        let mut outputs = vec![Artifact::Source(output.to_path_buf())];
        if !matches!(self.compiler, CppCompiler::Msvc) {
            outputs.push(Artifact::Source(dep_file));
        }

        let env = HashMap::new();
        let action_id = Action::compute_id(&inputs, &command, &env)?;

        Ok(Action {
            id: action_id,
            inputs,
            outputs,
            command,
            env,
            mnemonic: "CppCompile".to_string(),
        })
    }

    /// Link object files into a library.
    fn link_library(
        &self,
        objects: &[PathBuf],
        output: &Path,
        linkopts: &[String],
        linkage: LinkageType,
    ) -> Result<Action> {
        let mut args = Vec::new();

        match (self.compiler, linkage) {
            (CppCompiler::Msvc, LinkageType::Static) => {
                // MSVC static library (use lib.exe)
                args.push(format!("/OUT:{}", output.display()));
                for obj in objects {
                    args.push(obj.display().to_string());
                }
            }
            (CppCompiler::Msvc, LinkageType::Shared) => {
                // MSVC DLL
                args.push("/DLL".to_string());
                args.push(format!("/OUT:{}", output.display()));
                args.extend(linkopts.iter().cloned());
                for obj in objects {
                    args.push(obj.display().to_string());
                }
            }
            (_, LinkageType::Static) => {
                // Unix static library (use ar)
                args.push("rcs".to_string());
                args.push(output.display().to_string());
                for obj in objects {
                    args.push(obj.display().to_string());
                }
            }
            (_, LinkageType::Shared) => {
                // Unix shared library
                args.push("-shared".to_string());
                args.push("-o".to_string());
                args.push(output.display().to_string());
                args.extend(linkopts.iter().cloned());
                for obj in objects {
                    args.push(obj.display().to_string());
                }
            }
        }

        let executable = match (self.compiler, linkage) {
            (CppCompiler::Msvc, LinkageType::Static) => "lib.exe",
            (_, LinkageType::Static) => "ar",
            _ => self.compiler.executable(),
        };

        let command = Command {
            executable: PathBuf::from(executable),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let inputs: Vec<_> = objects
            .iter()
            .map(|p| Artifact::Source(p.clone()))
            .collect();

        let env = HashMap::new();
        let action_id = Action::compute_id(&inputs, &command, &env)?;

        Ok(Action {
            id: action_id,
            inputs,
            outputs: vec![Artifact::Source(output.to_path_buf())],
            command,
            env,
            mnemonic: "CppLink".to_string(),
        })
    }

    /// Link object files into a binary.
    fn link_binary(
        &self,
        objects: &[PathBuf],
        output: &Path,
        linkopts: &[String],
    ) -> Result<Action> {
        let mut args = Vec::new();

        match self.compiler {
            CppCompiler::Msvc => {
                args.push(format!("/Fe{}", output.display()));
                args.extend(linkopts.iter().cloned());
                for obj in objects {
                    args.push(obj.display().to_string());
                }
            }
            _ => {
                args.push("-o".to_string());
                args.push(output.display().to_string());
                args.extend(linkopts.iter().cloned());
                for obj in objects {
                    args.push(obj.display().to_string());
                }
            }
        }

        let command = Command {
            executable: self.compiler.executable().into(),
            args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };

        let inputs: Vec<_> = objects
            .iter()
            .map(|p| Artifact::Source(p.clone()))
            .collect();

        let env = HashMap::new();
        let action_id = Action::compute_id(&inputs, &command, &env)?;

        Ok(Action {
            id: action_id,
            inputs,
            outputs: vec![Artifact::Source(output.to_path_buf())],
            command,
            env,
            mnemonic: "CppLinkBinary".to_string(),
        })
    }

    /// Detect optimal compiler flags for the current platform.
    pub fn detect_platform_flags(&self) -> Vec<String> {
        let mut flags = Vec::new();

        match self.compiler {
            CppCompiler::Gcc | CppCompiler::Clang => {
                // Enable optimizations
                flags.push("-O2".to_string());

                // Enable warnings
                flags.push("-Wall".to_string());
                flags.push("-Wextra".to_string());

                // Position independent code for shared libraries
                flags.push("-fPIC".to_string());

                // Debug info
                flags.push("-g".to_string());
            }
            CppCompiler::Msvc => {
                // Enable optimizations
                flags.push("/O2".to_string());

                // Enable warnings
                flags.push("/W4".to_string());

                // Debug info
                flags.push("/Zi".to_string());
            }
        }

        flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpp_compiler() {
        assert_eq!(CppCompiler::Gcc.executable(), "g++");
        assert_eq!(CppCompiler::Clang.executable(), "clang++");
        assert_eq!(CppCompiler::Gcc.c_executable(), "gcc");
    }

    #[test]
    fn test_cpp_standard_flags() {
        assert_eq!(CppStandard::Cpp17.flag(CppCompiler::Gcc), "-std=c++17");
        assert_eq!(CppStandard::Cpp20.flag(CppCompiler::Clang), "-std=c++20");
        assert_eq!(CppStandard::Cpp17.flag(CppCompiler::Msvc), "/std:c++17");
    }

    #[test]
    fn test_library_attrs() {
        let mut attrs = CppLibraryAttrs::new("mylib".to_string());
        attrs.srcs.push(PathBuf::from("src/lib.cpp"));
        attrs.hdrs.push(PathBuf::from("include/lib.h"));

        assert_eq!(attrs.name, "mylib");
        assert_eq!(attrs.srcs.len(), 1);
        assert_eq!(attrs.hdrs.len(), 1);
    }

    #[test]
    fn test_binary_attrs() {
        let mut attrs = CppBinaryAttrs::new("myapp".to_string());
        attrs.srcs.push(PathBuf::from("src/main.cpp"));

        assert_eq!(attrs.name, "myapp");
        assert_eq!(attrs.srcs.len(), 1);
    }

    #[test]
    fn test_platform_flags() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let rules = CppRules::new(CppCompiler::Gcc, temp_dir.path().to_path_buf());

        let flags = rules.detect_platform_flags();
        assert!(flags.contains(&"-O2".to_string()));
        assert!(flags.contains(&"-Wall".to_string()));
        assert!(flags.contains(&"-fPIC".to_string()));
    }
}
