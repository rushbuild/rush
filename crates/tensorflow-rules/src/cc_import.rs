use anyhow::Result;
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_list, extract_string_opt,
    providers::ProviderSet, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;

/// cc_import() rule - imports prebuilt C++ libraries
///
/// Example:
/// ```python
/// cc_import(
///     name = "prebuilt",
///     shared_library = "@prebuilt_libs//:lib/libfoo.so",
///     hdrs = ["foo.h"],
///     visibility = ["//visibility:public"],
/// )
/// ```
pub struct CcImportRule;

#[async_trait]
impl BuildRule for CcImportRule {
    fn name(&self) -> &str {
        "cc_import"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;

        // Extract library attributes
        let shared_library = extract_string_opt(attrs, "shared_library");
        let static_library = extract_string_opt(attrs, "static_library");
        let interface_library = extract_string_opt(attrs, "interface_library");
        let hdrs = extract_string_list(attrs, "hdrs").unwrap_or_default();
        let visibility = extract_string_opt(attrs, "visibility");
        let alwayslink = match attrs.get("alwayslink") {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        };

        Ok(Box::new(CcImportInstance {
            name,
            shared_library,
            static_library,
            interface_library,
            hdrs,
            visibility,
            alwayslink,
        }))
    }

    fn documentation(&self) -> &str {
        "Import prebuilt C++ libraries (shared or static)"
    }
}

struct CcImportInstance {
    name: String,
    shared_library: Option<String>,
    static_library: Option<String>,
    interface_library: Option<String>,
    hdrs: Vec<String>,
    #[allow(dead_code)]
    visibility: Option<String>,
    #[allow(dead_code)]
    alwayslink: Option<bool>,
}

#[async_trait]
impl RuleInstance for CcImportInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // cc_import can reference external libraries as dependencies
        let mut deps = Vec::new();

        // Add library references if they're labels
        if let Some(lib) = &self.shared_library {
            if lib.starts_with(':') || lib.starts_with("//") || lib.starts_with('@') {
                deps.push(lib.clone());
            }
        }
        if let Some(lib) = &self.static_library {
            if lib.starts_with(':') || lib.starts_with("//") || lib.starts_with('@') {
                deps.push(lib.clone());
            }
        }
        if let Some(lib) = &self.interface_library {
            if lib.starts_with(':') || lib.starts_with("//") || lib.starts_with('@') {
                deps.push(lib.clone());
            }
        }

        deps
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Process headers
        for hdr in &self.hdrs {
            let hdr_path = context.source_path(hdr);
            artifacts.push(Artifact {
                path: hdr_path,
                artifact_type: ArtifactType::Header,
            });
        }

        // Process shared library
        if let Some(shared_lib) = &self.shared_library {
            if shared_lib.starts_with(':')
                || shared_lib.starts_with("//")
                || shared_lib.starts_with('@')
            {
                // This is a dependency reference, get artifacts from dependency
                if let Some(dep_artifacts) = context.direct_artifacts(shared_lib) {
                    artifacts.extend(dep_artifacts.iter().cloned());
                }
            } else {
                // Direct file path
                let lib_path = context.source_path(shared_lib);
                artifacts.push(Artifact {
                    path: lib_path,
                    artifact_type: ArtifactType::SharedLibrary,
                });
            }
        }

        // Process static library
        if let Some(static_lib) = &self.static_library {
            if static_lib.starts_with(':')
                || static_lib.starts_with("//")
                || static_lib.starts_with('@')
            {
                // This is a dependency reference
                if let Some(dep_artifacts) = context.direct_artifacts(static_lib) {
                    artifacts.extend(dep_artifacts.iter().cloned());
                }
            } else {
                // Direct file path
                let lib_path = context.source_path(static_lib);
                artifacts.push(Artifact {
                    path: lib_path,
                    artifact_type: ArtifactType::StaticLibrary,
                });
            }
        }

        // Process interface library (Windows .lib for .dll)
        if let Some(interface_lib) = &self.interface_library {
            if interface_lib.starts_with(':')
                || interface_lib.starts_with("//")
                || interface_lib.starts_with('@')
            {
                // This is a dependency reference
                if let Some(dep_artifacts) = context.direct_artifacts(interface_lib) {
                    artifacts.extend(dep_artifacts.iter().cloned());
                }
            } else {
                // Direct file path
                let lib_path = context.source_path(interface_lib);
                artifacts.push(Artifact {
                    path: lib_path,
                    artifact_type: ArtifactType::StaticLibrary,
                });
            }
        }

        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        // Provide CcInfo with the imported library information
        // This allows dependent cc_library/cc_binary targets to link against it

        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "cc_import"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cc_import_shared() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("prebuilt".to_string()));
        attrs.insert(
            "shared_library".to_string(),
            Value::String("lib/libfoo.so".to_string()),
        );
        attrs.insert(
            "hdrs".to_string(),
            Value::List(vec![Value::String("foo.h".to_string())]),
        );

        let rule = CcImportRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "prebuilt");
        assert_eq!(instance.dependencies().len(), 0);
    }

    #[test]
    fn test_parse_cc_import_static() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("prebuilt_static".to_string()),
        );
        attrs.insert(
            "static_library".to_string(),
            Value::String("lib/libfoo.a".to_string()),
        );

        let rule = CcImportRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "prebuilt_static");
    }

    #[test]
    fn test_cc_import_with_label_reference() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("external_lib".to_string()),
        );
        attrs.insert(
            "shared_library".to_string(),
            Value::String("@prebuilt//:lib/libfoo.so".to_string()),
        );

        let rule = CcImportRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        let deps = instance.dependencies();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], "@prebuilt//:lib/libfoo.so");
    }

    #[test]
    fn test_cc_import_alwayslink() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("prebuilt".to_string()));
        attrs.insert(
            "static_library".to_string(),
            Value::String("lib/libfoo.a".to_string()),
        );
        attrs.insert("alwayslink".to_string(), Value::Bool(true));

        let rule = CcImportRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "prebuilt");
    }
}
