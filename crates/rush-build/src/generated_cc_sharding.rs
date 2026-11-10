use anyhow::{anyhow, bail, Context, Result};
use rush_action::Action;
use std::path::{Path, PathBuf};

const SHARDABLE_SOURCES: [&str; 2] = ["math_ops.cc", "array_ops.cc"];
const GENERATED_CC_SHARDS_ENV: &str = "RUSH_TF_GENERATED_CC_SHARDS";
const CXX_ENV: &str = "RUSH_CXX";
const CRITICAL_CXX_ENV: &str = "RUSH_TF_CRITICAL_CXX";
const META_OPTIMIZER_SOURCE_SUFFIX: &str =
    "/tensorflow/core/grappler/optimizers/meta_optimizer.cc";
const META_OPTIMIZER_SHARDS_ENV: &str = "RUSH_TF_META_OPT_SHARDS";
const META_OPTIMIZER_CPU_SLOTS_MARKER: &str =
    "# rush-tf-meta-optimizer-cpu-slots=";
// Relative code-generation costs for the 20 validated definitions in
// meta_optimizer.cc. Isolated optimized object sizes provide a substantially
// better partitioning proxy than source bytes for this template-heavy file.
const META_OPTIMIZER_DEFINITION_COSTS: [usize; 20] = [
    3_368, 34_608, 31_584, 65_360, 17_544, 12_840, 6_704, 3_176, 5_848,
    39_752, 51_528, 14_456, 80_704, 41_104, 152_664, 4_832, 5_568, 6_576,
    30_760, 36_592,
];
const ARG_RET_PLACEMENT_SOURCE_SUFFIX: &str =
    "/tensorflow/core/common_runtime/arg_ret_placement.cc";
const BATCH_UTIL_SOURCE_SUFFIX: &str = "/tensorflow/core/util/batch_util.cc";
const BATCH_UTIL_RANK_SHARDS_ENV: &str = "RUSH_TF_BATCH_UTIL_RANK_SHARDS";
const TF_IR_OP_SHARDS_ENV: &str = "RUSH_TF_IR_OP_SHARDS";
const TF_IR_OP_SOURCE_SUFFIX: &str = "tensorflow/core/ir/ops.cc";
const TF_IR_OP_GENERATED_RELATIVE: &str =
    "rush-out/genfiles/tensorflow/core/ir/ops.cc.inc";
const TF_IR_OP_CPU_SLOTS_MARKER: &str = "# rush-tf-ir-ops-cpu-slots=";

// Profile-derived byte-work estimate for TensorFlow's repeated clean-build
// tail. Raw source bytes substantially underprice this header-heavy
// translation unit, which leaves its generated-header producer chain late in
// the target frontier. A 96-CPU clean-build sweep selected the lowest cost that
// consistently cleared the action from the tail without increasing compiler
// contention. It affects ready-target order only and never alters action
// dependencies or commands.
const PROFILED_COMPILE_PRIORITY_COSTS: [(&str, u64); 1] = [
    (
        "/tensorflow/core/grappler/optimizers/tfg_optimizer_hook.cc",
        96_000,
    ),
];
const TFG_OPTIMIZER_PRIORITY_COST_ENV: &str =
    "RUSH_TF_TFG_OPT_PRIORITY_COST";

