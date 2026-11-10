// Rush Build - immutable expanded build request transport
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Client-owned build argument boundaries after common/build/config expansion.
//! This is distinct from the native-option subset: configured metadata needs
//! every analysis option. Missing or malformed snapshots never use daemon argv.

use crate::CommandSemantics;

/// Internal client-to-command transport. Included in the config fingerprint.
pub const EFFECTIVE_BUILD_ARGUMENTS_ENV: &str = "RUSH_EFFECTIVE_BUILD_ARGUMENTS_V1";

/// Normalize supported value-taking options before any semantic scan. A value
/// is consumed even when it looks like another option (Bazel 6.5 behavior).
/// Unknown options remain intact and configured projection refuses ambiguity.
pub fn normalize_tokens(tokens: &[String]) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" {
            result.extend_from_slice(&tokens[index..]);
            break;
        }
        let valued = matches!(
            token.as_str(),
            "--config"
                | "--define"
                | "--action_env"
                | "--repo_env"
                | "--copt"
                | "--cxxopt"
                | "--conlyopt"
                | "--host_copt"
                | "--host_cxxopt"
                | "--linkopt"
                | "--host_linkopt"
                | "-c"
                | "--compilation_mode"
                | "--compilation-mode"
                | "--cpu"
                | "--apple_platform_type"
                | "--crosstool_top"
                | "--features"
                | "--strip"
                | "-j"
                | "--jobs"
                | "--repository_cache"
                | "--override_repository"
                | "--platforms"
                | "--extra_toolchains"
                | "--python_path"
                | "--output_filter"
        );
        if valued {
            index += 1;
            let value = tokens
                .get(index)
                .ok_or_else(|| format!("{token} requires a value"))?;
            let name = match token.as_str() {
                "-c" | "--compilation-mode" => "--compilation_mode",
                "-j" => "--jobs",
                other => other,
            };
            result.push(format!("{name}={value}"));
        } else {
            result.push(token.clone());
        }
        index += 1;
    }
    Ok(result)
}

/// Encode UTF-8 CLI tokens without joining or losing embedded whitespace.
pub fn encode(tokens: &[String]) -> String {
    encode_snapshot(tokens, None)
}

/// Preserve ordered rc startup arguments separately from build options. Their
/// Bazel compatibility is checked only when configured metadata is requested.
pub fn encode_with_startup(tokens: &[String], startup: &[String]) -> String {
    encode_snapshot(tokens, Some(startup))
}

fn encode_snapshot(tokens: &[String], startup: Option<&[String]>) -> String {
    // Keep invalid tokens serializable; decode rejects them rather than falling
    // back to another command's argv or environment.
    let normalized = normalize_tokens(tokens).unwrap_or_else(|_| tokens.to_vec());
    let tokens = &normalized;
    let mut semantic = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        if token == "--" {
            semantic.extend(tokens[index..].iter());
            break;
        }
        let (name, joined) = token
            .split_once('=')
            .map(|(name, _)| (name, true))
            .unwrap_or((token, false));
        if matches!(name, "--daemon" | "--watchfs") {
            if !joined {
                index += 1;
            }
        } else {
            semantic.push(token);
        }
        index += 1;
    }
    match startup {
        Some(startup) => {
            serde_json::json!({"version": 2, "tokens": semantic, "startup": startup}).to_string()
        }
        None => serde_json::json!({"version": 1, "tokens": semantic}).to_string(),
    }
}

/// Read the authoritative expanded request from this immutable command only.
pub fn from_command(command: &CommandSemantics) -> Result<Vec<String>, String> {
    snapshot_from_command(command).map(|(tokens, _)| tokens)
}

/// Read startup arguments from the same immutable snapshot as the build options.
/// Genuine version-one clients supplied no startup arguments.
pub fn startup_from_command(command: &CommandSemantics) -> Result<Vec<String>, String> {
    snapshot_from_command(command).map(|(_, startup)| startup)
}

fn snapshot_from_command(command: &CommandSemantics) -> Result<(Vec<String>, Vec<String>), String> {
    let bytes = command
        .getenv(EFFECTIVE_BUILD_ARGUMENTS_ENV.as_bytes())
        .ok_or("configured metadata requires the client's expanded build-argument snapshot")?;
    decode_snapshot(bytes)
}

/// Decode one versioned client snapshot without consulting process state.
pub fn decode(bytes: &[u8]) -> Result<Vec<String>, String> {
    decode_snapshot(bytes).map(|(tokens, _)| tokens)
}

