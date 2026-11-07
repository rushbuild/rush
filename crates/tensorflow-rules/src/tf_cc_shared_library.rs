// tf_cc_shared_library — TensorFlow's wrapper around cc_shared_library.
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Forward a `tf_cc_shared_library(...)` rule call to rush's native
//! `cc_shared_library` implementation, folding in TF-specific defaults:
//!
//!   * `framework_so` — a list of cc_import targets naming the
//!     framework `.so` (e.g. `libtensorflow_framework_import_lib`) —
//!     treated as additional `dynamic_deps` so `libtensorflow_cc.so`
//!     dlopens `libtensorflow_framework.so` at runtime instead of
//!     statically duplicating its object code.
//!   * `soversion` — TF's `.so` version suffix (e.g. `"2.15.0"`).
//!     When set, the output becomes `lib<name>.so.<soversion>` via
//!     cc_shared_library's `shared_lib_name` knob.
//!   * `linkopts` — mapped to `user_link_flags`.
//!   * `static_deps` — TF's package-pattern allowlist for libraries that may
//!     be linked statically. It constrains analysis but does not create graph
//!     or linker dependencies, so Rush accepts it without forwarding entries
//!     into `deps`.
//!
//! Attributes that are purely macro-level in Bazel (`srcs`, `data`,
//! `visibility`, `linkstatic`, `per_os_targets`, `win_def_file`) are
//! accepted and ignored: `srcs` on tf_cc_shared_library is a
//! compile-time equivalent of `deps` for TF's wrapper and the
//! compilation has already happened at the `cc_library` stage we
//! collected via CcInfo.

use anyhow::Result;
use async_trait::async_trait;
use rush_rules::{
    extract_string, extract_string_list, extract_string_opt, rules_cc::CcSharedLibraryRule,
    BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;

/// `tf_cc_shared_library` rule — thin wrapper over `cc_shared_library`
/// that folds TF-specific attributes into the shape cc_shared_library
/// expects.
pub struct TfCcSharedLibraryRule;

#[async_trait]
impl BuildRule for TfCcSharedLibraryRule {
    fn name(&self) -> &str {
        "tf_cc_shared_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        // Start from a shallow copy of the incoming attrs so we can rewrite
        // the ones cc_shared_library cares about without dropping anything
        // else the Starlark caller passed in.
        let mut forwarded: HashMap<String, Value> = attrs.clone();

        // framework_so → merge into dynamic_deps (plural).
        let framework_so = extract_string_list(attrs, "framework_so").unwrap_or_default();
        if !framework_so.is_empty() {
            let mut dd = extract_string_list(attrs, "dynamic_deps").unwrap_or_default();
            for fs in framework_so {
                if !dd.contains(&fs) {
                    dd.push(fs);
                }
            }
            forwarded.insert(
                "dynamic_deps".to_string(),
                Value::List(dd.into_iter().map(Value::String).collect()),
            );
        }

        // linkopts → user_link_flags. Bazel's native rule names the attr
        // `user_link_flags`; TF's wrapper uses the historical `linkopts`.
        if let Ok(linkopts) = extract_string_list(attrs, "linkopts") {
            if !linkopts.is_empty() {
                let mut ulf =
                    extract_string_list(attrs, "user_link_flags").unwrap_or_default();
                for lf in linkopts {
                    if !ulf.contains(&lf) {
                        ulf.push(lf);
                    }
                }
                forwarded.insert(
                    "user_link_flags".to_string(),
                    Value::List(ulf.into_iter().map(Value::String).collect()),
                );
            }
        }

        // soversion → shared_lib_name = lib<name>.so.<soversion> to match
        // Bazel's observable output path. TF's per-module Python
        // extensions and the downstream `libtensorflow.so` stub depend
        // on this exact filename via `dynamic_deps`.
        if let Some(soversion) = extract_string_opt(attrs, "soversion") {
            if !soversion.is_empty() {
                let name = extract_string(attrs, "name")?;
                let so_filename = format!("lib{}.so.{}", name, soversion);
                forwarded.insert("shared_lib_name".to_string(), Value::String(so_filename));
            }
        }

        // Hand the normalized attrs to cc_shared_library's parser.
        CcSharedLibraryRule.parse_attributes(&forwarded)
    }

    fn documentation(&self) -> &str {
        "TensorFlow wrapper over cc_shared_library — folds framework_so, \
         linkopts, and soversion into the native rule's attrs."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Value {
        Value::List(
            values
                .iter()
                .map(|value| Value::String((*value).to_string()))
                .collect(),
        )
    }

    #[test]
    fn static_deps_are_allowlist_patterns_not_graph_dependencies() {
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("shared".to_string())),
            ("roots".to_string(), strings(&[":root"])),
            ("deps".to_string(), strings(&[":direct"])),
            ("framework_so".to_string(), strings(&[":framework"])),
            (
                "static_deps".to_string(),
                strings(&[
                    "//:__subpackages__",
                    "@repo//:__subpackages__",
                    "//pkg:concrete_allowlist_entry",
                ]),
            ),
        ]);

        let instance = TfCcSharedLibraryRule.parse_attributes(&attrs).unwrap();
        assert_eq!(
            instance.dependencies(),
            vec![
                ":root".to_string(),
                ":direct".to_string(),
                ":framework".to_string(),
            ]
        );
    }
}