pub(crate) fn compile_priority_cost_bytes(
    source: &Path,
    source_bytes: u64,
) -> u64 {
    let configured = std::env::var(TFG_OPTIMIZER_PRIORITY_COST_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    compile_priority_cost_bytes_with(source, source_bytes, configured)
}

fn compile_priority_cost_bytes_with(
    source: &Path,
    source_bytes: u64,
    configured: Option<u64>,
) -> u64 {
    let source = source.to_string_lossy();
    PROFILED_COMPILE_PRIORITY_COSTS
        .iter()
        .find_map(|(suffix, cost)| source.ends_with(suffix).then_some(*cost))
        .map_or(source_bytes, |cost| {
            source_bytes.max(configured.unwrap_or(cost))
        })
}

/// Reserve capacity around TensorFlow's template-heavy IR operation compile.
///
/// On SMT hosts this single action remains on the generator critical path while
/// dozens of neighboring compilers stretch its wall time. One sixth of the
/// detected capacity reduced that contention on the 96-CPU profile host; the
/// cap keeps the estimate proportional on smaller and larger machines. The
/// environment override supports profiling or an explicit opt-out with `1`.
pub(crate) fn critical_compile_cpu_slots(
    argv: &[String],
    hardware_cpu_slots: usize,
) -> Option<usize> {
    if !cfg!(target_os = "linux") {
        return None;
    }

    let hardware_cpu_slots = hardware_cpu_slots.max(1);
    if argv
        .iter()
        .any(|arg| arg.ends_with(ARG_RET_PLACEMENT_SOURCE_SUFFIX))
    {
        return std::env::var("RUSH_TF_ARG_RET_CPU_SLOTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map(|configured| configured.clamp(1, hardware_cpu_slots));
    }
    if let Some(shard_slots) = argv.iter().find_map(|arg| {
        arg.lines().find_map(|line| {
            line.strip_prefix(META_OPTIMIZER_CPU_SLOTS_MARKER)
                .and_then(|value| value.parse::<usize>().ok())
        })
    }) {
        return Some(shard_slots.clamp(1, hardware_cpu_slots));
    }
    if let Some(shard_slots) = argv.iter().find_map(|arg| {
        arg.lines().find_map(|line| {
            line.strip_prefix(TF_IR_OP_CPU_SLOTS_MARKER)
                .and_then(|value| value.parse::<usize>().ok())
        })
    }) {
        return Some(shard_slots.clamp(1, hardware_cpu_slots));
    }
    if !argv
        .iter()
        .any(|arg| arg.ends_with("/tensorflow/core/ir/ops.cc"))
    {
        return None;
    }

    let automatic = (hardware_cpu_slots / 4).clamp(1, 24);
    let configured = std::env::var("RUSH_HEAVY_CPP_CPU_SLOTS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    Some(
        configured
            .unwrap_or(automatic)
            .clamp(1, hardware_cpu_slots),
    )
}

/// Apply native compiler selection without rewriting sources or argv.
/// Diagnostic direct compilation must reach its depfile boundary as one TU;
/// source sharding remains a separate ordinary-native preparation policy.
pub(crate) fn prepare_compiler_overrides(action: &mut Action) -> Result<bool> {
    if !cfg!(target_os = "linux") || action.mnemonic != "CppCompile" {
        return Ok(false);
    }
    let ordinary = override_cpp_compiler(action)?;
    let critical = override_critical_compiler(action)?;
    Ok(ordinary || critical)
}

pub(crate) fn prepare(action: &mut Action) -> Result<bool> {
    if !cfg!(target_os = "linux") || action.mnemonic != "CppCompile" {
        return Ok(false);
    }

    let compiler_overridden = prepare_compiler_overrides(action)?;
    if prepare_targeted_command(
        &mut action.command.executable,
        &mut action.command.args,
    )? {
        action.id = Action::compute_id(&action.inputs, &action.command, &action.env)?;
        return Ok(true);
    }
    let source_index = action
        .command
        .args
        .iter()
        .position(|arg| is_shardable_source(arg));
    let Some(source_index) = source_index else {
        return Ok(compiler_overridden);
    };
    let output_index = action
        .command
        .args
        .iter()
        .position(|arg| arg == "-o")
        .and_then(|index| index.checked_add(1))
        .filter(|index| *index < action.command.args.len())
        .ok_or_else(|| anyhow!("shardable C++ action has no -o output"))?;

    let source = PathBuf::from(&action.command.args[source_index]);
    let contents = std::fs::read_to_string(&source)
        .with_context(|| format!("reading generated C++ source {}", source.display()))?;
    let shard_count = std::env::var(GENERATED_CC_SHARDS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2)
        .clamp(2, 8);
    let shards = split_generated_source(&contents, shard_count)?;
    shard_command(
        &mut action.command.executable,
        &mut action.command.args,
        source_index,
        output_index,
        &shards,
    )?;
    action.id = Action::compute_id(&action.inputs, &action.command, &action.env)?;
    Ok(true)
}

pub(crate) fn prepare_scheduler_request(
    request: &mut rush_exec::local_action_runner::ExecutionRequest,
) -> Result<bool> {
    if !cfg!(target_os = "linux") || request.mnemonic != "CppCompile" {
        return Ok(false);
    }
    prepare_targeted_command(
        &mut request.command.executable,
        &mut request.command.argv,
    )
}

/// Optionally use a caller-provided compiler for every C++ compile action.
///
/// This is separate from `RUSH_TF_CRITICAL_CXX`: the global override selects
/// the build's C++ toolchain, while the critical override can still replace
/// selected TensorFlow tail actions with a specialized wrapper.
fn override_cpp_compiler(action: &mut Action) -> Result<bool> {
    let Some(compiler) = std::env::var_os(CXX_ENV).filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    if !action.command.args.iter().any(|arg| {
        matches!(
            Path::new(arg).extension().and_then(|extension| extension.to_str()),
            Some("C" | "cc" | "cpp" | "cxx" | "c++")
        )
    }) {
        return Ok(false);
    }

    action.command.executable = PathBuf::from(compiler);
    action.id = Action::compute_id(&action.inputs, &action.command, &action.env)?;
    Ok(true)
}

/// Optionally use a caller-provided compiler or wrapper for TensorFlow's
/// critical tail: the meta optimizer and generated C++ operation wrappers.
///
/// The override is deliberately opt-in because mixing C++ toolchains requires
/// ABI-compatible compiler flags and runtime libraries. Point the variable at
/// a self-contained wrapper when the alternate compiler needs extra setup.
fn override_critical_compiler(action: &mut Action) -> Result<bool> {
    let Some(compiler) = std::env::var_os(CRITICAL_CXX_ENV).filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    if !action
        .command
        .args
        .iter()
        .any(|arg| {
            is_meta_optimizer_source(arg)
                || is_generated_cc_op_source(arg)
                || arg.ends_with(BATCH_UTIL_SOURCE_SUFFIX)
        })
    {
        return Ok(false);
    }

    action.command.executable = PathBuf::from(compiler);
    action.id = Action::compute_id(&action.inputs, &action.command, &action.env)?;
    Ok(true)
}

fn is_shardable_source(arg: &str) -> bool {
    let path = Path::new(arg);
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    SHARDABLE_SOURCES.contains(&file_name)
        && arg.contains("/rush-out/genfiles/tensorflow/cc/ops/")
}

fn is_meta_optimizer_source(arg: &str) -> bool {
    arg.ends_with(META_OPTIMIZER_SOURCE_SUFFIX)
}

fn is_generated_cc_op_source(arg: &str) -> bool {
    arg.ends_with(".cc") && arg.contains("/rush-out/genfiles/tensorflow/cc/ops/")
}

fn is_batch_util_source(arg: &str) -> bool {
    arg.ends_with(BATCH_UTIL_SOURCE_SUFFIX)
}

fn prepare_targeted_command(
    executable: &mut PathBuf,
    args: &mut Vec<String>,
) -> Result<bool> {
    if prepare_batch_util_command(executable, args)? {
        return Ok(true);
    }
    if prepare_meta_optimizer_command(executable, args)? {
        return Ok(true);
    }
    prepare_tf_ir_ops_command(executable, args)
}

fn prepare_batch_util_command(
    executable: &mut PathBuf,
    args: &mut Vec<String>,
) -> Result<bool> {
    if std::env::var(BATCH_UTIL_RANK_SHARDS_ENV).as_deref() == Ok("0") {
        return Ok(false);
    }
    let Some(source_index) = args.iter().position(|arg| is_batch_util_source(arg)) else {
        return Ok(false);
    };
    let output_index = args
        .iter()
        .position(|arg| arg == "-o")
        .and_then(|index| index.checked_add(1))
        .filter(|index| *index < args.len())
        .ok_or_else(|| anyhow!("batch utility action has no -o output"))?;
    let source = PathBuf::from(&args[source_index]);
    let contents = std::fs::read_to_string(&source)
        .with_context(|| format!("reading batch utility source {}", source.display()))?;
    let shards = match split_batch_util_source(&contents) {
        Ok(shards) => shards,
        Err(_) => return Ok(false),
    };
    shard_command(
        executable,
        args,
        source_index,
        output_index,
        &shards,
    )?;
    Ok(true)
}

fn prepare_meta_optimizer_command(
    executable: &mut PathBuf,
    args: &mut Vec<String>,
) -> Result<bool> {
    if std::env::var(META_OPTIMIZER_SHARDS_ENV).as_deref() == Ok("0") {
        return Ok(false);
    }
    let Some(source_index) =
        args.iter().position(|arg| is_meta_optimizer_source(arg))
    else {
        return Ok(false);
    };
    let output_index = args
        .iter()
        .position(|arg| arg == "-o")
        .and_then(|index| index.checked_add(1))
        .filter(|index| *index < args.len())
        .ok_or_else(|| anyhow!("TensorFlow meta optimizer action has no -o output"))?;
    let source = PathBuf::from(&args[source_index]);
    let Ok(contents) = std::fs::read_to_string(&source) else {
        return Ok(false);
    };
    let shard_count = std::env::var(META_OPTIMIZER_SHARDS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(2, 8);
    let shards = match split_meta_optimizer_source(&contents, shard_count) {
        Ok(shards) => shards,
        Err(_) => return Ok(false),
    };
    shard_command(
        executable,
        args,
        source_index,
        output_index,
        &shards,
    )?;
    let script = args
        .get_mut(1)
        .ok_or_else(|| anyhow!("TensorFlow meta optimizer shard command has no script"))?;
    script.insert_str(
        0,
        &format!(
            "{META_OPTIMIZER_CPU_SLOTS_MARKER}{}\n",
            shards.len()
        ),
    );
    Ok(true)
}

fn prepare_tf_ir_ops_command(
    executable: &mut PathBuf,
    args: &mut Vec<String>,
) -> Result<bool> {
    if std::env::var(TF_IR_OP_SHARDS_ENV).as_deref() == Ok("0") {
        return Ok(false);
    }
    let Some(source_index) = args
        .iter()
        .position(|arg| arg.ends_with(TF_IR_OP_SOURCE_SUFFIX))
    else {
        return Ok(false);
    };
    let output_index = args
        .iter()
        .position(|arg| arg == "-o")
        .and_then(|index| index.checked_add(1))
        .filter(|index| *index < args.len())
        .ok_or_else(|| anyhow!("TensorFlow IR ops action has no -o output"))?;
    let source = PathBuf::from(&args[source_index]);
    let source_text = source.to_string_lossy();
    let Some(workspace_root) = source_text.strip_suffix(TF_IR_OP_SOURCE_SUFFIX)
    else {
        return Ok(false);
    };
    let generated_path =
        PathBuf::from(workspace_root).join(TF_IR_OP_GENERATED_RELATIVE);
    let Ok(contents) = std::fs::read_to_string(&source) else {
        return Ok(false);
    };
    let Ok(generated) = std::fs::read_to_string(&generated_path) else {
        return Ok(false);
    };
    let shard_count = std::env::var(TF_IR_OP_SHARDS_ENV)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(2, 8);
    let shards = match split_tf_ir_ops_source(&contents, &generated, shard_count)
    {
        Ok(shards) => shards,
        Err(_) => return Ok(false),
    };
    shard_command(
        executable,
        args,
        source_index,
        output_index,
        &shards,
    )?;
    let script = args
        .get_mut(1)
        .ok_or_else(|| anyhow!("TensorFlow IR shard command has no script"))?;
    script.insert_str(
        0,
        &format!("{TF_IR_OP_CPU_SLOTS_MARKER}{}\n", shards.len()),
    );
    Ok(true)
}

fn shard_command(
    executable: &mut PathBuf,
    args: &mut Vec<String>,
    source_index: usize,
    output_index: usize,
    shards: &[String],
) -> Result<()> {
    let output = PathBuf::from(&args[output_index]);
    let shard_count = shards.len();
    let shard_sources: Vec<PathBuf> = (0..shard_count)
        .map(|index| suffixed_path(&output, &format!("rush-shard{index}.cc")))
        .collect();
    let shard_objects: Vec<PathBuf> = (0..shard_count)
        .map(|index| suffixed_path(&output, &format!("rush-shard{index}.o")))
        .collect();
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating shard output directory {}", parent.display()))?;
    }
    for (path, contents) in shard_sources.iter().zip(shards.iter()) {
        std::fs::write(path, contents)
            .with_context(|| format!("writing generated C++ shard {}", path.display()))?;
    }

    let mut shard_commands = Vec::with_capacity(shard_count);
    for index in 0..shard_count {
        let mut shard_args = args.clone();
        shard_args[source_index] = shard_sources[index].display().to_string();
        shard_args[output_index] = shard_objects[index].display().to_string();
        shard_commands.push(shell_command_line(executable, &shard_args));
    }

    let mut script = String::from("# rush-generated-cc-shard-v1\nset -u\n");
    for (index, command) in shard_commands.iter().enumerate() {
        script.push_str(command);
        script.push_str(" &\n");
        script.push_str(&format!("p{index}=$!\n"));
    }
    script.push_str("status=0\n");
    for index in 0..shard_count {
        script.push_str(&format!("wait \"$p{index}\" || status=$?\n"));
    }
    script.push_str("if [ \"$status\" -ne 0 ]; then exit \"$status\"; fi\n");
    script.push_str("exec /usr/bin/ld -r -o ");
    script.push_str(&shell_quote(&output.display().to_string()));
    for object in &shard_objects {
        script.push(' ');
        script.push_str(&shell_quote(&object.display().to_string()));
    }
    script.push('\n');
    *executable = PathBuf::from("/bin/sh");
    *args = vec!["-c".to_string(), script];
    Ok(())
}

fn append_lines(output: &mut String, lines: &[&str], start: usize, end: usize) {
    for line in &lines[start..end] {
        output.push_str(line);
    }
}

fn extract_top_level_definitions(
    lines: &[&str],
    start: usize,
    end: usize,
) -> Result<Vec<String>> {
    let mut definitions = Vec::new();
    let mut cursor = start;
    while cursor < end {
        while cursor < end && lines[cursor].trim().is_empty() {
            cursor += 1;
        }
        if cursor == end {
            break;
        }

        let definition_start = cursor;
        let mut depth = 0isize;
        let mut saw_open = false;
        let mut in_block_comment = false;
        let mut complete = false;
        while cursor < end {
            let (opens, closes) =
                lexical_braces(lines[cursor], &mut in_block_comment);
            if opens > 0 {
                saw_open = true;
            }
            depth += opens as isize - closes as isize;
            if depth < 0 {
                bail!("unbalanced TensorFlow meta optimizer definition");
            }
            cursor += 1;
            if saw_open && depth == 0 {
                complete = true;
                break;
            }
        }
        if !complete || in_block_comment {
            bail!("unterminated TensorFlow meta optimizer definition");
        }
        definitions.push(lines[definition_start..cursor].concat());
    }
    Ok(definitions)
}

fn split_meta_optimizer_source(
    source: &str,
    shard_count: usize,
) -> Result<Vec<String>> {
    const FIRST_DEFINITION: &str = "bool MetaOptimizer::LowerControlFlow(";
    const PROPAGATE_FORWARD_DECLARATION: &str = concat!(
        "void PropagateTFDataAttrs(const FunctionLibraryDefinition& flib,\n",
        "                          FunctionDefLibrary& fdef_lib);\n",
    );

    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let first_definition = find_line(&lines, FIRST_DEFINITION)?;
    let footer_start = lines
        .len()
        .checked_sub(2)
        .filter(|index| {
            lines[*index].contains("namespace grappler")
                && lines[*index + 1].contains("namespace tensorflow")
        })
        .ok_or_else(|| anyhow!("TensorFlow meta optimizer has no namespace footer"))?;
    if first_definition >= footer_start {
        bail!("invalid TensorFlow meta optimizer definition layout");
    }

    let preamble = lines[..first_definition].concat();
    if !preamble.contains("#define MK_OPT") {
        bail!("TensorFlow meta optimizer has no optimizer macro");
    }
    let mut definitions =
        extract_top_level_definitions(&lines, first_definition, footer_start)?;
    if definitions.len() != 20 {
        bail!(
            "expected 20 TensorFlow meta optimizer definitions, found {}",
            definitions.len()
        );
    }
    for (index, marker) in [
        (0, "MetaOptimizer::LowerControlFlow("),
        (13, "PropagateTFDataAttrs("),
        (14, "MetaOptimizer::OptimizeConsumeItem("),
        (17, "MetaOptimizerEnabled("),
        (18, "RunMetaOptimizer("),
        (19, "OptimizeGraph("),
    ] {
        if !definitions[index].contains(marker) {
            bail!(
                "unexpected TensorFlow meta optimizer definition {index}"
            );
        }
    }

    let shard_count = shard_count.min(definitions.len());
    let mut weighted_definitions = definitions
        .drain(..)
        .enumerate()
        .map(|(index, definition)| {
            (index, META_OPTIMIZER_DEFINITION_COSTS[index], definition)
        })
        .collect::<Vec<(usize, usize, String)>>();
    weighted_definitions.sort_by(
        |(left_index, left_cost, _), (right_index, right_cost, _)| {
            right_cost
                .cmp(left_cost)
                .then_with(|| left_index.cmp(right_index))
        },
    );
    let mut shard_definitions =
        vec![Vec::<(usize, String)>::new(); shard_count];
    let mut shard_bytes = vec![0usize; shard_count];
    for (source_index, definition_cost, definition) in weighted_definitions {
        let destination = shard_bytes
            .iter()
            .enumerate()
            .min_by_key(|(_, bytes)| *bytes)
            .map(|(index, _)| index)
            .ok_or_else(|| anyhow!("no TensorFlow meta optimizer shard"))?;
        shard_bytes[destination] += definition_cost;
        shard_definitions[destination].push((source_index, definition));
    }
    for definitions in &mut shard_definitions {
        definitions.sort_by_key(|(source_index, _)| *source_index);
    }

    let footer = lines[footer_start..].concat();
    Ok(shard_definitions
        .into_iter()
        .map(|definitions| {
            let mut shard = String::from(&preamble);
            shard.push_str(PROPAGATE_FORWARD_DECLARATION);
            for (_, definition) in definitions {
                shard.push_str(&definition);
            }
            shard.push_str("\n#undef MK_OPT\n");
            shard.push_str(&footer);
            shard
        })
        .collect())
}

fn extract_braced_definition(lines: &[&str], start: usize) -> Result<String> {
    let mut output = String::new();
    let mut depth = 0isize;
    let mut saw_open = false;
    let mut in_block_comment = false;
    for line in &lines[start..] {
        output.push_str(line);
        let (opens, closes) = lexical_braces(line, &mut in_block_comment);
        if opens > 0 {
            saw_open = true;
        }
        depth += opens as isize - closes as isize;
        if depth < 0 {
            bail!("unbalanced TensorFlow IR helper definition");
        }
        if saw_open && depth == 0 {
            return Ok(output);
        }
    }
    bail!("unterminated TensorFlow IR helper definition")
}

fn find_line(lines: &[&str], prefix: &str) -> Result<usize> {
    lines
        .iter()
        .position(|line| line.trim_start().starts_with(prefix))
        .ok_or_else(|| anyhow!("missing TensorFlow IR source marker {prefix}"))
}

fn split_tf_ir_ops_source(
    source: &str,
    generated: &str,
    shard_count: usize,
) -> Result<Vec<String>> {
    const GENERATED_MARKER: &str = "// Generated definitions.";
    const OP_CLASSES_DEFINE: &str = "#define GET_OP_CLASSES";
    const OP_CLASSES_INCLUDE: &str =
        "#include \"tensorflow/core/ir/ops.cc.inc\"";

    let source_lines: Vec<&str> = source.split_inclusive('\n').collect();
    let classes_define = source_lines
        .iter()
        .position(|line| line.trim_end() == OP_CLASSES_DEFINE)
        .ok_or_else(|| anyhow!("missing TensorFlow IR op class definition"))?;
    if source_lines
        .get(classes_define + 1)
        .map(|line| line.trim_end())
        != Some(OP_CLASSES_INCLUDE)
    {
        bail!("unexpected TensorFlow IR op class include");
    }
    let mut manual = String::new();
    append_lines(&mut manual, &source_lines, 0, classes_define);
    append_lines(
        &mut manual,
        &source_lines,
        classes_define + 2,
        source_lines.len(),
    );

    let preamble_end = source_lines
        .iter()
        .position(|line| line.trim_end() == GENERATED_MARKER)
        .ok_or_else(|| anyhow!("missing TensorFlow IR generated marker"))?;
    let mut preamble = String::new();
    append_lines(&mut preamble, &source_lines, 0, preamble_end);

    let generic_start = find_line(
        &source_lines,
        "static void GenericGetAsmResultNames(",
    )?;
    let generic_definition =
        extract_braced_definition(&source_lines, generic_start)?;
    let generic_helper = format!(
        "namespace mlir {{\nnamespace tfg {{\n\
         {generic_definition}\
         }}  // namespace tfg\n}}  // namespace mlir\n"
    );

    let support_1_start =
        find_line(&source_lines, "static FailureOr<TypeRange> VerifyOperands(")?;
    let support_1_end =
        find_line(&source_lines, "LogicalResult ForOp::verifySymbolUses(")?;
    let support_2_start = find_line(
        &source_lines,
        "static LogicalResult VerifyPreservedAttrs(",
    )?;
    let support_2_end = find_line(
        &source_lines,
        "MutableOperandRange YieldOp::getMutableSuccessorOperands(",
    )?;
    let support_3_start =
        find_line(&source_lines, "static bool TerminatedByYield(")?;
    let support_3_end = find_line(
        &source_lines,
        "MutableOperandRange ConditionOp::getMutableSuccessorOperands(",
    )?;
    let support_4_start = find_line(
        &source_lines,
        "static LogicalResult VerifyLoopRegionArgs(",
    )?;
    let support_4_end =
        find_line(&source_lines, "LogicalResult ForRegionOp::verify(")?;
    if !(support_1_start < support_1_end
        && support_1_end < support_2_start
        && support_2_start < support_2_end
        && support_2_end < support_3_start
        && support_3_start < support_3_end
        && support_3_end < support_4_start
        && support_4_start < support_4_end)
    {
        bail!("unexpected TensorFlow IR support ordering");
    }
    let mut support = String::from("namespace mlir {\nnamespace tfg {\n");
    append_lines(
        &mut support,
        &source_lines,
        support_1_start,
        support_1_end,
    );
    append_lines(
        &mut support,
        &source_lines,
        support_2_start,
        support_2_end,
    );
    append_lines(
        &mut support,
        &source_lines,
        support_3_start,
        support_3_end,
    );
    append_lines(
        &mut support,
        &source_lines,
        support_4_start,
        support_4_end,
    );
    support.push_str("}  // namespace tfg\n}  // namespace mlir\n");

    let generated_lines: Vec<&str> = generated.split_inclusive('\n').collect();
    let classes_undef = generated_lines
        .iter()
        .position(|line| line.trim_end() == "#undef GET_OP_CLASSES")
        .ok_or_else(|| anyhow!("missing generated TensorFlow IR class guard"))?;
    let classes_end = generated_lines
        .iter()
        .rposition(|line| line.trim_end() == "#endif  // GET_OP_CLASSES")
        .ok_or_else(|| anyhow!("missing generated TensorFlow IR class end"))?;
    let mut section_starts = Vec::new();
    for (index, line) in generated_lines.iter().enumerate() {
        let line = line.trim_end();
        if line.starts_with("// ::mlir::tfg::") && line.ends_with(" definitions")
        {
            let start = index.checked_sub(4).ok_or_else(|| {
                anyhow!("truncated generated TensorFlow IR operation section")
            })?;
            if generated_lines[start].trim_end() != "namespace mlir {"
                || generated_lines[start + 1].trim_end() != "namespace tfg {"
                || !generated_lines[start + 2].trim().is_empty()
                || generated_lines[start + 3].trim_end()
                    != "//===----------------------------------------------------------------------===//"
            {
                bail!("unexpected generated TensorFlow IR operation boundary");
            }
            section_starts.push(start);
        }
    }
    let Some(&first_section) = section_starts.first() else {
        bail!("no generated TensorFlow IR operation sections");
    };
    if classes_undef + 1 > first_section || first_section >= classes_end {
        bail!("invalid generated TensorFlow IR class layout");
    }
    let generated_shards = shard_count.min(section_starts.len());
    let mut assignments = vec![Vec::<(usize, usize)>::new(); generated_shards];
    let mut assigned_bytes = vec![0usize; generated_shards];
    for (section, &start) in section_starts.iter().enumerate() {
        let end = section_starts
            .get(section + 1)
            .copied()
            .unwrap_or(classes_end);
        let bytes = generated_lines[start..end]
            .iter()
            .map(|line| line.len())
            .sum::<usize>();
        let destination = assigned_bytes
            .iter()
            .enumerate()
            .min_by_key(|(_, bytes)| *bytes)
            .map(|(index, _)| index)
            .ok_or_else(|| anyhow!("no TensorFlow IR generated shard"))?;
        assignments[destination].push((start, end));
        assigned_bytes[destination] += bytes;
    }

    let mut shards = Vec::with_capacity(generated_shards + 1);
    shards.push(manual);
    for assignment in assignments {
        let mut shard = String::new();
        shard.push_str(&preamble);
        shard.push_str(&generic_helper);
        shard.push_str(&support);
        append_lines(
            &mut shard,
            &generated_lines,
            classes_undef + 1,
            first_section,
        );
        for (start, end) in assignment {
            append_lines(&mut shard, &generated_lines, start, end);
        }
        shards.push(shard);
    }
    Ok(shards)
}

fn suffixed_path(output: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}.{}", output.display(), suffix))
}

fn shell_command_line(executable: &Path, args: &[String]) -> String {
    std::iter::once(shell_quote(&executable.display().to_string()))
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn split_batch_util_source(source: &str) -> Result<Vec<String>> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let namespace_end = lines
        .iter()
        .position(|line| line.trim() == "namespace batch_util {")
        .ok_or_else(|| anyhow!("batch utility has no batch_util namespace"))?;
    let copied_helpers_start = lines
        .iter()
        .position(|line| {
            line.trim_start()
                .starts_with("// The following five functions are copied")
        })
        .ok_or_else(|| anyhow!("batch utility has no copied-helper marker"))?;
    let rank_template_start = lines
        .iter()
        .position(|line| line.trim() == "template <typename T, int NDIMS>")
        .ok_or_else(|| anyhow!("batch utility has no rank template"))?;
    let dispatcher_start = lines
        .iter()
        .position(|line| {
            line.trim_start()
                .starts_with("Status CopyElementToLargerSlice(")
        })
        .ok_or_else(|| anyhow!("batch utility has no public dispatcher"))?;
    let footer_start = lines
        .len()
        .checked_sub(2)
        .filter(|index| {
            lines[*index].contains("namespace batch_util")
                && lines[*index + 1].contains("namespace tensorflow")
        })
        .ok_or_else(|| anyhow!("batch utility has no namespace footer"))?;
    if !(namespace_end < copied_helpers_start
        && copied_helpers_start < rank_template_start
        && rank_template_start < dispatcher_start
        && dispatcher_start < footer_start)
    {
        bail!("batch utility declarations are not in the expected order");
    }

    const FOOTER: &str = "}  // namespace batch_util\n}  // namespace tensorflow\n";
    let preamble = lines[..=namespace_end].concat();
    let rank_template = lines[rank_template_start..dispatcher_start].concat();

    let mut shards = Vec::with_capacity(8);
    let mut first = lines[..copied_helpers_start].concat();
    first.push_str(FOOTER);
    shards.push(first);

    let mut dispatcher = preamble.clone();
    dispatcher.push_str(&lines[copied_helpers_start..rank_template_start].concat());
    dispatcher.push_str(
        "template <int NDIMS>\n\
         Status HandleElementToLargerSliceWithRank(const Tensor&, Tensor*, int);\n",
    );
    for rank in 0..6 {
        dispatcher.push_str(&format!(
            "extern template Status HandleElementToLargerSliceWithRank<{rank}>(\n\
             const Tensor&, Tensor*, int);\n"
        ));
    }
    dispatcher.push_str(&lines[dispatcher_start..footer_start].concat());
    dispatcher.push_str(FOOTER);
    shards.push(dispatcher);

    for rank in 0..6 {
        let mut shard = preamble.clone();
        shard.push_str("Status ValidateElementToLargerSlice(const Tensor&, Tensor*);\n");
        shard.push_str(&rank_template);
        shard.push_str(&format!(
            "template Status HandleElementToLargerSliceWithRank<{rank}>(\n\
             const Tensor&, Tensor*, int);\n"
        ));
        shard.push_str(FOOTER);
        shards.push(shard);
    }
    Ok(shards)
}

fn split_generated_source(source: &str, shard_count: usize) -> Result<Vec<String>> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let preamble_end = lines
        .iter()
        .position(|line| line.trim() == "namespace ops {")
        .map(|index| index + 1)
        .ok_or_else(|| anyhow!("generated op wrapper has no namespace ops preamble"))?;
    let footer_start = lines
        .iter()
        .enumerate()
        .skip(preamble_end)
        .find(|(_, line)| {
            line.trim_start().starts_with('}') && line.contains("namespace ops")
        })
        .map(|(index, _)| index)
        .ok_or_else(|| anyhow!("generated op wrapper has no namespace ops footer"))?;

    let preamble = lines[..preamble_end].concat();
    let footer = lines[footer_start..].concat();
    let mut blocks = Vec::new();
    let mut block = String::new();
    let mut block_lines = 0usize;
    let mut depth = 2i32;
    let mut in_block_comment = false;

    for line in &lines[preamble_end..footer_start] {
        block.push_str(line);
        block_lines += 1;
        let before = depth;
        let (opens, closes) = lexical_braces(line, &mut in_block_comment);
        depth += opens as i32 - closes as i32;
        if depth < 2 {
            bail!("generated op wrapper closed its namespace inside the body");
        }
        if depth == 2 && opens + closes > 0 && (before > 2 || opens > 0) {
            blocks.push((std::mem::take(&mut block), block_lines));
            block_lines = 0;
        }
    }
    if block.chars().any(|character| !character.is_whitespace()) {
        blocks.push((block, block_lines));
    }
    if depth != 2 || in_block_comment {
        bail!("generated op wrapper body is lexically unbalanced");
    }
    if blocks.len() < shard_count {
        bail!("generated op wrapper has too few definitions to shard");
    }

    let mut shard_bodies = vec![String::new(); shard_count];
    let mut shard_lines = vec![0usize; shard_count];
    for (block, lines) in blocks {
        let index = shard_lines
            .iter()
            .enumerate()
            .min_by_key(|(_, lines)| *lines)
            .map(|(index, _)| index)
            .expect("shard count is at least two");
        shard_bodies[index].push_str(&block);
        shard_lines[index] += lines;
    }

    Ok(shard_bodies
        .into_iter()
        .map(|body| format!("{preamble}{body}{footer}"))
        .collect())
}

