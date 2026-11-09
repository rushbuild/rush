//! Typed native options used by declared `config_setting(values = ...)`.
//!
//! Defaults and converters follow Bazel 6.5.0's CoreOptions, CppOptions and
//! AppleCommandLineOptions. These are option values, not inferred meanings of
//! target names or claims that a cross-platform toolchain is installed.
//!
//! Pinned source: https://github.com/bazelbuild/bazel/tree/6.5.0/src/main/java/com/google/devtools/build/lib
//! (`rules/apple/AppleCommandLineOptions.java`, `rules/cpp/CppOptions.java`,
//! and `analysis/config/CoreOptions.java`).
//!
//! Execution scope: `compilation_mode` and ordered `copt` values also configure
//! Rush's C/C++ command flags. CPU/Apple/toolchain options, coverage, stamping,
//! feature and strip values configure declared conditions. Implementing the
//! associated cross-toolchain, instrumentation, stamp-data, feature-expansion
//! and strip actions remains separate rule/toolchain work.

use crate::CommandSemantics;
use std::collections::BTreeMap;

/// Client-expanded, ordered `.bazelrc` and command-line native flags, as JSON.
pub const EFFECTIVE_NATIVE_FLAGS_ENV: &str = "RUSH_EFFECTIVE_NATIVE_FLAGS";

/// Native configuration inputs currently understood by Rush's selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeConfiguration {
    pub cpu: String,
    pub compilation_mode: String,
    pub apple_platform_type: String,
    pub crosstool_top: String,
    pub stamp: bool,
    pub copts: Vec<String>,
    pub defines: BTreeMap<String, String>,
    pub collect_code_coverage: bool,
    pub features: Vec<String>,
    pub strip: String,
}

impl Default for NativeConfiguration {
    fn default() -> Self {
        Self {
            cpu: default_cpu().to_string(),
            compilation_mode: "fastbuild".to_string(),
            apple_platform_type: "macos".to_string(),
            crosstool_top: "@bazel_tools//tools/cpp:toolchain".to_string(),
            stamp: false,
            copts: Vec::new(),
            defines: BTreeMap::new(),
            collect_code_coverage: false,
            features: Vec::new(),
            strip: "sometimes".to_string(),
        }
    }
}

fn default_cpu() -> &'static str {
    // Bazel 6.5.0 analysis/config/AutoCpuConverter.java.
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin_arm64",
        ("macos", _) => "darwin",
        ("windows", "aarch64") => "arm64_windows",
        ("windows", _) => "x64_windows",
        ("freebsd", _) => "freebsd",
        (_, "x86_64") => "k8",
        (_, "x86") => "piii",
        (_, "aarch64") => "aarch64",
        (_, "arm") => "arm",
        (_, "powerpc64") => "ppc",
        (_, "s390x") => "s390x",
        (_, "riscv64") => "riscv64",
        _ => "unknown",
    }
}

/// True for typed options supported in native configuration predicates.
pub fn supported_native_option(name: &str) -> bool {
    matches!(
        name,
        "cpu"
            | "compilation_mode"
            | "apple_platform_type"
            | "crosstool_top"
            | "stamp"
            | "copt"
            | "define"
            | "collect_code_coverage"
            | "features"
            | "strip"
    )
}

/// True when a flag is one of the native inputs that must retain rc ordering.
pub fn is_native_configuration_flag(flag: &str) -> bool {
    flag == "-c"
        || flag.starts_with("-c=")
        || matches!(flag, "--nostamp" | "--nocollect_code_coverage")
        || flag.strip_prefix("--").is_some_and(|name| {
            let name = name.split('=').next().unwrap_or(name);
            supported_native_option(name) || name == "compilation-mode"
        })
}

fn parse_bool(value: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "t" | "y" => Ok(true),
        "0" | "false" | "no" | "f" | "n" => Ok(false),
        _ => Err(format!("{value:?} is not a boolean")),
    }
}

fn enum_value(value: &str, accepted: &[&str], name: &str) -> Result<String, String> {
    let normalized = value.to_ascii_lowercase();
    if accepted.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(format!(
            "invalid --{name}={value:?}; expected {}",
            accepted.join(", ")
        ))
    }
}

