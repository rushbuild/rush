use anyhow::{anyhow, Context, Result};
use rush_parse::{BuildFile, Rule, Value as ParseValue};
use rush_rules::registry::RuleRegistry;
use rush_rules::{RuleInstance, Value as RushRuleValue};
/// Rule execution bridge
///
/// Converts Starlark evaluation results into executable rule instances.
use rush_starlark::{
    rule_call_to_attrs, ConfigurableOp, PlatformConfig, RuleCall, RuleValue,
    VISIBILITY_EXPLICIT_MARKER,
};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default)]
struct SelectLabelContext<'a> {
    current_repository: Option<&'a str>,
    current_package: Option<&'a str>,
    main_repository_name: Option<&'a str>,
    repository_mapping: Option<&'a HashMap<String, String>>,
}

impl<'a> From<&'a InstantiationContext> for SelectLabelContext<'a> {
    fn from(context: &'a InstantiationContext) -> Self {
        Self {
            current_repository: context.repo_name.as_deref(),
            current_package: Some(context.package.as_str()),
            main_repository_name: context.main_repository_name.as_deref(),
            repository_mapping: Some(&context.repository_mapping),
        }
    }
}

fn mapped_repository<'a>(
    context: SelectLabelContext<'a>,
    apparent: &str,
) -> Option<&'a str> {
    context.repository_mapping.and_then(|mapping| {
        mapping
            .get(apparent)
            .or_else(|| mapping.get(&format!("@{apparent}")))
            .map(String::as_str)
    })
}

/// True when a select key denotes Bazel's one virtual fallback label.
fn is_default_select_condition(
    condition: &str,
    context: SelectLabelContext<'_>,
) -> bool {
    if matches!(
        condition,
        "//conditions:default"
            | "@//conditions:default"
            | "@@//conditions:default"
    ) {
        return true;
    }

    let named_repository = condition
        .strip_prefix('@')
        .filter(|condition| !condition.starts_with('@'))
        .and_then(|condition| condition.split_once("//"));
    if let Some((repository, label)) = named_repository {
        // An apparent name is interpreted in the consuming repository.  Its
        // owner mapping therefore wins even when that apparent name happens
        // to equal the main workspace/module name.
        if let Some(mapped) = mapped_repository(context, repository) {
            return label == "conditions:default" && mapped.is_empty();
        }
        if context.current_repository.is_none()
            && context.main_repository_name == Some(repository)
        {
            return label == "conditions:default";
        }
    }

    if context.current_repository.is_none()
        && context.current_package == Some("conditions")
    {
        return condition == ":default" || condition == "default";
    }
    false
}

/// Canonicalize a non-default select key for configuration matching.
/// Relative and repo-relative labels acquire the instantiating target's
/// package/repository identity, while explicit main-repository spellings lose
/// their marker. This prevents an external `conditions:linux` declaration from
/// activating main `//conditions:linux`.
fn canonical_select_condition(
    condition: &str,
    context: SelectLabelContext<'_>,
) -> String {
    if let Some(rest) = condition
        .strip_prefix("@//")
        .or_else(|| condition.strip_prefix("@@//"))
    {
        return format!("//{rest}");
    }

    if let Some(rest) = condition.strip_prefix("@@") {
        return format!("@{rest}");
    }
    if let Some(rest) = condition.strip_prefix('@') {
        if let Some((repository, label)) = rest.split_once("//") {
            let repository = if let Some(mapped) = mapped_repository(context, repository) {
                mapped
            } else if context.current_repository.is_none()
                && context.main_repository_name == Some(repository)
            {
                ""
            } else {
                repository
            };
            return if repository.is_empty() {
                format!("//{label}")
            } else {
                format!("@{repository}//{label}")
            };
        }
        return condition.to_string();
    }
    if let Some(rest) = condition.strip_prefix("//") {
        return match context.current_repository {
            Some(repository) => format!("@{repository}//{rest}"),
            None => condition.to_string(),
        };
    }

    let name = condition.strip_prefix(':').unwrap_or(condition);
    let package = context.current_package.unwrap_or_default();
    match (context.current_repository, package.is_empty()) {
        (Some(repository), true) => format!("@{repository}//:{name}"),
        (Some(repository), false) => format!("@{repository}//{package}:{name}"),
        (None, true) => format!("//:{name}"),
        (None, false) => format!("//{package}:{name}"),
    }
}

/// Canonicalize a select key for storage on a rule instance.
///
/// Evaluation uses `//pkg:target` for the main repository, but that spelling
/// becomes owner-relative when the dependency graph reparses metadata from an
/// external rule. Keep an explicit root marker until `Label::parse` consumes
/// it so `@@//pkg:target` cannot become `@owner//pkg:target`.
fn canonical_select_metadata_condition(
    condition: &str,
    context: SelectLabelContext<'_>,
) -> String {
    if let Some(rest) = condition
        .strip_prefix("@//")
        .or_else(|| condition.strip_prefix("@@//"))
    {
        return format!("@//{rest}");
    }

    let canonical = canonical_select_condition(condition, context);
    if condition.starts_with('@') && canonical.starts_with("//") {
        format!("@{canonical}")
    } else {
        canonical
    }
}

/// Canonicalize a label-valued configuration metadata attribute in the same
/// owner repository context used for select keys.
pub(crate) fn canonicalize_configuration_reference(
    label: &str,
    context: &InstantiationContext,
) -> String {
    canonical_select_metadata_condition(label, SelectLabelContext::from(context))
}

fn canonicalize_configuration_metadata_attrs(
    rule_type: &str,
    attrs: &mut HashMap<String, RushRuleValue>,
    context: &InstantiationContext,
) {
    match rule_type {
        "config_setting" => {
            if let Some(RushRuleValue::List(values)) = attrs.get_mut("constraint_values") {
                for value in values {
                    if let RushRuleValue::String(label) = value {
                        *label = canonicalize_configuration_reference(label, context);
                    }
                }
            }
            if let Some(RushRuleValue::Dict(values)) = attrs.get_mut("flag_values") {
                let mut canonical = values
                    .drain()
                    .map(|(label, value)| {
                        (
                            canonicalize_configuration_reference(&label, context),
                            value,
                        )
                    })
                    .collect::<Vec<_>>();
                canonical.sort_by(|(left, _), (right, _)| left.cmp(right));
                values.extend(canonical);
            }
        }
        "alias" => {
            if let Some(RushRuleValue::String(actual)) = attrs.get_mut("actual") {
                *actual = canonicalize_configuration_reference(actual, context);
            }
        }
        "constraint_value" => {
            if let Some(RushRuleValue::String(setting)) = attrs.get_mut("constraint_setting") {
                *setting = canonicalize_configuration_reference(setting, context);
            }
        }
        _ => {}
    }
}

fn select_condition_matches(
    platform: &PlatformConfig,
    condition: &str,
    context: SelectLabelContext<'_>,
) -> bool {
    platform.matches(&canonical_select_condition(condition, context))
}

fn maximal_config_matches<'a, T>(
    matches: Vec<(String, &'a T)>,
    platform: &PlatformConfig,
) -> Vec<(String, &'a T)> {
    let mut maximal = matches
        .iter()
        .enumerate()
        .filter(|(candidate_index, (candidate, _))| {
            !matches.iter().enumerate().any(|(other_index, (other, _))| {
                candidate_index != &other_index
                    && platform.strictly_specializes(other, candidate)
            })
        })
        .map(|(_, (condition, value))| (condition.clone(), *value))
        .collect::<Vec<_>>();
    maximal.sort_by(|(left, _), (right, _)| left.cmp(right));
    maximal
}

fn ambiguous_select_error<T>(matches: &[(String, &T)]) -> anyhow::Error {
    let labels = matches
        .iter()
        .map(|(condition, _)| condition.as_str())
        .collect::<Vec<_>>();
    anyhow!(
        "Illegal ambiguous match in select(): incomparable matching conditions resolve to different values: {}",
        labels.join(", ")
    )
}

fn conflicting_default_select_error<T>(defaults: &[(String, &T)]) -> anyhow::Error {
    let mut labels = defaults
        .iter()
        .map(|(condition, _)| condition.as_str())
        .collect::<Vec<_>>();
    labels.sort_unstable();
    anyhow!(
        "select() contains canonically equivalent default conditions with different values: {}. Rush cannot preserve Bazel's source-order precedence after select branches enter an unordered map",
        labels.join(", ")
    )
}

fn coalesce_canonical_matches<'a, T>(
    mut matches: Vec<(String, String, &'a T)>,
    equal: impl Fn(&T, &T) -> bool,
) -> Result<Vec<(String, &'a T)>> {
    matches.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    let mut coalesced = Vec::new();
    let mut index = 0;
    while index < matches.len() {
        let end = matches[index + 1..]
            .iter()
            .position(|candidate| candidate.0 != matches[index].0)
            .map(|offset| index + 1 + offset)
            .unwrap_or(matches.len());
        let group = &matches[index..end];
        if group.len() > 1
            && !group
                .iter()
                .skip(1)
                .all(|(_, _, value)| equal(value, group[0].2))
        {
            let labels = group
                .iter()
                .map(|(_, raw, _)| raw.as_str())
                .collect::<Vec<_>>();
            return Err(anyhow!(
                "select() contains canonically equivalent conditions with different values: {}. Rush cannot preserve Bazel's source-order precedence after select branches enter an unordered map",
                labels.join(", ")
            ));
        }
        coalesced.push((group[0].0.clone(), group[0].2));
        index = end;
    }
    Ok(coalesced)
}

/// Evaluate select() in RuleValue based on platform
fn evaluate_rule_value_select(
    select_map: &HashMap<String, RuleValue>,
    platform: &PlatformConfig,
    label_context: SelectLabelContext<'_>,
) -> Result<RuleValue> {
    // Collect every match before choosing. HashMap order is nondeterministic,
    // and silently taking its first entry is never a valid approximation of
    // Bazel's specialization rule. Until predicate-specificity metadata is
    // available here, fail closed on more than one match.
    let mut default_values = Vec::new();
    let mut matches = Vec::new();
    for (condition, value) in select_map {
        if is_default_select_condition(condition, label_context) {
            default_values.push((condition.clone(), value));
            continue;
        }
        if select_condition_matches(platform, condition, label_context) {
            matches.push((
                canonical_select_condition(condition, label_context),
                condition.clone(),
                value,
            ));
        }
    }

    let maximal = maximal_config_matches(
        coalesce_canonical_matches(matches, |left, right| left == right)?,
        platform,
    );
    if maximal.len() > 1
        && !maximal
            .iter()
            .skip(1)
            .all(|(_, value)| *value == maximal[0].1)
    {
        return Err(ambiguous_select_error(&maximal));
    }
    if let Some((_, value)) = maximal.first() {
        return Ok((*value).clone());
    }

    // No match found - look for default
    default_values.sort_by(|(left, _), (right, _)| left.cmp(right));
    if default_values.len() > 1
        && !default_values
            .iter()
            .skip(1)
            .all(|(_, value)| *value == default_values[0].1)
    {
        return Err(conflicting_default_select_error(&default_values));
    }
    if let Some((_, default_value)) = default_values.first() {
        return Ok((**default_value).clone());
    }

    anyhow::bail!("No matching condition in select() and no default provided")
}