fn lexical_braces(line: &str, in_block_comment: &mut bool) -> (usize, usize) {
    let bytes = line.as_bytes();
    let mut index = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut opens = 0usize;
    let mut closes = 0usize;

    while index < bytes.len() {
        if *in_block_comment {
            if index + 1 < bytes.len() && bytes[index] == b'*' && bytes[index + 1] == b'/' {
                *in_block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if bytes[index] == b'\\' {
                escaped = true;
            } else if bytes[index] == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        if index + 1 < bytes.len() && bytes[index] == b'/' && bytes[index + 1] == b'/' {
            break;
        }
        if index + 1 < bytes.len() && bytes[index] == b'/' && bytes[index + 1] == b'*' {
            *in_block_comment = true;
            index += 2;
            continue;
        }
        match bytes[index] {
            b'"' | b'\'' => quote = Some(bytes[index]),
            b'{' => opens += 1,
            b'}' => closes += 1,
            _ => {}
        }
        index += 1;
    }
    (opens, closes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_complete_definitions_across_two_translation_units() {
        let source = r#"#include "ops.h"
namespace tensorflow {
namespace ops {
Foo::Foo() {
  const char* brace = "{";
}

Bar::Bar()
    : Bar(Attrs()) {}

}  // namespace ops
}  // namespace tensorflow
"#;
        let shards = split_generated_source(source, 2).unwrap();
        assert!(shards.iter().all(|shard| shard.contains("#include \"ops.h\"")));
        assert!(shards
            .iter()
            .all(|shard| shard.contains("}  // namespace tensorflow")));
        assert_eq!(
            shards.iter().filter(|shard| shard.contains("Foo::Foo")).count(),
            1
        );
        assert_eq!(
            shards.iter().filter(|shard| shard.contains("Bar::Bar")).count(),
            1
        );
    }

    #[test]
    fn recognizes_tensorflow_critical_tail_sources() {
        assert!(is_meta_optimizer_source(
            "/workspace/tensorflow/core/grappler/optimizers/meta_optimizer.cc"
        ));
        assert!(is_generated_cc_op_source(
            "/workspace/rush-out/genfiles/tensorflow/cc/ops/dataset_ops.cc"
        ));
        assert!(!is_generated_cc_op_source(
            "/workspace/tensorflow/core/framework/ops.cc"
        ));
        assert!(is_batch_util_source(
            "/workspace/tensorflow/core/util/batch_util.cc"
        ));
    }

    #[test]
    fn uses_profiled_compile_priority_cost_for_known_tail_sources() {
        let source = Path::new(
            "/workspace/tensorflow/core/grappler/optimizers/tfg_optimizer_hook.cc",
        );
        assert_eq!(
            compile_priority_cost_bytes_with(source, 7_038, None),
            96_000
        );
        assert_eq!(
            compile_priority_cost_bytes_with(source, 7_038, Some(192_000)),
            192_000
        );
        assert_eq!(
            compile_priority_cost_bytes_with(source, 1_000_000, None),
            1_000_000
        );
        assert_eq!(
            compile_priority_cost_bytes_with(
                Path::new("/workspace/other.cc"),
                42,
                Some(192_000)
            ),
            42
        );
    }

    #[test]
    fn shards_complete_meta_optimizer_definitions() {
        let mut source = String::from(
            "#include \"meta_optimizer.h\"\n\
             namespace tensorflow {\n\
             namespace grappler {\n\
             namespace {\n\
             int InternalHelper() { return 1; }\n\
             }  // namespace\n\
             #define MK_OPT(NAME, OPT) NAME\n\
             bool MetaOptimizer::LowerControlFlow() { return false; }\n",
        );
        for index in 1..13 {
            source.push_str(&format!(
                "void MetaOptimizerDefinition{index}() {{}}\n"
            ));
        }
        source.push_str(
            "void PropagateTFDataAttrs(\n\
                 const FunctionLibraryDefinition&, FunctionDefLibrary&) {}\n\
             Status MetaOptimizer::OptimizeConsumeItem() { return OkStatus(); }\n\
             void MetaOptimizerDefinition15() {}\n\
             void MetaOptimizerDefinition16() {}\n\
             bool MetaOptimizerEnabled() { return true; }\n\
             Status RunMetaOptimizer() { return OkStatus(); }\n\
             Status OptimizeGraph() { return OkStatus(); }\n\
             }  // namespace grappler\n\
             }  // namespace tensorflow\n",
        );

        let shards = split_meta_optimizer_source(&source, 4).unwrap();
        assert_eq!(shards.len(), 4);
        assert!(shards
            .iter()
            .all(|shard| shard.contains("InternalHelper")));
        assert!(shards
            .iter()
            .all(|shard| shard.contains("void PropagateTFDataAttrs(")));
        assert!(shards
            .iter()
            .all(|shard| shard.contains("#undef MK_OPT")));
        assert_eq!(
            shards
                .iter()
                .filter(|shard| shard.contains(
                    "Status MetaOptimizer::OptimizeConsumeItem()"
                ))
                .count(),
            1
        );
    }
}
