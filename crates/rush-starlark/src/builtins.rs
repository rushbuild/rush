use crate::evaluator::{
    configuration_label_dictionary_key, dictionary_key_value, sorted_dictionary_key_values, Value,
};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Built-in function signatures for Starlark
pub type BuiltinFn = fn(&[Value], &[(String, Value)]) -> Result<Value>;

/// Get all built-in functions
pub fn get_builtins() -> HashMap<String, BuiltinFn> {
    let mut builtins: HashMap<String, BuiltinFn> = HashMap::new();

    builtins.insert("len".to_string(), builtin_len);
    builtins.insert("hash".to_string(), builtin_hash);
    builtins.insert("max".to_string(), builtin_max);
    builtins.insert("min".to_string(), builtin_min);
    builtins.insert("range".to_string(), builtin_range);
    builtins.insert("str".to_string(), builtin_str);
    builtins.insert("repr".to_string(), builtin_repr);
    builtins.insert("int".to_string(), builtin_int);
    builtins.insert("float".to_string(), builtin_float);
    builtins.insert("bool".to_string(), builtin_bool);
    builtins.insert("list".to_string(), builtin_list);
    builtins.insert("tuple".to_string(), builtin_tuple);
    builtins.insert("dict".to_string(), builtin_dict);
    builtins.insert("print".to_string(), builtin_print);
    builtins.insert("type".to_string(), builtin_type);
    builtins.insert("glob".to_string(), builtin_glob);
    builtins.insert("select".to_string(), builtin_select);
    builtins.insert("package".to_string(), builtin_package);
    builtins.insert("licenses".to_string(), builtin_licenses);
    builtins.insert("visibility".to_string(), builtin_visibility);
    builtins.insert("provider".to_string(), builtin_provider);
    builtins.insert("Label".to_string(), builtin_label);
    builtins.insert("struct".to_string(), builtin_struct);
    builtins.insert("depset".to_string(), builtin_depset);
    builtins.insert("aspect".to_string(), builtin_aspect);
    builtins.insert("transition".to_string(), builtin_transition);
    builtins.insert("rule".to_string(), builtin_rule);
    builtins.insert("hasattr".to_string(), builtin_hasattr);
    builtins.insert("getattr".to_string(), builtin_getattr);
    builtins.insert("module_extension".to_string(), builtin_module_extension);
    builtins.insert("repository_rule".to_string(), builtin_repository_rule);
    builtins.insert("exec_group".to_string(), builtin_exec_group);
    builtins.insert("attr".to_string(), builtin_attr);
    builtins.insert("fail".to_string(), builtin_fail);
    builtins.insert("sorted".to_string(), builtin_sorted);
    builtins.insert("enumerate".to_string(), builtin_enumerate);
    builtins.insert("zip".to_string(), builtin_zip);
    builtins.insert("any".to_string(), builtin_any);
    builtins.insert("all".to_string(), builtin_all);
    builtins.insert("existing_rule".to_string(), builtin_existing_rule);
    builtins.insert("existing_rules".to_string(), builtin_existing_rules);
    builtins.insert("native".to_string(), builtin_native);
    // package_group is registered as a rule function in the evaluator (it
    // creates a real, requestable graph target in Bazel), not as a no-op
    // builtin — a builtins-map entry would shadow the rule registration.
    builtins.insert("if_google".to_string(), builtin_if_google);
    builtins.insert("if_oss".to_string(), builtin_if_oss);
    builtins.insert("if_windows".to_string(), builtin_if_windows);
    builtins.insert("if_not_windows".to_string(), builtin_if_not_windows);
    builtins.insert(
        "get_compatible_with_portable".to_string(),
        builtin_get_compatible_with_portable,
    );
    builtins.insert("tf_copts".to_string(), builtin_tf_copts);
    builtins.insert("rocm_copts".to_string(), builtin_rocm_copts);
    builtins.insert(
        "if_not_mobile_or_arm_or_lgpl_restricted".to_string(),
        builtin_if_not_mobile_or_arm_or_lgpl_restricted,
    );
    builtins.insert(
        "_if_not_mobile_or_arm_or_lgpl_restricted".to_string(),
        builtin_if_not_mobile_or_arm_or_lgpl_restricted,
    );
    builtins.insert("test_suite".to_string(), builtin_test_suite);
    // proto_library, config_setting, py_library, py_binary, py_test are
    // intentionally NOT in this map — they're captured as RuleCalls via
    // `register_rule_function(...)` in evaluator.rs / starlark_bridge.rs so
    // they land in the dep graph and route to the real rule handlers.
    // Dead `builtin_*` no-ops used to live here; removed — see
    // memory/feedback_no_fake_stubs.md.
    // cc_shared_library is handled by MACRO_PRIORITY_OVERRIDE +
    // CcSharedLibraryRule — no builtin needed here.
    builtins.insert("objc_library".to_string(), builtin_objc_library);
    builtins.insert("if_portable".to_string(), builtin_if_portable);
    builtins.insert("_if_portable".to_string(), builtin_if_portable);
    builtins.insert(
        "tf_lib_proto_parsing_deps".to_string(),
        builtin_tf_lib_proto_parsing_deps,
    );
    builtins.insert(
        "tf_additional_test_deps".to_string(),
        builtin_tf_additional_test_deps,
    );
    builtins.insert("if_mkl".to_string(), builtin_if_mkl);
    builtins.insert(
        "if_with_tpu_support".to_string(),
        builtin_if_with_tpu_support,
    );
    builtins.insert("if_libtpu".to_string(), builtin_if_conditional);
    builtins.insert("if_android".to_string(), builtin_if_conditional);
    builtins.insert("if_ios".to_string(), builtin_if_conditional);
    builtins.insert("if_mobile".to_string(), builtin_if_conditional);
    builtins.insert("if_nccl".to_string(), builtin_if_conditional);
    builtins.insert("if_cuda_or_rocm".to_string(), builtin_if_cuda_or_rocm);
    builtins.insert("if_dynamic_kernels".to_string(), builtin_if_conditional);
    builtins.insert("if_static".to_string(), builtin_if_static);
    builtins.insert(
        "if_not_mobile_or_arm_or_lgpl_restricted".to_string(),
        builtin_if_conditional,
    );
    builtins.insert(
        "tf_tpu_dependencies".to_string(),
        builtin_tf_tpu_dependencies,
    );
    builtins.insert(
        "tf_dtensor_tpu_dependencies".to_string(),
        builtin_tf_dtensor_tpu_dependencies,
    );
    builtins.insert(
        "tensorflow_opensource_extra_deps".to_string(),
        builtin_tensorflow_opensource_extra_deps,
    );

    // Bazel configuration_field builtin - used to access configuration values
    builtins.insert(
        "configuration_field".to_string(),
        builtin_configuration_field,
    );
    builtins.insert(
        "tf_opts_nortti_if_lite_protos".to_string(),
        builtin_tf_opts_nortti_if_lite_protos,
    );
    builtins.insert(
        "tf_defines_nortti_if_lite_protos".to_string(),
        builtin_tf_defines_nortti_if_lite_protos,
    );
    builtins.insert(
        "tf_portable_proto_lib".to_string(),
        builtin_tf_portable_proto_lib,
    );
    builtins.insert(
        "tf_portable_deps_no_runtime".to_string(),
        builtin_tf_portable_deps_no_runtime,
    );
    builtins.insert(
        "tf_additional_lib_deps".to_string(),
        builtin_tf_additional_lib_deps,
    );
    builtins.insert(
        "tf_features_nomodules_if_mobile".to_string(),
        builtin_tf_features_nomodules_if_mobile,
    );
    builtins.insert(
        "tf_protos_grappler_impl".to_string(),
        builtin_tf_protos_grappler_impl,
    );
    builtins.insert(
        "tf_monitoring_framework_deps".to_string(),
        builtin_tf_monitoring_framework_deps,
    );
    // tf_proto_library is captured as a RuleCall (MACRO_PRIORITY_OVERRIDE
    // + expand_tf_proto_library in evaluator). filegroup is a native
    // rule. tf_custom_op_library is handled by expand_tf_custom_op_library
    // in evaluator. None need builtin stubs here.
    builtins.insert(
        "tf_jspb_proto_library".to_string(),
        builtin_tf_jspb_proto_library,
    );
    builtins.insert(
        "tf_version_info_genrule".to_string(),
        builtin_tf_version_info_genrule,
    );
    // Under RUSH_ENABLE_ROCM or RUSH_ENABLE_CUDA we faithfully expand
    // tf_cuda_cc_test/tf_gpu_cc_test into the suffixed tf_cc_test targets and
    // their original-name suite. The macro expander handles it; registering
    // the no-op builtin here would return before evaluator dispatch and shadow
    // it. CPU-only builds keep the no-op path byte-identical.
    let rocm_enabled = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
    let cuda_enabled = std::env::var("RUSH_ENABLE_CUDA").as_deref() == Ok("1");
    if !rocm_enabled && !cuda_enabled {
        builtins.insert("tf_cuda_cc_test".to_string(), builtin_tf_cuda_cc_test);
    }

    // Bazel C++ toolchain providers and common module
    builtins.insert("CcInfo".to_string(), builtin_cc_info);
    builtins.insert("cc_common".to_string(), builtin_cc_common);
    builtins.insert(
        "cc_common.internal_DO_NOT_USE".to_string(),
        builtin_cc_internal,
    );
    builtins.insert(
        "cc_internal.create_header_info".to_string(),
        builtin_struct,
    );
    builtins.insert(
        "cc_internal.create_header_info_with_deps".to_string(),
        builtin_struct,
    );
    builtins.insert(
        "proto_common_do_not_use".to_string(),
        builtin_proto_common,
    );
    builtins.insert("DefaultInfo".to_string(), builtin_default_info);
    builtins.insert("ProtoInfo".to_string(), builtin_proto_info);
    builtins.insert("OutputGroupInfo".to_string(), builtin_output_group_info);
    builtins.insert("InstrumentedFilesInfo".to_string(), builtin_instrumented_files_info);
    builtins.insert("coverage_common".to_string(), builtin_coverage_common);
    builtins.insert("testing".to_string(), builtin_testing);

    // Additional Bazel common modules
    builtins.insert("config_common".to_string(), builtin_config_common);
    builtins.insert("config".to_string(), builtin_config);
    builtins.insert("platform_common".to_string(), builtin_platform_common);
    builtins.insert("proto_common".to_string(), builtin_proto_common);
    builtins.insert("java_common".to_string(), builtin_java_common);
    builtins.insert(
        "__rush_java_info_init".to_string(),
        builtin_java_info_init,
    );
    builtins.insert(
        "__rush_java_plugin_info_init".to_string(),
        builtin_java_plugin_info_init,
    );
    builtins.insert("apple_common".to_string(), builtin_apple_common);
    builtins.insert(
        "apple_common.apple_toolchain".to_string(),
        builtin_apple_toolchain,
    );
    builtins.insert("AndroidIdeInfo".to_string(), builtin_provider_stub);
    builtins.insert("AnalysisTestResultInfo".to_string(), builtin_provider_stub);
    builtins.insert("RunEnvironmentInfo".to_string(), builtin_provider_stub);
    builtins.insert("DebugPackageInfo".to_string(), builtin_provider_stub);
    builtins.insert("FilesToRunProvider".to_string(), builtin_provider_stub);
    builtins.insert("PyInfo".to_string(), builtin_provider_stub);
    builtins.insert("PyRuntimeInfo".to_string(), builtin_provider_stub);
    builtins.insert("py_internal".to_string(), builtin_py_internal);

    // C++ providers. CcSharedLibraryInfo / CcSharedLibraryHintInfo are native
    // providers modern rules_cc (0.0.17, used by protobuf via Bzlmod) references
    // in cc/private/rules_impl/native.bzl; without them the .bzl eval fails
    // "Undefined variable: CcSharedLibraryInfo". TF 2.15's older WORKSPACE rules_cc
    // didn't reference them, hence they were absent.
    builtins.insert("CcSharedLibraryInfo".to_string(), builtin_provider_stub);
    builtins.insert("CcSharedLibraryHintInfo".to_string(), builtin_provider_stub);
    // C++ toolchain providers
    builtins.insert("CcToolchainConfigInfo".to_string(), builtin_provider_stub);
    builtins.insert("cc_toolchain_type".to_string(), builtin_label_stub);
    builtins.insert("ToolchainTypeInfo".to_string(), builtin_provider_stub);
    builtins.insert("ToolchainInfo".to_string(), builtin_provider_stub);
    builtins.insert(
        "TemplateVariableInfo".to_string(),
        builtin_template_variable_info,
    );
    builtins.insert("ConstraintValueInfo".to_string(), builtin_provider_stub);

    // fragment() - returns fragment object
    builtins.insert("fragment".to_string(), builtin_fragment);

    builtins
}

/// len(obj) - Return the length of a sequence
fn builtin_len(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("len() takes exactly 1 argument"));
    }

    match &args[0] {
        Value::String(s) => Ok(Value::Int(s.len() as i64)),
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => Ok(Value::Int(items.len() as i64)),
        Value::Dict(map) => Ok(Value::Int(map.len() as i64)),
        Value::ImmutableMapping(view) => Ok(Value::Int(view.entries.len() as i64)),
        _ => Err(anyhow!("len() argument must be a sequence")),
    }
}

/// hash(value) - Return a hash value for the given value
/// In Starlark, hash() can only be called on immutable (frozen) values:
/// strings, ints, bools, and tuples of hashable values.
fn builtin_hash(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("hash() takes exactly 1 argument"));
    }

    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();

    match &args[0] {
        Value::String(s) => {
            s.hash(&mut hasher);
            Ok(Value::Int(hasher.finish() as i64))
        }
        Value::Int(i) => {
            i.hash(&mut hasher);
            Ok(Value::Int(hasher.finish() as i64))
        }
        Value::Bool(b) => {
            b.hash(&mut hasher);
            Ok(Value::Int(hasher.finish() as i64))
        }
        Value::None => {
            "None".hash(&mut hasher);
            Ok(Value::Int(hasher.finish() as i64))
        }
        // Preserve the existing nonstandard list hash extension. Real Tuple
        // values do not enter this arm (Bazel 6.5 hash() accepts strings only).
        Value::List(items) => {
            for item in items {
                match item {
                    Value::String(s) => s.hash(&mut hasher),
                    Value::Int(i) => i.hash(&mut hasher),
                    Value::Bool(b) => b.hash(&mut hasher),
                    _ => return Err(anyhow!("unhashable type in tuple")),
                }
            }
            Ok(Value::Int(hasher.finish() as i64))
        }
        other => Err(anyhow!("unhashable type: {:?}", other)),
    }
}

/// max(...) - Return the maximum value
fn builtin_max(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("max() expected at least 1 argument, got 0"));
    }

    // Handle single list argument
    let items = if args.len() == 1 {
        if let Value::List(list) | Value::Tuple(list) | Value::FrozenList(list) = &args[0] {
            if list.is_empty() {
                return Err(anyhow!("max() arg is an empty sequence"));
            }
            list.as_slice()
        } else {
            args
        }
    } else {
        args
    };

    let mut max_val = &items[0];
    for item in &items[1..] {
        match (max_val, item) {
            (Value::Int(a), Value::Int(b)) => {
                if b > a {
                    max_val = item;
                }
            }
            (Value::String(a), Value::String(b)) => {
                if b > a {
                    max_val = item;
                }
            }
            _ => return Err(anyhow!("max() arguments must be comparable")),
        }
    }
    Ok(max_val.clone())
}

/// min(...) - Return the minimum value
fn builtin_min(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("min() expected at least 1 argument, got 0"));
    }

    // Handle single list argument
    let items = if args.len() == 1 {
        if let Value::List(list) | Value::Tuple(list) | Value::FrozenList(list) = &args[0] {
            if list.is_empty() {
                return Err(anyhow!("min() arg is an empty sequence"));
            }
            list.as_slice()
        } else {
            args
        }
    } else {
        args
    };

    let mut min_val = &items[0];
    for item in &items[1..] {
        match (min_val, item) {
            (Value::Int(a), Value::Int(b)) => {
                if b < a {
                    min_val = item;
                }
            }
            (Value::String(a), Value::String(b)) => {
                if b < a {
                    min_val = item;
                }
            }
            _ => return Err(anyhow!("min() arguments must be comparable")),
        }
    }
    Ok(min_val.clone())
}

/// range(start, stop, step) - Generate a range of integers
fn builtin_range(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let (start, stop, step) = match args.len() {
        1 => {
            let stop = args[0].to_int()?;
            (0, stop, 1)
        }
        2 => {
            let start = args[0].to_int()?;
            let stop = args[1].to_int()?;
            (start, stop, 1)
        }
        3 => {
            let start = args[0].to_int()?;
            let stop = args[1].to_int()?;
            let step = args[2].to_int()?;
            (start, stop, step)
        }
        _ => return Err(anyhow!("range() takes 1 to 3 arguments")),
    };

    if step == 0 {
        return Err(anyhow!("range() step cannot be zero"));
    }

    let mut result = Vec::new();
    let mut current = start;

    if step > 0 {
        while current < stop {
            result.push(Value::Int(current));
            current += step;
        }
    } else {
        while current > stop {
            result.push(Value::Int(current));
            current += step;
        }
    }

    Ok(Value::List(result))
}

/// str(obj) - Convert to string
fn builtin_str(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("str() takes exactly 1 argument"));
    }

    Ok(Value::String(args[0].to_string()))
}