fn normalize_label(label: &str) -> Result<String, String> {
    // Native option converters use the main command's package, including
    // predicates declared in an external repository. They do not inherit a
    // .bzl Label() constructor's defining repository or its repo_mapping.
    let absolute = if !label.starts_with('@') && !label.starts_with("//") {
        format!("//{label}")
    } else {
        label.to_string()
    };
    let invalid = |reason| format!("invalid native label option {label:?}: {reason}");
    let (repository, package_and_target, shorthand_target) = if let Some(external) = absolute
        .strip_prefix("@@")
        .or_else(|| absolute.strip_prefix('@'))
    {
        if let Some((repository, body)) = external.split_once("//") {
            (repository, body, None)
        } else {
            (external, "", Some(external))
        }
    } else {
        (
            "",
            absolute
                .strip_prefix("//")
                .ok_or_else(|| invalid("missing // prefix"))?,
            None,
        )
    };
    // Bazel 6.5.0 cmdline/RepositoryName.java validates canonical names too.
    if matches!(repository, "." | "..")
        || repository.starts_with('~')
        || !repository.bytes().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, b'_' | b'-' | b'.' | b'~')
        })
    {
        return Err(invalid("invalid repository name"));
    }
    let (package, target) = if let Some(target) = shorthand_target {
        ("", target)
    } else if let Some(parts) = package_and_target.split_once(':') {
        parts
    } else {
        (
            package_and_target,
            package_and_target.rsplit('/').next().unwrap_or(""),
        )
    };
    // Package components must be nonempty and contain more than dots. Bazel
    // permits printable ASCII (including spaces), excluding ':' and '\\'.
    if !package.is_empty()
        && (package
            .split('/')
            .any(|part| part.is_empty() || part.chars().all(|character| character == '.'))
            || !package.bytes().all(|character| {
                (b' '..=b'~').contains(&character) && !matches!(character, b':' | b'\\')
            }))
    {
        return Err(invalid("invalid package path or package characters"));
    }
    // LabelValidator.java also allows Unicode letters/digits and Latin-1
    // bytes in target names. Colons, backslashes and ASCII controls fail.
    static TARGET_CHARACTERS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let target_characters = TARGET_CHARACTERS.get_or_init(|| {
        regex::Regex::new(r"\A[\x20-\x39\x3b-\x5b\x5d-\x7e\x{80}-\x{ff}\p{L}\p{Nd}]+\z")
            .expect("fixed Bazel target-character expression")
    });
    if !target_characters.is_match(target) {
        return Err(invalid("invalid or empty target name"));
    }
    let parts: Vec<_> = target.split('/').collect();
    if parts.iter().enumerate().any(|(index, part)| {
        part.is_empty() || *part == ".." || (*part == "." && index + 1 != parts.len())
    }) {
        return Err(invalid("invalid target path components"));
    }
    // The pinned parser accepts a final '/.' for directory labels and removes
    // that suffix. A target consisting solely of '.' remains valid.
    let target = target.strip_suffix("/.").unwrap_or(target);
    let prefix = if repository.is_empty() {
        String::new()
    } else {
        format!("@{repository}")
    };
    Ok(format!("{prefix}//{package}:{target}"))
}

/// Canonical predicate identity, shared by matching and select specialization.
pub fn canonical_native_predicate(name: &str, value: &str) -> Result<String, String> {
    match name {
        "cpu" => Ok(if value.is_empty() {
            default_cpu().to_string()
        } else {
            value.to_string()
        }),
        "compilation_mode" => enum_value(value, &["fastbuild", "opt", "dbg"], name),
        "apple_platform_type" => enum_value(
            value,
            &["macos", "ios", "tvos", "watchos", "visionos", "catalyst"],
            name,
        ),
        "crosstool_top" => normalize_label(value),
        "stamp" => Ok(parse_bool(value)?.to_string()),
        "copt" => Ok(value.to_string()),
        "define" => {
            let (key, _) = value.split_once('=').ok_or_else(|| {
                format!("native define predicate requires KEY=VALUE, got {value:?}")
            })?;
            if key.is_empty() {
                return Err("native define key must not be empty".to_string());
            }
            Ok(value.to_string())
        }
        "collect_code_coverage" => Ok(parse_bool(value)?.to_string()),
        "features" => Ok(value.to_string()),
        "strip" => enum_value(value, &["always", "never", "sometimes"], name),
        _ => Err(format!("unsupported native configuration option: {name}")),
    }
}

