//! Strict ingestion of dependency receipts emitted by Linux Kbuild.
//!
//! This is deliberately not a Make evaluator. Kbuild's `fixdep` helper emits a
//! small receipt containing the command, source, dependencies, and Kconfig
//! probes for an action. Capture consumes only that emitted data format and
//! rejects constructs it does not understand.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

/// One wildcard dependency recorded by Kbuild.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbuildWildcardDependency {
    /// Pattern passed to Make's `wildcard` function, without evaluation.
    pub pattern: Vec<u8>,
}

/// Dependency information recovered from one Kbuild `.cmd` receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbuildReceipt {
    /// Kbuild output named by the command variable suffix.
    pub target: Vec<u8>,
    /// Shell command text after decoding Kbuild's supported Make escapes.
    pub command: Vec<u8>,
    /// Source value emitted by `fixdep`, when present.
    pub source: Option<Vec<u8>>,
    /// Literal dependency paths emitted by `fixdep`.
    pub dependencies: Vec<Vec<u8>>,
    /// Value-based `CONFIG_*` dependencies recovered from marker probes.
    pub config_deps: Vec<String>,
    /// Non-Kconfig wildcards requiring capture-observer resolution.
    pub wildcard_dependencies: Vec<KbuildWildcardDependency>,
}

/// Kconfig analysis dependencies recorded in `include/config/auto.conf.cmd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbuildConfigReceipt {
    /// Generated configuration file named by the receipt.
    pub autoconfig: Vec<u8>,
    /// Kconfig descriptions whose contents control that generated file.
    pub dependencies: Vec<Vec<u8>>,
}

/// Parse one successful Kbuild `.cmd` receipt.
///
/// Both the main Kbuild `savedcmd_*` dialect and the host-tool `cmd_*` dialect
/// are accepted. Unknown Make expressions in dependency data fail closed.
pub fn parse_kbuild_receipt(bytes: &[u8]) -> Result<KbuildReceipt> {
    let assignments = assignments(bytes)?;
    let commands = assignments
        .iter()
        .filter_map(|(name, value)| {
            name.strip_prefix(b"savedcmd_" as &[u8])
                .or_else(|| name.strip_prefix(b"cmd_" as &[u8]))
                .map(|target| (target, value))
        })
        .collect::<Vec<_>>();
    if commands.len() != 1 {
        anyhow::bail!(
            "Kbuild receipt requires exactly one savedcmd_ or cmd_ assignment, found {}",
            commands.len()
        );
    }
    let (target, command) = commands[0];
    if target.is_empty() || command.is_empty() {
        anyhow::bail!("Kbuild receipt has an empty target or command");
    }

    let source_name = prefixed_name(b"source_", target);
    let deps_name = prefixed_name(b"deps_", target);
    let source = assignments.get(source_name.as_slice()).cloned();
    let dependency_value = assignments
        .get(deps_name.as_slice())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let (dependencies, config_deps, wildcard_dependencies) = parse_dependencies(dependency_value)?;

    Ok(KbuildReceipt {
        target: target.to_vec(),
        command: decode_command(command)?,
        source,
        dependencies,
        config_deps,
        wildcard_dependencies,
    })
}

fn decode_command(value: &[u8]) -> Result<Vec<u8>> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut position = 0usize;
    while position < value.len() {
        if value[position..].starts_with(b"$$") {
            decoded.push(b'$');
            position += 2;
        } else if value[position..].starts_with(b"$(pound)") {
            decoded.push(b'#');
            position += b"$(pound)".len();
        } else if value[position] == b'$' {
            anyhow::bail!("unsupported Make expansion in Kbuild command");
        } else {
            decoded.push(value[position]);
            position += 1;
        }
    }
    Ok(decoded)
}

/// Parse Kbuild's configuration-analysis receipt.
///
/// This is a distinct dependency manifest, not an executable action. Unknown
/// assignments or Make expressions fail closed so callers cannot mistake an
/// unhandled action receipt for configuration metadata.
pub fn parse_kbuild_config_receipt(bytes: &[u8]) -> Result<KbuildConfigReceipt> {
    let assignments = assignments(bytes)?;
    let expected = [b"autoconfig".as_slice(), b"deps_config".as_slice()];
    if assignments.len() != expected.len()
        || assignments
            .keys()
            .any(|name| !expected.contains(&name.as_slice()))
    {
        anyhow::bail!("Kbuild config receipt has unexpected assignments");
    }
    let autoconfig = assignments
        .get(b"autoconfig".as_slice())
        .filter(|value| !value.is_empty())
        .context("Kbuild config receipt has no autoconfig target")?
        .clone();
    let dependency_value = assignments
        .get(b"deps_config".as_slice())
        .context("Kbuild config receipt has no dependency list")?;
    let (dependencies, config_deps, wildcard_dependencies) = parse_dependencies(dependency_value)?;
    if dependencies.is_empty() || !config_deps.is_empty() || !wildcard_dependencies.is_empty() {
        anyhow::bail!("Kbuild config receipt has unsupported dependencies");
    }
    Ok(KbuildConfigReceipt {
        autoconfig,
        dependencies,
    })
}