fn apply_configurable_op(
    op: ConfigurableOp,
    left: RushRuleValue,
    right: RushRuleValue,
) -> Result<RushRuleValue> {
    match op {
        ConfigurableOp::Add => match (left, right) {
            (RushRuleValue::Int(left), RushRuleValue::Int(right)) => {
                Ok(RushRuleValue::Int(left + right))
            }
            (RushRuleValue::String(left), RushRuleValue::String(right)) => {
                Ok(RushRuleValue::String(left + &right))
            }
            (RushRuleValue::List(mut left), RushRuleValue::List(right)) => {
                left.extend(right);
                Ok(RushRuleValue::List(left))
            }
            (RushRuleValue::None, RushRuleValue::List(right)) => {
                Ok(RushRuleValue::List(right))
            }
            (RushRuleValue::List(left), RushRuleValue::None) => {
                Ok(RushRuleValue::List(left))
            }
            (RushRuleValue::None, RushRuleValue::None) => Ok(RushRuleValue::None),
            (left, right) => anyhow::bail!(
                "cannot apply configurable + to {} and {}",
                rush_value_kind(&left),
                rush_value_kind(&right)
            ),
        },
        ConfigurableOp::BitOr => match (left, right) {
            (RushRuleValue::Dict(mut left), RushRuleValue::Dict(right)) => {
                left.extend(right);
                Ok(RushRuleValue::Dict(left))
            }
            (RushRuleValue::List(left), RushRuleValue::List(right)) => {
                let mut merged = Vec::new();
                for value in left.into_iter().chain(right) {
                    if !merged.contains(&value) {
                        merged.push(value);
                    }
                }
                Ok(RushRuleValue::List(merged))
            }
            (RushRuleValue::Int(left), RushRuleValue::Int(right)) => {
                Ok(RushRuleValue::Int(left | right))
            }
            (left, right) => anyhow::bail!(
                "cannot apply configurable | to {} and {}",
                rush_value_kind(&left),
                rush_value_kind(&right)
            ),
        },
    }
}

fn rush_value_kind(value: &RushRuleValue) -> &'static str {
    match value {
        RushRuleValue::String(_) => "string",
        RushRuleValue::Int(_) => "int",
        RushRuleValue::Bool(_) => "bool",
        RushRuleValue::List(_) => "list",
        RushRuleValue::Dict(_) => "dict",
        RushRuleValue::None => "None",
    }
}

/// Convert RuleValue (from Starlark) to rush_rules::Value
pub fn rule_value_to_rush_value(value: &RuleValue) -> Result<RushRuleValue> {
    rule_value_to_rush_value_with_platform(
        value,
        &PlatformConfig::new(),
        SelectLabelContext::default(),
    )
}