pub(crate) fn starlark_repr(value: &Value) -> String {
    match value {
        Value::Int(value) => value.to_string(),
        Value::Float(value) => {
            let mut rendered = value.to_string();
            if value.is_finite()
                && !rendered.contains('.')
                && !rendered.contains('e')
                && !rendered.contains('E')
            {
                rendered.push_str(".0");
            }
            rendered
        }
        Value::String(value) => format!("{:?}", value),
        Value::Bool(value) => if *value { "True" } else { "False" }.to_string(),
        Value::None => "None".to_string(),
        Value::List(values) | Value::FrozenList(values) => format!(
            "[{}]",
            values
                .iter()
                .map(starlark_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Tuple(values) => {
            let items = values.iter().map(starlark_repr).collect::<Vec<_>>().join(", ");
            format!("({}{})", items, if values.len() == 1 { "," } else { "" })
        }
        Value::Dict(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            format!(
                "{{{}}}",
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        format!(
                            "{}: {}",
                            starlark_repr(&dictionary_key_value(key)),
                            starlark_repr(value)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::Label { label, .. } => format!("Label({:?})", label),
        Value::Select(conditions) => {
            starlark_repr(&Value::Dict(conditions.clone())).replacen('{', "select({", 1) + ")"
        }
        Value::Configurable { op, left, right } => {
            let operator = match op {
                crate::evaluator::ConfigurableOp::Add => "+",
                crate::evaluator::ConfigurableOp::BitOr => "|",
            };
            format!(
                "{} {} {}",
                starlark_repr(left),
                operator,
                starlark_repr(right)
            )
        }
        _ => value.to_string(),
    }
}

pub(crate) fn starlark_type_name(value: &Value) -> Result<&str> {
    match value {
        Value::Int(_) => Ok("int"),
        Value::Float(_) => Ok("float"),
        Value::String(_) => Ok("string"),
        Value::Bool(_) => Ok("bool"),
        Value::None => Ok("NoneType"),
        Value::List(_) | Value::FrozenList(_) => Ok("list"),
        Value::Tuple(_) => Ok("tuple"),
        Value::Dict(_) => Ok("dict"),
        Value::Function { .. } | Value::Lambda { .. } => Ok("function"),
        Value::Namespace(_) | Value::WorkspaceNativeNamespace { .. } => Ok("namespace"),
        Value::RuleFunction(_) | Value::ProjectionRule(_) => Ok("rule_function"),
        Value::AttributeDefinition(_) => Ok("Attribute"),
        Value::NativeFunction(_) => Ok("builtin_function_or_method"),
        Value::ImmutableMapping(view) => Ok(view.type_name()),
        Value::ModuleExtension(_) => Ok("AutoValue_ModuleExtension"),
        Value::Provider { .. } => Ok("provider"),
        Value::ProviderInstance { provider_name, .. } => Ok(provider_name),
        Value::Select(_) | Value::Configurable { .. } => Ok("select"),
        Value::BoundMethod { .. } => Ok("method"),
        Value::Label { .. } => Ok("Label"),
        Value::LazyLoad {
            bzl_label,
            symbol_name,
        } => Err(anyhow!(
            "Internal error: LazyLoad value for symbol '{}' from '{}' should have been resolved before use",
            symbol_name,
            bzl_label
        )),
    }
}

/// repr(obj) - Return a Starlark literal representation.
fn builtin_repr(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 || !kwargs.is_empty() {
        return Err(anyhow!("repr() takes exactly 1 argument"));
    }

    Ok(Value::String(starlark_repr(&args[0])))
}

/// Parse a string into an integer the way Starlark's `int(s, base)` does:
/// optional surrounding whitespace, an optional +/- sign, an optional radix
/// prefix (0x/0o/0b) consistent with `base` (or inferred when base==0).
fn parse_int_str(s: &str, base: i64) -> Result<Value> {
    let t = s.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, digits): (u32, &str) = if base == 0 {
        if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            (16, r)
        } else if let Some(r) = t.strip_prefix("0o").or_else(|| t.strip_prefix("0O")) {
            (8, r)
        } else if let Some(r) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
            (2, r)
        } else {
            (10, t)
        }
    } else {
        let prefixes: &[&str] = match base {
            16 => &["0x", "0X"],
            8 => &["0o", "0O"],
            2 => &["0b", "0B"],
            _ => &[],
        };
        let stripped = prefixes.iter().find_map(|p| t.strip_prefix(p)).unwrap_or(t);
        (base as u32, stripped)
    };
    match i64::from_str_radix(digits, radix) {
        Ok(n) => Ok(Value::Int(if neg { -n } else { n })),
        Err(_) => Err(anyhow!("int(): invalid literal for base {}: {:?}", base, s)),
    }
}

/// int(obj, base=10) - Convert to integer (parses strings, like Starlark/Bazel)
fn builtin_int(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(anyhow!("int() takes 1 or 2 arguments"));
    }
    // `base` may be the second positional arg or a `base=` keyword.
    let base_val = if args.len() == 2 {
        Some(args[1].to_int()?)
    } else {
        match kwargs.iter().find(|(k, _)| k == "base") {
            Some((_, v)) => Some(v.to_int()?),
            None => None,
        }
    };
    match &args[0] {
        // int("7"), int("0x1f", 16), … — parse the string.
        Value::String(s) => parse_int_str(s, base_val.unwrap_or(10)),
        // int(3.9) / int(True) — numeric coercion; base is not allowed.
        other => {
            if base_val.is_some() {
                return Err(anyhow!("int(): can't convert non-string with explicit base"));
            }
            Ok(Value::Int(other.to_int()?))
        }
    }
}

/// float(obj) - Convert a string or numeric value to a floating-point value.
fn builtin_float(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 || !kwargs.is_empty() {
        return Err(anyhow!("float() takes exactly 1 argument"));
    }

    match &args[0] {
        Value::String(value) => value
            .trim()
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| anyhow!("float(): invalid literal: {:?}", value)),
        value => Ok(Value::Float(value.to_float()?)),
    }
}

/// bool(obj) - Convert to boolean
fn builtin_bool(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("bool() takes exactly 1 argument"));
    }

    Ok(Value::Bool(args[0].is_truthy()))
}

/// list(iterable) - Convert to list
/// Bazel 6.5 MethodLibrary.tuple: an optional positional Starlark iterable.
/// Strings are not StarlarkIterable; an existing tuple retains its value/kind.
fn builtin_tuple(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if !kwargs.is_empty() {
        return Err(anyhow!("tuple() does not accept keyword arguments"));
    }
    match args {
        [] => Ok(Value::Tuple(Vec::new())),
        [Value::List(items) | Value::Tuple(items) | Value::FrozenList(items)] => Ok(Value::Tuple(items.clone())),
        [Value::Dict(map)] => Ok(Value::Tuple(sorted_dictionary_key_values(map))),
        [Value::ImmutableMapping(view)] => Ok(Value::Tuple(sorted_dictionary_key_values(&view.entries))),
        [value] => Err(anyhow!("tuple() argument must be iterable, got {}", starlark_type_name(value)?)),
        _ => Err(anyhow!("tuple() accepts at most 1 positional argument")),
    }
}

fn builtin_list(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() {
        return Ok(Value::List(Vec::new()));
    }

    if args.len() != 1 {
        return Err(anyhow!("list() takes at most 1 argument"));
    }

    match &args[0] {
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => Ok(Value::List(items.clone())),
        Value::String(s) => {
            let chars: Vec<Value> = s.chars().map(|c| Value::String(c.to_string())).collect();
            Ok(Value::List(chars))
        }
        // Starlark `list(dict)` yields the dict's keys. Bazel preserves
        // insertion order; rush's Value::Dict is a HashMap, so we sort the
        // keys to guarantee deterministic output (the alternative — HashMap
        // iteration order — varies per run and would break build
        // reproducibility). TF's tflite_combine_cc_tests does
        // `srcs = list(combined_test_srcs)` over a {src: True} set-dict, where
        // sorted order is fine.
        Value::Dict(map) => {
            Ok(Value::List(sorted_dictionary_key_values(map)))
        }
        Value::ImmutableMapping(view) => Ok(Value::List(sorted_dictionary_key_values(&view.entries))),
        _ => Err(anyhow!("list() argument must be iterable")),
    }
}

/// dict(items) - Create a dictionary
fn builtin_dict(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if matches!(args.first(), Some(Value::ImmutableMapping(_))) && args.len() > 1 {
        return Err(anyhow!("dict() accepts at most one positional argument"));
    }
    let mut map = match args {
        [Value::ImmutableMapping(view)] => view.copied_entries(),
        _ => HashMap::new(),
    };
    for (key, value) in kwargs {
        map.insert(key.clone(), value.clone());
    }
    Ok(Value::Dict(map))
}

/// print(*args) - Print values
fn builtin_print(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let output: Vec<String> = args.iter().map(|v| v.to_string()).collect();
    println!("{}", output.join(" "));
    Ok(Value::None)
}

/// type(obj) - Return the type name
fn builtin_type(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("type() takes exactly 1 argument"));
    }

    Ok(Value::String(starlark_type_name(&args[0])?.to_string()))
}

/// Extract (include, exclude) pattern lists from a glob() call's args/kwargs.
/// Positional `glob(["*.cc"])` or keyword `glob(include=[...], exclude=[...])`.
fn extract_glob_patterns(
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<(Vec<String>, Vec<String>)> {
    let patterns: Vec<String> = if !args.is_empty() {
        match &args[0] {
            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items.iter().map(|v| v.to_string()).collect(),
            Value::String(s) => vec![s.clone()],
            _ => {
                return Err(anyhow!(
                    "glob() patterns must be strings or list of strings"
                ))
            }
        }
    } else {
        kwargs
            .iter()
            .find(|(k, _)| k == "include")
            .and_then(|(_, v)| match v {
                Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => Some(items.iter().map(|v| v.to_string()).collect()),
                _ => None,
            })
            .ok_or_else(|| {
                anyhow!("glob() requires either positional patterns or include= kwarg")
            })?
    };
    let exclude_patterns: Vec<String> = kwargs
        .iter()
        .find(|(k, _)| k == "exclude")
        .and_then(|(_, v)| match v {
            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => Some(items.iter().map(|v| v.to_string()).collect()),
            _ => None,
        })
        .unwrap_or_default();
    Ok((patterns, exclude_patterns))
}

/// Match a single path SEGMENT (no `/`) against a glob segment pattern.
///
/// Supports `*` (any run of chars), `?` (one char), and `[...]` character
/// classes (with a leading `!` or `^` negation), mirroring the `glob` crate's
/// per-segment semantics. A literal pattern with no metacharacters matches
/// verbatim. `**` is handled by the walker, never here.
fn glob_segment_matches(pattern: &str, name: &str) -> bool {
    fn rec_ascii(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => (0..=t.len()).any(|i| rec_ascii(&p[1..], &t[i..])),
            b'?' => !t.is_empty() && rec_ascii(&p[1..], &t[1..]),
            b'[' => {
                if t.is_empty() {
                    return false;
                }
                let mut j = 1;
                let mut negate = false;
                if j < p.len() && (p[j] == b'!' || p[j] == b'^') {
                    negate = true;
                    j += 1;
                }
                let class_start = j;
                if j < p.len() && p[j] == b']' {
                    j += 1;
                }
                while j < p.len() && p[j] != b']' {
                    j += 1;
                }
                if j >= p.len() {
                    return t[0] == b'[' && rec_ascii(&p[1..], &t[1..]);
                }
                let class = &p[class_start..j];
                let mut hit = false;
                let mut k = 0;
                while k < class.len() {
                    if k + 2 < class.len() && class[k + 1] == b'-' {
                        if t[0] >= class[k] && t[0] <= class[k + 2] {
                            hit = true;
                        }
                        k += 3;
                    } else {
                        if t[0] == class[k] {
                            hit = true;
                        }
                        k += 1;
                    }
                }
                hit != negate && rec_ascii(&p[j + 1..], &t[1..])
            }
            byte => !t.is_empty() && t[0] == byte && rec_ascii(&p[1..], &t[1..]),
        }
    }

    if pattern.is_ascii() && name.is_ascii() {
        return rec_ascii(pattern.as_bytes(), name.as_bytes());
    }

    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = name.chars().collect();
    // Recursive backtracking matcher over char slices.
    fn rec(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => {
                // `*` matches zero or more chars within a segment.
                for i in 0..=t.len() {
                    if rec(&p[1..], &t[i..]) {
                        return true;
                    }
                }
                false
            }
            '?' => !t.is_empty() && rec(&p[1..], &t[1..]),
            '[' => {
                if t.is_empty() {
                    return false;
                }
                // Find the closing ']'. A ']' immediately after '[' or '[!' is
                // treated as a literal member, matching glob conventions.
                let mut j = 1;
                let mut negate = false;
                if j < p.len() && (p[j] == '!' || p[j] == '^') {
                    negate = true;
                    j += 1;
                }
                let class_start = j;
                if j < p.len() && p[j] == ']' {
                    j += 1;
                }
                while j < p.len() && p[j] != ']' {
                    j += 1;
                }
                if j >= p.len() {
                    // Unterminated class: treat '[' as a literal.
                    return t[0] == '[' && rec(&p[1..], &t[1..]);
                }
                let class = &p[class_start..j];
                let mut hit = false;
                let mut k = 0;
                while k < class.len() {
                    if k + 2 < class.len() && class[k + 1] == '-' {
                        // Range a-z.
                        if t[0] >= class[k] && t[0] <= class[k + 2] {
                            hit = true;
                        }
                        k += 3;
                    } else {
                        if t[0] == class[k] {
                            hit = true;
                        }
                        k += 1;
                    }
                }
                if hit != negate {
                    rec(&p[j + 1..], &t[1..])
                } else {
                    false
                }
            }
            c => !t.is_empty() && t[0] == c && rec(&p[1..], &t[1..]),
        }
    }
    rec(&pat, &txt)
}

/// Match a complete slash-separated path against pre-split glob segments.
/// `*`/`?`/classes stay within one segment; `**` consumes zero or more path
/// segments. A trailing `**` must consume at least one segment because glob()
/// returns files, not the directory immediately preceding the `**`.
fn glob_path_matches(pattern: &[&str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    if pattern[0] == "**" {
        if pattern.len() == 1 {
            return !path.is_empty();
        }
        return glob_path_matches(&pattern[1..], path)
            || (!path.is_empty() && glob_path_matches(pattern, &path[1..]));
    }
    !path.is_empty()
        && glob_segment_matches(pattern[0], path[0])
        && glob_path_matches(&pattern[1..], &path[1..])
}

/// Walk the filesystem expanding glob `pattern` (a slash-separated path glob),
/// invoking `emit` for every matching REGULAR FILE. Every directory consulted is
/// listed via `tracked::read_dir_names`, so a `DirListing` dependency is recorded
/// for it — that is the whole point of the tracked walk: adding or removing a
/// file in any globbed directory changes a recorded listing and invalidates a
/// reused analysis graph.
///
/// `pattern` is resolved relative to `base` when supplied, otherwise relative to
/// the process CWD. Package-aware callers supply their package directory as
/// `base` and set `stop_at_subpackages`, which prevents any match from crossing a
/// nested BUILD/BUILD.bazel boundary. `**` matches zero or more directory levels.
fn tracked_glob_walk(
    pattern: &str,
    base: Option<&Path>,
    stop_at_subpackages: bool,
    dir_cache: &mut TrackedDirListingCache,
    emit: &mut dyn FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let start = match base {
        Some(base) => base.to_path_buf(),
        None if pattern.starts_with('/') => PathBuf::from("/"),
        None => PathBuf::from("."),
    };
    walk_segments(
        &start,
        &segments,
        stop_at_subpackages,
        dir_cache,
        emit,
    )
}

/// True if a segment contains any glob metacharacter.
fn has_glob_meta(seg: &str) -> bool {
    seg.contains('*') || seg.contains('?') || seg.contains('[')
}

/// List `dir`'s entry names through the tracked IO, recording a `DirListing`
/// dependency. A missing/unlistable directory yields an empty listing (the
/// negative probe is still recorded by `read_dir_names`).
type TrackedDirListingCache = HashMap<PathBuf, std::sync::Arc<[String]>>;

thread_local! {
    /// Shared only across glob() calls in one package evaluation. The evaluator
    /// clears it whenever it installs a package context, so daemon commands and
    /// later BUILD files never reuse stale directory observations.
    static PACKAGE_GLOB_DIR_CACHE: std::cell::RefCell<TrackedDirListingCache> =
        std::cell::RefCell::new(TrackedDirListingCache::new());
}

pub(crate) fn clear_package_glob_cache() {
    PACKAGE_GLOB_DIR_CACHE.with(|cache| cache.borrow_mut().clear());
}

fn tracked_list(
    dir: &Path,
    dir_cache: &mut TrackedDirListingCache,
) -> std::sync::Arc<[String]> {
    if let Some(names) = dir_cache.get(dir) {
        return std::sync::Arc::clone(names);
    }
    let names: std::sync::Arc<[String]> = match rush_incremental::tracked::read_dir_names(dir) {
        Ok(names) => names
            .into_iter()
            .filter_map(|n| n.to_str().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .into(),
        Err(_) => Vec::<String>::new().into(),
    };
    dir_cache.insert(dir.to_path_buf(), std::sync::Arc::clone(&names));
    names
}

/// Resolve a path's kind through the tracked IO, following ONE level of symlink
/// when needed (the tracked `metadata_kind` uses `symlink_metadata`, i.e. does
/// NOT follow links, so a symlinked source file reports `Symlink`). BUILD globs
/// regularly match symlinked sources in external/bazel symlink forests, so we
/// resolve the link target and record its kind too. Returns the effective kind
/// after one hop (`File`/`Dir`), or `None` for a dangling/missing path.
fn tracked_resolved_kind(path: &Path) -> Option<rush_incremental::FileKind> {
    use rush_incremental::FileKind;
    // Follow symlink CHAINS to a bounded fixpoint. Bazel 8's
    // repo-contents-cache materializes every external repo as
    // output_base/external/<repo> -> cache/repos/v1/contents/<hash>/<id>,
    // and rush's own external/<name> registration symlink stacks a second
    // hop on top — 2-hop chains are the NORM for external-repo globs, so
    // stopping at one hop made glob() return empty for those repos. Each
    // read_link/metadata_kind call goes through tracked IO, keeping
    // incremental invalidation sound. Bound matches the kernel's ELOOP
    // nesting limit.
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match rush_incremental::tracked::metadata_kind(&current) {
            Some(FileKind::Symlink) => {
                let target = rush_incremental::tracked::read_link(&current)?;
                current = if target.is_absolute() {
                    target
                } else {
                    current.parent().map(|p| p.join(&target)).unwrap_or(target)
                };
            }
            other => return other,
        }
    }
    // Cycle or chain too deep — treat as missing (ELOOP parity).
    None
}

/// Is `path` a regular file (following symlinks)? Probed through the tracked IO
/// so a `FileExistence` (with kind) dependency is recorded.
fn tracked_is_file(path: &Path) -> bool {
    matches!(tracked_resolved_kind(path), Some(rush_incremental::FileKind::File))
}

/// Is `path` a directory (following symlinks)? Probed through the tracked IO.
fn tracked_is_dir(path: &Path) -> bool {
    matches!(tracked_resolved_kind(path), Some(rush_incremental::FileKind::Dir))
}

/// True when `dir` starts a Bazel subpackage. The directory listing supplies
/// the negative dependency for absent BUILD names; when a candidate name is
/// present, tracked kind resolution ensures a directory named `BUILD` is not
/// mistaken for a BUILD file and handles symlinked files consistently.
fn tracked_is_package_directory(
    dir: &Path,
    dir_cache: &mut TrackedDirListingCache,
) -> bool {
    let names = tracked_list(dir, dir_cache);
    ["BUILD", "BUILD.bazel"].iter().any(|build_name| {
        names.iter().any(|name| name == *build_name)
            && tracked_is_file(&dir.join(*build_name))
    })
}

/// Recursively match the remaining glob `segments` starting at directory `dir`,
/// invoking `emit` for matching regular files. Directory listings are recorded as
/// `DirListing` deps via [`tracked_list`].
fn walk_segments(
    dir: &Path,
    segments: &[&str],
    stop_at_subpackages: bool,
    dir_cache: &mut TrackedDirListingCache,
    emit: &mut dyn FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let strip_dot = |p: &Path| -> PathBuf {
        // Keep results free of a leading "./" so callers' prefix-stripping works.
        p.strip_prefix(".").map(|s| s.to_path_buf()).unwrap_or_else(|_| p.to_path_buf())
    };

    if segments.is_empty() {
        return Ok(());
    }
    let seg = segments[0];
    let rest = &segments[1..];

    if seg == "**" {
        // `**` matches zero or more directory levels. Try matching `rest` at the
        // current level (zero levels), then recurse into each subdirectory
        // consuming one level (keeping `**` at the head).
        // Zero-level case: apply the remaining pattern here.
        if rest.is_empty() {
            // A trailing `**` matches all files at/under `dir`.
            for name in tracked_list(dir, dir_cache).iter() {
                let child = dir.join(&name);
                if tracked_is_file(&child) {
                    emit(&strip_dot(&child))?;
                } else if tracked_is_dir(&child)
                    && (!stop_at_subpackages
                        || !tracked_is_package_directory(&child, dir_cache))
                {
                    walk_segments(
                        &child,
                        segments,
                        stop_at_subpackages,
                        dir_cache,
                        emit,
                    )?;
                }
            }
            return Ok(());
        }
        walk_segments(dir, rest, stop_at_subpackages, dir_cache, emit)?;
        for name in tracked_list(dir, dir_cache).iter() {
            let child = dir.join(&name);
            if tracked_is_dir(&child)
                && (!stop_at_subpackages
                    || !tracked_is_package_directory(&child, dir_cache))
            {
                walk_segments(
                    &child,
                    segments,
                    stop_at_subpackages,
                    dir_cache,
                    emit,
                )?;
            }
        }
        return Ok(());
    }

    let last = rest.is_empty();

    if !has_glob_meta(seg) {
        // Literal segment: follow it without listing the parent.
        let child = dir.join(seg);
        if last {
            if tracked_is_file(&child) {
                emit(&strip_dot(&child))?;
            }
        } else if tracked_is_dir(&child)
            && (!stop_at_subpackages
                || !tracked_is_package_directory(&child, dir_cache))
        {
            walk_segments(
                &child,
                rest,
                stop_at_subpackages,
                dir_cache,
                emit,
            )?;
        }
        return Ok(());
    }

    // Wildcard segment: list this directory and match each entry.
    for name in tracked_list(dir, dir_cache).iter() {
        if !glob_segment_matches(seg, &name) {
            continue;
        }
        let child = dir.join(&name);
        if last {
            if tracked_is_file(&child) {
                emit(&strip_dot(&child))?;
            }
        } else if tracked_is_dir(&child)
            && (!stop_at_subpackages
                || !tracked_is_package_directory(&child, dir_cache))
        {
            walk_segments(
                &child,
                rest,
                stop_at_subpackages,
                dir_cache,
                emit,
            )?;
        }
    }
    Ok(())
}

/// Core glob: match `include` minus `exclude` patterns under `base` (a
/// directory the patterns are resolved relative to), returning paths RELATIVE
/// to `base`, sorted. A supplied base denotes BUILD-package evaluation, so nested
/// packages are pruned. With no base, patterns resolve relative to the process
/// CWD and results are returned verbatim (legacy fallback behavior).
///
/// Bazel evaluates `glob()` relative to the PACKAGE directory and yields
/// package-relative names; pass the package dir as `base` to get that. The
/// `base/` prefix is stripped from results so consumers
/// (FilegroupInstance::analyze, cc rules via resolve_cc_src_path) resolve the
/// names under `<workspace>/<package>` without doubling the package.
fn glob_core(
    include: &[String],
    exclude: &[String],
    base: Option<&Path>,
    dir_cache: &mut TrackedDirListingCache,
) -> Result<Value> {
    let strip = |path: &Path| -> String {
        let relative = base
            .and_then(|base| path.strip_prefix(base).ok())
            .unwrap_or(path);
        relative.to_string_lossy().replace('\\', "/")
    };

    // #39 graph-RSS guard: cap the number of matched files. A BUILD glob that
    // matches hundreds of thousands of files is pathological — almost always a
    // recursive `**` pattern resolving against the wrong base (e.g. an
    // external-repo glob that fell back to the workspace-root CWD and follows
    // bazel-*/.cache symlink forests). Materializing that set OOM'd the 91GB
    // host instantiating @local_xla//xla:c_srcs (glob(["**/*.cc","**/*.h"]) ->
    // 55GB+). Bail with a named error instead so the loader stubs the package
    // and continues and the runaway pattern is visible. RUSH_GLOB_MAX_MATCHES
    // overrides; 0 disables.
    let max_matches: usize = std::env::var("RUSH_GLOB_MAX_MATCHES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(100_000);

    // Persistent-daemon TrackedIo migration: glob expansion walks the filesystem
    // by LISTING directories, and the SET of names in each listed directory is
    // what determines the match set — so a created/removed file in a globbed
    // directory must invalidate any reused analysis graph. We therefore drive the
    // walk through `tracked::read_dir_names` (which records a `DirListing`
    // dependency per directory) instead of the `glob` crate, whose internal
    // `fs::read_dir` we cannot intercept. The matcher supports the same
    // `*`/`?`/`[...]`/`**` syntax the BUILD globs rely on.
    let mut matched_files = HashSet::new();
    for pattern in include {
        let full = base
            .map(|base| base.join(pattern).display().to_string())
            .unwrap_or_else(|| pattern.clone());
        tracked_glob_walk(pattern, base, base.is_some(), dir_cache, &mut |path| {
            matched_files.insert(strip(path));
            if max_matches != 0 && matched_files.len() > max_matches {
                return Err(anyhow!(
                    "glob pattern '{}' (base '{}') exceeded {} matches — \
                     refusing to materialize a runaway match set (likely a \
                     recursive '**' glob against the wrong base / symlink \
                     forest). Set RUSH_GLOB_MAX_MATCHES to override.",
                    full,
                    base.map(|base| base.display().to_string())
                        .unwrap_or_default(),
                    max_matches
                ));
            }
            Ok(())
        })?;
    }
    if !exclude.is_empty() && !matched_files.is_empty() {
        // Excludes can only remove paths found by the include walk. Rewalking
        // the filesystem for every exclude is redundant and, for TensorFlow's
        // large kernel glob, traverses the same cached tree dozens of times.
        // The include walk already recorded every directory listing capable of
        // changing the result set, so filtering here preserves invalidation.
        let exclude_segments: Vec<Vec<&str>> = exclude
            .iter()
            .map(|pattern| pattern.split('/').filter(|s| !s.is_empty()).collect())
            .collect();
        matched_files.retain(|path| {
            let path_segments: Vec<&str> =
                path.split('/').filter(|segment| !segment.is_empty()).collect();
            !exclude_segments
                .iter()
                .any(|pattern| glob_path_matches(pattern, &path_segments))
        });
    }

    let mut result: Vec<String> = matched_files.into_iter().collect();
    result.sort();
    Ok(Value::List(result.into_iter().map(Value::String).collect()))
}

/// glob(patterns, exclude=[]) — CWD-relative variant (legacy / fallback for
/// non-BUILD eval contexts without a package). BUILD evaluation routes through
/// `glob_in_package` instead so patterns resolve under the package dir.
fn builtin_glob(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let (include, exclude) = extract_glob_patterns(args, kwargs)?;
    let mut dir_cache = TrackedDirListingCache::new();
    glob_core(&include, &exclude, None, &mut dir_cache)
}

/// Package-aware glob used during BUILD evaluation: resolves patterns relative
/// to `package` and returns package-relative names. Full BUILD evaluation passes
/// the package's absolute on-disk directory for both main and external
/// repositories; the relative form remains supported for evaluator callers that
/// have only a main-workspace package name.
pub fn glob_in_package(
    args: &[Value],
    kwargs: &[(String, Value)],
    package: &str,
) -> Result<Value> {
    let (include, exclude) = extract_glob_patterns(args, kwargs)?;
    PACKAGE_GLOB_DIR_CACHE.with(|cache| {
        glob_core(
            &include,
            &exclude,
            Some(Path::new(package)),
            &mut cache.borrow_mut(),
        )
    })
}

/// package(**kwargs) - Set package-level configuration
///
/// In Bazel/Starlark, package() sets metadata and defaults for all rules
/// in the BUILD file. Common arguments:
/// - default_visibility: List of visibility labels
/// - licenses: License identifiers
/// - features: List of features to enable/disable
///
/// BUILD evaluation intercepts this native call to retain default visibility.
/// This stateless fallback acknowledges package metadata in evaluation
/// contexts that do not capture BUILD targets.
fn builtin_package(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::None)
}

/// licenses(license_list) - Declare licenses for the package
///
/// Bazel metadata function that declares the type of licenses that apply to code
/// in this package. Common values are "notice" (Apache, BSD, MIT) and "restricted".
///
/// This is metadata-only and doesn't affect the build. Rush acknowledges the
/// declaration but doesn't enforce license checks.
///
/// Examples:
/// - licenses(["notice"])  # Apache 2.0, BSD, MIT, etc.
/// - licenses(["restricted"])  # Proprietary or restricted licenses
fn builtin_licenses(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // License declarations are metadata-only
    // Rush doesn't enforce license compatibility checks yet
    Ok(Value::None)
}

/// visibility("public" | [labels]) — the .bzl-file-level load-visibility
/// declaration (Bazel 6+). It restricts which packages may load() this .bzl;
/// rush does not enforce bzl-visibility, so this is a no-op. protobuf's
/// bazel/*.bzl call it at module scope.
fn builtin_visibility(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::None)
}