fn assignments(bytes: &[u8]) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut result = BTreeMap::new();
    for line in logical_lines(bytes)? {
        let Some(separator) = find_subslice(&line, b" := ") else {
            continue;
        };
        let name = &line[..separator];
        if name.is_empty()
            || name
                .iter()
                .any(|byte| byte.is_ascii_whitespace() || *byte == 0)
        {
            anyhow::bail!("Kbuild receipt has an invalid assignment name");
        }
        let value = trim_ascii(&line[separator + 4..]).to_vec();
        if result.insert(name.to_vec(), value).is_some() {
            anyhow::bail!(
                "Kbuild receipt repeats assignment {:?}",
                String::from_utf8_lossy(name)
            );
        }
    }
    Ok(result)
}

fn logical_lines(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    if bytes.contains(&0) {
        anyhow::bail!("Kbuild receipt contains NUL");
    }
    let mut lines = Vec::new();
    let mut current = Vec::new();
    for physical in bytes.split(|byte| *byte == b'\n') {
        let physical = physical.strip_suffix(b"\r").unwrap_or(physical);
        let trimmed = trim_ascii_end(physical);
        let continued = trimmed.ends_with(b"\\");
        let part = if continued {
            &trimmed[..trimmed.len() - 1]
        } else {
            physical
        };
        if !current.is_empty() {
            current.push(b' ');
        }
        current.extend_from_slice(part);
        if !continued {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        anyhow::bail!("Kbuild receipt ends with an unfinished continuation");
    }
    Ok(lines)
}

fn parse_dependencies(
    value: &[u8],
) -> Result<(Vec<Vec<u8>>, Vec<String>, Vec<KbuildWildcardDependency>)> {
    let mut dependencies = BTreeSet::new();
    let mut config_deps = BTreeSet::new();
    let mut wildcards = BTreeSet::new();
    let mut position = 0usize;
    while position < value.len() {
        while position < value.len() && value[position].is_ascii_whitespace() {
            position += 1;
        }
        if position == value.len() {
            break;
        }
        if value[position..].starts_with(b"$(") {
            let end = value[position..]
                .iter()
                .position(|byte| *byte == b')')
                .map(|offset| position + offset)
                .context("unterminated Make expression in Kbuild dependencies")?;
            let expression = &value[position + 2..end];
            let pattern = expression
                .strip_prefix(b"wildcard " as &[u8])
                .context("unsupported Make expression in Kbuild dependencies")?;
            let pattern = trim_ascii(pattern);
            if pattern.is_empty() {
                anyhow::bail!("empty wildcard in Kbuild dependencies");
            }
            if let Some(marker) = pattern.strip_prefix(b"include/config/" as &[u8]) {
                config_deps.insert(config_symbol(marker)?);
            } else {
                wildcards.insert(pattern.to_vec());
            }
            position = end + 1;
            continue;
        }

        let mut word = Vec::new();
        while position < value.len() && !value[position].is_ascii_whitespace() {
            let byte = value[position];
            if byte == b'$' {
                anyhow::bail!("unsupported Make expansion in Kbuild dependency path");
            }
            if byte == b'\\' {
                position += 1;
                if position == value.len() {
                    anyhow::bail!("unfinished escape in Kbuild dependency path");
                }
                word.push(value[position]);
            } else {
                word.push(byte);
            }
            position += 1;
        }
        if !word.is_empty() {
            dependencies.insert(word);
        }
    }
    Ok((
        dependencies.into_iter().collect(),
        config_deps.into_iter().collect(),
        wildcards
            .into_iter()
            .map(|pattern| KbuildWildcardDependency { pattern })
            .collect(),
    ))
}

fn config_symbol(marker: &[u8]) -> Result<String> {
    if marker.is_empty()
        || marker
            .iter()
            .any(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'/'))
    {
        anyhow::bail!("invalid Kconfig marker in Kbuild receipt");
    }
    let mut symbol = String::from("CONFIG_");
    for byte in marker {
        symbol.push(if *byte == b'/' {
            '_'
        } else {
            char::from(*byte)
        });
    }
    Ok(symbol)
}

fn prefixed_name(prefix: &[u8], suffix: &[u8]) -> Vec<u8> {
    let mut name = Vec::with_capacity(prefix.len() + suffix.len());
    name.extend_from_slice(prefix);
    name.extend_from_slice(suffix);
    name
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    trim_ascii_end(&value[start..])
}