impl NativeConfiguration {
    /// Read only the immutable client snapshot. A daemon's own environment is
    /// never a source of native option values.
    pub fn from_command_semantics(semantics: &CommandSemantics) -> Result<Self, String> {
        let tokens: Vec<String> =
            if let Some(raw) = semantics.getenv(EFFECTIVE_NATIVE_FLAGS_ENV.as_bytes()) {
                serde_json::from_slice(raw)
                    .map_err(|error| format!("invalid {EFFECTIVE_NATIVE_FLAGS_ENV}: {error}"))?
            } else {
                semantics
                    .argv()
                    .iter()
                    .map(|token| String::from_utf8_lossy(token).into_owned())
                    .collect()
            };
        let mut configuration = Self::from_tokens(&tokens)?;
        let mut defines = BTreeMap::new();
        if let Some(inherited) = semantics.getenv(b"RUSH_DEFINES") {
            let inherited = std::str::from_utf8(inherited)
                .map_err(|error| format!("invalid RUSH_DEFINES: {error}"))?;
            for token in inherited
                .split([',', ' '])
                .filter(|token| !token.is_empty())
            {
                if let Some((key, value)) = token.split_once('=') {
                    defines.insert(key.trim().to_string(), value.trim().to_string());
                }
            }
        }
        for define in semantics.defines() {
            defines.insert(define.key.clone(), define.value.clone());
        }
        defines.extend(configuration.defines);
        configuration.defines = defines;
        Ok(configuration)
    }

    /// Parse ordered flags. Scalar options use the last value; repeatable
    /// compiler options retain every occurrence and its order.
    pub fn from_tokens(tokens: &[String]) -> Result<Self, String> {
        let mut configuration = Self::default();
        let mut index = 0;
        while index < tokens.len() {
            let token = &tokens[index];
            if token == "--" {
                break;
            }
            if token == "--nostamp" {
                configuration.stamp = false;
                index += 1;
                continue;
            }
            if token == "--nocollect_code_coverage" {
                configuration.collect_code_coverage = false;
                index += 1;
                continue;
            }
            if !is_native_configuration_flag(token) {
                index += 1;
                continue;
            }
            let (name, inline) = token
                .split_once('=')
                .map(|(name, value)| (name, Some(value)))
                .unwrap_or((token.as_str(), None));
            let name = if matches!(name, "-c" | "--compilation-mode") {
                "compilation_mode"
            } else {
                name.trim_start_matches('-')
            };
            let value = if let Some(value) = inline {
                value
            } else if matches!(name, "stamp" | "collect_code_coverage") {
                "true"
            } else {
                index += 1;
                tokens
                    .get(index)
                    .ok_or_else(|| format!("--{name} requires a value"))?
            };
            match name {
                "cpu" => {
                    configuration.cpu = if value.is_empty() {
                        default_cpu().to_string()
                    } else {
                        value.to_string()
                    }
                }
                "compilation_mode" => {
                    configuration.compilation_mode =
                        enum_value(value, &["fastbuild", "opt", "dbg"], name)?
                }
                "apple_platform_type" => {
                    configuration.apple_platform_type = enum_value(
                        value,
                        &["macos", "ios", "tvos", "watchos", "visionos", "catalyst"],
                        name,
                    )?
                }
                "crosstool_top" => configuration.crosstool_top = normalize_label(value)?,
                "stamp" => configuration.stamp = parse_bool(value)?,
                "copt" => configuration.copts.push(value.to_string()),
                "define" => {
                    let assignment = canonical_native_predicate(name, value)?;
                    let (key, value) = assignment.split_once('=').unwrap();
                    configuration
                        .defines
                        .insert(key.to_string(), value.to_string());
                }
                "collect_code_coverage" => configuration.collect_code_coverage = parse_bool(value)?,
                "features" => configuration.features.push(value.to_string()),
                "strip" => configuration.strip = canonical_native_predicate(name, value)?,
                _ => unreachable!("supported flag was classified above"),
            }
            index += 1;
        }
        Ok(configuration)
    }