/// select(conditions) - Select values based on configuration
///
/// Produces a configurable value whose branch is selected when a compatible
/// target is instantiated. Falls back to "//conditions:default" if no specific
/// condition matches at that point.
///
/// Example:
/// ```starlark
/// deps = select({
///     "@platforms//os:linux": ["linux_lib"],
///     "@platforms//os:osx": ["macos_lib"],
///     "//conditions:default": ["default_lib"],
/// })
/// ```
///
/// Every non-default key must name a declared config_setting or
/// constraint_value. `//conditions:default` is the only built-in key and is
/// used solely as the fallback.
fn builtin_select(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("select() takes exactly 1 argument"));
    }

    match &args[0] {
        // Bazel's select() produces a configurable value; it does not choose a
        // branch while loading a .bzl/BUILD module. Keep every selector intact
        // so later-registered config_settings and target configuration get the
        // final say. The rule-execution bridge resolves it for a compatible
        // target and defers a remaining miss to that target's build request.
        Value::Dict(conditions) => {
            let mut selectors = HashMap::with_capacity(conditions.len());
            for (key, value) in conditions {
                let selector = configuration_label_dictionary_key(key).ok_or_else(|| {
                    anyhow!("select() keys must be label strings or Label values")
                })?;
                if selectors.insert(selector.clone(), value.clone()).is_some() {
                    return Err(anyhow!(
                        "select() contains duplicate condition `{selector}`"
                    ));
                }
            }
            Ok(Value::Select(selectors))
        }
        _ => Err(anyhow!("select() argument must be a dict")),
    }
}

/// provider(doc, fields=[...]) - Create a provider type
///
/// Providers are used to pass structured information between rules.
/// Supports multiple formats:
/// - provider() - No fields specified (accepts any fields)
/// - provider(fields = ["foo", "bar"]) - List of field names
/// - provider("doc string", fields = {"foo": "field doc", "bar": "field doc"}) - Dict with docs
/// - provider(fields = [...], init = fn) - Public and raw constructors
///
/// In Bazel, the `fields` parameter is optional. When not specified, the provider
/// accepts any keyword arguments when instantiated.
fn builtin_provider(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // Extract optional doc string (first positional argument)
    let _doc = if !args.is_empty() {
        match &args[0] {
            Value::String(s) => Some(s.clone()),
            _ => return Err(anyhow!("provider() first argument must be a doc string")),
        }
    } else {
        None
    };

    // Extract fields from kwargs (optional in Bazel)
    let fields_value = kwargs
        .iter()
        .find(|(key, _)| key == "fields")
        .map(|(_, val)| val);

    // Extract field names from list or dict, or use empty vec if not specified
    let fields = match fields_value {
        Some(Value::List(items) | Value::Tuple(items) | Value::FrozenList(items)) => {
            // List format: fields = ["foo", "bar"]
            let mut field_names = Vec::new();
            for item in items {
                match item {
                    Value::String(s) => field_names.push(s.clone()),
                    _ => return Err(anyhow!("provider fields must be strings")),
                }
            }
            field_names
        }
        Some(Value::Dict(map)) => {
            // Dict format: fields = {"foo": "doc", "bar": "doc"}
            // Extract keys from the dict (field names)
            let mut field_names: Vec<String> = sorted_dictionary_key_values(map)
                .into_iter()
                .map(|key| match key {
                    Value::String(key) => Ok(key),
                    _ => Err(anyhow!("provider fields must be strings")),
                })
                .collect::<Result<_>>()?;
            field_names.sort(); // Sort for determinism
            field_names
        }
        Some(_) => return Err(anyhow!("provider 'fields' must be a list or dict")),
        None => {
            // No fields specified - provider accepts any fields (Bazel behavior)
            // We represent this with an empty Vec and handle it specially in evaluation
            Vec::new()
        }
    };

    let init = kwargs
        .iter()
        .find(|(key, _)| key == "init")
        .map(|(_, value)| value.clone());
    if let Some(value) = init.as_ref() {
        if !matches!(value, Value::Function { .. }) {
            return Err(anyhow!("provider 'init' must be a function"));
        }
    }

    // Generate a provider name (in real Starlark, this would come from variable assignment)
    let name = if fields.is_empty() {
        "DynamicProvider".to_string()
    } else {
        format!("Provider{}", fields.len())
    };

    if let Some(init) = init {
        // Bazel returns (public_constructor, raw_constructor) only when an
        // initializer is supplied. Rush represents that tuple as a List.
        let raw_provider = Value::Provider {
            name: name.clone(),
            fields: fields.clone(),
            init: None,
        };
        let public_provider = Value::Provider {
            name,
            fields,
            init: Some(Box::new(init)),
        };
        Ok(Value::List(vec![public_provider, raw_provider]))
    } else {
        // Without init=, provider() itself is the callable constructor.
        Ok(Value::Provider {
            name,
            fields,
            init: None,
        })
    }
}

/// Label(label_string) - Create a label object
///
/// In Bazel, Label() creates a label object from a string.
/// Labels are used to reference build targets, e.g., "//foo:bar" or ":target"
///
/// Example: Label("//tensorflow:tensorflow")
///
/// Returns a Label object with attributes: name, package, workspace_name, workspace_root
fn builtin_label(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("Label() takes exactly 1 argument"));
    }

    match &args[0] {
        Value::String(label_str) => {
            // Parse label format: [@workspace]//package:name
            let (workspace_name, rest) = if let Some(stripped) = label_str
                .strip_prefix("@@")
                .or_else(|| label_str.strip_prefix('@'))
            {
                let parts: Vec<&str> = stripped.splitn(2, "//").collect();
                if parts.len() == 2 {
                    (parts[0].to_string(), parts[1])
                } else {
                    (String::new(), label_str.as_str())
                }
            } else {
                (String::new(), label_str.as_str())
            };

            // Remove leading "//" if present
            let rest = rest.strip_prefix("//").unwrap_or(rest);

            // Split package and name
            let (package, name) = if let Some(colon_pos) = rest.find(':') {
                let pkg = &rest[..colon_pos];
                let tgt = &rest[colon_pos + 1..];
                (pkg.to_string(), tgt.to_string())
            } else {
                // No colon means the whole thing is the package, name is package name
                let last_part = rest.split('/').next_back().unwrap_or(rest);
                (rest.to_string(), last_part.to_string())
            };

            // workspace_root is empty for main workspace, workspace name for external
            let workspace_root = if workspace_name.is_empty() {
                String::new()
            } else {
                format!("external/{}", workspace_name)
            };

            Ok(Value::Label {
                label: label_str.clone(),
                package,
                name,
                workspace_name,
                workspace_root,
            })
        }
        _ => Err(anyhow!("Label() argument must be a string")),
    }
}

/// struct(**kwargs) - Create a struct with named fields
///
/// In Bazel/Starlark, struct() creates a simple object with named fields.
/// Fields can be accessed using dot notation: obj.field_name
///
/// Example:
/// ```starlark
/// point = struct(x=1, y=2)
/// print(point.x)  # prints 1
/// ```
///
/// Returns a Namespace value containing the fields.
fn builtin_struct(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();

    for (key, value) in kwargs {
        fields.insert(key.clone(), value.clone());
    }

    Ok(Value::Namespace(fields))
}

/// depset(direct=[], transitive=[], order="default") - Create an efficient set structure
///
/// In Bazel, depset is an optimized data structure for accumulating large sets of values
/// across rule dependencies. It supports efficient merging of sets and deduplication.
///
/// Arguments:
/// - direct: List of direct values to include
/// - transitive: List of other depsets whose values should be included transitively
/// - order: Order in which elements are iterated ("default", "preorder", "postorder", "topological")
///
/// Example:
/// ```starlark
/// direct_deps = depset(["a.o", "b.o"])
/// all_deps = depset(direct = ["main.o"], transitive = [direct_deps])
/// ```
///
/// Current implementation: Returns a List containing the merged direct and transitive items.
/// FIXME: Future enhancement: proper depset semantics with deduplication and ordering.
fn builtin_depset(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut all_items = Vec::new();
    let mut seen = HashSet::new();

    // Extract 'direct' parameter
    if let Some((_, direct_value)) = kwargs.iter().find(|(k, _)| k == "direct") {
        match direct_value {
            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                for item in items {
                    let item_str = item.to_string();
                    if seen.insert(item_str.clone()) {
                        all_items.push(item.clone());
                    }
                }
            }
            _ => return Err(anyhow!("depset 'direct' parameter must be a list")),
        }
    }

    // Extract 'transitive' parameter (list of depsets)
    if let Some((_, transitive_value)) = kwargs.iter().find(|(k, _)| k == "transitive") {
        match transitive_value {
            Value::List(depsets) | Value::Tuple(depsets) | Value::FrozenList(depsets) => {
                for depset in depsets {
                    // Each depset is just a list in our current implementation
                    match depset {
                        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => {
                            for item in items {
                                let item_str = item.to_string();
                                if seen.insert(item_str.clone()) {
                                    all_items.push(item.clone());
                                }
                            }
                        }
                        _ => {
                            // Ignore non-list items (could be actual depset values)
                            // In a full implementation, we'd traverse the depset structure
                        }
                    }
                }
            }
            _ => return Err(anyhow!("depset 'transitive' parameter must be a list")),
        }
    }

    // For now, return as a regular list
    // TODO: Return a proper Depset value type with ordering semantics
    Ok(Value::List(all_items))
}

/// aspect(implementation=None, attr_aspects=[], attrs={}) - Define an aspect
///
/// In Bazel, aspects are like "visitors" that can propagate information through
/// the dependency graph. They attach additional information to rules and their dependencies.
///
/// Arguments:
/// - implementation: Function that implements the aspect logic
/// - attr_aspects: List of attribute names to propagate the aspect through
/// - attrs: Dictionary of attributes the aspect requires
///
/// Example:
/// ```starlark
/// def _my_aspect_impl(target, ctx):
///     return struct(...)
///
/// my_aspect = aspect(
///     implementation = _my_aspect_impl,
///     attr_aspects = ["deps"],
/// )
/// ```
///
/// Current implementation: Returns a minimal Namespace representing the aspect.
/// FIXME: Future enhancement: proper aspect evaluation during dependency analysis.
fn builtin_aspect(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // Extract aspect configuration
    let mut aspect_config = HashMap::new();

    // Store implementation function
    if let Some((_, impl_value)) = kwargs.iter().find(|(k, _)| k == "implementation") {
        aspect_config.insert("implementation".to_string(), impl_value.clone());
    }

    // Store attr_aspects (attributes to propagate through)
    if let Some((_, attr_aspects)) = kwargs.iter().find(|(k, _)| k == "attr_aspects") {
        aspect_config.insert("attr_aspects".to_string(), attr_aspects.clone());
    }

    // Store attrs (aspect's own attributes)
    if let Some((_, attrs)) = kwargs.iter().find(|(k, _)| k == "attrs") {
        aspect_config.insert("attrs".to_string(), attrs.clone());
    }

    // Return a namespace representing the aspect
    // In real Bazel, this would be registered in the build graph
    Ok(Value::Namespace(aspect_config))
}

/// transition(implementation=None, inputs=[], outputs=[]) - Define a configuration transition
///
/// In Bazel, transition() creates a configuration transition that can change build settings
/// for dependencies. This is used for cross-compilation, changing optimization levels, etc.
///
/// Arguments:
/// - implementation: Function that implements the transition logic
/// - inputs: List of configuration settings to read
/// - outputs: List of configuration settings to write
///
/// Example:
/// ```starlark
/// def _my_transition_impl(settings, attr):
///     return {
///         "//command_line_option:cpu": "arm64",
///     }
///
/// my_transition = transition(
///     implementation = _my_transition_impl,
///     inputs = [],
///     outputs = ["//command_line_option:cpu"],
/// )
/// ```
///
/// Current implementation: Returns a minimal Namespace representing the transition.
/// Rush does not execute transitions - they are stubs for .bzl file parsing only.
fn builtin_transition(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // Extract transition configuration
    let mut transition_config = HashMap::new();

    // Store all kwargs in the transition configuration
    for (key, value) in kwargs {
        transition_config.insert(key.clone(), value.clone());
    }

    // Implementation is optional for stubs
    if !kwargs.iter().any(|(k, _)| k == "implementation") {
        #[cfg(feature = "bzl-debug")]
        eprintln!("[WARN] transition() called without 'implementation' parameter - creating stub transition");
    }

    // Return a namespace representing the transition
    // In real Bazel, this would modify the configuration graph
    Ok(Value::Namespace(transition_config))
}