fn decode_snapshot(bytes: &[u8]) -> Result<(Vec<String>, Vec<String>), String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid expanded build arguments: {error}"))?;
    let object = value
        .as_object()
        .ok_or("expanded build arguments must be an object")?;
    let version = object.get("version").and_then(|v| v.as_u64());
    if !matches!((version, object.len()), (Some(1), 2) | (Some(2), 3))
        || (version == Some(1) && object.contains_key("startup"))
    {
        return Err("unsupported expanded build-argument version or fields".into());
    }
    let decode_tokens = |field: &str| -> Result<Vec<String>, String> {
        object
            .get(field)
            .and_then(|v| v.as_array())
            .ok_or("expanded build arguments require a token array")?
            .iter()
            .map(|value| {
                let token = value
                    .as_str()
                    .ok_or("expanded build argument is not a string")?;
                if token.contains('\0') {
                    return Err("expanded build argument contains NUL".into());
                }
                Ok(token.to_owned())
            })
            .collect::<Result<Vec<_>, String>>()
    };
    let tokens = normalize_tokens(&decode_tokens("tokens")?)?;
    let startup = if version == Some(2) {
        decode_tokens("startup")?
    } else {
        Vec::new()
    };
    Ok((tokens, startup))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    fn try_command(snapshot: Option<&str>) -> Result<CommandSemantics, crate::CommandError> {
        CommandSemantics::from_run_request(&RunRequest {
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            environment: snapshot
                .map(|value| {
                    vec![EnvironmentVariable {
                        name: Bytes::from(EFFECTIVE_BUILD_ARGUMENTS_ENV),
                        value: Bytes::copy_from_slice(value.as_bytes()),
                    }]
                })
                .unwrap_or_default(),
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        })
    }
    fn command(snapshot: Option<&str>) -> CommandSemantics {
        try_command(snapshot).unwrap()
    }
    #[test]
    fn expanded_build_arguments_keep_boundaries_and_reset_per_command() {
        let a = vec!["--copt=-DVALUE=a b\nc".into(), "//pkg:target".into()];
        let first = command(Some(&encode(&a)));
        let empty = command(Some(&encode(&[])));
        assert_eq!(from_command(&first).unwrap(), a);
        assert_eq!(from_command(&empty).unwrap(), Vec::<String>::new());
        assert_ne!(first.config_fingerprint(), empty.config_fingerprint());
        assert!(from_command(&command(None)).is_err());
        assert_eq!(from_command(&first).unwrap(), a);
    }
    #[test]
    fn expanded_build_arguments_v2_preserve_startup_order_and_fingerprint() {
        let tokens = vec!["--define=KEY=build".into()];
        let startup = [
            "--windows_enable_symlinks",
            "--host_jvm_args",
            "-Dkey=two words",
            "--host_jvm_args=-Xmx1g",
            "--nowindows_enable_symlinks",
        ]
        .map(str::to_owned);
        let first = command(Some(&encode_with_startup(&tokens, &startup)));
        assert_eq!(from_command(&first).unwrap(), tokens);
        assert_eq!(startup_from_command(&first).unwrap(), startup);
        let reset = command(Some(&encode_with_startup(&tokens, &[])));
        assert!(startup_from_command(&reset).unwrap().is_empty());
        assert_ne!(first.config_fingerprint(), reset.config_fingerprint());
        let mut reversed = startup.to_vec();
        reversed.reverse();
        let changed = command(Some(&encode_with_startup(&tokens, &reversed)));
        assert_ne!(first.config_fingerprint(), changed.config_fingerprint());
        assert_eq!(startup_from_command(&first).unwrap(), startup);
        let legacy = command(Some(&encode(&tokens)));
        assert_eq!(from_command(&legacy).unwrap(), tokens);
        assert!(startup_from_command(&legacy).unwrap().is_empty());
    }
    #[test]
    fn expanded_build_arguments_defines_follow_final_platform_order() {
        let snapshot = encode(&[
            "--define=KEY=base".into(),
            "--define".into(),
            "KEY=cli".into(),
            "--define=KEY=platform".into(),
        ]);
        let semantics = command(Some(&snapshot));
        assert_eq!(
            semantics.defines(),
            &[crate::BuildDefine {
                key: "KEY".into(),
                value: "platform".into()
            }]
        );
        let reset = command(Some(&encode(&[])));
        assert!(reset.defines().is_empty());
        assert_eq!(semantics.defines()[0].value, "platform");
    }
    #[test]
    fn expanded_build_arguments_do_not_hash_routing_but_keep_unknown_analysis_values() {
        let first = command(Some(&encode(&[
            "--daemon=auto".into(),
            "--watchfs".into(),
            "auto".into(),
            "--future_option=a".into(),
        ])));
        let second = command(Some(&encode(&[
            "--daemon=off".into(),
            "--watchfs=off".into(),
            "--future_option=a".into(),
        ])));
        let changed = command(Some(&encode(&["--future_option=b".into()])));
        assert_eq!(first.config_fingerprint(), second.config_fingerprint());
        assert_ne!(first.config_fingerprint(), changed.config_fingerprint());
    }
    #[test]
    fn expanded_build_arguments_preserve_option_shaped_operands_before_semantic_scans() {
        let tokens = [
            "--action_env",
            "--daemon=off",
            "--action_env",
            "--copt=-O3",
            "--action_env",
            "--define=GHOST=value",
        ]
        .map(str::to_owned);
        let snapshot = encode(&tokens);
        let semantics = command(Some(&snapshot));
        assert_eq!(
            from_command(&semantics).unwrap(),
            [
                "--action_env=--daemon=off",
                "--action_env=--copt=-O3",
                "--action_env=--define=GHOST=value"
            ]
        );
        let native =
            crate::native_options::NativeConfiguration::from_command_semantics(&semantics).unwrap();
        assert!(native.copts.is_empty());
        assert!(native.defines.is_empty());
        assert!(semantics.defines().is_empty());
    }
    #[test]
    fn expanded_build_arguments_reject_dual_native_disagreement_and_merge_inherited_defines() {
        let full = encode(&["--compilation_mode=opt".into(), "--define=KEY=cli".into()]);
        let make = |native: &str| {
            CommandSemantics::from_run_request(&RunRequest {
                argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
                environment: vec![
                    EnvironmentVariable {
                        name: Bytes::from(EFFECTIVE_BUILD_ARGUMENTS_ENV),
                        value: Bytes::copy_from_slice(full.as_bytes()),
                    },
                    EnvironmentVariable {
                        name: Bytes::from(crate::native_options::EFFECTIVE_NATIVE_FLAGS_ENV),
                        value: Bytes::copy_from_slice(native.as_bytes()),
                    },
                    EnvironmentVariable {
                        name: Bytes::from_static(b"RUSH_DEFINES"),
                        value: Bytes::from_static(b"KEY=inherited ONLY_ENV=retained"),
                    },
                ],
                protocol_major: rush_protocol::PROTOCOL_MAJOR,
                protocol_minor: rush_protocol::PROTOCOL_MINOR,
                ..Default::default()
            })
            .unwrap()
        };
        assert!(
            crate::native_options::NativeConfiguration::from_command_semantics(&make(
                r#"["--compilation_mode=dbg"]"#
            ))
            .is_err()
        );
        let config = crate::native_options::NativeConfiguration::from_command_semantics(&make(
            r#"["--compilation_mode=opt","--define=KEY=cli"]"#,
        ))
        .unwrap();
        assert_eq!(config.defines["KEY"], "cli");
        assert_eq!(config.defines["ONLY_ENV"], "retained");
    }
    #[test]
    fn expanded_build_arguments_reject_malformed_or_unknown_snapshots() {
        assert!(decode(&[0xff]).is_err());
        for raw in [
            "[]",
            "{\"version\":2,\"tokens\":[]}",
            "{\"version\":1,\"tokens\":[],\"unknown\":0}",
            "{\"version\":1,\"tokens\":[1]}",
            "{\"version\":1,\"tokens\":[\"a\\u0000b\"]}",
            "{\"version\":1,\"tokens\":[],\"startup\":[]}",
            "{\"version\":2,\"tokens\":[],\"startup\":[],\"unknown\":0}",
            "{\"version\":2,\"tokens\":[],\"startup\":null}",
            "{\"version\":2,\"tokens\":[],\"startup\":[1]}",
            "{\"version\":2,\"tokens\":[],\"startup\":[\"a\\u0000b\"]}",
            "{\"version\":3,\"tokens\":[],\"startup\":[]}",
        ] {
            assert!(try_command(Some(raw)).is_err(), "{raw}");
        }
    }
}