    /// Compare a declared predicate using the same converter as the option.
    /// Unknown option names and malformed expected values remain errors.
    pub fn matches(&self, name: &str, expected: &str) -> Result<bool, String> {
        let expected = canonical_native_predicate(name, expected)?;
        Ok(match name {
            "cpu" => self.cpu == expected,
            "compilation_mode" => self.compilation_mode == expected,
            "apple_platform_type" => self.apple_platform_type == expected,
            "crosstool_top" => self.crosstool_top == expected,
            "stamp" => self.stamp.to_string() == expected,
            "copt" => self.copts.iter().any(|value| value == &expected),
            "define" => {
                let (key, value) = expected.split_once('=').unwrap();
                self.defines.get(key).is_some_and(|actual| actual == value)
            }
            "collect_code_coverage" => self.collect_code_coverage.to_string() == expected,
            "features" => self.features.iter().any(|value| value == &expected),
            "strip" => self.strip == expected,
            _ => return Err(format!("unsupported native configuration option: {name}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_defaults_are_option_values_even_on_non_apple_hosts() {
        let configuration = NativeConfiguration::default();
        assert!(configuration
            .matches("apple_platform_type", "macos")
            .unwrap());
        assert!(!configuration.matches("apple_platform_type", "ios").unwrap());
        assert!(configuration
            .matches("compilation_mode", "fastbuild")
            .unwrap());
        assert!(configuration.matches("stamp", "0").unwrap());
        assert!(configuration
            .matches("collect_code_coverage", "false")
            .unwrap());
        assert!(configuration.matches("strip", "sometimes").unwrap());
        assert!(!configuration.matches("features", "one").unwrap());
        assert!(!configuration.matches("define", "K=A").unwrap());
        assert!(configuration
            .matches("crosstool_top", "@@bazel_tools//tools/cpp:toolchain")
            .unwrap());
        assert!(!configuration.matches("copt", "-DUNSET").unwrap());
        assert!(configuration
            .matches("invented_option", "anything")
            .is_err());
        assert!(configuration
            .matches("apple_platform_type", "invented")
            .is_err());
    }

    #[test]
    fn native_flags_use_scalar_precedence_and_repeatable_membership() {
        let tokens = [
            "--apple_platform_type=ios",
            "-c",
            "opt",
            "--compilation_mode=dbg",
            "--cpu=aarch64",
            "--stamp=1",
            "--nostamp",
            "--copt=-DFIRST",
            "--copt",
            "-DSECOND",
            "--",
            "--apple_platform_type=macos",
        ]
        .map(str::to_string);
        let configuration = NativeConfiguration::from_tokens(&tokens).unwrap();
        assert!(configuration.matches("apple_platform_type", "IOS").unwrap());
        assert!(configuration.matches("compilation_mode", "dbg").unwrap());
        assert!(configuration.matches("cpu", "aarch64").unwrap());
        assert!(configuration.matches("stamp", "false").unwrap());
        assert!(configuration.matches("copt", "-DFIRST").unwrap());
        assert!(configuration.matches("copt", "-DSECOND").unwrap());
        assert!(!configuration.matches("copt", "-DFIRST,-DSECOND").unwrap());
        assert!(NativeConfiguration::from_tokens(&["--apple_platform_type=linux".into()]).is_err());
        assert!(NativeConfiguration::from_tokens(&["--cpu".into()]).is_err());
    }

    #[test]
    fn compilation_mode_spellings_share_precedence_and_command_reset() {
        for tokens in [
            vec!["--compilation-mode=opt", "--compilation_mode=dbg"],
            vec!["--compilation_mode=opt", "--compilation-mode", "dbg"],
        ] {
            let configuration = NativeConfiguration::from_tokens(
                &tokens.into_iter().map(str::to_string).collect::<Vec<_>>(),
            )
            .unwrap();
            assert_eq!(configuration.compilation_mode, "dbg");
        }
        assert_eq!(
            NativeConfiguration::from_tokens(&[])
                .unwrap()
                .compilation_mode,
            "fastbuild"
        );
    }

    #[test]
    fn native_label_converter_uses_command_line_package_shorthand() {
        for (input, expected) in [
            ("tools:custom", "//tools:custom"),
            ("tools/cpp", "//tools/cpp:cpp"),
            (":tc", "//:tc"),
        ] {
            let configuration =
                NativeConfiguration::from_tokens(&[format!("--crosstool_top={input}")]).unwrap();
            assert_eq!(configuration.crosstool_top, expected);
            assert!(configuration.matches("crosstool_top", expected).unwrap());
            assert!(configuration.matches("crosstool_top", input).unwrap());
            assert_eq!(
                canonical_native_predicate("crosstool_top", input).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn native_label_options_and_conditions_reject_invalid_components() {
        for invalid in [
            "//bad/../pkg:tc",
            "//pkg//sub:tc",
            "/absolute:tc",
            "//./pkg:tc",
            "//.../pkg:tc",
            "//pkg/:tc",
            "//pkg:",
            "//pkg:../tc",
            "//pkg:sub/../tc",
            "//pkg:sub/./tc",
            "//pkg:sub//tc",
            "//pkg:/tc",
            "//pkg:tc/",
            "//pkg:tc:other",
            "//pkg:tc\\other",
            "//pkg:tc\n",
            "//pkg:tc\u{7f}",
            "//páckage:tc",
            "//pkg:😀",
            "@bad/name//pkg:tc",
            "@~repo//pkg:tc",
            "@..//pkg:tc",
            "@repo+name//pkg:tc",
        ] {
            assert!(
                NativeConfiguration::from_tokens(&[format!("--crosstool_top={invalid}")]).is_err(),
                "accepted option {invalid:?}"
            );
            assert!(
                NativeConfiguration::default()
                    .matches("crosstool_top", invalid)
                    .is_err(),
                "accepted condition {invalid:?}"
            );
        }
    }

    #[test]
    fn native_label_validation_preserves_valid_bazel_names_and_canonicalization() {
        for (input, canonical) in [
            ("//some pkg:some target", "//some pkg:some target"),
            ("//pkg:dir.with.dots/.hidden", "//pkg:dir.with.dots/.hidden"),
            ("//pkg:sub/...", "//pkg:sub/..."),
            ("//pkg:sub/.", "//pkg:sub"),
            ("//pkg:.", "//pkg:."),
            ("//pkg:é_α_中_١", "//pkg:é_α_中_١"),
            ("@dep~1.0//pkg:tc", "@dep~1.0//pkg:tc"),
            ("@@dep//pkg:tc", "@dep//pkg:tc"),
            ("@repo", "@repo//:repo"),
            ("@@repo", "@repo//:repo"),
            ("@//:tc", "//:tc"),
            ("@@//:tc", "//:tc"),
        ] {
            let configuration =
                NativeConfiguration::from_tokens(&[format!("--crosstool_top={input}")]).unwrap();
            assert_eq!(configuration.crosstool_top, canonical);
            assert!(configuration.matches("crosstool_top", input).unwrap());
            assert!(configuration.matches("crosstool_top", canonical).unwrap());
        }
    }

    #[test]
    fn native_dependency_options_use_typed_defaults_and_real_membership() {
        let tokens = [
            "--define=K=A",
            "--define",
            "K=B",
            "--features=one",
            "--features=two",
            "--collect_code_coverage",
            "--strip=never",
            "--crosstool_top=:tc",
        ]
        .map(str::to_string);
        let configuration = NativeConfiguration::from_tokens(&tokens).unwrap();
        assert!(!configuration.matches("define", "K=A").unwrap());
        assert!(configuration.matches("define", "K=B").unwrap());
        assert!(configuration.matches("features", "one").unwrap());
        assert!(configuration.matches("features", "two").unwrap());
        assert!(!configuration.matches("features", "one,two").unwrap());
        assert!(configuration.matches("collect_code_coverage", "1").unwrap());
        assert!(configuration.matches("strip", "never").unwrap());
        assert!(configuration.matches("crosstool_top", "//:tc").unwrap());
        assert!(configuration.matches("crosstool_top", ":tc").unwrap());
        assert!(!configuration
            .matches("crosstool_top", "@owner//:tc")
            .unwrap());
        assert!(configuration.matches("define", "invalid").is_err());
        assert!(configuration.matches("strip", "invalid").is_err());
    }

    #[test]
    fn native_command_snapshot_covers_space_values_and_client_expansion() {
        use bytes::Bytes;
        use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
        use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};
        let mut request = RunRequest {
            auth_cookie: Bytes::new(),
            argv: ["rush", "build", "//:root", "--cpu", "first_cpu"]
                .map(|value| Bytes::from_static(value.as_bytes()))
                .to_vec(),
            working_directory: Bytes::from_static(b"/workspace"),
            environment: Vec::new(),
            block_for_lock: true,
            client_build_id: String::new(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            workspace_config: Vec::new(),
        };
        let first = CommandSemantics::from_run_request(&request).unwrap();
        assert!(NativeConfiguration::from_command_semantics(&first)
            .unwrap()
            .matches("cpu", "first_cpu")
            .unwrap());
        request.argv[4] = Bytes::from_static(b"second_cpu");
        let second = CommandSemantics::from_run_request(&request).unwrap();
        assert_ne!(
            first.config_fingerprint(),
            second.config_fingerprint(),
            "space-form values must invalidate retained analysis"
        );
        let tokens = vec![
            "--apple_platform_type=ios",
            "--copt=-DVALUE=two words",
            "--apple_platform_type=macos",
        ];
        request.environment.push(EnvironmentVariable {
            name: Bytes::from_static(EFFECTIVE_NATIVE_FLAGS_ENV.as_bytes()),
            value: Bytes::from(serde_json::to_vec(&tokens).unwrap()),
        });
        let expanded = CommandSemantics::from_run_request(&request).unwrap();
        let configuration = NativeConfiguration::from_command_semantics(&expanded).unwrap();
        assert!(configuration
            .matches("apple_platform_type", "macos")
            .unwrap());
        assert_eq!(configuration.copts, ["-DVALUE=two words"]);
        assert_eq!(
            configuration.cpu,
            NativeConfiguration::default().cpu,
            "expanded command stream is authoritative, not daemon or argv fallback"
        );
        assert_ne!(second.config_fingerprint(), expanded.config_fingerprint());
    }
}