fn trim_ascii_end(value: &[u8]) -> &[u8] {
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|position| position + 1)
        .unwrap_or(0);
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn parses_fixdep_receipt_without_evaluating_make() {
        let receipt = parse_kbuild_receipt(
            br#"savedcmd_kernel/example.o := cc -DNAME='value' -c -o kernel/example.o ../kernel/example.c

source_kernel/example.o := ../kernel/example.c

deps_kernel/example.o := \
  ../kernel/example.c \
  ../include/space\ name.h \
    $(wildcard include/config/PREEMPT_RT) \
    $(wildcard include/config/ARCH/X86) \
    $(wildcard generated/*.h)

kernel/example.o: $(deps_kernel/example.o)
"#,
        )
        .unwrap();

        assert_eq!(receipt.target, b"kernel/example.o");
        assert_eq!(receipt.source, Some(b"../kernel/example.c".to_vec()));
        assert_eq!(
            receipt.dependencies,
            vec![
                b"../include/space name.h".to_vec(),
                b"../kernel/example.c".to_vec()
            ]
        );
        assert_eq!(
            receipt.config_deps,
            vec!["CONFIG_ARCH_X86", "CONFIG_PREEMPT_RT"]
        );
        assert_eq!(
            receipt.wildcard_dependencies,
            vec![KbuildWildcardDependency {
                pattern: b"generated/*.h".to_vec()
            }]
        );
    }

    #[test]
    fn parses_host_tool_cmd_dialect() {
        let receipt = parse_kbuild_receipt(
            b"cmd_tools/helper.o := cc -c -o tools/helper.o helper.c\n\
source_tools/helper.o := helper.c\n\
deps_tools/helper.o := helper.c /usr/include/stddef.h\n",
        )
        .unwrap();
        assert_eq!(receipt.target, b"tools/helper.o");
        assert_eq!(receipt.dependencies.len(), 2);
        assert!(receipt.config_deps.is_empty());
    }

    #[test]
    fn decodes_supported_make_escapes_in_saved_command() {
        let receipt =
            parse_kbuild_receipt(b"savedcmd_out := printf '$$0 $(pound) $$$(pound)' > out\n")
                .unwrap();

        assert_eq!(receipt.command, b"printf '$0 # $#' > out");
    }

    #[test]
    fn parses_kconfig_analysis_receipt() {
        let receipt = parse_kbuild_config_receipt(
            b"autoconfig := include/config/auto.conf\n\
deps_config := \\\n  Kconfig \\\n  arch/x86/Kconfig\n\
\n\
$(deps_config): ;\n",
        )
        .unwrap();
        assert_eq!(receipt.autoconfig, b"include/config/auto.conf");
        assert_eq!(
            receipt.dependencies,
            vec![b"Kconfig".to_vec(), b"arch/x86/Kconfig".to_vec()]
        );
    }

    #[test]
    fn rejects_ambiguous_or_unhandled_receipts() {
        assert!(parse_kbuild_receipt(b"deps_x := x.h\n").is_err());
        assert!(parse_kbuild_receipt(b"savedcmd_x := cc\nsavedcmd_y := cc\n").is_err());
        assert!(parse_kbuild_receipt(b"savedcmd_x := echo $(unknown)\n").is_err());
        assert!(parse_kbuild_receipt(b"savedcmd_x := echo $x\n").is_err());
        assert!(
            parse_kbuild_receipt(b"savedcmd_x := cc\ndeps_x := $(foreach h,headers,$(h))\n")
                .is_err()
        );
        assert!(parse_kbuild_receipt(b"savedcmd_x := cc\ndeps_x := x.h \\").is_err());
    }

    #[test]
    #[ignore = "requires RUSH_KBUILD_RECEIPT_CORPUS"]
    fn parses_requested_real_receipt_corpus() {
        let root = PathBuf::from(
            std::env::var_os("RUSH_KBUILD_RECEIPT_CORPUS")
                .expect("RUSH_KBUILD_RECEIPT_CORPUS must name an output tree"),
        );
        let mut pending = vec![root.clone()];
        let mut action_count = 0usize;
        let mut config_count = 0usize;
        let mut failures = Vec::new();
        while let Some(directory) = pending.pop() {
            let mut entries = std::fs::read_dir(&directory)
                .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()))
                .collect::<std::io::Result<Vec<_>>>()
                .unwrap_or_else(|error| panic!("read entries in {}: {error}", directory.display()));
            entries.sort_by_key(std::fs::DirEntry::file_name);
            for entry in entries {
                let path = entry.path();
                let file_type = entry
                    .file_type()
                    .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
                if file_type.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !file_type.is_file()
                    || path.extension().and_then(|value| value.to_str()) != Some("cmd")
                {
                    continue;
                }
                let bytes = std::fs::read(&path)
                    .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
                match parse_kbuild_receipt(&bytes) {
                    Ok(_) => action_count += 1,
                    Err(action_error) => match parse_kbuild_config_receipt(&bytes) {
                        Ok(_) => config_count += 1,
                        Err(config_error) => failures.push(format!(
                            "{}: action: {action_error:#}; config: {config_error:#}",
                            relative(&root, &path)
                        )),
                    },
                }
            }
        }
        let receipt_count = action_count + config_count;
        assert!(
            action_count > 0,
            "requested Kbuild receipt corpus has no actions"
        );
        assert!(
            config_count > 0,
            "requested Kbuild receipt corpus has no config receipt"
        );
        assert!(
            failures.is_empty(),
            "{} of {receipt_count} Kbuild receipts failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
        eprintln!(
            "parsed {receipt_count} Kbuild receipts: {action_count} actions, \
             {config_count} config"
        );
    }

    fn relative(root: &Path, path: &Path) -> String {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}