fn rule_value_to_rush_value_with_platform(
    value: &RuleValue,
    platform: &PlatformConfig,
    label_context: SelectLabelContext<'_>,
) -> Result<RushRuleValue> {
    match value {
        RuleValue::String(s) => Ok(RushRuleValue::String(s.clone())),
        RuleValue::Int(i) => Ok(RushRuleValue::Int(*i)),
        RuleValue::Bool(b) => Ok(RushRuleValue::Bool(*b)),
        RuleValue::None => Ok(RushRuleValue::None),
        RuleValue::List(items) => Ok(RushRuleValue::List(
            items
                .iter()
                .map(|v| {
                    rule_value_to_rush_value_with_platform(
                        v,
                        platform,
                        label_context,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
        )),
        RuleValue::Dict(map) => Ok(RushRuleValue::Dict(
            map.iter()
                .map(|(k, v)| {
                    Ok((
                        k.clone(),
                        rule_value_to_rush_value_with_platform(
                            v,
                            platform,
                            label_context,
                        )?,
                    ))
                })
                .collect::<Result<HashMap<_, _>>>()?,
        )),
        RuleValue::Select(select_map) => {
            let evaluated = evaluate_rule_value_select(
                select_map,
                platform,
                label_context,
            )?;
            rule_value_to_rush_value_with_platform(
                &evaluated,
                platform,
                label_context,
            )
        }
        RuleValue::Configurable { op, left, right } => apply_configurable_op(
            *op,
            rule_value_to_rush_value_with_platform(
                left,
                platform,
                label_context,
            )?,
            rule_value_to_rush_value_with_platform(
                right,
                platform,
                label_context,
            )?,
        ),
    }
}

/// Convert attribute map from Starlark to rush_rules format
pub fn convert_attrs(
    attrs: &HashMap<String, RuleValue>,
) -> Result<HashMap<String, RushRuleValue>> {
    convert_attrs_with_platform(
        attrs,
        &PlatformConfig::new(),
        SelectLabelContext::default(),
    )
}

fn convert_attrs_with_platform(
    attrs: &HashMap<String, RuleValue>,
    platform: &PlatformConfig,
    label_context: SelectLabelContext<'_>,
) -> Result<HashMap<String, RushRuleValue>> {
    attrs
        .iter()
        .filter(|(key, _)| key.as_str() != VISIBILITY_EXPLICIT_MARKER)
        .map(|(k, v)| {
            Ok((
                k.clone(),
                rule_value_to_rush_value_with_platform(
                    v,
                    platform,
                    label_context,
                )
                    .with_context(|| format!("resolving attribute '{}'", k))?,
            ))
        })
        .collect()
}

fn visibility(attrs: &HashMap<String, RushRuleValue>) -> Option<Vec<String>> {
    match attrs.get("visibility") {
        Some(RushRuleValue::String(value)) => Some(vec![value.clone()]),
        Some(RushRuleValue::List(values)) => Some(
            values
                .iter()
                .filter_map(|value| value.as_string().map(ToOwned::to_owned))
                .collect(),
        ),
        _ => None,
    }
}

fn attach_rule_metadata(
    instance: Box<dyn RuleInstance>,
    attrs: &HashMap<String, RushRuleValue>,
    visibility_was_explicit: bool,
    configuration_conditions: Vec<String>,
) -> Box<dyn RuleInstance> {
    rush_rules::with_rule_metadata(
        instance,
        visibility(attrs),
        visibility_was_explicit,
        configuration_conditions,
    )
}

fn rule_visibility_was_explicit(attrs: &HashMap<String, RuleValue>) -> bool {
    match attrs.get(VISIBILITY_EXPLICIT_MARKER) {
        Some(RuleValue::Bool(value)) => *value,
        _ => !matches!(attrs.get("visibility"), None | Some(RuleValue::None)),
    }
}

fn parse_visibility_was_explicit(attrs: &HashMap<String, ParseValue>) -> bool {
    match attrs.get(VISIBILITY_EXPLICIT_MARKER) {
        Some(ParseValue::Bool(value)) => *value,
        _ => !matches!(attrs.get("visibility"), None | Some(ParseValue::None)),
    }
}

fn collect_rule_value_conditions(
    value: &RuleValue,
    conditions: &mut Vec<String>,
    label_context: SelectLabelContext<'_>,
) {
    match value {
        RuleValue::Select(branches) => {
            for (condition, branch) in branches {
                if !is_default_select_condition(condition, label_context) {
                    conditions.push(canonical_select_metadata_condition(
                        condition,
                        label_context,
                    ));
                }
                collect_rule_value_conditions(
                    branch,
                    conditions,
                    label_context,
                );
            }
        }
        RuleValue::Configurable { left, right, .. } => {
            collect_rule_value_conditions(left, conditions, label_context);
            collect_rule_value_conditions(right, conditions, label_context);
        }
        RuleValue::List(values) => {
            for value in values {
                collect_rule_value_conditions(value, conditions, label_context);
            }
        }
        RuleValue::Dict(values) => {
            for value in values.values() {
                collect_rule_value_conditions(value, conditions, label_context);
            }
        }
        _ => {}
    }
}

fn rule_configuration_conditions(
    attrs: &HashMap<String, RuleValue>,
    label_context: SelectLabelContext<'_>,
) -> Vec<String> {
    let mut conditions = Vec::new();
    for value in attrs.values() {
        collect_rule_value_conditions(value, &mut conditions, label_context);
    }
    conditions.sort();
    conditions.dedup();
    conditions
}

fn collect_parse_value_conditions(
    value: &ParseValue,
    conditions: &mut Vec<String>,
    label_context: SelectLabelContext<'_>,
) {
    match value {
        ParseValue::Select(branches) => {
            for (condition, branch) in branches {
                if !is_default_select_condition(condition, label_context) {
                    conditions.push(canonical_select_metadata_condition(
                        condition,
                        label_context,
                    ));
                }
                collect_parse_value_conditions(
                    branch,
                    conditions,
                    label_context,
                );
            }
        }
        ParseValue::Configurable { left, right, .. } => {
            collect_parse_value_conditions(
                left,
                conditions,
                label_context,
            );
            collect_parse_value_conditions(
                right,
                conditions,
                label_context,
            );
        }
        ParseValue::List(values) => {
            for value in values {
                collect_parse_value_conditions(
                    value,
                    conditions,
                    label_context,
                );
            }
        }
        ParseValue::Dict(values) => {
            for value in values.values() {
                collect_parse_value_conditions(
                    value,
                    conditions,
                    label_context,
                );
            }
        }
        _ => {}
    }
}

fn collect_raw_parse_value_conditions(
    value: &ParseValue,
    conditions: &mut Vec<String>,
    label_context: SelectLabelContext<'_>,
) {
    match value {
        ParseValue::Select(branches) => {
            for (condition, branch) in branches {
                if !is_default_select_condition(condition, label_context) {
                    // Preserve an explicit `@@` canonical repository marker
                    // through metadata preload. Canonicalizing it to single
                    // `@` here would let the owner's apparent-name mapping
                    // redirect the package before the condition is loaded.
                    conditions.push(condition.clone());
                }
                collect_raw_parse_value_conditions(branch, conditions, label_context);
            }
        }
        ParseValue::Configurable { left, right, .. } => {
            collect_raw_parse_value_conditions(left, conditions, label_context);
            collect_raw_parse_value_conditions(right, conditions, label_context);
        }
        ParseValue::List(values) => {
            for value in values {
                collect_raw_parse_value_conditions(value, conditions, label_context);
            }
        }
        ParseValue::Dict(values) => {
            for value in values.values() {
                collect_raw_parse_value_conditions(value, conditions, label_context);
            }
        }
        _ => {}
    }
}

fn parse_configuration_conditions(
    attrs: &HashMap<String, ParseValue>,
    label_context: SelectLabelContext<'_>,
) -> Vec<String> {
    let mut conditions = Vec::new();
    for value in attrs.values() {
        collect_parse_value_conditions(value, &mut conditions, label_context);
    }
    conditions.sort();
    conditions.dedup();
    conditions
}

/// Collect select keys for exact package preload without erasing label
/// provenance such as Bazel's canonical `@@repo` spelling.
pub(crate) fn build_file_raw_configuration_conditions(
    build_file: &BuildFile,
    context: &InstantiationContext,
) -> Vec<String> {
    let label_context = SelectLabelContext::from(context);
    let mut conditions = Vec::new();
    for rule in &build_file.rules {
        for value in rule.attributes.values() {
            collect_raw_parse_value_conditions(value, &mut conditions, label_context);
        }
    }
    conditions.sort();
    conditions.dedup();
    conditions
}

fn resolved_rule_visibility(attrs: &HashMap<String, RuleValue>) -> HashMap<String, RushRuleValue> {
    let visibility = match attrs.get("visibility") {
        Some(RuleValue::String(value)) => RushRuleValue::String(value.clone()),
        Some(RuleValue::List(values)) => {
            let Some(values) = values
                .iter()
                .map(|value| match value {
                    RuleValue::String(value) => Some(RushRuleValue::String(value.clone())),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
            else {
                return HashMap::new();
            };
            RushRuleValue::List(values)
        }
        _ => return HashMap::new(),
    };
    HashMap::from([("visibility".to_string(), visibility)])
}

fn resolved_parse_visibility(
    attrs: &HashMap<String, ParseValue>,
) -> HashMap<String, RushRuleValue> {
    let visibility = match attrs.get("visibility") {
        Some(ParseValue::String(value)) => RushRuleValue::String(value.clone()),
        Some(ParseValue::List(values)) => {
            let Some(values) = values
                .iter()
                .map(|value| match value {
                    ParseValue::String(value) => Some(RushRuleValue::String(value.clone())),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()
            else {
                return HashMap::new();
            };
            RushRuleValue::List(values)
        }
        _ => return HashMap::new(),
    };
    HashMap::from([("visibility".to_string(), visibility)])
}

fn unconfigurable_target(
    name: String,
    error: anyhow::Error,
    visibility_attrs: &HashMap<String, RushRuleValue>,
    visibility_was_explicit: bool,
    configuration_conditions: Vec<String>,
) -> Box<dyn RuleInstance> {
    let instance = Box::new(
        rush_rules::rules_config::UnconfigurableTargetInstance
            ::from_configurable_error(
                name,
                format!("{error:#}"),
            ),
    );
    attach_rule_metadata(
        instance,
        visibility_attrs,
        visibility_was_explicit,
        configuration_conditions,
    )
}

fn rule_value_contains_configurable(value: &RuleValue) -> bool {
    match value {
        RuleValue::Select(_) | RuleValue::Configurable { .. } => true,
        RuleValue::List(values) => values.iter().any(rule_value_contains_configurable),
        RuleValue::Dict(values) => values.values().any(rule_value_contains_configurable),
        _ => false,
    }
}

fn parse_value_contains_configurable(value: &ParseValue) -> bool {
    match value {
        ParseValue::Select(_) | ParseValue::Configurable { .. } => true,
        ParseValue::List(values) => values.iter().any(parse_value_contains_configurable),
        ParseValue::Dict(values) => values.values().any(parse_value_contains_configurable),
        _ => false,
    }
}

fn nonconfigurable_package_group_attribute<V>(
    rule_type: &str,
    attrs: &HashMap<String, V>,
    contains_configurable: impl Fn(&V) -> bool,
) -> Option<&'static str> {
    if rule_type != "package_group" {
        return None;
    }
    ["packages", "includes"].into_iter().find(|attribute| {
        attrs
            .get(*attribute)
            .is_some_and(|value| contains_configurable(value))
    })
}

fn package_group_nonconfigurable_error(attribute: &str) -> anyhow::Error {
    anyhow!(
        "in call to package_group(), parameter '{}' got value of type 'select', want 'sequence'",
        attribute
    )
}

fn visibility_nonconfigurable_error() -> anyhow::Error {
    anyhow!("attribute 'visibility' is not configurable")
}

/// Instantiate a rule from a RuleCall using the RuleRegistry
pub fn instantiate_rule(
    rule_call: &RuleCall,
    registry: &RuleRegistry,
) -> Result<Box<dyn RuleInstance>> {
    let name = rule_call
        .kwargs
        .get("name")
        .and_then(|v| match v {
            rush_starlark::Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();

    // Get the rule from registry
    let rule = registry.get(&rule_call.rule_type).ok_or_else(|| {
        anyhow!(
            "Unknown rule type: {} (target: {}, BUILD file context needed)",
            rule_call.rule_type,
            if name.is_empty() {
                "unknown"
            } else {
                name.as_str()
            }
        )
    })?;

    // Convert RuleCall to attribute map
    let starlark_attrs = rule_call_to_attrs(rule_call)?;
    let platform = PlatformConfig::new();
    let label_context = SelectLabelContext::default();
    let visibility_was_explicit = rule_visibility_was_explicit(&starlark_attrs);
    let configuration_conditions =
        rule_configuration_conditions(&starlark_attrs, label_context);

    if starlark_attrs
        .get("visibility")
        .is_some_and(rule_value_contains_configurable)
    {
        let visibility_attrs = resolved_rule_visibility(&starlark_attrs);
        return Ok(unconfigurable_target(
            name,
            visibility_nonconfigurable_error(),
            &visibility_attrs,
            visibility_was_explicit,
            configuration_conditions,
        ));
    }

    // packages/includes are nonconfigurable in Bazel 6.1. Reject the raw
    // expression before platform resolution, even when the selected/default
    // branch would happen to be a valid string list.
    if let Some(attribute) = nonconfigurable_package_group_attribute(
        &rule_call.rule_type,
        &starlark_attrs,
        rule_value_contains_configurable,
    ) {
        let visibility_attrs = resolved_rule_visibility(&starlark_attrs);
        return Ok(unconfigurable_target(
            name,
            package_group_nonconfigurable_error(attribute),
            &visibility_attrs,
            visibility_was_explicit,
            configuration_conditions,
        ));
    }

    // Compatibility is the only attribute that must resolve before the
    // target's remaining attributes. An incompatible target is registered but
    // never configured, so a bad select() in srcs/actual/etc. is irrelevant.
    if let Some(compatibility) = starlark_attrs.get("target_compatible_with") {
        let compatibility = match rule_value_to_rush_value_with_platform(
            compatibility,
            &platform,
            label_context,
        )
        .context("resolving attribute 'target_compatible_with'")
        {
            Ok(value) => value,
            Err(err) => {
                let visibility_attrs = resolved_rule_visibility(&starlark_attrs);
                return Ok(unconfigurable_target(
                    name,
                    err,
                    &visibility_attrs,
                    visibility_was_explicit,
                    configuration_conditions,
                ));
            }
        };
        if incompatible_value(&compatibility) {
            let visibility_attrs = resolved_rule_visibility(&starlark_attrs);
            let instance = Box::new(
                rush_rules::rules_config::IncompatibleTargetInstance::new(name),
            );
            return Ok(attach_rule_metadata(
                instance,
                &visibility_attrs,
                visibility_was_explicit,
                configuration_conditions,
            ));
        }
    }

    let visibility_attrs = resolved_rule_visibility(&starlark_attrs);
    let rush_attrs = match convert_attrs_with_platform(
        &starlark_attrs,
        &platform,
        label_context,
    ) {
        Ok(attrs) => attrs,
        Err(err) => {
            return Ok(unconfigurable_target(
                name,
                err,
                &visibility_attrs,
                visibility_was_explicit,
                configuration_conditions,
            ))
        }
    };

    // Parse attributes and create a rule instance. A failure is deferred until
    // the target enters the scoped configured-target closure (Bazel resolves
    // attributes at configuration time, so an unrequested sibling must not
    // fail package loading).
    let instance = match rule.parse_attributes(&rush_attrs) {
        Ok(instance) => instance,
        Err(err) => Box::new(
            rush_rules::rules_config::UnconfigurableTargetInstance::new(
                name,
                format!("instantiating {}: {err:#}", rule_call.rule_type),
            ),
        ),
    };
    Ok(attach_rule_metadata(
        instance,
        &rush_attrs,
        visibility_was_explicit,
        configuration_conditions,
    ))
}

fn incompatible_value(value: &RushRuleValue) -> bool {
    match value {
        RushRuleValue::List(items) => items.iter().any(|v| {
            matches!(v, RushRuleValue::String(s) if s.contains("platforms//:incompatible"))
        }),
        _ => false,
    }
}

/// Process a list of RuleCalls and create RuleInstances
pub fn instantiate_rules(
    rule_calls: &[RuleCall],
    registry: &RuleRegistry,
) -> Result<Vec<Box<dyn RuleInstance>>> {
    rule_calls
        .iter()
        .map(|call| instantiate_rule(call, registry))
        .collect()
}

/// Evaluate select() in ParseValue based on platform
fn evaluate_parse_value_select(
    select_map: &HashMap<String, ParseValue>,
    platform: &PlatformConfig,
    label_context: SelectLabelContext<'_>,
) -> Result<ParseValue> {
    let mut default_values = Vec::new();
    let mut matches = Vec::new();
    for (condition, value) in select_map {
        if is_default_select_condition(condition, label_context) {
            default_values.push((condition.clone(), value));
            continue;
        }
        if select_condition_matches(platform, condition, label_context) {
            matches.push((
                canonical_select_condition(condition, label_context),
                condition.clone(),
                value,
            ));
        }
    }

    let maximal = maximal_config_matches(
        coalesce_canonical_matches(matches, parse_values_equal)?,
        platform,
    );
    if maximal.len() > 1
        && !maximal
            .iter()
            .skip(1)
            .all(|(_, value)| parse_values_equal(value, maximal[0].1))
    {
        return Err(ambiguous_select_error(&maximal));
    }
    if let Some((_, value)) = maximal.first() {
        return Ok((*value).clone());
    }

    default_values.sort_by(|(left, _), (right, _)| left.cmp(right));
    if default_values.len() > 1
        && !default_values
            .iter()
            .skip(1)
            .all(|(_, value)| parse_values_equal(value, default_values[0].1))
    {
        return Err(conflicting_default_select_error(&default_values));
    }
    if let Some((_, default_value)) = default_values.first() {
        return Ok((**default_value).clone());
    }

    anyhow::bail!("No matching condition in select() and no default provided")
}

fn parse_values_equal(left: &ParseValue, right: &ParseValue) -> bool {
    match (left, right) {
        (ParseValue::String(left), ParseValue::String(right)) => left == right,
        (ParseValue::Int(left), ParseValue::Int(right)) => left == right,
        (ParseValue::Bool(left), ParseValue::Bool(right)) => left == right,
        (ParseValue::None, ParseValue::None) => true,
        (ParseValue::List(left), ParseValue::List(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| parse_values_equal(left, right))
        }
        (ParseValue::Dict(left), ParseValue::Dict(right))
        | (ParseValue::Select(left), ParseValue::Select(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| parse_values_equal(left, right))
                })
        }
        (
            ParseValue::Configurable {
                op: left_op,
                left: left_left,
                right: left_right,
            },
            ParseValue::Configurable {
                op: right_op,
                left: right_left,
                right: right_right,
            },
        ) => {
            left_op == right_op
                && parse_values_equal(left_left, right_left)
                && parse_values_equal(left_right, right_right)
        }
        _ => false,
    }
}

/// Convert rush_parse::Value to rush_rules::Value
pub fn parse_value_to_rush_value(value: &ParseValue) -> Result<RushRuleValue> {
    parse_value_to_rush_value_with_platform(
        value,
        &PlatformConfig::new(),
        SelectLabelContext::default(),
    )
}

/// Resolve one parsed configurable attribute as a label in its owning rule's
/// repository/package context. Configuration-metadata loading uses this for
/// native `alias(actual = select(...))` chains after it has loaded the exact
/// select-key closure.
pub(crate) fn resolve_parse_value_as_label(
    value: &ParseValue,
    context: &InstantiationContext,
) -> Result<String> {
    let platform = PlatformConfig::for_configuration_scope(
        context.configuration_scope.clone(),
    );
    match parse_value_to_rush_value_with_platform(
        value,
        &platform,
        SelectLabelContext::from(context),
    )? {
        RushRuleValue::String(label) => Ok(label),
        other => anyhow::bail!(
            "alias.actual resolved to {}, want label",
            rush_rule_value_type_name(&other)
        ),
    }
}

fn rush_rule_value_type_name(value: &RushRuleValue) -> &'static str {
    match value {
        RushRuleValue::String(_) => "string",
        RushRuleValue::Int(_) => "int",
        RushRuleValue::Bool(_) => "bool",
        RushRuleValue::List(_) => "list",
        RushRuleValue::Dict(_) => "dict",
        RushRuleValue::None => "NoneType",
    }
}

fn parse_value_to_rush_value_with_platform(
    value: &ParseValue,
    platform: &PlatformConfig,
    label_context: SelectLabelContext<'_>,
) -> Result<RushRuleValue> {
    match value {
        ParseValue::String(s) => Ok(RushRuleValue::String(s.clone())),
        ParseValue::Int(value) => Ok(RushRuleValue::Int(*value)),
        ParseValue::Bool(b) => Ok(RushRuleValue::Bool(*b)),
        ParseValue::None => Ok(RushRuleValue::None),
        ParseValue::List(items) => Ok(RushRuleValue::List(
            items
                .iter()
                .map(|v| {
                    parse_value_to_rush_value_with_platform(
                        v,
                        platform,
                        label_context,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
        )),
        ParseValue::Dict(items) => Ok(RushRuleValue::Dict(
            items
                .iter()
                .map(|(key, value)| {
                    Ok((
                        key.clone(),
                        parse_value_to_rush_value_with_platform(
                            value,
                            platform,
                            label_context,
                        )?,
                    ))
                })
                .collect::<Result<HashMap<_, _>>>()?,
        )),
        ParseValue::Select(select_map) => {
            let evaluated = evaluate_parse_value_select(
                select_map,
                platform,
                label_context,
            )?;
            parse_value_to_rush_value_with_platform(
                &evaluated,
                platform,
                label_context,
            )
        }
        ParseValue::Configurable { op, left, right } => apply_configurable_op(
            *op,
            parse_value_to_rush_value_with_platform(
                left,
                platform,
                label_context,
            )?,
            parse_value_to_rush_value_with_platform(
                right,
                platform,
                label_context,
            )?,
        ),
    }
}

fn convert_parse_attrs_with_platform(
    attrs: &HashMap<String, ParseValue>,
    platform: &PlatformConfig,
    label_context: SelectLabelContext<'_>,
) -> Result<HashMap<String, RushRuleValue>> {
    attrs
        .iter()
        .filter(|(key, _)| key.as_str() != VISIBILITY_EXPLICIT_MARKER)
        .map(|(k, v)| {
            Ok((
                k.clone(),
                parse_value_to_rush_value_with_platform(
                    v,
                    platform,
                    label_context,
                )
                    .with_context(|| format!("resolving attribute '{}'", k))?,
            ))
        })
        .collect()
}

/// Context for rule instantiation. Lets the protobuf Bzlmod path re-route
/// `CustomRule` proto rules to the native `proto_library`/`cc_proto_library`
/// (which the repo's `.bzl` definitions shadow) while the attributes — crucially
/// `srcs` — are still live. CustomRuleStub drops everything but name+deps, so by
/// graph-add-node time the proto sources are gone; this catches them at parse.
#[derive(Default)]
pub struct InstantiationContext {
    pub bzlmod: bool,
    /// Non-empty workspace configuration registry used by production
    /// analysis. Keeping it explicit prevents select resolution from falling
    /// back to the thread-local compatibility scope after an async boundary.
    pub configuration_scope: String,
    /// Current external repository, or `None` for the main repository.
    pub repo_name: Option<String>,
    pub package: String,
    /// Declared WORKSPACE/module name that may explicitly spell the main repo.
    pub main_repository_name: Option<String>,
    /// Apparent-to-canonical repository names visible from this repository.
    pub repository_mapping: HashMap<String, String>,
}

/// True if `srcs` is a list containing at least one `*.proto` source.
fn has_proto_srcs(attrs: &HashMap<String, RushRuleValue>) -> bool {
    matches!(attrs.get("srcs"), Some(RushRuleValue::List(items))
        if items.iter().any(|v| matches!(v, RushRuleValue::String(s) if s.ends_with(".proto"))))
}

/// Convert rush_parse::Rule to RuleInstance using RuleRegistry.
pub fn parse_rule_to_instance(
    rule: &Rule,
    registry: &RuleRegistry,
) -> Result<Box<dyn RuleInstance>> {
    parse_rule_to_instance_with_context(rule, registry, &InstantiationContext::default())
}

/// Context-aware instantiation. See [`InstantiationContext`].
pub fn parse_rule_to_instance_with_context(
    rule: &Rule,
    registry: &RuleRegistry,
    inst_ctx: &InstantiationContext,
) -> Result<Box<dyn RuleInstance>> {
    let platform = if inst_ctx.configuration_scope.is_empty() {
        PlatformConfig::new()
    } else {
        PlatformConfig::for_configuration_scope(
            inst_ctx.configuration_scope.clone(),
        )
    };
    let label_context = SelectLabelContext::from(inst_ctx);
    let visibility_was_explicit =
        parse_visibility_was_explicit(&rule.attributes);
    let configuration_conditions =
        parse_configuration_conditions(&rule.attributes, label_context);

    if rule
        .attributes
        .get("visibility")
        .is_some_and(parse_value_contains_configurable)
    {
        let visibility_attrs = resolved_parse_visibility(&rule.attributes);
        return Ok(unconfigurable_target(
            rule.name.clone(),
            visibility_nonconfigurable_error(),
            &visibility_attrs,
            visibility_was_explicit,
            configuration_conditions,
        ));
    }

    // Preserve the package_group attribute's nonconfigurable contract across
    // the rush_parse bridge. This check must precede select resolution.
    if let Some(attribute) = nonconfigurable_package_group_attribute(
        &rule.rule_type,
        &rule.attributes,
        parse_value_contains_configurable,
    ) {
        let visibility_attrs = resolved_parse_visibility(&rule.attributes);
        return Ok(unconfigurable_target(
            rule.name.clone(),
            package_group_nonconfigurable_error(attribute),
            &visibility_attrs,
            visibility_was_explicit,
            configuration_conditions,
        ));
    }

    // Resolve compatibility before touching any other configurable attribute.
    // Bazel never configures an incompatible target, so an unmatched select in
    // one of its other attributes must not turn it into an error target.
    if let Some(compatibility) = rule.attributes.get("target_compatible_with") {
        let compatibility = match parse_value_to_rush_value_with_platform(
            compatibility,
            &platform,
            label_context,
        )
        .context("resolving attribute 'target_compatible_with'")
        {
            Ok(value) => value,
            Err(err) => {
                let visibility_attrs =
                    resolved_parse_visibility(&rule.attributes);
                return Ok(unconfigurable_target(
                    rule.name.clone(),
                    err,
                    &visibility_attrs,
                    visibility_was_explicit,
                    configuration_conditions,
                ));
            }
        };
        if incompatible_value(&compatibility) {
            let visibility_attrs = resolved_parse_visibility(&rule.attributes);
            let instance = Box::new(
                rush_rules::rules_config::IncompatibleTargetInstance::new(rule.name.clone()),
            );
            return Ok(attach_rule_metadata(
                instance,
                &visibility_attrs,
                visibility_was_explicit,
                configuration_conditions,
            ));
        }
    }

    // Resolve the remaining attributes only for a compatible target. A miss is
    // represented by an instance that errors if that target enters the
    // requested closure; loading the package and building an unrelated sibling
    // still succeeds.
    let visibility_attrs = resolved_parse_visibility(&rule.attributes);
    let mut rush_attrs = match convert_parse_attrs_with_platform(
        &rule.attributes,
        &platform,
        label_context,
    ) {
        Ok(attrs) => attrs,
        Err(err) => {
            return Ok(unconfigurable_target(
                rule.name.clone(),
                err,
                &visibility_attrs,
                visibility_was_explicit,
                configuration_conditions,
            ));
        }
    };
    canonicalize_configuration_metadata_attrs(&rule.rule_type, &mut rush_attrs, inst_ctx);

    // TF kernel-library macros default `alwayslink = 1` (and `linkstatic = 1`):
    // OpKernels register via static REGISTER_KERNEL_BUILDER initializers that
    // nothing references by symbol (the runtime resolves kernels by op NAME via
    // the registry), so without --whole-archive the linker drops them and a
    // test that builds a graph node by name (e.g. ShuffleDataset) aborts "No
    // registered OpKernel". rush captures these as a leaf rule (rule_type =
    // tf_kernel_library) without running the TF macro body that adds the
    // default, so inject it here in the BUILD-file instantiation path (this is
    // what build_file_to_instances uses for the analyze whole-archive
    // linker_input). Matches Bazel; an explicit BUILD value wins.
    // (Codex gpt-5.5 xhigh thread 019ec55c.)
    if rule.attributes.get("name").map(|v| format!("{:?}", v)).unwrap_or_default().contains("shuffle_dataset_op") {
        eprintln!("RUSHDBG parse_rule_to_instance: name~shuffle_dataset_op rule_type={} alwayslink_attr={:?}", rule.rule_type, rush_attrs.get("alwayslink"));
    }
    if matches!(
        rule.rule_type.as_str(),
        "tf_kernel_library"
            | "tf_mkl_kernel_library"
            | "tf_gpu_kernel_library"
            | "gpu_kernel_library"
    ) {
        rush_attrs
            .entry("alwayslink".to_string())
            .or_insert(RushRuleValue::Int(1));
        rush_attrs
            .entry("linkstatic".to_string())
            .or_insert(RushRuleValue::Int(1));

        // GPU: TF kernel-library macros put GPU kernel sources (.cu.cc) in `gpu_srcs`
        // and, on the GPU path, compile them alongside `srcs`. rush has no gpu_srcs handling
        // (CPU builds ignore them). Fold them into srcs for either explicit GPU backend;
        // per-source compiler routing chooses HIP or CUDA. CPU builds are unchanged.
        let rocm_enabled =
            rush_incremental::tracked::getenv_str("RUSH_ENABLE_ROCM").as_deref() == Some("1");
        let cuda_enabled =
            rush_incremental::tracked::getenv_str("RUSH_ENABLE_CUDA").as_deref() == Some("1");
        if rocm_enabled || cuda_enabled {
            if let Some(RushRuleValue::List(gpu_srcs)) = rush_attrs.get("gpu_srcs").cloned() {
                if !gpu_srcs.is_empty() {
                    match rush_attrs.get_mut("srcs") {
                        Some(RushRuleValue::List(srcs)) => srcs.extend(gpu_srcs),
                        _ => {
                            rush_attrs
                                .insert("srcs".to_string(), RushRuleValue::List(gpu_srcs));
                        }
                    }
                }
            }
        }
    }

    // Bzlmod: BCR repos load proto_library/cc_proto_library from .bzl (rules_proto,
    // protobuf), which shadow the native top-level names, so rule() emits
    // rule_type="CustomRule" → CustomRuleStub (which drops srcs). Re-route to the native
    // rules at parse time — while attrs are still live — so the proto closure generates
    // real C++. Repo-agnostic: keyed on the _cc_proto / _proto (with .proto srcs) name
    // pattern, which is proto-specific by convention. Gated by RUSH_ENABLE_BZLMOD, so
    // the certified TF/463 path is unaffected.
    let effective_rule_type: &str = if inst_ctx.bzlmod && rule.rule_type == "CustomRule" {
        if rule.name.ends_with("_cc_proto") {
            "cc_proto_library"
        } else if rule.name.ends_with("_proto") && has_proto_srcs(&rush_attrs) {
            "proto_library"
        } else {
            rule.rule_type.as_str()
        }
    } else {
        rule.rule_type.as_str()
    };

    // Get the rule definition from registry
    let rule_def = registry
        .get(effective_rule_type)
        .ok_or_else(|| anyhow!("Unknown rule type: {}", effective_rule_type))?;

    // Parse attributes and create rule instance; failures defer until the
    // instance enters the requested configured-target closure.
    let instance = match rule_def.parse_attributes(&rush_attrs) {
        Ok(instance) => instance,
        Err(err) => Box::new(
            rush_rules::rules_config::UnconfigurableTargetInstance::new(
                rule.name.clone(),
                format!("instantiating {}: {err:#}", effective_rule_type),
            ),
        ),
    };
    Ok(attach_rule_metadata(
        instance,
        &rush_attrs,
        visibility_was_explicit,
        configuration_conditions,
    ))
}

/// Convert a BuildFile (with rush_parse::Rule) to Vec<RuleInstance>
pub fn build_file_to_instances(
    build_file: &BuildFile,
    registry: &RuleRegistry,
) -> Result<Vec<Box<dyn RuleInstance>>> {
    build_file_to_instances_with_context(build_file, registry, &InstantiationContext::default())
}

/// Context-aware variant of [`build_file_to_instances`]. See [`InstantiationContext`].
pub fn build_file_to_instances_with_context(
    build_file: &BuildFile,
    registry: &RuleRegistry,
    inst_ctx: &InstantiationContext,
) -> Result<Vec<Box<dyn RuleInstance>>> {
    build_file
        .rules
        .iter()
        .map(|rule| parse_rule_to_instance_with_context(rule, registry, inst_ctx))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_rules::context::BuildContext;
    use rush_starlark::Value as StarlarkValue;

    const NEVER_CONDITION: &str = "//conditions:rush_select_test_never";

    fn cc_registry() -> RuleRegistry {
        let mut registry = RuleRegistry::new();
        registry.register(Box::new(rush_rules::rules_cc::CcLibraryRule));
        registry
    }

    fn package_group_registry() -> RuleRegistry {
        let mut registry = RuleRegistry::new();
        registry.register(Box::new(rush_rules::rules_cc::PackageGroupRule));
        registry
    }

    fn assert_nonconfigurable_package_group(instance: &dyn RuleInstance, attribute: &str) {
        let error = instance
            .configuration_error()
            .expect("configurable package_group attribute must be retained as an error");
        assert!(error.contains("in call to package_group()"), "{error}");
        assert!(
            error.contains(&format!("parameter '{attribute}'")),
            "{error}"
        );
        assert!(error.contains("type 'select', want 'sequence'"), "{error}");
    }

    fn assert_nonconfigurable_visibility(instance: &dyn RuleInstance) {
        let error = instance
            .configuration_error()
            .expect("configurable visibility must be retained as an error");
        assert!(
            error.contains("attribute 'visibility' is not configurable"),
            "{error}"
        );
    }

    fn unmatched_parse_select(value: ParseValue) -> ParseValue {
        ParseValue::Select(HashMap::from([(NEVER_CONDITION.to_string(), value)]))
    }

    fn cc_rule(name: &str, srcs: ParseValue) -> Rule {
        Rule {
            name: name.to_string(),
            rule_type: "cc_library".to_string(),
            attributes: HashMap::from([
                ("name".to_string(), ParseValue::String(name.to_string())),
                ("srcs".to_string(), srcs),
            ]),
        }
    }

    #[test]
    fn test_convert_string() {
        let rule_val = RuleValue::String("test".to_string());
        let rush_val = rule_value_to_rush_value(&rule_val).unwrap();
        assert!(matches!(rush_val, RushRuleValue::String(s) if s == "test"));
    }

    #[test]
    fn test_convert_list() {
        let rule_val = RuleValue::List(vec![
            RuleValue::String("a".to_string()),
            RuleValue::String("b".to_string()),
        ]);
        let rush_val = rule_value_to_rush_value(&rule_val).unwrap();
        match rush_val {
            RushRuleValue::List(items) => {
                assert_eq!(items.len(), 2);
            }
            _ => panic!("Expected List"),
        }
    }

    #[test]
    fn test_rule_call_conversion() {
        let mut kwargs = HashMap::new();
        kwargs.insert(
            "name".to_string(),
            StarlarkValue::String("test".to_string()),
        );

        let rule_call = RuleCall {
            rule_type: "cc_library".to_string(),
            args: vec![],
            kwargs,
        };

        // This would fail without a registered cc_library rule, so we just test conversion
        let starlark_attrs = rule_call_to_attrs(&rule_call).unwrap();
        let rush_attrs = convert_attrs(&starlark_attrs).unwrap();

        assert!(rush_attrs.contains_key("name"));
    }

    #[test]
    fn rule_bridge_retains_select_and_visibility_origin_metadata() {
        let configurable = RuleCall {
            rule_type: "cc_library".to_string(),
            args: vec![],
            kwargs: HashMap::from([
                (
                    "name".to_string(),
                    StarlarkValue::String("configurable".to_string()),
                ),
                (
                    "srcs".to_string(),
                    StarlarkValue::Select(HashMap::from([
                        (
                            NEVER_CONDITION.to_string(),
                            StarlarkValue::List(vec![StarlarkValue::String(
                                "selected.cc".to_string(),
                            )]),
                        ),
                        (
                            "//conditions:default".to_string(),
                            StarlarkValue::List(vec![]),
                        ),
                    ])),
                ),
            ]),
        };
        let instance = instantiate_rule(&configurable, &cc_registry()).unwrap();
        assert_eq!(instance.configuration_conditions(), &[NEVER_CONDITION]);

        let mut registry = RuleRegistry::new();
        registry.register(Box::new(rush_rules::ConfigSettingRule));
        let config_setting = RuleCall {
            rule_type: "config_setting".to_string(),
            args: vec![],
            kwargs: HashMap::from([
                (
                    "name".to_string(),
                    StarlarkValue::String("mode".to_string()),
                ),
                (
                    "visibility".to_string(),
                    StarlarkValue::List(vec![StarlarkValue::String(
                        "//visibility:private".to_string(),
                    )]),
                ),
                (
                    VISIBILITY_EXPLICIT_MARKER.to_string(),
                    StarlarkValue::Bool(false),
                ),
            ]),
        };
        let instance = instantiate_rule(&config_setting, &registry).unwrap();
        assert_eq!(
            instance.visibility(),
            Some(["//visibility:private".to_string()].as_slice())
        );
        assert!(!instance.visibility_was_explicit());
    }

    #[test]
    fn package_group_rejects_configurable_attributes_before_rule_call_resolution() {
        let registry = package_group_registry();
        let packages_select = RuleCall {
            rule_type: "package_group".to_string(),
            args: vec![],
            kwargs: HashMap::from([
                (
                    "name".to_string(),
                    StarlarkValue::String("packages".to_string()),
                ),
                (
                    "packages".to_string(),
                    StarlarkValue::Select(HashMap::from([(
                        "//conditions:default".to_string(),
                        StarlarkValue::List(vec![StarlarkValue::String("//allowed".to_string())]),
                    )])),
                ),
            ]),
        };
        let instance = instantiate_rule(&packages_select, &registry).unwrap();
        assert_nonconfigurable_package_group(instance.as_ref(), "packages");

        let includes_configurable = RuleCall {
            rule_type: "package_group".to_string(),
            args: vec![],
            kwargs: HashMap::from([
                (
                    "name".to_string(),
                    StarlarkValue::String("includes".to_string()),
                ),
                (
                    "packages".to_string(),
                    StarlarkValue::List(vec![StarlarkValue::String("//allowed".to_string())]),
                ),
                (
                    "includes".to_string(),
                    StarlarkValue::Configurable {
                        op: ConfigurableOp::Add,
                        left: std::sync::Arc::new(StarlarkValue::List(vec![])),
                        right: std::sync::Arc::new(StarlarkValue::Select(HashMap::from([(
                            "//conditions:default".to_string(),
                            StarlarkValue::List(vec![]),
                        )]))),
                    },
                ),
            ]),
        };
        let instance = instantiate_rule(&includes_configurable, &registry).unwrap();
        assert_nonconfigurable_package_group(instance.as_ref(), "includes");
    }

    #[test]
    fn package_group_rejects_configurable_attributes_before_parse_bridge_resolution() {
        let registry = package_group_registry();
        let packages_configurable = Rule {
            name: "packages".to_string(),
            rule_type: "package_group".to_string(),
            attributes: HashMap::from([
                (
                    "name".to_string(),
                    ParseValue::String("packages".to_string()),
                ),
                (
                    "packages".to_string(),
                    ParseValue::Configurable {
                        op: ConfigurableOp::Add,
                        left: Box::new(ParseValue::List(vec![ParseValue::String(
                            "//allowed".to_string(),
                        )])),
                        right: Box::new(ParseValue::Select(HashMap::from([(
                            "//conditions:default".to_string(),
                            ParseValue::List(vec![ParseValue::String(
                                "//also_allowed".to_string(),
                            )]),
                        )]))),
                    },
                ),
            ]),
        };
        let instance = parse_rule_to_instance(&packages_configurable, &registry).unwrap();
        assert_nonconfigurable_package_group(instance.as_ref(), "packages");

        let includes_select = Rule {
            name: "includes".to_string(),
            rule_type: "package_group".to_string(),
            attributes: HashMap::from([
                (
                    "name".to_string(),
                    ParseValue::String("includes".to_string()),
                ),
                (
                    "packages".to_string(),
                    ParseValue::List(vec![ParseValue::String("//allowed".to_string())]),
                ),
                (
                    "includes".to_string(),
                    ParseValue::Select(HashMap::from([(
                        "//conditions:default".to_string(),
                        ParseValue::List(vec![]),
                    )])),
                ),
            ]),
        };
        let instance = parse_rule_to_instance(&includes_select, &registry).unwrap();
        assert_nonconfigurable_package_group(instance.as_ref(), "includes");
    }

    #[test]
    fn package_group_accepts_constant_lists_through_both_bridges() {
        let registry = package_group_registry();
        let rule_call = RuleCall {
            rule_type: "package_group".to_string(),
            args: vec![],
            kwargs: HashMap::from([
                ("name".to_string(), StarlarkValue::String("raw".to_string())),
                (
                    "packages".to_string(),
                    StarlarkValue::List(vec![StarlarkValue::String("//allowed".to_string())]),
                ),
                ("includes".to_string(), StarlarkValue::List(vec![])),
            ]),
        };
        let raw_instance = instantiate_rule(&rule_call, &registry).unwrap();
        assert_eq!(raw_instance.rule_type(), "package_group");
        assert!(raw_instance.configuration_error().is_none());

        let rule = Rule {
            name: "parsed".to_string(),
            rule_type: "package_group".to_string(),
            attributes: HashMap::from([
                ("name".to_string(), ParseValue::String("parsed".to_string())),
                (
                    "packages".to_string(),
                    ParseValue::List(vec![ParseValue::String("//allowed".to_string())]),
                ),
                ("includes".to_string(), ParseValue::List(vec![])),
            ]),
        };
        let parsed_instance = parse_rule_to_instance(&rule, &registry).unwrap();
        assert_eq!(parsed_instance.rule_type(), "package_group");
        assert!(parsed_instance.configuration_error().is_none());
    }

    #[test]
    fn visibility_rejects_selects_before_both_bridge_resolvers() {
        let registry = cc_registry();
        let rule_call = RuleCall {
            rule_type: "cc_library".to_string(),
            args: vec![],
            kwargs: HashMap::from([
                (
                    "name".to_string(),
                    StarlarkValue::String("raw".to_string()),
                ),
                (
                    "visibility".to_string(),
                    StarlarkValue::Select(HashMap::from([(
                        "//conditions:default".to_string(),
                        StarlarkValue::List(vec![StarlarkValue::String(
                            "//visibility:public".to_string(),
                        )]),
                    )])),
                ),
            ]),
        };
        let raw_instance = instantiate_rule(&rule_call, &registry).unwrap();
        assert_nonconfigurable_visibility(raw_instance.as_ref());

        let rule = Rule {
            name: "parsed".to_string(),
            rule_type: "cc_library".to_string(),
            attributes: HashMap::from([
                (
                    "name".to_string(),
                    ParseValue::String("parsed".to_string()),
                ),
                (
                    "visibility".to_string(),
                    ParseValue::Select(HashMap::from([(
                        "//conditions:default".to_string(),
                        ParseValue::List(vec![ParseValue::String(
                            "//visibility:public".to_string(),
                        )]),
                    )])),
                ),
            ]),
        };
        let parsed_instance =
            parse_rule_to_instance(&rule, &registry).unwrap();
        assert_nonconfigurable_visibility(parsed_instance.as_ref());
    }

    #[test]
    fn unmatched_select_conversion_is_fallible_in_both_bridges() {
        let rule_value = RuleValue::Select(HashMap::from([(
            NEVER_CONDITION.to_string(),
            RuleValue::List(vec![]),
        )]));
        let parse_value = unmatched_parse_select(ParseValue::List(vec![]));

        assert!(rule_value_to_rush_value(&rule_value).is_err());
        assert!(parse_value_to_rush_value(&parse_value).is_err());
    }

    #[test]
    fn deferred_select_can_match_at_instantiation_time() {
        let mut platform = PlatformConfig::new();
        platform.set_condition(NEVER_CONDITION.to_string(), true);
        let value = unmatched_parse_select(ParseValue::List(vec![ParseValue::String(
            "late.cc".to_string(),
        )]));

        let resolved = parse_value_to_rush_value_with_platform(
            &value,
            &platform,
            SelectLabelContext::default(),
        )
        .unwrap();
        assert_eq!(
            resolved,
            RushRuleValue::List(vec![RushRuleValue::String("late.cc".to_string())])
        );
    }

    #[test]
    fn configuration_metadata_labels_use_the_owner_repository_mapping() {
        let context = InstantiationContext {
            repo_name: Some("consumer+".to_string()),
            package: "cfg".to_string(),
            repository_mapping: HashMap::from([(
                "p".to_string(),
                "platforms".to_string(),
            )]),
            ..InstantiationContext::default()
        };
        let mut registry = RuleRegistry::new();
        registry.register(Box::new(rush_rules::ConfigSettingRule));
        registry.register(Box::new(rush_rules::rules_config::ConfigNounRule {
            rule_type: "constraint_value",
        }));
        registry.register(Box::new(tensorflow_rules::AliasRule));

        let config_setting = Rule {
            name: "host".to_string(),
            rule_type: "config_setting".to_string(),
            attributes: HashMap::from([
                ("name".to_string(), ParseValue::String("host".to_string())),
                (
                    "constraint_values".to_string(),
                    ParseValue::List(vec![ParseValue::String(
                        "@p//os:linux".to_string(),
                    )]),
                ),
            ]),
        };
        let config_setting =
            parse_rule_to_instance_with_context(&config_setting, &registry, &context).unwrap();
        assert_eq!(
            config_setting.configuration_constraint_values(),
            ["@platforms//os:linux".to_string()]
        );

        let constraint_value = Rule {
            name: "linux".to_string(),
            rule_type: "constraint_value".to_string(),
            attributes: HashMap::from([
                ("name".to_string(), ParseValue::String("linux".to_string())),
                (
                    "constraint_setting".to_string(),
                    ParseValue::String("@p//os:os".to_string()),
                ),
            ]),
        };
        let constraint_value =
            parse_rule_to_instance_with_context(&constraint_value, &registry, &context).unwrap();
        assert_eq!(
            constraint_value.configuration_constraint_setting(),
            Some("@platforms//os:os")
        );

        let alias = Rule {
            name: "linux_alias".to_string(),
            rule_type: "alias".to_string(),
            attributes: HashMap::from([
                (
                    "name".to_string(),
                    ParseValue::String("linux_alias".to_string()),
                ),
                (
                    "actual".to_string(),
                    ParseValue::String("@p//os:linux".to_string()),
                ),
            ]),
        };
        let alias = parse_rule_to_instance_with_context(&alias, &registry, &context).unwrap();
        assert_eq!(
            alias.dependencies(),
            ["@platforms//os:linux".to_string()]
        );
    }

    #[test]
    fn multiple_matching_conditions_fail_deterministically_in_both_bridges() {
        let first = "//cfg:first";
        let second = "//cfg:second";
        let mut platform = PlatformConfig::new();
        platform.set_condition(first.to_string(), true);
        platform.set_condition(second.to_string(), true);

        let rule_value = RuleValue::Select(HashMap::from([
            (first.to_string(), RuleValue::String("first".to_string())),
            (second.to_string(), RuleValue::String("second".to_string())),
        ]));
        let rule_error = rule_value_to_rush_value_with_platform(
            &rule_value,
            &platform,
            SelectLabelContext::default(),
        )
        .expect_err("ambiguous RuleValue select must fail");

        let parse_value = ParseValue::Select(HashMap::from([
            (first.to_string(), ParseValue::String("first".to_string())),
            (second.to_string(), ParseValue::String("second".to_string())),
        ]));
        let parse_error = parse_value_to_rush_value_with_platform(
            &parse_value,
            &platform,
            SelectLabelContext::default(),
        )
        .expect_err("ambiguous ParseValue select must fail");

        let expected = "//cfg:first, //cfg:second";
        assert!(rule_error.to_string().contains("Illegal ambiguous match"));
        assert!(parse_error.to_string().contains("Illegal ambiguous match"));
        assert!(rule_error.to_string().contains(expected));
        assert!(parse_error.to_string().contains(expected));

        let rule_same = RuleValue::Select(HashMap::from([
            (first.to_string(), RuleValue::String("shared".to_string())),
            (second.to_string(), RuleValue::String("shared".to_string())),
        ]));
        let parse_same = ParseValue::Select(HashMap::from([
            (first.to_string(), ParseValue::String("shared".to_string())),
            (second.to_string(), ParseValue::String("shared".to_string())),
        ]));
        assert_eq!(
            rule_value_to_rush_value_with_platform(
                &rule_same,
                &platform,
                SelectLabelContext::default(),
            )
            .unwrap(),
            RushRuleValue::String("shared".to_string())
        );
        assert_eq!(
            parse_value_to_rush_value_with_platform(
                &parse_same,
                &platform,
                SelectLabelContext::default(),
            )
            .unwrap(),
            RushRuleValue::String("shared".to_string())
        );

        // Rush intentionally compares branch values before attribute-schema
        // label conversion. Bazel treats these two list(label) spellings as
        // equal, but Rush must fail closed until typed attribute conversion is
        // available here; never normalize arbitrary strings globally.
        let rule_typed_equivalent = RuleValue::Select(HashMap::from([
            (
                first.to_string(),
                RuleValue::List(vec![RuleValue::String(":canonical_target".to_string())]),
            ),
            (
                second.to_string(),
                RuleValue::List(vec![RuleValue::String(
                    "//pkg:canonical_target".to_string(),
                )]),
            ),
        ]));
        let rule_typed_error = rule_value_to_rush_value_with_platform(
            &rule_typed_equivalent,
            &platform,
            SelectLabelContext::default(),
        )
        .expect_err("raw-different RuleValue label lists must fail closed");

        let parse_typed_equivalent = ParseValue::Select(HashMap::from([
            (
                first.to_string(),
                ParseValue::List(vec![ParseValue::String(":canonical_target".to_string())]),
            ),
            (
                second.to_string(),
                ParseValue::List(vec![ParseValue::String(
                    "//pkg:canonical_target".to_string(),
                )]),
            ),
        ]));
        let parse_typed_error = parse_value_to_rush_value_with_platform(
            &parse_typed_equivalent,
            &platform,
            SelectLabelContext::default(),
        )
        .expect_err("raw-different ParseValue label lists must fail closed");
        for error in [rule_typed_error, parse_typed_error] {
            let message = error.to_string();
            assert!(message.contains("Illegal ambiguous match"), "{message}");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn duplicate_canonical_defaults_fail_closed_in_both_bridges() {
        let repository_mapping = HashMap::from([("root".to_string(), String::new())]);
        let context = SelectLabelContext {
            repository_mapping: Some(&repository_mapping),
            ..SelectLabelContext::default()
        };
        let platform = PlatformConfig::new();
        let spellings = [
            "//conditions:default",
            "@//conditions:default",
            "@root//conditions:default",
        ];

        let evaluate_order = |order: [usize; 3]| {
            let mut rule_branches = HashMap::new();
            let mut parse_branches = HashMap::new();
            for index in order {
                let value = format!("branch-{index}");
                rule_branches.insert(
                    spellings[index].to_string(),
                    RuleValue::String(value.clone()),
                );
                parse_branches.insert(
                    spellings[index].to_string(),
                    ParseValue::String(value),
                );
            }
            let rule_error = evaluate_rule_value_select(
                &rule_branches,
                &platform,
                context,
            )
            .expect_err("different canonical-default branches must fail closed");
            let parse_error = evaluate_parse_value_select(
                &parse_branches,
                &platform,
                context,
            )
            .expect_err("different canonical-default branches must fail closed");
            (rule_error.to_string(), parse_error.to_string())
        };

        let forward = evaluate_order([0, 1, 2]);
        let reversed = evaluate_order([2, 1, 0]);
        assert_eq!(forward, reversed);
        for message in [&forward.0, &forward.1] {
            assert!(message.contains("canonically equivalent default"), "{message}");
            assert!(
                message.contains(
                    "//conditions:default, @//conditions:default, @root//conditions:default"
                ),
                "{message}"
            );
        }

        let rule_equal = HashMap::from([
            (
                spellings[0].to_string(),
                RuleValue::String("shared".to_string()),
            ),
            (
                spellings[1].to_string(),
                RuleValue::String("shared".to_string()),
            ),
            (
                spellings[2].to_string(),
                RuleValue::String("shared".to_string()),
            ),
        ]);
        let parse_equal = HashMap::from([
            (
                spellings[0].to_string(),
                ParseValue::String("shared".to_string()),
            ),
            (
                spellings[1].to_string(),
                ParseValue::String("shared".to_string()),
            ),
            (
                spellings[2].to_string(),
                ParseValue::String("shared".to_string()),
            ),
        ]);
        assert_eq!(
            evaluate_rule_value_select(&rule_equal, &platform, context).unwrap(),
            RuleValue::String("shared".to_string())
        );
        assert!(matches!(
            evaluate_parse_value_select(&parse_equal, &platform, context).unwrap(),
            ParseValue::String(value) if value == "shared"
        ));
    }

    #[test]
    fn duplicate_canonical_nondefaults_fail_closed_in_both_bridges() {
        let context = SelectLabelContext {
            current_package: Some("pkg"),
            ..SelectLabelContext::default()
        };
        let mut platform = PlatformConfig::new();
        platform.set_condition("//pkg:on".to_string(), true);
        let spellings = [":on", "//pkg:on"];

        let evaluate_order = |order: [usize; 2]| {
            let mut rule_branches = HashMap::new();
            let mut parse_branches = HashMap::new();
            for index in order {
                let value = format!("branch-{index}");
                rule_branches.insert(
                    spellings[index].to_string(),
                    RuleValue::String(value.clone()),
                );
                parse_branches.insert(
                    spellings[index].to_string(),
                    ParseValue::String(value),
                );
            }
            (
                evaluate_rule_value_select(&rule_branches, &platform, context)
                    .expect_err("different equivalent RuleValue keys must fail closed")
                    .to_string(),
                evaluate_parse_value_select(&parse_branches, &platform, context)
                    .expect_err("different equivalent ParseValue keys must fail closed")
                    .to_string(),
            )
        };

        let forward = evaluate_order([0, 1]);
        let reversed = evaluate_order([1, 0]);
        assert_eq!(forward, reversed);
        for message in [&forward.0, &forward.1] {
            assert!(message.contains("canonically equivalent conditions"), "{message}");
            assert!(message.contains("//pkg:on, :on"), "{message}");
        }

        let rule_equal = HashMap::from([
            (
                spellings[0].to_string(),
                RuleValue::String("shared".to_string()),
            ),
            (
                spellings[1].to_string(),
                RuleValue::String("shared".to_string()),
            ),
        ]);
        let parse_equal = HashMap::from([
            (
                spellings[0].to_string(),
                ParseValue::String("shared".to_string()),
            ),
            (
                spellings[1].to_string(),
                ParseValue::String("shared".to_string()),
            ),
        ]);
        assert_eq!(
            evaluate_rule_value_select(&rule_equal, &platform, context).unwrap(),
            RuleValue::String("shared".to_string())
        );
        assert!(matches!(
            evaluate_parse_value_select(&parse_equal, &platform, context).unwrap(),
            ParseValue::String(value) if value == "shared"
        ));
    }

    #[test]
    fn strict_predicate_superset_wins_in_both_bridges() {
        rush_starlark::config_defines::with_configuration_scope(
            "rule-execution-specialization-test",
            || {
                rush_starlark::config_defines::set_define(
                    "base".to_string(),
                    "1".to_string(),
                );
                rush_starlark::config_defines::set_define(
                    "mode".to_string(),
                    "1".to_string(),
                );
                rush_starlark::config_defines::consider_config_setting(
                    "cfg",
                    "base",
                    &HashMap::from([("base".to_string(), "1".to_string())]),
                );
                rush_starlark::config_defines::consider_config_setting(
                    "cfg",
                    "specialized",
                    &HashMap::from([
                        ("base".to_string(), "1".to_string()),
                        ("mode".to_string(), "1".to_string()),
                    ]),
                );
                let platform = PlatformConfig::new();
                let rule_value = RuleValue::Select(HashMap::from([
                    (
                        "//cfg:base".to_string(),
                        RuleValue::String("base".to_string()),
                    ),
                    (
                        "//cfg:specialized".to_string(),
                        RuleValue::String("specialized".to_string()),
                    ),
                ]));
                let parse_value = ParseValue::Select(HashMap::from([
                    (
                        "//cfg:base".to_string(),
                        ParseValue::String("base".to_string()),
                    ),
                    (
                        "//cfg:specialized".to_string(),
                        ParseValue::String("specialized".to_string()),
                    ),
                ]));
                assert_eq!(
                    rule_value_to_rush_value_with_platform(
                        &rule_value,
                        &platform,
                        SelectLabelContext::default(),
                    )
                    .unwrap(),
                    RushRuleValue::String("specialized".to_string())
                );
                assert_eq!(
                    parse_value_to_rush_value_with_platform(
                        &parse_value,
                        &platform,
                        SelectLabelContext::default(),
                    )
                    .unwrap(),
                    RushRuleValue::String("specialized".to_string())
                );
            },
        );
    }

    #[test]
    fn config_settings_outrank_direct_constraint_values_in_both_bridges() {
        rush_starlark::config_defines::with_configuration_scope(
            "rule-execution-condition-kind-priority-test",
            || {
                use rush_starlark::config_defines::{
                    consider_config_alias, consider_config_setting_predicates,
                    consider_constraint_value, set_define,
                };

                consider_constraint_value(None, "cv", "linux");
                consider_constraint_value(None, "cv", "x86");
                set_define("mode".to_string(), "on".to_string());
                consider_config_setting_predicates(
                    None,
                    "cfg",
                    "equal",
                    &HashMap::new(),
                    &HashMap::new(),
                    &["//cv:linux".to_string()],
                    &HashMap::new(),
                );
                consider_config_setting_predicates(
                    None,
                    "cfg",
                    "incomparable",
                    &HashMap::from([("mode".to_string(), "on".to_string())]),
                    &HashMap::new(),
                    &[],
                    &HashMap::new(),
                );
                consider_config_setting_predicates(
                    None,
                    "cfg",
                    "strict",
                    &HashMap::from([("mode".to_string(), "on".to_string())]),
                    &HashMap::new(),
                    &["//cv:linux".to_string()],
                    &HashMap::new(),
                );
                consider_config_alias(
                    None,
                    "cfg",
                    "setting_alias",
                    "//cfg:incomparable",
                );
                consider_config_alias(
                    None,
                    "cfg",
                    "constraint_alias",
                    "//cv:linux",
                );

                let mut platform = PlatformConfig::new();
                for condition in [
                    "//cv:linux",
                    "//cv:x86",
                    "//cfg:equal",
                    "//cfg:incomparable",
                    "//cfg:strict",
                    "//cfg:setting_alias",
                    "//cfg:constraint_alias",
                ] {
                    platform.set_condition(condition.to_string(), true);
                }

                let assert_setting_wins = |setting: &str, constraint: &str| {
                    let rule_value = RuleValue::Select(HashMap::from([
                        (
                            constraint.to_string(),
                            RuleValue::String("constraint".to_string()),
                        ),
                        (
                            setting.to_string(),
                            RuleValue::String("setting".to_string()),
                        ),
                    ]));
                    assert_eq!(
                        rule_value_to_rush_value_with_platform(
                            &rule_value,
                            &platform,
                            SelectLabelContext::default(),
                        )
                        .unwrap(),
                        RushRuleValue::String("setting".to_string())
                    );

                    let parse_value = ParseValue::Select(HashMap::from([
                        (
                            constraint.to_string(),
                            ParseValue::String("constraint".to_string()),
                        ),
                        (
                            setting.to_string(),
                            ParseValue::String("setting".to_string()),
                        ),
                    ]));
                    assert_eq!(
                        parse_value_to_rush_value_with_platform(
                            &parse_value,
                            &platform,
                            SelectLabelContext::default(),
                        )
                        .unwrap(),
                        RushRuleValue::String("setting".to_string())
                    );
                };

                // Equal, strict-superset, and incomparable predicate sets all
                // obey Bazel's config_setting-over-constraint_value priority.
                assert_setting_wins("//cfg:equal", "//cv:linux");
                assert_setting_wins("//cfg:strict", "//cv:linux");
                assert_setting_wins("//cfg:incomparable", "//cv:linux");
                // Alias chains inherit their terminal condition kind.
                assert_setting_wins("//cfg:setting_alias", "//cv:linux");
                assert_setting_wins("//cfg:incomparable", "//cfg:constraint_alias");

                let rule_constraints = RuleValue::Select(HashMap::from([
                    (
                        "//cv:linux".to_string(),
                        RuleValue::String("linux".to_string()),
                    ),
                    (
                        "//cv:x86".to_string(),
                        RuleValue::String("x86".to_string()),
                    ),
                ]));
                let parse_constraints = ParseValue::Select(HashMap::from([
                    (
                        "//cv:linux".to_string(),
                        ParseValue::String("linux".to_string()),
                    ),
                    (
                        "//cv:x86".to_string(),
                        ParseValue::String("x86".to_string()),
                    ),
                ]));
                for error in [
                    rule_value_to_rush_value_with_platform(
                        &rule_constraints,
                        &platform,
                        SelectLabelContext::default(),
                    )
                    .expect_err("two direct constraints remain ambiguous"),
                    parse_value_to_rush_value_with_platform(
                        &parse_constraints,
                        &platform,
                        SelectLabelContext::default(),
                    )
                    .expect_err("two direct constraints remain ambiguous"),
                ] {
                    assert!(
                        error.to_string().contains("Illegal ambiguous match"),
                        "{error:#}"
                    );
                }
            },
        );
    }

    #[test]
    fn deferred_select_uses_default_at_instantiation_time() {
        let value = ParseValue::Select(HashMap::from([
            (
                NEVER_CONDITION.to_string(),
                ParseValue::List(vec![ParseValue::String("wrong.cc".to_string())]),
            ),
            (
                "//conditions:default".to_string(),
                ParseValue::List(vec![ParseValue::String("default.cc".to_string())]),
            ),
        ]));

        let resolved = parse_value_to_rush_value(&value).unwrap();
        assert_eq!(
            resolved,
            RushRuleValue::List(vec![RushRuleValue::String("default.cc".to_string())])
        );
    }

    #[test]
    fn named_host_condition_is_not_implicit() {
        let value = ParseValue::Select(HashMap::from([
            (
                "//conditions:linux".to_string(),
                ParseValue::String("implicit".to_string()),
            ),
            (
                "//conditions:default".to_string(),
                ParseValue::String("fallback".to_string()),
            ),
        ]));

        assert_eq!(
            parse_value_to_rush_value(&value).unwrap(),
            RushRuleValue::String("fallback".to_string())
        );
    }

    #[test]
    fn declared_main_condition_spellings_share_one_identity() {
        let mut platform = PlatformConfig::new();
        platform.set_condition("//conditions:linux".to_string(), true);
        let instantiation_context = InstantiationContext {
            package: "consumer".to_string(),
            main_repository_name: Some("main_repo".to_string()),
            ..InstantiationContext::default()
        };
        let label_context = SelectLabelContext::from(&instantiation_context);

        for condition in [
            "//conditions:linux",
            "@//conditions:linux",
            "@@//conditions:linux",
            "@main_repo//conditions:linux",
        ] {
            assert!(select_condition_matches(
                &platform,
                condition,
                label_context,
            ));
        }
    }

    #[test]
    fn external_named_condition_does_not_alias_main_repository() {
        let mut platform = PlatformConfig::new();
        platform.set_condition("//conditions:linux".to_string(), true);
        let context = InstantiationContext {
            repo_name: Some("ext".to_string()),
            package: "consumer".to_string(),
            ..InstantiationContext::default()
        };
        let label_context = SelectLabelContext::from(&context);

        assert!(!select_condition_matches(
            &platform,
            "//conditions:linux",
            label_context,
        ));

        platform.set_condition("@ext//conditions:linux".to_string(), true);
        assert!(select_condition_matches(
            &platform,
            "//conditions:linux",
            label_context,
        ));

        let main_local_context = InstantiationContext {
            package: "unrelated".to_string(),
            ..InstantiationContext::default()
        };
        assert!(!select_condition_matches(
            &platform,
            ":linux",
            SelectLabelContext::from(&main_local_context),
        ));
    }

    #[test]
    fn canonical_main_default_spellings_resolve_and_leave_no_metadata_edge() {
        for condition in [
            "//conditions:default",
            "@//conditions:default",
            "@@//conditions:default",
        ] {
            let value = RuleValue::Select(HashMap::from([
                (
                    NEVER_CONDITION.to_string(),
                    RuleValue::String("wrong".to_string()),
                ),
                (
                    condition.to_string(),
                    RuleValue::String("selected".to_string()),
                ),
            ]));
            assert_eq!(
                rule_value_to_rush_value(&value).unwrap(),
                RushRuleValue::String("selected".to_string()),
                "default spelling {condition} did not resolve"
            );

            let attrs = HashMap::from([("value".to_string(), value)]);
            assert_eq!(
                rule_configuration_conditions(
                    &attrs,
                    SelectLabelContext::default(),
                ),
                vec![NEVER_CONDITION.to_string()],
                "default spelling {condition} became a dependency"
            );
        }
    }

    #[test]
    fn main_conditions_relative_default_uses_instantiation_context() {
        let value = ParseValue::Select(HashMap::from([(
            ":default".to_string(),
            ParseValue::String("selected".to_string()),
        )]));
        let instantiation_context = InstantiationContext {
            package: "conditions".to_string(),
            ..InstantiationContext::default()
        };
        let label_context = SelectLabelContext::from(&instantiation_context);

        assert_eq!(
            parse_value_to_rush_value_with_platform(
                &value,
                &PlatformConfig::new(),
                label_context,
            )
            .unwrap(),
            RushRuleValue::String("selected".to_string())
        );
        let attrs = HashMap::from([("value".to_string(), value)]);
        assert!(
            parse_configuration_conditions(&attrs, label_context).is_empty()
        );
    }

    #[test]
    fn external_conditions_default_is_not_the_builtin_default() {
        for condition in [
            "@ext//conditions:default",
            "@@ext//conditions:default",
        ] {
            let value = ParseValue::Select(HashMap::from([(
                condition.to_string(),
                ParseValue::String("external".to_string()),
            )]));
            assert!(parse_value_to_rush_value(&value).is_err());

            let attrs = HashMap::from([("value".to_string(), value)]);
            let canonical = condition
                .strip_prefix("@@")
                .map(|rest| format!("@{rest}"))
                .unwrap_or_else(|| condition.to_string());
            assert_eq!(
                parse_configuration_conditions(
                    &attrs,
                    SelectLabelContext::default(),
                ),
                vec![canonical]
            );
        }

        let relative = ParseValue::Select(HashMap::from([(
            ":default".to_string(),
            ParseValue::String("external".to_string()),
        )]));
        let instantiation_context = InstantiationContext {
            repo_name: Some("ext".to_string()),
            package: "conditions".to_string(),
            ..InstantiationContext::default()
        };
        let label_context = SelectLabelContext::from(&instantiation_context);

        assert!(
            parse_value_to_rush_value_with_platform(
                &relative,
                &PlatformConfig::new(),
                label_context,
            )
            .is_err()
        );
        let attrs = HashMap::from([("value".to_string(), relative)]);
        assert_eq!(
            parse_configuration_conditions(&attrs, label_context),
            vec!["@ext//conditions:default".to_string()]
        );
    }

    #[test]
    fn reserved_absolute_default_stays_main_from_external_repository() {
        let value = ParseValue::Select(HashMap::from([(
            "//conditions:default".to_string(),
            ParseValue::String("main".to_string()),
        )]));
        let instantiation_context = InstantiationContext {
            repo_name: Some("ext".to_string()),
            package: "conditions".to_string(),
            ..InstantiationContext::default()
        };
        let label_context = SelectLabelContext::from(&instantiation_context);

        assert_eq!(
            parse_value_to_rush_value_with_platform(
                &value,
                &PlatformConfig::new(),
                label_context,
            )
            .unwrap(),
            RushRuleValue::String("main".to_string())
        );
        let attrs = HashMap::from([("value".to_string(), value)]);
        assert!(
            parse_configuration_conditions(&attrs, label_context).is_empty()
        );
    }

    #[test]
    fn main_owner_repository_name_canonicalizes_named_default() {
        let condition = "@main_repo//conditions:default";
        let value = ParseValue::Select(HashMap::from([(
            condition.to_string(),
            ParseValue::String("main".to_string()),
        )]));
        let instantiation_context = InstantiationContext {
            package: "consumer".to_string(),
            main_repository_name: Some("main_repo".to_string()),
            ..InstantiationContext::default()
        };
        let label_context = SelectLabelContext::from(&instantiation_context);

        assert_eq!(
            parse_value_to_rush_value_with_platform(
                &value,
                &PlatformConfig::new(),
                label_context,
            )
            .unwrap(),
            RushRuleValue::String("main".to_string())
        );
        let attrs = HashMap::from([("value".to_string(), value)]);
        assert!(
            parse_configuration_conditions(&attrs, label_context).is_empty(),
            "main repository spelling {condition} became a dependency"
        );

        let canonical_external = "@@main_repo//conditions:default";
        assert!(!is_default_select_condition(
            canonical_external,
            label_context,
        ));
    }

    #[test]
    fn external_owner_mapping_shadows_the_main_repository_name() {
        let mapped = InstantiationContext {
            repo_name: Some("consumer+".to_string()),
            package: "consumer".to_string(),
            main_repository_name: Some("root".to_string()),
            repository_mapping: HashMap::from([(
                "root".to_string(),
                "cfg+".to_string(),
            )]),
            ..InstantiationContext::default()
        };
        let context = SelectLabelContext::from(&mapped);
        assert_eq!(
            canonical_select_condition("@root//:enabled", context),
            "@cfg+//:enabled"
        );
        assert_eq!(
            canonical_select_condition("@root//conditions:default", context),
            "@cfg+//conditions:default"
        );
        assert!(!is_default_select_condition(
            "@root//conditions:default",
            context
        ));

        let unknown = InstantiationContext {
            repo_name: Some("consumer+".to_string()),
            package: "consumer".to_string(),
            main_repository_name: Some("root".to_string()),
            ..InstantiationContext::default()
        };
        let unknown_context = SelectLabelContext::from(&unknown);
        assert_eq!(
            canonical_select_condition("@root//:enabled", unknown_context),
            "@root//:enabled",
            "an external owner must preserve an unknown apparent name for metadata preload"
        );
        assert!(!is_default_select_condition(
            "@root//conditions:default",
            unknown_context
        ));

        assert_eq!(
            canonical_select_condition("@@//:enabled", unknown_context),
            "//:enabled",
            "select evaluation uses the canonical main-repository identity"
        );
        assert_eq!(
            canonical_select_metadata_condition("@@//:enabled", unknown_context),
            "@//:enabled",
            "rule metadata must retain an explicit main-repository marker"
        );
        assert_eq!(
            canonical_select_metadata_condition("//:enabled", unknown_context),
            "@consumer+//:enabled",
            "an ordinary absolute label remains relative to its external owner"
        );

        let mapped_main = InstantiationContext {
            repository_mapping: HashMap::from([("root".to_string(), String::new())]),
            ..unknown
        };
        let mapped_main_context = SelectLabelContext::from(&mapped_main);
        assert!(is_default_select_condition(
            "@root//conditions:default",
            mapped_main_context
        ));
        assert_eq!(
            canonical_select_metadata_condition("@root//:enabled", mapped_main_context),
            "@//:enabled",
            "an apparent repository mapped to main must retain root provenance"
        );

        let canonical_select = ParseValue::Select(HashMap::from([(
            "@@platforms//os:linux".to_string(),
            ParseValue::List(Vec::new()),
        )]));
        let mut raw_conditions = Vec::new();
        collect_raw_parse_value_conditions(
            &canonical_select,
            &mut raw_conditions,
            context,
        );
        assert_eq!(raw_conditions, vec!["@@platforms//os:linux"]);
        let mut matching_conditions = Vec::new();
        collect_parse_value_conditions(
            &canonical_select,
            &mut matching_conditions,
            context,
        );
        assert_eq!(matching_conditions, vec!["@platforms//os:linux"]);
    }

    #[test]
    fn compact_configurable_add_resolves_in_both_bridges() {
        let rule_value = RuleValue::Configurable {
            op: ConfigurableOp::Add,
            left: Box::new(RuleValue::List(vec![RuleValue::String(
                "base.cc".to_string(),
            )])),
            right: Box::new(RuleValue::Select(HashMap::from([(
                "//conditions:default".to_string(),
                RuleValue::List(vec![RuleValue::String("selected.cc".to_string())]),
            )]))),
        };
        let parse_value = ParseValue::Configurable {
            op: ConfigurableOp::Add,
            left: Box::new(ParseValue::List(vec![ParseValue::String(
                "base.cc".to_string(),
            )])),
            right: Box::new(ParseValue::Select(HashMap::from([(
                "//conditions:default".to_string(),
                ParseValue::List(vec![ParseValue::String("selected.cc".to_string())]),
            )]))),
        };
        let expected = RushRuleValue::List(vec![
            RushRuleValue::String("base.cc".to_string()),
            RushRuleValue::String("selected.cc".to_string()),
        ]);

        assert_eq!(rule_value_to_rush_value(&rule_value).unwrap(), expected);
        assert_eq!(parse_value_to_rush_value(&parse_value).unwrap(), expected);
    }

    #[tokio::test]
    async fn full_parse_bridge_preserves_native_configurable_semantics() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("WORKSPACE"), "").unwrap();
        let build_path = workspace.path().join("BUILD");
        std::fs::write(
            &build_path,
            r#"
cc_library(
    name = "native_configurable",
    integer_value = select({
        "//conditions:rush_select_test_never": 99,
        "//conditions:default": 1,
    }) + 2,
    dictionary_value = {"left": "one"} | select({
        "//conditions:rush_select_test_never": {"wrong": "branch"},
        "//conditions:default": {"right": "two"},
    }),
)
"#,
        )
        .unwrap();

        let orchestrator = crate::BuildOrchestrator::with_workspace(workspace.path().to_path_buf());
        orchestrator.ensure_tracked_io();
        let build_file = orchestrator
            .with_tracked_scope(|| rush_parse::parse_build_file_with_starlark(&build_path))
            .unwrap();
        let rule = build_file
            .rules
            .iter()
            .find(|rule| rule.name == "native_configurable")
            .unwrap();

        assert_eq!(
            parse_value_to_rush_value(rule.attributes.get("integer_value").unwrap()).unwrap(),
            RushRuleValue::Int(3)
        );
        assert_eq!(
            parse_value_to_rush_value(rule.attributes.get("dictionary_value").unwrap()).unwrap(),
            RushRuleValue::Dict(HashMap::from([
                (
                    "left".to_string(),
                    RushRuleValue::String("one".to_string())
                ),
                (
                    "right".to_string(),
                    RushRuleValue::String("two".to_string())
                ),
            ]))
        );
    }

    #[test]
    fn long_configurable_add_chain_resolves_without_branch_expansion() {
        let mut value = RuleValue::List(Vec::new());
        for index in 0..64 {
            value = RuleValue::Configurable {
                op: ConfigurableOp::Add,
                left: Box::new(value),
                right: Box::new(RuleValue::Select(HashMap::from([
                    (
                        NEVER_CONDITION.to_string(),
                        RuleValue::List(vec![RuleValue::String(format!(
                            "wrong-feature-{index}"
                        ))]),
                    ),
                    (
                        "//conditions:default".to_string(),
                        RuleValue::List(vec![RuleValue::String(format!("feature-{index}"))]),
                    ),
                ]))),
            };
        }

        let resolved = rule_value_to_rush_value(&value).unwrap();
        let RushRuleValue::List(values) = resolved else {
            panic!("configurable list chain must resolve to a list");
        };
        assert_eq!(values.len(), 64);
        assert_eq!(
            values.first(),
            Some(&RushRuleValue::String("feature-0".to_string()))
        );
        assert_eq!(
            values.last(),
            Some(&RushRuleValue::String("feature-63".to_string()))
        );
    }

    #[tokio::test]
    async fn requested_target_reports_deferred_unmatched_select() {
        let registry = cc_registry();
        let rule = cc_rule(
            "bad",
            unmatched_parse_select(ParseValue::List(vec![ParseValue::String(
                "conditional.cc".to_string(),
            )])),
        );
        let instance = parse_rule_to_instance(&rule, &registry).unwrap();
        let configuration_error = instance
            .configuration_error()
            .expect("requested target must retain its configuration error");
        assert!(configuration_error.contains("resolving attribute 'srcs'"));
        assert!(configuration_error.contains("No matching condition in select()"));
        let context = BuildContext::new(
            std::path::PathBuf::from("/tmp/rush-select-test-workspace"),
            std::path::PathBuf::from("/tmp/rush-select-test-output"),
        );

        let error = instance.build(&context).await.unwrap_err().to_string();
        assert!(error.contains("target 'bad' cannot be configured"));
        assert!(error.contains("No matching condition in select()"));
    }

    #[test]
    fn unmatched_select_on_unused_sibling_does_not_fail_package() {
        let registry = cc_registry();
        let build_file = BuildFile {
            path: std::path::PathBuf::from("/tmp/rush-select-test/BUILD"),
            rules: vec![
                cc_rule(
                    "unused_bad",
                    unmatched_parse_select(ParseValue::List(vec![ParseValue::String(
                        "conditional.cc".to_string(),
                    )])),
                ),
                cc_rule("requested_good", ParseValue::List(vec![])),
            ],
        };

        let instances = build_file_to_instances(&build_file, &registry).unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].target_name(), "unused_bad");
        assert!(instances[0].configuration_error().is_some());
        assert_eq!(instances[1].target_name(), "requested_good");
        assert_eq!(instances[1].rule_type(), "cc_library");
        assert!(instances[1].configuration_error().is_none());
    }

    #[test]
    fn known_unsupported_java_sibling_defers_failure_until_selected() {
        let mut registry = cc_registry();
        for java_rule in crate::rules_java::UnsupportedJavaRule::all() {
            registry.register(Box::new(java_rule));
        }
        let build_file = BuildFile {
            path: std::path::PathBuf::from("/tmp/rush-java-sibling/BUILD"),
            rules: vec![
                Rule {
                    name: "unused_java".to_string(),
                    rule_type: "java_plugin".to_string(),
                    attributes: HashMap::from([
                        (
                            "name".to_string(),
                            ParseValue::String("unused_java".to_string()),
                        ),
                        (
                            "deps".to_string(),
                            ParseValue::List(vec![ParseValue::String(":proto".to_string())]),
                        ),
                    ]),
                },
                cc_rule("requested_cpp", ParseValue::List(vec![])),
            ],
        };

        let instances = build_file_to_instances(&build_file, &registry).unwrap();
        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].target_name(), "unused_java");
        let error = instances[0]
            .configuration_error()
            .expect("selecting the Java sibling must fail");
        assert!(error.contains("annotation-processor toolchain"));
        assert_eq!(instances[0].dependencies(), vec![":proto"]);
        assert_eq!(instances[1].target_name(), "requested_cpp");
        assert!(instances[1].configuration_error().is_none());
    }

    #[tokio::test]
    async fn incompatible_target_does_not_resolve_other_selects() {
        let registry = cc_registry();
        let mut rule = cc_rule(
            "incompatible",
            unmatched_parse_select(ParseValue::List(vec![ParseValue::String(
                "conditional.cc".to_string(),
            )])),
        );
        rule.attributes.insert(
            "target_compatible_with".to_string(),
            ParseValue::List(vec![ParseValue::String(
                "@platforms//:incompatible".to_string(),
            )]),
        );
        let instance = parse_rule_to_instance(&rule, &registry).unwrap();
        assert!(instance.configuration_error().is_none());
        let context = BuildContext::new(
            std::path::PathBuf::from("/tmp/rush-select-test-workspace"),
            std::path::PathBuf::from("/tmp/rush-select-test-output"),
        );

        assert!(instance.build(&context).await.is_ok());
    }
}