/// rule(implementation=None, attrs={}, outputs={}, executable=False) - Define a build rule
///
/// In Bazel, rule() creates a custom build rule type that can be instantiated in BUILD files.
///
/// Arguments:
/// - implementation: Function that implements the rule's build logic
/// - attrs: Dictionary of attributes the rule accepts
/// - outputs: Dictionary of output file templates
/// - executable: Whether the rule produces an executable output
/// - test: Whether this is a test rule
///
/// Example:
/// ```starlark
/// def _my_rule_impl(ctx):
///     # Build logic here
///     pass
///
/// my_rule = rule(
///     implementation = _my_rule_impl,
///     attrs = {
///         "srcs": attr.label_list(allow_files = True),
///         "deps": attr.label_list(),
///     },
/// )
/// ```
///
/// Current implementation: Returns a RuleFunction that can be called in BUILD files.
/// FIXME: Future enhancement: proper rule evaluation and action registration.
fn builtin_rule(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // Extract rule configuration
    let mut rule_config = HashMap::new();

    // Store all kwargs in the rule configuration
    for (key, value) in kwargs {
        rule_config.insert(key.clone(), value.clone());
    }

    // Extract implementation function (optional for stubs)
    // Some .bzl files define rules without implementations for type checking/exports
    if !kwargs.iter().any(|(k, _)| k == "implementation") {
        #[cfg(feature = "bzl-debug")]
        eprintln!("[WARN] rule() called without 'implementation' parameter - creating stub rule");
    }

    // Preserve whether this rule definition actually declares Bazel build-
    // setting semantics.  An instance attribute named
    // `build_setting_default` is not itself proof that the target is a build
    // setting; ordinary rules may legally use that spelling.  The evaluator
    // turns this private marker into instance metadata when the rule is
    // called.
    let rule_name = if rule_config.contains_key("build_setting") {
        "build_setting:CustomRule"
    } else {
        "CustomRule"
    }
    .to_string();

    // Return a RuleFunction value
    // When this is called in a BUILD file, it will create a rule instance
    Ok(Value::RuleFunction(rule_name))
}

/// hasattr(object, name) - Check if object has an attribute
///
/// Returns True if the object has an attribute with the given name, False otherwise.
///
/// Example:
/// ```starlark
/// ctx = struct(attr = struct(deps = []))
/// has_deps = hasattr(ctx.attr, "deps")  # True
/// has_srcs = hasattr(ctx.attr, "srcs")  # False
/// ```
fn builtin_hasattr(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 2 {
        return Err(anyhow!("hasattr() takes exactly 2 arguments"));
    }

    let obj = &args[0];
    let attr_name = match &args[1] {
        Value::String(s) => s,
        _ => return Err(anyhow!("hasattr() attribute name must be a string")),
    };

    let has_attr = match obj {
        Value::FrozenList(_) => matches!(attr_name.as_str(), "append" | "clear" | "extend" | "index" | "insert" | "pop" | "remove" | "count"),
        Value::ImmutableMapping(view) => view.has_method(attr_name),
        Value::WorkspaceNativeNamespace { attrs, .. } => {
            attr_name == "bazel_version" || attrs.contains_key(attr_name)
        }
        Value::Namespace(attrs) => attrs.contains_key(attr_name),
        Value::ProviderInstance { fields, .. } => fields.contains_key(attr_name),
        Value::Label { .. } => {
            matches!(
                attr_name.as_str(),
                "name" | "package" | "workspace_name" | "workspace_root"
            )
        }
        Value::String(_) => {
            matches!(
                attr_name.as_str(),
                "elems"
                    | "split"
                    | "strip"
                    | "replace"
                    | "startswith"
                    | "endswith"
                    | "upper"
                    | "lower"
                    | "format"
                    | "join"
                    | "capitalize"
                    | "count"
                    | "find"
                    | "rfind"
                    | "index"
                    | "rindex"
                    | "lstrip"
                    | "rstrip"
            )
        }
        _ => false,
    };

    Ok(Value::Bool(has_attr))
}

/// getattr(object, name, default=None) - Get attribute value from object
///
/// Returns the value of the named attribute, or default if not present.
///
/// Example:
/// ```starlark
/// ctx = struct(attr = struct(deps = ["foo"]))
/// deps = getattr(ctx.attr, "deps", [])  # ["foo"]
/// srcs = getattr(ctx.attr, "srcs", [])  # []
/// ```
fn builtin_getattr(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() < 2 || args.len() > 3 {
        return Err(anyhow!("getattr() takes 2 or 3 arguments"));
    }

    let obj = &args[0];
    let attr_name = match &args[1] {
        Value::String(s) => s,
        _ => return Err(anyhow!("getattr() attribute name must be a string")),
    };
    let default_value = args.get(2).unwrap_or(&Value::None);

    if matches!(obj, Value::String(_)) && attr_name == "elems" {
        return Ok(Value::BoundMethod { object: Box::new(obj.clone()), method: attr_name.clone() });
    }
    if let Value::FrozenList(_) = obj {
        if matches!(attr_name.as_str(), "append" | "clear" | "extend" | "index" | "insert" | "pop" | "remove" | "count") {
            return Ok(Value::BoundMethod { object: Box::new(obj.clone()), method: attr_name.clone() });
        }
    }
    if let Value::ImmutableMapping(view) = obj {
        return if view.has_method(attr_name) {
            Ok(Value::BoundMethod { object: Box::new(obj.clone()), method: attr_name.clone() })
        } else if let Some(default) = args.get(2) {
            Ok(default.clone())
        } else {
            Err(anyhow!("immutable mapping has no attribute '{attr_name}'"))
        };
    }
    if matches!(obj, Value::ModuleExtension(_)) {
        return args.get(2).cloned().ok_or_else(||
            anyhow!("module-extension declarations have no attribute '{attr_name}'"));
    }

    if let Value::WorkspaceNativeNamespace { attrs, workspace_root } = obj {
        return if attr_name == "bazel_version" {
            crate::bzl_loader::workspace_repository_analyzer_version(workspace_root)
                .map(Value::String)
        } else {
            Ok(attrs.get(attr_name).cloned().unwrap_or_else(|| default_value.clone()))
        };
    }

    let attr_value = match obj {
        Value::Namespace(attrs) => attrs.get(attr_name),
        Value::ProviderInstance { fields, .. } => fields.get(attr_name),
        Value::Label {
            package,
            name,
            workspace_name,
            workspace_root,
            ..
        } => match attr_name.as_str() {
            "name" => Some(&Value::String(name.clone())),
            "package" => Some(&Value::String(package.clone())),
            "workspace_name" => Some(&Value::String(workspace_name.clone())),
            "workspace_root" => Some(&Value::String(workspace_root.clone())),
            _ => None,
        },
        _ => None,
    };

    Ok(attr_value.cloned().unwrap_or_else(|| default_value.clone()))
}

/// Preserve a Bazel 6.5 module-extension declaration without executing it.
fn builtin_module_extension(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    use crate::evaluator::ModuleExtensionDeclaration;
    if args.len() > 1 {
        return Err(anyhow!("module_extension() accepts only implementation positionally; remaining arguments are keyword-only"));
    }
    let mut arguments = HashMap::new();
    for (name, value) in kwargs {
        if !matches!(name.as_str(), "implementation" | "tag_classes" | "doc" | "environ" | "os_dependent" | "arch_dependent") {
            return Err(anyhow!("module_extension() got unexpected keyword argument '{name}'"));
        }
        if arguments.insert(name.as_str(), value.clone()).is_some() {
            return Err(anyhow!("module_extension() got multiple values for '{name}'"));
        }
    }
    if let Some(implementation) = args.first() {
        if arguments.insert("implementation", implementation.clone()).is_some() {
            return Err(anyhow!("module_extension() got multiple values for 'implementation'"));
        }
    }
    let implementation = arguments.remove("implementation")
        .ok_or_else(|| anyhow!("module_extension() missing required argument 'implementation'"))?;
    if !matches!(&implementation, Value::Function { .. } | Value::Lambda { .. }
        | Value::NativeFunction(_) | Value::Provider { .. } | Value::BoundMethod { .. })
    {
        return Err(anyhow!("module_extension() implementation must be a supported callable; compatibility rule markers cannot implement extensions"));
    }
    let tag_classes = match arguments.remove("tag_classes") {
        None => HashMap::new(),
        Some(Value::Dict(values)) if values.is_empty() => values,
        Some(Value::Dict(_)) => return Err(anyhow!("module_extension() nonempty tag_classes requires genuine tag_class declarations; native tag_class construction is not supported, delegate extension evaluation to Bazel")),
        Some(_) => return Err(anyhow!("module_extension() tag_classes must be a dict")),
    };
    let doc = match arguments.remove("doc") {
        None => String::new(),
        Some(Value::String(value)) => value,
        Some(_) => return Err(anyhow!("module_extension() doc must be a string")),
    };
    let environ = match arguments.remove("environ") {
        None => Vec::new(),
        Some(Value::List(values) | Value::Tuple(values) | Value::FrozenList(values)) => values.into_iter().map(|value| match value {
            Value::String(value) => Ok(value),
            _ => Err(anyhow!("module_extension() environ must contain strings")),
        }).collect::<Result<Vec<_>>>()?,
        Some(_) => return Err(anyhow!("module_extension() environ must be a list or tuple of strings")),
    };
    let boolean = |value: Option<Value>, name: &str| match value {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(value),
        Some(_) => Err(anyhow!("module_extension() {name} must be a bool")),
    };
    let os_dependent = boolean(arguments.remove("os_dependent"), "os_dependent")?;
    let arch_dependent = boolean(arguments.remove("arch_dependent"), "arch_dependent")?;
    let source_context = crate::bzl_loader::BzlLoader::capture_thread_context()
        .and_then(|context| context.load_stack.last().cloned());
    Ok(Value::ModuleExtension(std::sync::Arc::new(ModuleExtensionDeclaration {
        implementation, tag_classes, doc, environ, os_dependent, arch_dependent, source_context,
    })))
}

/// repository_rule(implementation, attrs={}, environ=[]) - Define a repository rule
///
/// Repository rules are used to fetch external dependencies and create external repositories.
///
/// Arguments:
/// - implementation: Function that implements the repository rule logic
/// - attrs: Dictionary of attributes the repository rule accepts
/// - environ: List of environment variable names the rule can access
///
/// Example:
/// ```starlark
/// def _python_repository_impl(repository_ctx):
///     version = repository_ctx.os.environ.get("TF_PYTHON_VERSION", "3.10")
///     repository_ctx.file("BUILD", "")
///     repository_ctx.file("py_version.bzl", "VERSION = \"%s\"" % version)
///
/// python_repository = repository_rule(
///     implementation = _python_repository_impl,
///     environ = ["TF_PYTHON_VERSION"],
/// )
/// ```
///
/// Current implementation: Returns a minimal RuleFunction placeholder.
/// FIXME: Future enhancement: proper repository rule evaluation with repository_ctx.
fn builtin_repository_rule(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // Extract repository rule configuration
    let mut rule_config = HashMap::new();

    // Store all kwargs in the rule configuration
    for (key, value) in kwargs {
        rule_config.insert(key.clone(), value.clone());
    }

    // Extract implementation function (optional for stubs)
    // Some .bzl files define repository rules without implementations for type checking/exports
    if !kwargs.iter().any(|(k, _)| k == "implementation") {
        #[cfg(feature = "bzl-debug")]
        eprintln!("[WARN] repository_rule() called without 'implementation' parameter - creating stub rule");
    }

    // Generate a rule name (in real Starlark, this comes from variable assignment)
    let rule_name = "RepositoryRule".to_string();

    // Return a RuleFunction value
    // When this is called, it will create a repository instance
    Ok(Value::RuleFunction(rule_name))
}

/// attr() - Return the attr module namespace
///
/// The attr module provides attribute descriptor functions for defining rule attributes.
/// Available descriptors:
/// - attr.label() - Single label attribute
/// - attr.label_list() - List of labels
/// - attr.string() - String attribute
/// - attr.string_list() - List of strings
/// - attr.string_dict() - String dictionary
/// - attr.string_list_dict() - Dictionary with string keys and list-of-strings values
/// - attr.int() - Integer attribute
/// - attr.bool() - Boolean attribute
/// - attr.label_keyed_string_dict() - Label-keyed string dictionary
///
/// Example:
/// ```starlark
/// my_rule = rule(
///     implementation = _impl,
///     attrs = {
///         "srcs": attr.label_list(allow_files = True),
///         "name": attr.string(mandatory = True),
///     },
/// )
/// ```
/// exec_group(toolchains=[], exec_compatible_with=[]) — declares an execution
/// group within a rule()/aspect() definition (Bazel's multi-exec-platform
/// support). rush runs all actions on the host and does not model exec groups,
/// so this returns an opaque struct carrying the kwargs purely so rule defs that
/// reference exec_group(...) (e.g. protobuf's upb proto-library rules) evaluate.
fn builtin_exec_group(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    for (k, v) in kwargs {
        fields.insert(k.clone(), v.clone());
    }
    Ok(Value::Namespace(fields))
}

fn builtin_attr(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut attr_module = HashMap::new();
    attr_module.insert(
        "label".to_string(),
        Value::NativeFunction("attr.label".to_string()),
    );
    attr_module.insert(
        "label_list".to_string(),
        Value::NativeFunction("attr.label_list".to_string()),
    );
    attr_module.insert(
        "string".to_string(),
        Value::NativeFunction("attr.string".to_string()),
    );
    attr_module.insert(
        "string_list".to_string(),
        Value::NativeFunction("attr.string_list".to_string()),
    );
    attr_module.insert(
        "string_dict".to_string(),
        Value::NativeFunction("attr.string_dict".to_string()),
    );
    attr_module.insert(
        "string_list_dict".to_string(),
        Value::NativeFunction("attr.string_list_dict".to_string()),
    );
    attr_module.insert(
        "int".to_string(),
        Value::NativeFunction("attr.int".to_string()),
    );
    attr_module.insert(
        "bool".to_string(),
        Value::NativeFunction("attr.bool".to_string()),
    );
    attr_module.insert(
        "label_keyed_string_dict".to_string(),
        Value::NativeFunction("attr.label_keyed_string_dict".to_string()),
    );
    // Bazel 7.4+: dict attribute with string keys and Label values
    // (used by @bazel_tools http.bzl _http_archive_attrs "files").
    attr_module.insert(
        "string_keyed_label_dict".to_string(),
        Value::NativeFunction("attr.string_keyed_label_dict".to_string()),
    );
    // Keep this whitelist in sync with register_builtins in evaluator.rs:
    // int_list/output/output_list exist there but were missing here.
    attr_module.insert(
        "int_list".to_string(),
        Value::NativeFunction("attr.int_list".to_string()),
    );
    attr_module.insert(
        "output".to_string(),
        Value::NativeFunction("attr.output".to_string()),
    );
    attr_module.insert(
        "output_list".to_string(),
        Value::NativeFunction("attr.output_list".to_string()),
    );

    Ok(Value::Namespace(attr_module))
}

/// fail(message) - Fail the build with an error message
///
/// Terminates execution and returns an error with the given message.
/// Used extensively in TensorFlow build files for build-time validation.
///
/// Example:
/// ```starlark
/// if not srcs:
///     fail("srcs must not be empty")
/// ```
fn builtin_fail(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("fail() requires at least one argument"));
    }

    let message = args[0].to_string();
    Err(anyhow!("{}", message))
}

/// sorted(iterable, key=None, reverse=False) - Return a sorted list
///
/// Returns a new sorted list from the items in iterable.
/// Supports optional `reverse` parameter for descending order.
///
/// Example:
/// ```starlark
/// sorted([3, 1, 2])  # [1, 2, 3]
/// sorted(["b", "a", "c"])  # ["a", "b", "c"]
/// sorted([1, 5, 3], reverse=True)  # [5, 3, 1]
/// ```
fn builtin_sorted(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("sorted() requires at least one argument"));
    }

    let items = match &args[0] {
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items.clone(),
        _ => return Err(anyhow!("sorted() argument must be a list")),
    };

    // Check for reverse parameter
    let reverse = kwargs
        .iter()
        .find(|(k, _)| k == "reverse")
        .and_then(|(_, v)| match v {
            Value::Bool(b) => Some(*b),
            _ => None,
        })
        .unwrap_or(false);

    let mut sorted_items = items;

    // Sort based on value type
    sorted_items.sort_by(|a, b| {
        use std::cmp::Ordering;
        let cmp = match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            (Value::Float(x), Value::Float(y)) => {
                if x < y {
                    Ordering::Less
                } else if x > y {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            }
            (Value::String(x), Value::String(y)) => x.cmp(y),
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            _ => Ordering::Equal,
        };

        if reverse {
            cmp.reverse()
        } else {
            cmp
        }
    });

    Ok(Value::List(sorted_items))
}

/// enumerate(iterable, start=0) - Return enumerate object
///
/// Returns a list of tuples containing (index, value) pairs.
/// Optionally starts counting from a different value.
///
/// Example:
/// ```starlark
/// enumerate(["a", "b", "c"])  # [(0, "a"), (1, "b"), (2, "c")]
/// enumerate(["x", "y"], start=10)  # [(10, "x"), (11, "y")]
/// ```
fn builtin_enumerate(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() {
        return Err(anyhow!("enumerate() requires at least one argument"));
    }

    let items = match &args[0] {
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items.clone(),
        Value::String(value) => value
            .chars()
            .map(|character| Value::String(character.to_string()))
            .collect(),
        Value::Dict(values) => {
            sorted_dictionary_key_values(values)
        }
        value => {
            return Err(anyhow!(
                "enumerate() argument must be iterable, got {:?}",
                value
            ))
        }
    };

    // Check for start parameter
    let start = kwargs
        .iter()
        .find(|(k, _)| k == "start")
        .and_then(|(_, v)| match v {
            Value::Int(i) => Some(*i),
            _ => None,
        })
        .unwrap_or(0);

    let result: Vec<Value> = items
        .into_iter()
        .enumerate()
        .map(|(idx, val)| Value::Tuple(vec![Value::Int(start + idx as i64), val]))
        .collect();

    Ok(Value::List(result))
}

/// zip(*iterables) - Zip multiple iterables together
///
/// Returns a list of tuples where the i-th tuple contains the i-th element
/// from each of the argument sequences. Stops when the shortest iterable is exhausted.
///
/// Example:
/// ```starlark
/// zip([1, 2, 3], ["a", "b", "c"])  # [(1, "a"), (2, "b"), (3, "c")]
/// zip([1, 2], ["a", "b", "c"])  # [(1, "a"), (2, "b")]
/// ```
fn builtin_zip(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() {
        return Ok(Value::List(vec![]));
    }

    // Extract all lists and find minimum length
    let lists: Result<Vec<&Vec<Value>>> = args
        .iter()
        .map(|arg| match arg {
            Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => Ok(items),
            _ => Err(anyhow!("zip() arguments must be lists")),
        })
        .collect();
    let lists = lists?;

    // Find the minimum length
    let min_len = lists.iter().map(|l| l.len()).min().unwrap_or(0);

    // Create tuples
    let mut result = Vec::new();
    for i in 0..min_len {
        let tuple: Vec<Value> = lists.iter().map(|list| list[i].clone()).collect();
        result.push(Value::Tuple(tuple));
    }

    Ok(Value::List(result))
}

/// any(iterable) - Return True if any element is truthy
///
/// Returns True if any element of the iterable is truthy. If the iterable
/// is empty, returns False.
///
/// Example:
/// ```starlark
/// any([False, True, False])  # True
/// any([False, False])  # False
/// any([])  # False
/// any([0, 1, 2])  # True (1 and 2 are truthy)
/// ```
fn builtin_any(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("any() takes exactly 1 argument"));
    }

    let items = match &args[0] {
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items,
        _ => return Err(anyhow!("any() argument must be iterable")),
    };

    for item in items {
        if item.is_truthy() {
            return Ok(Value::Bool(true));
        }
    }

    Ok(Value::Bool(false))
}

