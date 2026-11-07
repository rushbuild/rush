// Rush Build - Proto Alias Expander
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use crate::dependency_graph::{DependencyGraph, Label};
use anyhow::Result;

/// Expands implicit aliases for proto targets.
pub struct ProtoAliasExpander;

impl ProtoAliasExpander {
    /// Expand implicit aliases for a proto target.
    ///
    /// This handles:
    /// - `_cc` aliases for C++ proto libraries
    /// - `_cc_impl` aliases for implementation targets
    /// - `_genproto` aliases for generated code
    /// - Base name aliases for header-only targets
    pub fn expand(
        target_name: &str,
        rule_type: &str,
        label: &Label,
        graph: &mut DependencyGraph,
    ) -> Result<()> {
        // For tf_proto_library/proto_library targets, add implicit _cc aliases
        // TensorFlow's tf_proto_library macro creates:
        //   {name}: proto_library (base target)
        //   {name}_cc: cc_proto_library (C++ version)
        //   {name}_cc_impl: implementation target
        // We register aliases so dependencies on {name}_cc resolve to {name}

        // FIX #215: Skip alias creation for @com_google_protobuf well-known types
        // since they're manually defined in the stub BUILD file (Fix #214)
        let is_protobuf_repo = label
            .workspace
            .as_ref()
            .map(|w| w == "com_google_protobuf")
            .unwrap_or(false);

        // Skip alias rules - they don't need alias expansion.
        // "proto_alias" is created by ProtoAliasExpander itself.
        // "alias" is created by macro expansion (expand_tf_proto_library).
        // Both types should not trigger further alias creation.
        if matches!(
            rule_type,
            "proto_alias" | "alias" | "shared_library_output_alias"
        ) {
            return Ok(());
        }

        // tf_proto_library is expanded by rush-starlark macro, so we only handle proto_library/py_proto_library
        let is_proto_rule = rule_type == "proto_library" || rule_type == "py_proto_library";

        // FIX: Don't treat derived targets as proto targets that need expansion.
        // Targets ending with these suffixes are already generated aliases.
        let is_derived_alias = target_name.ends_with("_genproto")
            || target_name.ends_with("_proto_srcs")
            || target_name.ends_with("_cc_impl")
            || target_name.ends_with("_headers_only");

        let is_proto_target = !is_derived_alias
            && (is_proto_rule
                || target_name.ends_with("_proto")
                || target_name.ends_with("_proto_cc"));

        if is_proto_target && !is_protobuf_repo {
            // Only add {name}_cc alias if target doesn't already end with _cc
            if !target_name.ends_with("_cc") {
                let cc_alias_name = format!("{}_cc", target_name);
                Self::create_alias(
                    &cc_alias_name,
                    &format!(":{}", target_name),
                    label,
                    graph,
                    true, // implicit
                )?;
            }

            // Add {name}_cc_impl alias
            let cc_impl_alias_name = format!("{}_cc_impl", target_name);
            Self::create_alias(
                &cc_impl_alias_name,
                &format!(":{}", target_name),
                label,
                graph,
                true,
            )?;

            // FIX #195: Add {name}_genproto alias (points to base target)
            // Bazel's proto rules create genproto targets for generated proto code
            if !target_name.ends_with("_cc") {
                let genproto_alias_name = format!("{}_genproto", target_name);
                Self::create_alias(
                    &genproto_alias_name,
                    &format!(":{}", target_name),
                    label,
                    graph,
                    true,
                )?;

                // FIX #195: Add {name}_cc_genproto alias (points to base target)
                let cc_genproto_alias_name = format!("{}_cc_genproto", target_name);
                Self::create_alias(
                    &cc_genproto_alias_name,
                    target_name, // Direct reference as it's in the same package
                    label,
                    graph,
                    true,
                )?;
            }
        }

        // FIX #180: For proto targets registered as {name}_cc (due to make_default_target_header_only=True),
        // also create the base name alias pointing to the _cc target
        if target_name.ends_with("_proto_cc") && target_name.len() > 9 {
            let base_name = &target_name[..target_name.len() - 3]; // Strip _cc
            Self::create_alias(
                base_name,
                target_name,
                label,
                graph,
                false, // base name alias
            )?;
        }
        // Handle header-only tf_proto_library targets whose names end with _cc but not _proto_cc
        else if target_name.ends_with("_cc") {
            let base_name = target_name.trim_end_matches("_cc");
            if !base_name.is_empty() {
                Self::create_alias(base_name, target_name, label, graph, false)?;

                let cc_impl_alias_name = format!("{}_cc_impl", base_name);
                Self::create_alias(
                    &cc_impl_alias_name,
                    &format!(":{}", target_name),
                    label,
                    graph,
                    true,
                )?;

                let genproto_alias_name = format!("{}_genproto", base_name);
                Self::create_alias(
                    &genproto_alias_name,
                    &format!(":{}", target_name),
                    label,
                    graph,
                    true,
                )?;
            }
        }

        Ok(())
    }

    fn create_alias(
        alias_name: &str,
        actual_target: &str,
        label: &Label,
        graph: &mut DependencyGraph,
        is_implicit: bool,
    ) -> Result<()> {
        let mut producer = Label::parse(
            actual_target,
            label.workspace.as_deref(),
            Some(&label.package),
        )?;
        let relative = actual_target.starts_with(':')
            || (!actual_target.starts_with("//") && !actual_target.starts_with('@'));
        if relative && producer.workspace.is_none() {
            producer.workspace.clone_from(&label.workspace);
        }
        let visibility = graph
            .get(&producer)
            .and_then(|node| node.rule_instance.visibility().map(<[String]>::to_vec));
        let alias = rush_rules::with_visibility(
            tensorflow_rules::tf_proto_library::ProtoAliasInstance::new(
                alias_name.to_string(),
                actual_target.to_string(),
            ),
            visibility,
        );

        let alias_label = Label::parse(
            &format!(
                "{}//{}:{}",
                label
                    .workspace
                    .as_ref()
                    .map(|w| format!("@{}", w))
                    .unwrap_or_default(),
                label.package,
                alias_name
            ),
            label.workspace.as_deref(),
            Some(&label.package),
        )?;

        if graph.has_node(&alias_label) {
            // Alias or target already exists, don't overwrite
            return Ok(());
        }

        if is_implicit {
            eprintln!("      → {} (implicit alias)", alias_name);
        } else {
            eprintln!(
                "      → {} (base name alias for header-only proto)",
                alias_name
            );
        }

        graph.add_node(alias_label, alias)?;
        Ok(())
    }
}