/// all(iterable) - Return True if all elements are truthy
///
/// Returns True if all elements of the iterable are truthy. If the iterable
/// is empty, returns True.
///
/// Example:
/// ```starlark
/// all([True, True, True])  # True
/// all([True, False])  # False
/// all([])  # True
/// all([1, 2, 3])  # True (all non-zero)
/// all([1, 0, 2])  # False (0 is falsy)
/// ```
fn builtin_all(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("all() takes exactly 1 argument"));
    }

    let items = match &args[0] {
        Value::List(items) | Value::Tuple(items) | Value::FrozenList(items) => items,
        _ => return Err(anyhow!("all() argument must be iterable")),
    };

    for item in items {
        if !item.is_truthy() {
            return Ok(Value::Bool(false));
        }
    }

    Ok(Value::Bool(true))
}

/// existing_rule(name) - Check if a rule with the given name already exists
///
/// In Bazel WORKSPACE files, this is used to avoid re-declaring external repositories
/// or rules that may have been defined by previous workspace includes.
///
/// Returns None if no rule exists, or a dict with rule information if it does.
///
/// Example:
/// ```starlark
/// if not native.existing_rule("my_repo"):
///     http_archive(name = "my_repo", ...)
/// ```
///
/// Current implementation: Always returns None (minimal implementation)
/// TODO: Track declared rules in evaluator context
fn builtin_existing_rule(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("existing_rule() takes exactly 1 argument"));
    }

    let _name = match &args[0] {
        Value::String(s) => s,
        _ => return Err(anyhow!("existing_rule() argument must be a string")),
    };

    // TODO: Check if rule with this name exists in current context
    // For now, always return None to allow rules to be defined
    Ok(Value::None)
}

/// existing_rules() - Get dict of all rules defined in current BUILD/WORKSPACE file
///
/// Returns a dictionary mapping rule names to their configurations.
///
/// Example:
/// ```starlark
/// rules = native.existing_rules()
/// if "my_repo" in rules:
///     print("my_repo already defined")
/// ```
///
/// Current implementation: Returns empty dict (minimal implementation)
/// TODO: Track all declared rules in evaluator context
fn builtin_existing_rules(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // TODO: Return actual rules from current context
    // For now, return empty dict
    Ok(Value::Dict(HashMap::new()))
}

/// native - Return the native module namespace
///
/// The native module provides access to native Bazel functions like existing_rule,
/// existing_rules, glob, etc. In WORKSPACE files, functions like http_archive are
/// accessed via native.http_archive().
///
/// Returns a namespace with native Bazel functions.
///
/// Example:
/// ```starlark
/// if not native.existing_rule("my_repo"):
///     native.http_archive(name = "my_repo", ...)
/// ```
fn builtin_native(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut native_module = HashMap::new();
    native_module.insert("repository_name".into(), Value::NativeFunction("repository_name".into()));

    // Add native functions to the module
    native_module.insert(
        "existing_rule".to_string(),
        Value::NativeFunction("native.existing_rule".to_string()),
    );
    native_module.insert(
        "existing_rules".to_string(),
        Value::NativeFunction("native.existing_rules".to_string()),
    );
    native_module.insert(
        "glob".to_string(),
        Value::NativeFunction("native.glob".to_string()),
    );
    native_module.insert(
        "package".to_string(),
        Value::NativeFunction("native.package".to_string()),
    );

    // Add built-in rule functions (matching Bazel's native module)
    // This allows macros to call native.cc_library(), native.py_binary(), etc.

    // C/C++ rules
    native_module.insert(
        "cc_library".to_string(),
        Value::RuleFunction("cc_library".to_string()),
    );
    native_module.insert(
        "cc_binary".to_string(),
        Value::RuleFunction("cc_binary".to_string()),
    );
    native_module.insert(
        "cc_test".to_string(),
        Value::RuleFunction("cc_test".to_string()),
    );
    native_module.insert(
        "cc_import".to_string(),
        Value::RuleFunction("cc_import".to_string()),
    );
    // This records a real declaration. Configured toolchain consumption is
    // rejected by the native rule until its provider/exec semantics exist.
    native_module.insert(
        "proto_lang_toolchain".to_string(),
        Value::RuleFunction("proto_lang_toolchain".to_string()),
    );
    native_module.insert(
        "cc_proto_library".to_string(),
        Value::RuleFunction("cc_proto_library".to_string()),
    );
    native_module.insert(
        "objc_library".to_string(),
        Value::NativeFunction("objc_library".to_string()),
    );

    // Java rules
    native_module.insert(
        "java_library".to_string(),
        Value::RuleFunction("java_library".to_string()),
    );
    native_module.insert(
        "java_binary".to_string(),
        Value::RuleFunction("java_binary".to_string()),
    );
    native_module.insert(
        "java_test".to_string(),
        Value::RuleFunction("java_test".to_string()),
    );
    native_module.insert(
        "java_import".to_string(),
        Value::RuleFunction("java_import".to_string()),
    );

    // Python rules
    native_module.insert(
        "py_library".to_string(),
        Value::RuleFunction("py_library".to_string()),
    );
    native_module.insert(
        "py_binary".to_string(),
        Value::RuleFunction("py_binary".to_string()),
    );
    native_module.insert(
        "py_test".to_string(),
        Value::RuleFunction("py_test".to_string()),
    );
    native_module.insert(
        "py_runtime".to_string(),
        Value::RuleFunction("py_runtime".to_string()),
    );
    native_module.insert(
        "py_runtime_pair".to_string(),
        Value::RuleFunction("py_runtime_pair".to_string()),
    );

    // Protocol buffers
    native_module.insert(
        "proto_library".to_string(),
        Value::RuleFunction("proto_library".to_string()),
    );

    // Generic rules
    native_module.insert(
        "genrule".to_string(),
        Value::RuleFunction("genrule".to_string()),
    );
    native_module.insert(
        "filegroup".to_string(),
        Value::RuleFunction("filegroup".to_string()),
    );
    native_module.insert(
        "exports_files".to_string(),
        Value::NativeFunction("exports_files".to_string()),
    );
    native_module.insert(
        "alias".to_string(),
        Value::RuleFunction("alias".to_string()),
    );

    // Shell rules
    native_module.insert(
        "sh_binary".to_string(),
        Value::RuleFunction("sh_binary".to_string()),
    );
    native_module.insert(
        "sh_test".to_string(),
        Value::RuleFunction("sh_test".to_string()),
    );
    native_module.insert(
        "sh_library".to_string(),
        Value::RuleFunction("sh_library".to_string()),
    );

    // Test suite
    native_module.insert(
        "test_suite".to_string(),
        Value::RuleFunction("test_suite".to_string()),
    );

    // Repository-rule names recognized at Starlark evaluation time.
    //
    // Bazel's `.bzl` files (e.g. `@bazel_tools//tools/build_defs/repo:
    // http.bzl`) declare `native.http_archive(...)` etc. Rush doesn't
    // perform the fetch itself — external repo materialization is
    // handled by external_deps.rs and repository_manager.rs — but the
    // name must resolve during evaluation so the `.bzl` parses and can
    // be cached. Binding to a RuleFunction is the mechanism: it tells
    // the evaluator "this identifier is a rule", capturing the call as
    // a RuleCall that the orchestrator then routes to the real fetch
    // path.
    //
    // Historical note: adding these bindings raised rush's .bzl cache
    // hit rate from 69% to ~80% because files that referenced
    // http_archive were previously failing evaluation and getting
    // re-parsed every invocation.
    native_module.insert(
        "http_archive".to_string(),
        Value::RuleFunction("http_archive".to_string()),
    );
    native_module.insert(
        "http_file".to_string(),
        Value::RuleFunction("http_file".to_string()),
    );
    native_module.insert(
        "http_jar".to_string(),
        Value::RuleFunction("http_jar".to_string()),
    );
    native_module.insert(
        "git_repository".to_string(),
        Value::RuleFunction("git_repository".to_string()),
    );
    native_module.insert(
        "new_git_repository".to_string(),
        Value::RuleFunction("new_git_repository".to_string()),
    );
    native_module.insert(
        "new_local_repository".to_string(),
        Value::RuleFunction("new_local_repository".to_string()),
    );
    native_module.insert(
        "local_repository".to_string(),
        Value::RuleFunction("local_repository".to_string()),
    );

    Ok(Value::Namespace(native_module))
}

/// package_group(name, packages=[], includes=[]) - Define a package group for visibility
///
/// In Bazel, package_group defines a set of packages that can be used in visibility
/// declarations to control which packages can depend on a target.
///
/// Arguments:
/// - name: Name of the package group
/// - packages: List of package specifications (e.g., ["//foo/...", "//bar:__pkg__"])
/// - includes: List of other package_group labels to include
///
/// Example:
/// ```starlark
/// package_group(
///     name = "internal",
///     packages = [
///         "//tensorflow/...",
///         "//tensorflow_models/...",
///     ],
/// )
/// ```
///
/// Accepted and ignored: rush does not enforce Bazel visibility rules,
/// so package_group declarations have no effect beyond succeeding the
/// Starlark evaluation. See UNIMPLEMENTED.md "visibility enforcement"
/// for the path to implementing this.
fn builtin_package_group(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Metadata-only: package_group controls visibility, which rush
    // doesn't enforce. Returning Value::None keeps the evaluator
    // moving; the dep graph doesn't need this target.
    Ok(Value::None)
}

/// if_google(google_value, oss_value=[]) - Return value based on build environment
///
/// In TensorFlow/Bazel builds, if_google() returns different values depending on whether
/// the build is happening in Google's internal environment or in the open-source environment.
///
/// Arguments:
/// - google_value: Value to return if building in Google's internal environment
/// - oss_value: Value to return in open-source builds (defaults to empty list)
///
/// Example:
/// ```starlark
/// features = if_google(
///     ["-layering_check", "-parse_headers"],
///     [],  # Empty features for OSS builds
/// )
/// ```
///
/// Current implementation: Always returns oss_value since Rush is an open-source build system.
fn builtin_if_google(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // FIX #183: if_google() can take 0, 1, or 2 arguments
    if args.len() > 2 {
        return Err(anyhow!("if_google() takes 0, 1, or 2 arguments"));
    }

    // Return the second argument (oss_value) if provided, otherwise return empty list
    if args.len() == 2 {
        Ok(args[1].clone())
    } else {
        // Default to empty list for open-source builds (0 or 1 argument)
        Ok(Value::List(vec![]))
    }
}

/// if_oss(oss_value, google_value=[]) - Return value based on build environment
fn builtin_if_oss(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(anyhow!("if_oss() takes 1 or 2 arguments"));
    }

    // Return the first argument (oss_value) for open-source builds
    Ok(args[0].clone())
}

/// if_windows(windows_value, otherwise=[]) - Return value based on build platform
///
/// In TensorFlow/Bazel builds, if_windows() returns different values depending on whether
/// the build is happening on Windows or other platforms (Linux, macOS).
///
/// Arguments:
/// - windows_value: Value to return if building on Windows
/// - otherwise: Value to return on non-Windows platforms (defaults to empty list)
///
/// Example:
/// ```starlark
/// srcs = if_windows(
///     ["windows_impl.cc"],
///     ["posix_impl.cc"],  # Linux/macOS implementation
/// )
/// ```
///
/// Current implementation: Always returns otherwise since Rush builds are on Linux.
fn builtin_if_windows(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(anyhow!("if_windows() takes 1 or 2 arguments"));
    }

    // Return the second argument (otherwise) if provided, otherwise return empty list
    if args.len() == 2 {
        Ok(args[1].clone())
    } else {
        // Default to empty list for non-Windows builds
        Ok(Value::List(vec![]))
    }
}

/// if_not_windows() - Conditionally include values based on platform (inverse of if_windows)
///
/// This function enables platform-specific conditional compilation.
/// It returns the provided value on non-Windows platforms, and an empty list on Windows.
///
/// Args:
///   a: List of values to include on non-Windows platforms
///
/// Returns:
///   On non-Windows (Linux): The provided list
///   On Windows: Empty list
///
/// Example:
/// ```starlark
/// cc_library(
///     name = "foo",
///     copts = if_not_windows(["-Wno-sign-compare"]),  # Only on Linux
/// )
/// ```
///
/// Current implementation: Always returns the argument since Rush builds are on Linux.
fn builtin_if_not_windows(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.len() != 1 {
        return Err(anyhow!("if_not_windows() takes exactly 1 argument"));
    }

    // Return the argument on non-Windows platforms (Linux)
    Ok(args[0].clone())
}

/// get_compatible_with_portable() - Return platform compatibility constraints
///
/// In TensorFlow builds, this function returns a list of platform constraint labels
/// that specify which platforms a target is compatible with.
///
/// In Google's internal build environment, this might return specific platform constraints.
/// In OSS builds, it returns an empty list, meaning the target is compatible with all platforms.
///
/// Example:
/// ```starlark
/// cc_library(
///     name = "portable_lib",
///     compatible_with = get_compatible_with_portable(),
/// )
/// ```
///
/// Current implementation: Always returns empty list (compatible with all platforms).
fn builtin_get_compatible_with_portable(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    // Return empty list - no platform restrictions in OSS builds
    Ok(Value::List(vec![]))
}

/// tf_copts(android_optimization_level_override="-O2", is_external=False, allow_exceptions=False)
///
/// Return TensorFlow-specific compiler options.
///
/// In TensorFlow builds, this function returns a list of compiler flags that should be
/// used when compiling TensorFlow code. It includes platform-specific flags and optimization
/// settings.
///
/// Arguments:
/// - android_optimization_level_override: Optimization level for Android (default: "-O2")
/// - is_external: Whether this is external code (default: False)
/// - allow_exceptions: Whether to allow C++ exceptions (default: False)
///
/// Example:
/// ```starlark
/// cc_library(
///     name = "my_lib",
///     copts = tf_copts(),
/// )
/// ```
///
/// Current implementation: Returns a minimal set of common compiler options.
/// A full implementation would return platform-specific flags based on the build configuration.
fn builtin_tf_copts(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Return a minimal set of compiler options that work across platforms
    // In a full implementation, this would check the platform and return appropriate flags
    let copts = vec![
        Value::String("-DEIGEN_AVOID_STL_ARRAY".to_string()),
        Value::String("-Wno-sign-compare".to_string()),
        Value::String("-ftemplate-depth=900".to_string()),
    ];

    Ok(Value::List(copts))
}

/// rocm_copts(opts = []) - Return ROCm compiler options
///
/// Returns compiler options for ROCm (AMD GPU) compilation.
/// Since Rush uses system packages and doesn't build for ROCm by default,
/// this returns an empty list.
///
/// Parameters:
///   opts: Optional list of additional compiler options to append (default: [])
///
/// Example:
/// ```starlark
/// copts = rocm_copts(opts = ["-DSOME_FLAG"])
/// ```
///
/// Current implementation: Returns empty list regardless of input.
fn builtin_rocm_copts(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Return empty list - we don't build for ROCm
    Ok(Value::List(Vec::new()))
}

/// if_not_mobile_or_arm_or_lgpl_restricted(a) - Conditional for non-mobile/ARM/LGPL builds
///
/// Returns an empty list when building for mobile, ARM, or LGPL-restricted targets.
/// Otherwise returns the provided argument list.
///
/// Parameters:
///   a: List of items to include when not building for mobile/ARM/LGPL-restricted
///
/// Example:
/// ```starlark
/// deps = if_not_mobile_or_arm_or_lgpl_restricted([":some_library"])
/// ```
///
/// Current implementation: Returns empty list (we don't restrict based on platform).
fn builtin_if_not_mobile_or_arm_or_lgpl_restricted(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    // Return empty list - we don't build mobile/ARM/LGPL-restricted variants
    Ok(Value::List(Vec::new()))
}

/// test_suite(name, tests, tags, ...) - Define a test suite
///
/// Creates a test suite that groups multiple tests together. In Bazel, test_suite
/// is used to run multiple tests as a group.
///
/// Parameters:
///   name: Name of the test suite (required)
///   tests: List of test targets to include (optional)
///   tags: List of tags (optional)
///   ... other attributes are accepted but ignored
///
/// Example:
/// ```starlark
/// test_suite(
///     name = "all_tests",
///     tests = [":test1", ":test2"],
/// )
/// ```
///
/// Accepted and ignored: rush does not execute tests as part of a build,
/// so test_suite (a grouping target, not an artifact-producing rule) has
/// no effect on the dep graph. See UNIMPLEMENTED.md "test execution".
fn builtin_test_suite(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Metadata-only: test_suite is a grouping used by `bazel test`.
    // Rush doesn't run tests, so the declaration has nothing to do.
    Ok(Value::None)
}

/// cc_shared_library — handled by rush's native CcSharedLibraryRule via
/// MACRO_PRIORITY_OVERRIDE in evaluator.rs. No builtin here.

/// objc_library: Objective-C is outside rush's C++-only scope. The
/// Starlark call is accepted and ignored so TF iOS-stage BUILD files
/// parse, but no artifacts are produced and no dep-graph node is
/// added. See UNIMPLEMENTED.md "Objective-C compilation".
fn builtin_objc_library(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Out-of-scope by design: rush compiles C/C++ only.
    Ok(Value::None)
}

/// if_portable(if_true, if_false=[]) - Conditional for portable builds
///
/// Returns different values based on whether building for portable targets.
/// Portable builds are optimized to run on a wide range of hardware without
/// specialized CPU instructions.
///
/// Parameters:
///   if_true: Value to return if building for portable targets
///   if_false: Value to return otherwise (default: empty list)
///
/// Example:
/// ```starlark
/// copts = if_portable(["-march=x86-64"], ["-march=native"])
/// ```
///
/// Current implementation: Returns if_false (not building portable).
/// Rush assumes native builds for maximum performance.
fn builtin_if_portable(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Return if_false - we're not building for portable targets
    if args.len() >= 2 {
        Ok(args[1].clone())
    } else {
        Ok(Value::List(vec![]))
    }
}

/// tf_lib_proto_parsing_deps() - Return proto parsing library dependencies
///
/// Returns a list of dependencies required for protocol buffer parsing in TensorFlow.
/// In the full TensorFlow build, this includes protobuf libraries, Eigen, and
/// platform-specific proto parsing implementations.
///
/// Example:
/// ```starlark
/// cc_library(
///     name = "lib_proto_parsing",
///     deps = tf_lib_proto_parsing_deps() + [":platform_base"],
/// )
/// ```
///
/// Current implementation: Returns empty list.
/// Rush will handle proto dependencies through explicit deps in BUILD files.
fn builtin_tf_lib_proto_parsing_deps(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    // Return empty list - proto dependencies will be resolved through standard deps
    Ok(Value::List(vec![]))
}

/// tf_additional_test_deps() - Return additional test dependencies
///
/// Returns platform-specific test dependencies for TensorFlow tests.
/// In Google's internal build, this might include internal testing infrastructure.
/// In OSS builds, returns empty list.
///
/// Example:
/// ```starlark
/// cc_test(
///     name = "my_test",
///     deps = [":my_lib"] + tf_additional_test_deps(),
/// )
/// ```
fn builtin_tf_additional_test_deps(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::List(vec![]))
}

/// if_mkl(if_true, if_false=[]) - Conditional for Intel MKL library
///
/// Returns different values based on whether Intel MKL (Math Kernel Library) is enabled.
/// MKL provides optimized math routines for Intel processors.
///
/// Arguments:
/// - if_true: Value to return if MKL is enabled
/// - if_false: Value to return if MKL is disabled (default: [])
///
/// Example:
/// ```starlark
/// cc_library(
///     name = "math_ops",
///     deps = if_mkl([":mkl_ops"], [":default_ops"]),
/// )
/// ```
///
/// Current implementation: Always returns if_false (MKL not enabled in Rush).
fn builtin_if_mkl(args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    if args.is_empty() || args.len() > 2 {
        return Err(anyhow!("if_mkl() takes 1 or 2 arguments"));
    }

    // Return if_false (second argument if provided, otherwise empty list)
    if args.len() == 2 {
        Ok(args[1].clone())
    } else {
        Ok(Value::List(vec![]))
    }
}

fn builtin_if_with_tpu_support(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // Rush doesn't support TPU, so this always returns the if_false branch. Route through
    // the shared conditional helper so BOTH the positional (if_with_tpu_support(if_true,
    // if_false=[])) AND the all-kwargs form (if_false=[...] / if_true=[...] with 0 positional
    // args) work. TF 2.17's compiler/jit/BUILD calls if_with_tpu_support with kwargs, which the
    // old positional-only impl rejected with "takes 1 or 2 arguments" (args.is_empty()).
    builtin_if_conditional(args, kwargs)
}

/// Generic conditional helper for platform/feature conditionals
///
/// Handles TensorFlow conditional functions like if_android, if_ios, if_mobile, if_libtpu,
/// if_nccl, if_cuda_or_rocm, if_dynamic_kernels, if_static.
///
/// All these follow the same pattern: if_X(if_true, if_false=[])
/// In Rush OSS builds, these features are not enabled, so we always return if_false.
///
/// `if_false` may be passed POSITIONALLY (`if_X(if_true, if_false)`) OR by
/// KEYWORD (`if_X(if_false = [...], if_true = [...])`). TF's BUILD files use
/// both forms — e.g. `//tensorflow/core:ops` adds the MLIR passthrough op via
/// `if_libtpu(if_false = ["//tensorflow/compiler/mlir/tensorflow:mlir_passthrough_op"])`,
/// all-kwargs. The previous impl only read the 2nd POSITIONAL arg and ignored
/// kwargs, so the all-kwargs form silently returned `[]` — dropping the
/// `if_false` deps (the op never reached `:ops`, so MlirPassthroughOp was
/// unregistered and api_test / backwards_compatibility_test failed). Resolve
/// the `if_false` value from kwargs first, then the 2nd positional, else `[]`.
/// `if_cuda_or_rocm(if_true, if_false=[])` — true when GPU (CUDA or ROCm) is
/// configured. Under RUSH_ENABLE_CUDA=1 or RUSH_ENABLE_ROCM=1 the GPU path is
/// active, so take the if_true (first positional) branch; otherwise fall back
/// to the disabled (if_false) behavior shared with the other platform
/// conditionals.
fn builtin_if_cuda_or_rocm(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // Persistent-daemon TrackedIo migration (env slice): this builtin runs inside
    // the Starlark evaluator's call dispatch. The evaluator is now brought under
    // a `tracked::scope` at every rush-build invocation site (parse / macro
    // expansion / instantiate / analyze-only), so this analysis-affecting env read
    // records an `Env` dep through the fail-loud `tracked::getenv_str`.
    // Read both variables unconditionally so the incremental ledger tracks
    // both configuration inputs even when CUDA is already enabled.
    let cuda_enabled =
        rush_incremental::tracked::getenv_str("RUSH_ENABLE_CUDA").as_deref() == Some("1");
    let rocm_enabled =
        rush_incremental::tracked::getenv_str("RUSH_ENABLE_ROCM").as_deref() == Some("1");
    if cuda_enabled || rocm_enabled {
        // Resolve `if_true` the same way `builtin_if_conditional` resolves
        // `if_false`: KEYWORD first, then 1st POSITIONAL. TF's BUILD files write
        // both forms, and the all-kwargs form is the common one — e.g.
        // tensorflow/compiler/tf2xla/kernels/BUILD gates the
        // light_outside_compilation dep (which transitively pulls
        // :callback_proto_cc) as
        //   if_cuda_or_rocm(if_false = [], if_true = [":light_outside_compilation"])
        // The previous impl only read the 1st POSITIONAL arg; with both passed
        // by keyword, `args.first()` was None, so it fell through to the
        // if_false ([]) branch EVEN UNDER RUSH_ENABLE_ROCM=1 — silently dropping
        // light_outside_compilation from the transitive graph, so the
        // callback_proto tf_proto_library's protoc gen action never fired as a
        // transitive dep (it built fine when named explicitly, since that
        // bypasses the select). Same bug-class as the over-broad `_gpu` skip
        // fixed in dependency_graph.rs (0770b65). Read the kwarg first.
        if let Some((_, if_true)) = kwargs.iter().find(|(k, _)| k == "if_true") {
            return Ok(if_true.clone());
        }
        if let Some(if_true) = args.first() {
            return Ok(if_true.clone());
        }
    }
    builtin_if_conditional(args, kwargs)
}

fn builtin_if_conditional(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if let Some((_, v)) = kwargs.iter().find(|(k, _)| k == "if_false") {
        return Ok(v.clone());
    }
    if args.len() > 2 {
        return Err(anyhow!("Conditional function takes 0, 1, or 2 positional arguments"));
    }
    if args.len() == 2 {
        return Ok(args[1].clone());
    }
    Ok(Value::List(vec![]))
}

/// if_static(extra_deps, otherwise=[], macos=[]) — TF's monolithic-vs-shared
/// switch. UNLIKE if_cuda/if_nccl/if_tpu/etc. (genuinely-disabled OSS features
/// → if_false), `if_static` returns `extra_deps` in TF's DEFAULT monolithic
/// /static configuration (the non-`framework_shared_object` branch;
/// `tf_copts()` already carries `-DTENSORFLOW_MONOLITHIC_BUILD`). rush builds
/// TensorFlow statically and has no `framework_shared_object` mode, so the
/// bazel-faithful result here is ALWAYS `extra_deps` — which pulls in the
/// `*_impl` object-code archives (e.g. `:lib_internal_impl`,
/// `:framework_internal_impl`, `@nsync//:nsync_cpp`) that carry the real
/// symbol definitions. The previous shared `builtin_if_conditional` returned
/// the empty/`otherwise` branch, dropping every `_impl` archive and leaving
/// the `//tensorflow/core:{lib,framework,...}` umbrella cc_library targets
/// with empty `CcInfo.linking` — the root cause of the `tensorflow::*` /
/// `tsl::*` / `nsync::*` undefined-reference link failures.
///
/// Accepts `extra_deps` positionally (`if_static([...])`,
/// `if_static([...], otherwise=[...])`) or as the `extra_deps` kwarg
/// (`if_static(extra_deps = [...])`). NOTE: deliberately NOT gated behind an
/// env switch — a hidden runtime toggle would make analyze/link results
/// non-reproducible and is not a cache-key input. A future shared-framework
/// build would model `framework_shared_object = true` as a real config and
/// return `otherwise` then.
fn builtin_if_static(args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    if let Some(v) = args.first() {
        return Ok(v.clone());
    }
    if let Some((_, v)) = kwargs.iter().find(|(k, _)| k == "extra_deps") {
        return Ok(v.clone());
    }
    Ok(Value::List(vec![]))
}

/// tf_tpu_dependencies() - Return TPU-related dependencies
///
/// TensorFlow helper function for TPU (Tensor Processing Unit) dependencies.
/// In Rush OSS builds, TPU support is not included, so we return empty list.
fn builtin_tf_tpu_dependencies(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::List(vec![]))
}

/// tf_dtensor_tpu_dependencies() - Return DTensor TPU-related dependencies
///
/// TensorFlow helper function for DTensor (distributed tensor) TPU dependencies.
/// In Rush OSS builds, DTensor TPU support is not included, so we return empty list.
fn builtin_tf_dtensor_tpu_dependencies(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    Ok(Value::List(vec![]))
}

/// tensorflow_opensource_extra_deps() - Return extra dependencies for OSS builds
///
/// TensorFlow helper function for additional dependencies in open-source builds.
/// Returns empty list as Rush uses standard dependency resolution.
fn builtin_tensorflow_opensource_extra_deps(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    Ok(Value::List(vec![]))
}

/// configuration_field(fragment, name) - Access a configuration field
///
/// Returns a label that references a configuration fragment's field.
/// Used in rule attribute definitions to make attributes depend on configuration.
///
/// Example:
///   configuration_field(fragment = "python", name = "python_path")
///
/// In Rush, we return a stub label since we don't have full configuration support.
fn builtin_configuration_field(
    _args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value> {
    // Extract fragment and name for a descriptive stub
    let fragment = kwargs
        .iter()
        .find(|(k, _)| k == "fragment")
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let name = kwargs
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Return a NativeFunction marker that can be used in attr.label() default values
    Ok(Value::NativeFunction(format!(
        "configuration_field:{}:{}",
        fragment, name
    )))
}

/// tf_opts_nortti_if_lite_protos() - Return compiler options for lite protos
///
/// TensorFlow helper function that returns -fno-rtti flag when using lite protos.
/// Returns empty list in Rush OSS builds.
fn builtin_tf_opts_nortti_if_lite_protos(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    Ok(Value::List(vec![]))
}

/// tf_defines_nortti_if_lite_protos() - Return preprocessor defines for lite protos
///
/// TensorFlow helper function that returns defines when using lite protos without RTTI.
/// Returns empty list in Rush OSS builds.
fn builtin_tf_defines_nortti_if_lite_protos(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    Ok(Value::List(vec![]))
}

/// tf_portable_proto_lib() - Return portable protobuf library dependencies
///
/// TensorFlow helper that returns cross-platform protobuf library dependencies.
/// In Google internal builds, returns tensorflow/core:protos_all_cc_impl and tsl/protobuf:protos_all_cc_impl.
/// In OSS Rush builds, returns standard protobuf dependencies.
fn builtin_tf_portable_proto_lib(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Return protobuf dependencies for OSS builds
    // TensorFlow's implementation returns ["//tensorflow/core:protos_all_cc_impl", "//tsl/protobuf:protos_all_cc_impl"]
    // For Rush OSS, we return the standard protobuf library
    Ok(Value::List(vec![Value::String(
        "@com_google_protobuf//:protobuf".to_string(),
    )]))
}

/// tf_portable_deps_no_runtime() - Return portable dependencies without runtime
///
/// Returns core TensorFlow dependencies that don't include the runtime library.
/// Used for building TensorFlow components that link against the runtime separately.
fn builtin_tf_portable_deps_no_runtime(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    // TensorFlow's default implementation returns:
    // ["@eigen_archive//:eigen3", "@double_conversion//:double-conversion",
    //  "@nsync//:nsync_cpp", "@com_googlesource_code_re2//:re2", "@farmhash_archive//:farmhash"]
    Ok(Value::List(vec![
        Value::String("@eigen_archive//:eigen3".to_string()),
        Value::String("@double_conversion//:double-conversion".to_string()),
        Value::String("@nsync//:nsync_cpp".to_string()),
        Value::String("@com_googlesource_code_re2//:re2".to_string()),
        Value::String("@farmhash_archive//:farmhash".to_string()),
    ]))
}

/// tf_additional_lib_deps() - Return additional library dependencies for TensorFlow
///
/// Returns additional dependencies needed to build TensorFlow libraries.
/// Includes Abseil base libraries and synchronization primitives.
fn builtin_tf_additional_lib_deps(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // TensorFlow's implementation returns:
    // ["@com_google_absl//absl/base:base", "@com_google_absl//absl/container:inlined_vector",
    //  "@com_google_absl//absl/types:span"] + if_static(["@nsync//:nsync_cpp"], ["@nsync//:nsync_headers"])
    // rush builds TF statically (no framework_shared_object), so the if_static
    // branch is nsync_cpp — the actual nsync object code. Returning nsync_headers
    // (the shared-build branch) dropped the nsync definitions and caused the
    // `nsync::nsync_*` undefined-reference link failures. Matches builtin_if_static.
    Ok(Value::List(vec![
        Value::String("@com_google_absl//absl/base:base".to_string()),
        Value::String("@com_google_absl//absl/container:inlined_vector".to_string()),
        Value::String("@com_google_absl//absl/types:span".to_string()),
        Value::String("@nsync//:nsync_cpp".to_string()),
    ]))
}

/// tf_features_nomodules_if_mobile() - Return features for mobile builds
///
/// Returns build features that disable header modules for mobile platforms.
/// In non-mobile builds, returns empty list.
fn builtin_tf_features_nomodules_if_mobile(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    // TensorFlow's implementation: if_mobile(["-use_header_modules"])
    // Since we're not building for mobile, return empty list
    Ok(Value::List(vec![]))
}

/// tf_protos_grappler_impl() - Return Grappler protobuf implementation dependencies
///
/// Returns the protobuf implementation dependencies for TensorFlow's Grappler optimizer.
/// Grappler is TensorFlow's graph optimization framework.
fn builtin_tf_protos_grappler_impl(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // TensorFlow's implementation returns: ["//tensorflow/core/grappler/costs:op_performance_data_cc_impl"]
    Ok(Value::List(vec![Value::String(
        "//tensorflow/core/grappler/costs:op_performance_data_cc_impl".to_string(),
    )]))
}

/// tf_monitoring_framework_deps() - Return monitoring framework dependencies
///
/// Returns monitoring libraries to link to the TensorFlow framework.
/// In OSS builds without Stackdriver support, returns empty list.
fn builtin_tf_monitoring_framework_deps(
    _args: &[Value],
    _kwargs: &[(String, Value)],
) -> Result<Value> {
    // TensorFlow's implementation uses select() to conditionally include Stackdriver
    // In OSS Rush builds without Stackdriver support, return empty list
    Ok(Value::List(vec![]))
}

// tf_proto_library, tf_custom_op_library, filegroup: no builtin here.
// tf_proto_library is captured as a RuleCall and routed to the native
// TfProtoLibraryRule via MACRO_PRIORITY_OVERRIDE.
// tf_custom_op_library is expanded by expand_tf_custom_op_library.
// filegroup is a native rule registered via register_rule_function.

/// tf_jspb_proto_library — JavaScript codegen is outside rush's C++ scope.
/// Accepted and ignored so BUILD files that declare JS proto targets
/// alongside C++ ones continue to parse. See UNIMPLEMENTED.md
/// "JavaScript proto codegen".
fn builtin_tf_jspb_proto_library(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::None)
}

/// tf_version_info_genrule — accepted and ignored. TF embeds a version
/// string into the binary; rush doesn't produce that file and no
/// libtensorflow_cc.so consumer we build today requires it. If one
/// starts to, implement a real rule that runs `tf_version.sh` (or
/// equivalent) and returns the generated header artifact — do not
/// silently allow link errors to surface at the consumer. See
/// UNIMPLEMENTED.md "TF version stamp".
fn builtin_tf_version_info_genrule(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::None)
}

/// tf_cuda_cc_test — accepted and ignored on CPU-only builds. CUDA and ROCm
/// builds omit this builtin so evaluator.rs can expand the real suffixed tests
/// and original-name suite.
fn builtin_tf_cuda_cc_test(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn if_conditional_returns_if_false_kwarg() {
        // TF BUILD calls if_libtpu(if_false=[...], if_true=[]) all-kwargs;
        // OSS (feature disabled) must yield if_false. Regression guard for the
        // dropped //tensorflow/core:ops -> mlir_passthrough_op edge.
        let if_false = Value::List(vec![Value::String(
            "//tensorflow/compiler/mlir/tensorflow:mlir_passthrough_op".to_string(),
        )]);
        let kwargs = vec![
            ("if_false".to_string(), if_false.clone()),
            ("if_true".to_string(), Value::List(vec![])),
        ];
        assert_eq!(builtin_if_conditional(&[], &kwargs).unwrap(), if_false);
    }

    #[test]
    fn if_conditional_positional_if_false_still_works() {
        // if_X(if_true, if_false) positional form.
        let if_true = Value::List(vec![Value::String("a".to_string())]);
        let if_false = Value::List(vec![Value::String("b".to_string())]);
        assert_eq!(
            builtin_if_conditional(&[if_true, if_false.clone()], &[]).unwrap(),
            if_false
        );
    }

    #[test]
    fn if_conditional_empty_when_no_if_false() {
        // if_X(if_true) with no if_false → OSS gets [].
        let if_true = Value::List(vec![Value::String("a".to_string())]);
        assert_eq!(
            builtin_if_conditional(&[if_true], &[]).unwrap(),
            Value::List(vec![])
        );
    }

    #[test]
    fn glob_segment_matcher_basics() {
        // `*` matches a run of chars within a segment (incl. dotfiles, matching
        // the `glob` crate default), but never crosses a `/` (segments are split
        // by the walker, so the matcher only sees one component).
        assert!(glob_segment_matches("*.cc", "foo.cc"));
        assert!(glob_segment_matches("*.cc", ".hidden.cc"));
        assert!(!glob_segment_matches("*.cc", "foo.h"));
        assert!(glob_segment_matches("*", "anything"));
        assert!(glob_segment_matches("foo.*", "foo.cc"));
        // `?` matches exactly one char.
        assert!(glob_segment_matches("foo?.cc", "foo1.cc"));
        assert!(!glob_segment_matches("foo?.cc", "foo.cc"));
        // literal match.
        assert!(glob_segment_matches("BUILD", "BUILD"));
        assert!(!glob_segment_matches("BUILD", "BUILD.bazel"));
    }

    #[test]
    fn glob_segment_matcher_char_classes() {
        assert!(glob_segment_matches("foo[0-9].cc", "foo3.cc"));
        assert!(!glob_segment_matches("foo[0-9].cc", "fooa.cc"));
        assert!(glob_segment_matches("[abc]x", "bx"));
        assert!(!glob_segment_matches("[abc]x", "dx"));
        // Negated class.
        assert!(glob_segment_matches("[!0-9]x", "ax"));
        assert!(!glob_segment_matches("[!0-9]x", "5x"));
    }

    #[test]
    fn test_len() {
        let result = builtin_len(&[Value::List(vec![Value::Int(1), Value::Int(2)])], &[]).unwrap();
        assert_eq!(result, Value::Int(2));

        let result = builtin_len(&[Value::String("hello".to_string())], &[]).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn test_list_of_dict_yields_sorted_keys() {
        let mut map = HashMap::new();
        map.insert("zebra".to_string(), Value::Bool(true));
        map.insert("alpha".to_string(), Value::Bool(true));
        map.insert("mike".to_string(), Value::Bool(true));
        let result = builtin_list(&[Value::Dict(map)], &[]).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::String("alpha".to_string()),
                Value::String("mike".to_string()),
                Value::String("zebra".to_string()),
            ])
        );
    }

    #[test]
    fn test_list_of_list_and_string() {
        let result = builtin_list(&[Value::List(vec![Value::Int(1)])], &[]).unwrap();
        assert_eq!(result, Value::List(vec![Value::Int(1)]));
        let result = builtin_list(&[Value::String("ab".to_string())], &[]).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string())
            ])
        );
    }

    #[test]
    fn test_range() {
        let result = builtin_range(&[Value::Int(5)], &[]).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::Int(0),
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Int(4)
            ])
        );

        let result = builtin_range(&[Value::Int(2), Value::Int(5)], &[]).unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::Int(2), Value::Int(3), Value::Int(4)])
        );
    }

    #[test]
    fn test_str() {
        let result = builtin_str(&[Value::Int(42)], &[]).unwrap();
        assert_eq!(result, Value::String("42".to_string()));
    }

    #[test]
    fn configurable_repr_preserves_selector_list_syntax() {
        let value = Value::Configurable {
            op: crate::evaluator::ConfigurableOp::Add,
            left: std::sync::Arc::new(Value::Select(HashMap::from([
                (
                    "//conditions:default".to_string(),
                    Value::List(Vec::new()),
                ),
                (
                    "//platforms:gpu".to_string(),
                    Value::List(vec![Value::String("//platforms:_gpu".to_string())]),
                ),
            ]))),
            right: std::sync::Arc::new(Value::List(Vec::new())),
        };

        assert_eq!(
            starlark_repr(&value),
            "select({\"//conditions:default\": [], \"//platforms:gpu\": \
             [\"//platforms:_gpu\"]}) + []"
        );
    }

    #[test]
    fn test_type() {
        let result = builtin_type(&[Value::Int(42)], &[]).unwrap();
        assert_eq!(result, Value::String("int".to_string()));

        let result = builtin_type(&[Value::List(vec![])], &[]).unwrap();
        assert_eq!(result, Value::String("list".to_string()));
    }

    #[test]
    fn test_select_with_default_is_deferred() {
        let mut conditions = HashMap::new();
        conditions.insert(
            "//conditions:nonexistent".to_string(),
            Value::String("wrong".to_string()),
        );
        conditions.insert(
            "//conditions:default".to_string(),
            Value::String("default_value".to_string()),
        );

        let result = builtin_select(&[Value::Dict(conditions.clone())], &[]).unwrap();
        assert_eq!(result, Value::Select(conditions));
    }

    #[test]
    fn test_select_platform_specific_is_deferred() {
        let mut conditions = HashMap::new();

        // Add all platform conditions
        #[cfg(target_os = "linux")]
        {
            conditions.insert(
                "@platforms//os:linux".to_string(),
                Value::String("linux_value".to_string()),
            );
        }

        #[cfg(target_os = "macos")]
        {
            conditions.insert(
                "@platforms//os:osx".to_string(),
                Value::String("macos_value".to_string()),
            );
        }

        #[cfg(target_os = "windows")]
        {
            conditions.insert(
                "@platforms//os:windows".to_string(),
                Value::String("windows_value".to_string()),
            );
        }

        conditions.insert(
            "//conditions:default".to_string(),
            Value::String("default_value".to_string()),
        );

        let result = builtin_select(&[Value::Dict(conditions.clone())], &[]).unwrap();
        assert_eq!(result, Value::Select(conditions));
    }

    #[test]
    fn test_select_no_match_no_default() {
        let mut conditions = HashMap::new();
        conditions.insert(
            "//conditions:nonexistent".to_string(),
            Value::String("value".to_string()),
        );

        let result = builtin_select(&[Value::Dict(conditions.clone())], &[]).unwrap();
        assert_eq!(result, Value::Select(conditions));
    }

    #[test]
    fn test_hasattr() {
        // Test with struct/namespace
        let mut attrs = HashMap::new();
        attrs.insert("deps".to_string(), Value::List(vec![]));
        attrs.insert("srcs".to_string(), Value::List(vec![]));
        let obj = Value::Namespace(attrs);

        // Attribute exists
        let result =
            builtin_hasattr(&[obj.clone(), Value::String("deps".to_string())], &[]).unwrap();
        assert_eq!(result, Value::Bool(true));

        // Attribute doesn't exist
        let result =
            builtin_hasattr(&[obj.clone(), Value::String("hdrs".to_string())], &[]).unwrap();
        assert_eq!(result, Value::Bool(false));

        // Test with Label
        let label = Value::Label {
            label: "//foo:bar".to_string(),
            package: "foo".to_string(),
            name: "bar".to_string(),
            workspace_name: String::new(),
            workspace_root: String::new(),
        };

        let result =
            builtin_hasattr(&[label.clone(), Value::String("name".to_string())], &[]).unwrap();
        assert_eq!(result, Value::Bool(true));

        let result =
            builtin_hasattr(&[label, Value::String("nonexistent".to_string())], &[]).unwrap();
        assert_eq!(result, Value::Bool(false));
    }

    #[test]
    fn test_getattr() {
        // Test with struct/namespace
        let mut attrs = HashMap::new();
        attrs.insert(
            "deps".to_string(),
            Value::List(vec![Value::String("foo".to_string())]),
        );
        let obj = Value::Namespace(attrs);

        // Attribute exists
        let result =
            builtin_getattr(&[obj.clone(), Value::String("deps".to_string())], &[]).unwrap();
        assert_eq!(result, Value::List(vec![Value::String("foo".to_string())]));

        // Attribute doesn't exist, no default
        let result =
            builtin_getattr(&[obj.clone(), Value::String("srcs".to_string())], &[]).unwrap();
        assert_eq!(result, Value::None);

        // Attribute doesn't exist, with default
        let default = Value::List(vec![]);
        let result = builtin_getattr(
            &[obj, Value::String("srcs".to_string()), default.clone()],
            &[],
        )
        .unwrap();
        assert_eq!(result, default);
    }

    #[test]
    fn test_repository_rule() {
        let impl_fn = Value::Function {
            params: vec![("repository_ctx".to_string(), None)],
            varargs_param: None,
            kwargs_param: None,
            body: vec![],
            closure: std::sync::Arc::new(crate::evaluator::Environment::new()),
        };

        let result =
            builtin_repository_rule(&[], &[("implementation".to_string(), impl_fn)]).unwrap();

        assert!(matches!(result, Value::RuleFunction(_)));
    }

    #[test]
    fn test_struct() {
        // Test creating a struct with multiple fields
        let result = builtin_struct(
            &[],
            &[
                ("x".to_string(), Value::Int(10)),
                ("y".to_string(), Value::Int(20)),
                ("name".to_string(), Value::String("point".to_string())),
            ],
        )
        .unwrap();

        match result {
            Value::Namespace(fields) => {
                assert_eq!(fields.get("x"), Some(&Value::Int(10)));
                assert_eq!(fields.get("y"), Some(&Value::Int(20)));
                assert_eq!(
                    fields.get("name"),
                    Some(&Value::String("point".to_string()))
                );
            }
            _ => panic!("Expected Namespace value"),
        }
    }

    #[test]
    fn test_struct_empty() {
        // Test creating an empty struct
        let result = builtin_struct(&[], &[]).unwrap();

        match result {
            Value::Namespace(fields) => {
                assert!(fields.is_empty());
            }
            _ => panic!("Expected Namespace value"),
        }
    }

    #[test]
    fn test_depset_direct() {
        // Test depset with only direct items
        let result = builtin_depset(
            &[],
            &[(
                "direct".to_string(),
                Value::List(vec![
                    Value::String("a.o".to_string()),
                    Value::String("b.o".to_string()),
                ]),
            )],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert!(items.contains(&Value::String("a.o".to_string())));
                assert!(items.contains(&Value::String("b.o".to_string())));
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_depset_with_transitive() {
        // Test depset with direct and transitive items
        let transitive_depset = Value::List(vec![
            Value::String("dep1.o".to_string()),
            Value::String("dep2.o".to_string()),
        ]);

        let result = builtin_depset(
            &[],
            &[
                (
                    "direct".to_string(),
                    Value::List(vec![Value::String("main.o".to_string())]),
                ),
                (
                    "transitive".to_string(),
                    Value::List(vec![transitive_depset]),
                ),
            ],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert!(items.contains(&Value::String("main.o".to_string())));
                assert!(items.contains(&Value::String("dep1.o".to_string())));
                assert!(items.contains(&Value::String("dep2.o".to_string())));
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_depset_deduplication() {
        // Test that depset deduplicates items
        let result = builtin_depset(
            &[],
            &[(
                "direct".to_string(),
                Value::List(vec![
                    Value::String("a.o".to_string()),
                    Value::String("b.o".to_string()),
                    Value::String("a.o".to_string()), // Duplicate
                ]),
            )],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                // Should only have 2 unique items
                assert_eq!(items.len(), 2);
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_rule() {
        let impl_fn = Value::Function {
            params: vec![("ctx".to_string(), None)],
            varargs_param: None,
            kwargs_param: None,
            body: vec![],
            closure: std::sync::Arc::new(crate::evaluator::Environment::new()),
        };

        let mut attrs = HashMap::new();
        attrs.insert("srcs".to_string(), Value::List(vec![]));
        attrs.insert("deps".to_string(), Value::List(vec![]));

        let result = builtin_rule(
            &[],
            &[
                ("implementation".to_string(), impl_fn),
                ("attrs".to_string(), Value::Dict(attrs)),
            ],
        )
        .unwrap();

        assert!(matches!(result, Value::RuleFunction(_)));
    }

    #[test]
    fn test_rule_missing_implementation() {
        // Phase 5.C.4 post-batch-N: behavior changed.
        // builtin_rule used to error on missing
        // `implementation` kwarg. The current impl
        // tolerates it (warns under `bzl-debug` and
        // creates a stub RuleFunction) because some
        // TF .bzl files define rules without
        // implementations for type-checking-only
        // purposes. Test now verifies the stub
        // return shape, not the error.
        let result = builtin_rule(&[], &[]);
        assert!(
            result.is_ok(),
            "rule() now tolerates missing implementation (stub mode)"
        );
        match result.unwrap() {
            Value::RuleFunction(name) => {
                assert!(
                    !name.is_empty(),
                    "stub rule should have a synthetic name"
                );
            }
            other => panic!("Expected RuleFunction stub, got: {:?}", other),
        }
    }

    #[test]
    fn test_provider() {
        let result = builtin_provider(
            &[],
            &[(
                "fields".to_string(),
                Value::List(vec![
                    Value::String("srcs".to_string()),
                    Value::String("deps".to_string()),
                ]),
            )],
        )
        .unwrap();

        match result {
            Value::Provider {
                name: _, fields, ..
            } => {
                assert_eq!(fields.len(), 2);
                assert!(fields.contains(&"srcs".to_string()));
                assert!(fields.contains(&"deps".to_string()));
            }
            _ => panic!("Expected Provider value"),
        }
    }

    #[test]
    fn test_provider_with_doc() {
        let result = builtin_provider(
            &[Value::String("Custom provider for build info".to_string())],
            &[(
                "fields".to_string(),
                Value::List(vec![Value::String("files".to_string())]),
            )],
        )
        .unwrap();

        match result {
            Value::Provider { fields, .. } => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0], "files");
            }
            _ => panic!("Expected Provider value"),
        }
    }

    #[test]
    fn test_provider_dict_fields() {
        let mut field_docs = HashMap::new();
        field_docs.insert(
            "srcs".to_string(),
            Value::String("Source files".to_string()),
        );
        field_docs.insert(
            "deps".to_string(),
            Value::String("Dependencies".to_string()),
        );

        let result =
            builtin_provider(&[], &[("fields".to_string(), Value::Dict(field_docs))]).unwrap();

        match result {
            Value::Provider { fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert!(fields.contains(&"srcs".to_string()));
                assert!(fields.contains(&"deps".to_string()));
            }
            _ => panic!("Expected Provider value"),
        }
    }

    #[test]
    fn test_provider_with_initializer_has_distinct_raw_constructor() {
        let init = Value::Function {
            params: vec![],
            varargs_param: None,
            kwargs_param: None,
            body: vec![],
            closure: std::sync::Arc::new(crate::evaluator::Environment::new()),
        };
        let result = builtin_provider(
            &[],
            &[
                (
                    "fields".to_string(),
                    Value::List(vec![Value::String("value".to_string())]),
                ),
                ("init".to_string(), init.clone()),
            ],
        )
        .unwrap();

        let Value::List(constructors) = result else {
            panic!("Expected List wrapper");
        };
        assert!(matches!(
            &constructors[0],
            Value::Provider {
                init: Some(stored),
                ..
            } if stored.as_ref() == &init
        ));
        assert!(matches!(
            &constructors[1],
            Value::Provider { init: None, .. }
        ));
    }

    #[test]
    fn test_aspect() {
        let impl_fn = Value::Function {
            params: vec![("target".to_string(), None), ("ctx".to_string(), None)],
            varargs_param: None,
            kwargs_param: None,
            body: vec![],
            closure: std::sync::Arc::new(crate::evaluator::Environment::new()),
        };

        let result = builtin_aspect(
            &[],
            &[
                ("implementation".to_string(), impl_fn.clone()),
                (
                    "attr_aspects".to_string(),
                    Value::List(vec![Value::String("deps".to_string())]),
                ),
            ],
        )
        .unwrap();

        match result {
            Value::Namespace(config) => {
                assert!(config.contains_key("implementation"));
                assert!(config.contains_key("attr_aspects"));
                assert_eq!(config.get("implementation"), Some(&impl_fn));
            }
            _ => panic!("Expected Namespace value"),
        }
    }

    #[test]
    fn test_label() {
        // Test simple label
        let result = builtin_label(&[Value::String("//foo:bar".to_string())], &[]).unwrap();
        match result {
            Value::Label {
                label,
                package,
                name,
                workspace_name,
                workspace_root,
            } => {
                assert_eq!(label, "//foo:bar");
                assert_eq!(package, "foo");
                assert_eq!(name, "bar");
                assert_eq!(workspace_name, "");
                assert_eq!(workspace_root, "");
            }
            _ => panic!("Expected Label value"),
        }
    }

    #[test]
    fn test_label_external_workspace() {
        // Test label with external workspace
        let result =
            builtin_label(&[Value::String("@tensorflow//core:ops".to_string())], &[]).unwrap();
        match result {
            Value::Label {
                label,
                package,
                name,
                workspace_name,
                workspace_root,
            } => {
                assert_eq!(label, "@tensorflow//core:ops");
                assert_eq!(package, "core");
                assert_eq!(name, "ops");
                assert_eq!(workspace_name, "tensorflow");
                assert_eq!(workspace_root, "external/tensorflow");
            }
            _ => panic!("Expected Label value"),
        }
    }

    #[test]
    fn test_fail() {
        // Test fail() function - should return an error
        let result = builtin_fail(&[Value::String("Test error message".to_string())], &[]);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Test error message"));
    }

    #[test]
    fn test_fail_with_format() {
        // Test fail() with formatted message
        let result = builtin_fail(&[Value::String("Failed with code 42".to_string())], &[]);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Failed"));
        assert!(err_msg.contains("42"));
    }

    #[test]
    fn test_sorted_list() {
        // Test sorted() with a list of integers
        let result = builtin_sorted(
            &[Value::List(vec![
                Value::Int(5),
                Value::Int(2),
                Value::Int(8),
                Value::Int(1),
            ])],
            &[],
        )
        .unwrap();

        assert_eq!(
            result,
            Value::List(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(5),
                Value::Int(8),
            ])
        );
    }

    #[test]
    fn test_sorted_strings() {
        // Test sorted() with a list of strings
        let result = builtin_sorted(
            &[Value::List(vec![
                Value::String("zebra".to_string()),
                Value::String("apple".to_string()),
                Value::String("banana".to_string()),
            ])],
            &[],
        )
        .unwrap();

        assert_eq!(
            result,
            Value::List(vec![
                Value::String("apple".to_string()),
                Value::String("banana".to_string()),
                Value::String("zebra".to_string()),
            ])
        );
    }

    #[test]
    fn test_sorted_reverse() {
        // Test sorted() with reverse parameter
        let result = builtin_sorted(
            &[Value::List(vec![
                Value::Int(1),
                Value::Int(5),
                Value::Int(3),
            ])],
            &[("reverse".to_string(), Value::Bool(true))],
        )
        .unwrap();

        assert_eq!(
            result,
            Value::List(vec![Value::Int(5), Value::Int(3), Value::Int(1),])
        );
    }

    #[test]
    fn test_enumerate() {
        // Test enumerate() function
        let result = builtin_enumerate(
            &[Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ])],
            &[],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                // Each item should be a tuple (index, value)
                assert_eq!(
                    items[0],
                    Value::Tuple(vec![Value::Int(0), Value::String("a".to_string())])
                );
                assert_eq!(
                    items[1],
                    Value::Tuple(vec![Value::Int(1), Value::String("b".to_string())])
                );
                assert_eq!(
                    items[2],
                    Value::Tuple(vec![Value::Int(2), Value::String("c".to_string())])
                );
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_enumerate_with_start() {
        // Test enumerate() with start parameter
        let result = builtin_enumerate(
            &[Value::List(vec![
                Value::String("x".to_string()),
                Value::String("y".to_string()),
            ])],
            &[("start".to_string(), Value::Int(10))],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(
                    items[0],
                    Value::Tuple(vec![Value::Int(10), Value::String("x".to_string())])
                );
                assert_eq!(
                    items[1],
                    Value::Tuple(vec![Value::Int(11), Value::String("y".to_string())])
                );
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_zip() {
        // Test zip() function with two lists
        let result = builtin_zip(
            &[
                Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                    Value::String("c".to_string()),
                ]),
            ],
            &[],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(
                    items[0],
                    Value::Tuple(vec![Value::Int(1), Value::String("a".to_string())])
                );
                assert_eq!(
                    items[1],
                    Value::Tuple(vec![Value::Int(2), Value::String("b".to_string())])
                );
                assert_eq!(
                    items[2],
                    Value::Tuple(vec![Value::Int(3), Value::String("c".to_string())])
                );
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_zip_different_lengths() {
        // Test zip() with different length lists - should stop at shortest
        let result = builtin_zip(
            &[
                Value::List(vec![Value::Int(1), Value::Int(2)]),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                    Value::String("c".to_string()),
                ]),
            ],
            &[],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                // Should only have 2 items (length of shortest list)
                assert_eq!(items.len(), 2);
                assert_eq!(
                    items[0],
                    Value::Tuple(vec![Value::Int(1), Value::String("a".to_string())])
                );
                assert_eq!(
                    items[1],
                    Value::Tuple(vec![Value::Int(2), Value::String("b".to_string())])
                );
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_zip_three_lists() {
        // Test zip() with three lists
        let result = builtin_zip(
            &[
                Value::List(vec![Value::Int(1), Value::Int(2)]),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                ]),
                Value::List(vec![Value::Bool(true), Value::Bool(false)]),
            ],
            &[],
        )
        .unwrap();

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(
                    items[0],
                    Value::Tuple(vec![
                        Value::Int(1),
                        Value::String("a".to_string()),
                        Value::Bool(true)
                    ])
                );
                assert_eq!(
                    items[1],
                    Value::Tuple(vec![
                        Value::Int(2),
                        Value::String("b".to_string()),
                        Value::Bool(false)
                    ])
                );
            }
            _ => panic!("Expected List value"),
        }
    }

    // ============================================================================
    // TensorFlow Helper Function Tests
    // ============================================================================

    #[test]
    fn test_tf_tpu_dependencies() {
        // TPU dependencies should return empty list in OSS builds
        let result = builtin_tf_tpu_dependencies(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tf_dtensor_tpu_dependencies() {
        // DTensor TPU dependencies should return empty list in OSS builds
        let result = builtin_tf_dtensor_tpu_dependencies(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tensorflow_opensource_extra_deps() {
        // OSS extra deps should return empty list
        let result = builtin_tensorflow_opensource_extra_deps(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tf_opts_nortti_if_lite_protos() {
        // Should return empty list in standard proto builds
        let result = builtin_tf_opts_nortti_if_lite_protos(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tf_defines_nortti_if_lite_protos() {
        // Should return empty list in standard proto builds
        let result = builtin_tf_defines_nortti_if_lite_protos(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tf_portable_proto_lib() {
        // Should return protobuf dependency
        let result = builtin_tf_portable_proto_lib(&[], &[]).unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::String(
                "@com_google_protobuf//:protobuf".to_string()
            )])
        );
    }

    #[test]
    fn test_tf_portable_deps_no_runtime() {
        // Should return core dependencies without runtime
        let result = builtin_tf_portable_deps_no_runtime(&[], &[]).unwrap();
        match result {
            Value::List(deps) => {
                assert_eq!(deps.len(), 5);
                assert!(deps.contains(&Value::String("@eigen_archive//:eigen3".to_string())));
                assert!(deps.contains(&Value::String(
                    "@double_conversion//:double-conversion".to_string()
                )));
                assert!(deps.contains(&Value::String("@nsync//:nsync_cpp".to_string())));
                assert!(deps.contains(&Value::String(
                    "@com_googlesource_code_re2//:re2".to_string()
                )));
                assert!(deps.contains(&Value::String("@farmhash_archive//:farmhash".to_string())));
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_tf_additional_lib_deps() {
        // Should return additional library dependencies
        let result = builtin_tf_additional_lib_deps(&[], &[]).unwrap();
        match result {
            Value::List(deps) => {
                assert_eq!(deps.len(), 4);
                assert!(deps.contains(&Value::String(
                    "@com_google_absl//absl/base:base".to_string()
                )));
                assert!(deps.contains(&Value::String(
                    "@com_google_absl//absl/container:inlined_vector".to_string()
                )));
                assert!(deps.contains(&Value::String(
                    "@com_google_absl//absl/types:span".to_string()
                )));
                assert!(deps.contains(&Value::String("@nsync//:nsync_cpp".to_string())));
                assert!(!deps.contains(&Value::String("@nsync//:nsync_headers".to_string())));
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_tf_features_nomodules_if_mobile() {
        // Should return empty list for non-mobile builds
        let result = builtin_tf_features_nomodules_if_mobile(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tf_protos_grappler_impl() {
        // Should return grappler implementation dependency
        let result = builtin_tf_protos_grappler_impl(&[], &[]).unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::String(
                "//tensorflow/core/grappler/costs:op_performance_data_cc_impl".to_string()
            )])
        );
    }

    #[test]
    fn test_tf_monitoring_framework_deps() {
        // Should return empty list without Stackdriver support
        let result = builtin_tf_monitoring_framework_deps(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    // ============================================================================
    // Conditional Function Tests
    // ============================================================================

    #[test]
    fn test_if_mkl_with_two_args() {
        // if_mkl(if_true, if_false) should return if_false in OSS builds
        let if_true = Value::List(vec![Value::String("mkl_lib".to_string())]);
        let if_false = Value::List(vec![Value::String("default_lib".to_string())]);
        let result = builtin_if_mkl(&[if_true, if_false.clone()], &[]).unwrap();
        assert_eq!(result, if_false);
    }

    #[test]
    fn test_if_mkl_with_one_arg() {
        // if_mkl(if_true) should return empty list in OSS builds
        let if_true = Value::List(vec![Value::String("mkl_lib".to_string())]);
        let result = builtin_if_mkl(&[if_true], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_if_mkl_invalid_args() {
        // if_mkl with 0 or 3+ args should fail
        assert!(builtin_if_mkl(&[], &[]).is_err());
        assert!(builtin_if_mkl(&[Value::Int(1), Value::Int(2), Value::Int(3)], &[]).is_err());
    }

    #[test]
    fn test_if_conditional_with_zero_args() {
        // Generic conditionals should accept 0 args and return empty list
        let result = builtin_if_conditional(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_if_conditional_with_one_arg() {
        // Should return empty list (if_false default)
        let if_true = Value::List(vec![Value::String("feature_enabled".to_string())]);
        let result = builtin_if_conditional(&[if_true], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_if_conditional_with_two_args() {
        // Should return if_false (second argument)
        let if_true = Value::List(vec![Value::String("enabled".to_string())]);
        let if_false = Value::List(vec![Value::String("disabled".to_string())]);
        let result = builtin_if_conditional(&[if_true, if_false.clone()], &[]).unwrap();
        assert_eq!(result, if_false);
    }

    #[test]
    fn test_if_conditional_too_many_args() {
        // Should fail with 3+ arguments
        assert!(
            builtin_if_conditional(&[Value::Int(1), Value::Int(2), Value::Int(3)], &[]).is_err()
        );
    }

    #[test]
    fn test_if_google() {
        // if_google should return if_false (OSS) value
        let if_true = Value::List(vec![Value::String("google_internal".to_string())]);
        let if_false = Value::List(vec![Value::String("oss".to_string())]);
        let result = builtin_if_google(&[if_true, if_false.clone()], &[]).unwrap();
        assert_eq!(result, if_false);

        // With one arg, should return empty list
        let if_true = Value::List(vec![Value::String("google_internal".to_string())]);
        let result = builtin_if_google(&[if_true], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_get_compatible_with_portable() {
        // Should return empty list (no platform restrictions)
        let result = builtin_get_compatible_with_portable(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tf_copts() {
        // Should return standard TensorFlow compiler options
        let result = builtin_tf_copts(&[], &[]).unwrap();
        match result {
            Value::List(copts) => {
                assert_eq!(copts.len(), 3);
                assert!(copts.contains(&Value::String("-DEIGEN_AVOID_STL_ARRAY".to_string())));
                assert!(copts.contains(&Value::String("-Wno-sign-compare".to_string())));
                assert!(copts.contains(&Value::String("-ftemplate-depth=900".to_string())));
            }
            _ => panic!("Expected List value"),
        }
    }

    #[test]
    fn test_tf_lib_proto_parsing_deps() {
        // Should return empty list (dependencies resolved through standard deps)
        let result = builtin_tf_lib_proto_parsing_deps(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn test_tf_additional_test_deps() {
        // Should return empty list in OSS builds
        let result = builtin_tf_additional_test_deps(&[], &[]).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }
}
/// py_test stub
fn builtin_py_test(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::None)
}

/// CcInfo provider - Bazel's C++ compilation info provider
/// Used to pass C++ compilation context and linking info between rules
fn builtin_cc_info(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    // CcInfo can be called as a constructor: CcInfo(compilation_context=..., linking_context=...)
    // Or used as a provider key: dep[CcInfo]
    let mut fields = HashMap::new();

    // Extract compilation_context
    if let Some((_, ctx)) = kwargs.iter().find(|(k, _)| k == "compilation_context") {
        fields.insert("compilation_context".to_string(), ctx.clone());
    } else {
        // Create a default empty compilation context
        let mut cc_ctx = HashMap::new();
        cc_ctx.insert("headers".to_string(), Value::List(vec![]));
        cc_ctx.insert("defines".to_string(), Value::List(vec![]));
        cc_ctx.insert("system_includes".to_string(), Value::List(vec![]));
        cc_ctx.insert("includes".to_string(), Value::List(vec![]));
        cc_ctx.insert("quote_includes".to_string(), Value::List(vec![]));
        cc_ctx.insert("framework_includes".to_string(), Value::List(vec![]));
        fields.insert("compilation_context".to_string(), Value::Dict(cc_ctx));
    }

    // Extract linking_context
    if let Some((_, ctx)) = kwargs.iter().find(|(k, _)| k == "linking_context") {
        fields.insert("linking_context".to_string(), ctx.clone());
    } else {
        let mut lc_ctx = HashMap::new();
        lc_ctx.insert("linker_inputs".to_string(), Value::List(vec![]));
        fields.insert("linking_context".to_string(), Value::Dict(lc_ctx));
    }

    Ok(Value::ProviderInstance {
        provider_name: "CcInfo".to_string(),
        fields,
    })
}

/// cc_common module - Bazel's C++ compilation common functions
fn builtin_cc_common(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();

    // Commonly used cc_common functions - return stub functions
    fields.insert("create_compilation_context".to_string(), Value::NativeFunction("cc_common.create_compilation_context".to_string()));
    fields.insert("create_linking_context".to_string(), Value::NativeFunction("cc_common.create_linking_context".to_string()));
    fields.insert("merge_compilation_contexts".to_string(), Value::NativeFunction("cc_common.merge_compilation_contexts".to_string()));
    fields.insert("merge_linking_contexts".to_string(), Value::NativeFunction("cc_common.merge_linking_contexts".to_string()));
    fields.insert("internal_DO_NOT_USE".to_string(), Value::NativeFunction("cc_common.internal_DO_NOT_USE".to_string()));
    fields.insert("do_not_use_tools_cpp_compiler_present".to_string(), Value::None);
    fields.insert("CcToolchainInfo".to_string(), Value::Provider {
        name: "CcToolchainInfo".to_string(),
        fields: vec![],
        init: None,
    });

    Ok(Value::Namespace(fields))
}

/// Native C++ bridge exposed by Bazel through cc_common.internal_DO_NOT_USE().
fn builtin_cc_internal(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    const METHODS: &[&str] = &[
        "absolute_symlink",
        "actions2ctx_cheat",
        "check_private_api",
        "check_toplevel",
        "collect_per_file_lto_backend_opts",
        "combine_cc_toolchain_variables",
        "create_cc_compile_action",
        "create_cc_compile_action_template",
        "create_header_info",
        "create_header_info_with_deps",
        "create_lto_backend_action",
        "create_lto_backend_action_template",
        "declare_compile_output_file",
        "declare_other_output_file",
        "dynamic_library_soname",
        "dynamic_library_symlink",
        "dynamic_library_symlink2",
        "exec_os",
        "expand_and_tokenize",
        "freeze",
        "get_artifact_name_extension_for_category",
        "get_artifact_name_for_category",
        "get_link_args",
        "intern_seq",
        "intern_string_sequence_variable_value",
        "is_tree_artifact",
        "per_file_copts",
        "rule_class",
        "solib_symlink_action",
        "wrap_link_actions",
    ];

    let fields = METHODS
        .iter()
        .map(|method| {
            (
                (*method).to_string(),
                Value::NativeFunction(format!("cc_internal.{}", method)),
            )
        })
        .collect();
    Ok(Value::Namespace(fields))
}

/// DefaultInfo provider - Bazel's default outputs provider
fn builtin_default_info(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();

    if let Some((_, files)) = kwargs.iter().find(|(k, _)| k == "files") {
        fields.insert("files".to_string(), files.clone());
    } else {
        fields.insert("files".to_string(), Value::List(vec![]));
    }

    if let Some((_, runfiles)) = kwargs.iter().find(|(k, _)| k == "runfiles") {
        fields.insert("runfiles".to_string(), runfiles.clone());
    }

    if let Some((_, data_runfiles)) = kwargs.iter().find(|(k, _)| k == "data_runfiles") {
        fields.insert("data_runfiles".to_string(), data_runfiles.clone());
    }

    if let Some((_, default_runfiles)) = kwargs.iter().find(|(k, _)| k == "default_runfiles") {
        fields.insert("default_runfiles".to_string(), default_runfiles.clone());
    }

    if let Some((_, executable)) = kwargs.iter().find(|(k, _)| k == "executable") {
        fields.insert("executable".to_string(), executable.clone());
    }

    Ok(Value::ProviderInstance {
        provider_name: "DefaultInfo".to_string(),
        fields,
    })
}

/// ProtoInfo provider - Bazel's protobuf compilation info provider
fn builtin_proto_info(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();

    if let Some((_, direct_sources)) = kwargs.iter().find(|(k, _)| k == "direct_sources") {
        fields.insert("direct_sources".to_string(), direct_sources.clone());
    } else {
        fields.insert("direct_sources".to_string(), Value::List(vec![]));
    }

    if let Some((_, transitive_sources)) = kwargs.iter().find(|(k, _)| k == "transitive_sources") {
        fields.insert("transitive_sources".to_string(), transitive_sources.clone());
    } else {
        fields.insert("transitive_sources".to_string(), Value::List(vec![]));
    }

    if let Some((_, transitive_proto_path)) = kwargs.iter().find(|(k, _)| k == "transitive_proto_path") {
        fields.insert("transitive_proto_path".to_string(), transitive_proto_path.clone());
    } else {
        fields.insert("transitive_proto_path".to_string(), Value::List(vec![]));
    }

    Ok(Value::ProviderInstance {
        provider_name: "ProtoInfo".to_string(),
        fields,
    })
}

/// OutputGroupInfo provider - Bazel's output group provider
fn builtin_output_group_info(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    for (key, value) in kwargs {
        fields.insert(key.clone(), value.clone());
    }
    Ok(Value::ProviderInstance {
        provider_name: "OutputGroupInfo".to_string(),
        fields,
    })
}

/// InstrumentedFilesInfo provider - Bazel's code coverage provider
fn builtin_instrumented_files_info(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    for (key, value) in kwargs {
        fields.insert(key.clone(), value.clone());
    }
    Ok(Value::ProviderInstance {
        provider_name: "InstrumentedFilesInfo".to_string(),
        fields,
    })
}

/// coverage_common module - Bazel's coverage common functions
fn builtin_coverage_common(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert("instrumented_files_info".to_string(), Value::NativeFunction("coverage_common.instrumented_files_info".to_string()));
    Ok(Value::Namespace(fields))
}

/// testing module - Bazel's testing utilities
fn builtin_testing(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert("ExecutionInfo".to_string(), Value::NativeFunction("testing.ExecutionInfo".to_string()));
    fields.insert("TestEnvironment".to_string(), Value::NativeFunction("testing.TestEnvironment".to_string()));
    Ok(Value::Namespace(fields))
}

/// config_common module - Bazel's configuration common functions
fn builtin_config_common(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert("FeatureFlagInfo".to_string(), Value::Provider {
        name: "FeatureFlagInfo".to_string(),
        fields: vec!["value".to_string()],
        init: None,
    });
    fields.insert("toolchain_type".to_string(), Value::NativeFunction("config_common.toolchain_type".to_string()));
    Ok(Value::Namespace(fields))
}

/// config — Bazel's top-level configuration-transition namespace used in attr
/// definitions: attr.label(cfg = config.exec(...)) / config.target() /
/// config.none(). rush runs all actions on the host and does not model split
/// configurations, so these return opaque transition markers purely so rule/attr
/// definitions that reference them evaluate. protobuf's build_defs/internal_shell
/// + exec-tool attrs use config.exec.
fn builtin_config(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert("exec".to_string(), Value::NativeFunction("config.exec".to_string()));
    fields.insert("target".to_string(), Value::NativeFunction("config.target".to_string()));
    fields.insert("none".to_string(), Value::NativeFunction("config.none".to_string()));
    Ok(Value::Namespace(fields))
}

/// platform_common module - Bazel's platform common functions
fn builtin_platform_common(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert(
        "TemplateVariableInfo".to_string(),
        Value::NativeFunction("TemplateVariableInfo".to_string()),
    );
    fields.insert("ConstraintSettingInfo".to_string(), Value::Provider {
        name: "ConstraintSettingInfo".to_string(),
        fields: vec![],
        init: None,
    });
    fields.insert("ConstraintValueInfo".to_string(), Value::Provider {
        name: "ConstraintValueInfo".to_string(),
        fields: vec![],
        init: None,
    });
    fields.insert("PlatformInfo".to_string(), Value::Provider {
        name: "PlatformInfo".to_string(),
        fields: vec![],
        init: None,
    });
    fields.insert("ToolchainInfo".to_string(), Value::Provider {
        name: "ToolchainInfo".to_string(),
        fields: vec![],
        init: None,
    });
    Ok(Value::Namespace(fields))
}

/// Bazel 7+ restricted Python-rule helpers re-exported by rules_python.
/// Rush does not execute native Python rule implementations while loading
/// BUILD metadata, so these members are opaque native functions.
fn builtin_py_internal(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    const METHODS: &[&str] = &[
        "cc_toolchain_build_info_files",
        "declare_shareable_artifact",
        "get_label_repo_runfiles_path",
        "is_tool_configuration",
        "link",
        "linkstamp_file",
        "regex_match",
        "runfiles_enabled",
        "share_native_deps",
    ];

    Ok(Value::Namespace(
        METHODS
            .iter()
            .map(|method| {
                (
                    (*method).to_string(),
                    Value::NativeFunction(format!("py_internal.{}", method)),
                )
            })
            .collect(),
    ))
}

fn builtin_template_variable_info(
    args: &[Value],
    kwargs: &[(String, Value)],
) -> Result<Value> {
    if args.len() > 1 {
        return Err(anyhow!(
            "TemplateVariableInfo() takes exactly one variables dictionary"
        ));
    }

    let mut variables = args.first().cloned();
    for (key, value) in kwargs {
        if key != "variables" {
            return Err(anyhow!(
                "TemplateVariableInfo() got unexpected argument '{}'",
                key
            ));
        }
        if variables.is_some() {
            return Err(anyhow!(
                "TemplateVariableInfo() received multiple values for variables"
            ));
        }
        variables = Some(value.clone());
    }

    let variables = variables.ok_or_else(|| {
        anyhow!("TemplateVariableInfo() requires a variables dictionary")
    })?;
    if !matches!(variables, Value::Dict(_)) {
        return Err(anyhow!(
            "TemplateVariableInfo() variables argument must be a dict"
        ));
    }

    Ok(Value::ProviderInstance {
        provider_name: "TemplateVariableInfo".to_string(),
        fields: HashMap::from([("variables".to_string(), variables)]),
    })
}

/// proto_common module - Bazel's proto common functions
fn builtin_proto_common(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert("compile".to_string(), Value::NativeFunction("proto_common.compile".to_string()));
    fields.insert("descriptor_set".to_string(), Value::NativeFunction("proto_common.descriptor_set".to_string()));
    Ok(Value::Namespace(fields))
}

/// Expose the native provider identity without fabricating Java artifacts.
pub(crate) fn native_java_info_provider() -> Value {
    Value::Provider {
        name: "JavaInfo".to_string(),
        fields: Vec::new(),
        init: Some(Box::new(Value::NativeFunction(
            "__rush_java_info_init".to_string(),
        ))),
    }
}

/// Expose the native plugin provider identity without fabricating processors.
pub(crate) fn native_java_plugin_info_provider() -> Value {
    Value::Provider {
        name: "JavaPluginInfo".to_string(),
        fields: Vec::new(),
        init: Some(Box::new(Value::NativeFunction(
            "__rush_java_plugin_info_init".to_string(),
        ))),
    }
}

fn builtin_java_info_init(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Err(anyhow!(
        "JavaInfo construction is unsupported: Java artifact and transitive classpath semantics are not implemented"
    ))
}

fn builtin_java_plugin_info_init(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Err(anyhow!(
        "JavaPluginInfo construction is unsupported: Java annotation-processor semantics are not implemented"
    ))
}

/// java_common module - Bazel's Java common functions
fn builtin_java_common(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert("compile".to_string(), Value::NativeFunction("java_common.compile".to_string()));
    fields.insert("merge".to_string(), Value::NativeFunction("java_common.merge".to_string()));
    fields.insert("make_non_strict".to_string(), Value::NativeFunction("java_common.make_non_strict".to_string()));
    fields.insert("provider".to_string(), native_java_info_provider());
    fields.insert("JavaPluginInfo".to_string(), native_java_plugin_info_provider());
    fields.insert("JavaToolchainInfo".to_string(), Value::Provider {
        name: "JavaToolchainInfo".to_string(),
        fields: vec![],
        init: None,
    });
    Ok(Value::Namespace(fields))
}

/// apple_common module - Bazel's Apple common functions
fn builtin_apple_common(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    fields.insert("AppleDynamicFramework".to_string(), Value::Provider {
        name: "AppleDynamicFramework".to_string(),
        fields: vec![],
        init: None,
    });
    fields.insert("Objc".to_string(), Value::Provider {
        name: "Objc".to_string(),
        fields: vec![],
        init: None,
    });
    fields.insert("new_objc_provider".to_string(), Value::Provider {
        name: "Objc".to_string(),
        fields: vec![],
        init: None,
    });
    fields.insert("apple_toolchain".to_string(), Value::NativeFunction("apple_common.apple_toolchain".to_string()));
    fields.insert("XcodeVersionConfig".to_string(), Value::NativeFunction("apple_common.XcodeVersionConfig".to_string()));
    fields.insert("platform_type".to_string(), Value::Namespace(HashMap::from([
        ("macos".to_string(), Value::String("macos".to_string())),
    ])));
    Ok(Value::Namespace(fields))
}

fn builtin_apple_toolchain(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::Namespace(HashMap::from([(
        "sdk_dir".to_string(),
        Value::NativeFunction("apple_toolchain.sdk_dir".to_string()),
    )])))
}

/// Generic provider stub for providers that don't need special handling
fn builtin_provider_stub(_args: &[Value], kwargs: &[(String, Value)]) -> Result<Value> {
    let mut fields = HashMap::new();
    for (key, value) in kwargs {
        fields.insert(key.clone(), value.clone());
    }
    Ok(Value::ProviderInstance {
        provider_name: "ProviderStub".to_string(),
        fields,
    })
}

/// Label stub - returns a label-like value
fn builtin_label_stub(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    Ok(Value::String("//label:stub".to_string()))
}

/// fragment() - returns a fragment object for accessing configuration fragments
fn builtin_fragment(_args: &[Value], _kwargs: &[(String, Value)]) -> Result<Value> {
    // Return an empty namespace that can have arbitrary attributes accessed
    Ok(Value::Namespace(HashMap::new()))
}
