pub mod ast;
pub mod builtins;
pub mod bzl_loader;
pub mod config_defines;
pub mod evaluator;
pub mod fetch_plan;
pub mod fetch_strategy;
pub mod lazy_evaluator;
pub mod lexer;
pub mod parser;
pub mod platform;
pub mod rule_bridge;
pub mod select_evaluator;
pub mod token;
pub mod work_first_orchestrator;

pub use ast::{BindingPattern, CallExpr, Expr, Program, Stmt};
pub use bzl_loader::{
    BzlLoader, CacheStats, FetchDecision, FetchReportEntry, GlobalBzlCache,
};
pub use fetch_plan::{
    classify_repo_for_fetch_plan, FetchClassification, FetchPlan,
    FetchPlanEntry,
};
pub use fetch_strategy::{
    execute_combined_fetch, execute_parallel_fetch, parse_concurrency_env,
    probe_combined_capability, resolve_runtime_strategy, CombinedCapability,
    FetchStrategy, ParallelFetchResult, DEFAULT_FETCH_CONCURRENCY,
};
pub use evaluator::{
    BuildFileContextGuard, ConfigurableOp, Evaluator, RuleCall, Value,
    VISIBILITY_EXPLICIT_MARKER,
};
pub use lazy_evaluator::{
    ActionType, EvaluatorMode, ImmediateAction, LazyBuildAnalysis, TargetDefinition,
};
pub use lexer::Lexer;
pub use parser::Parser;
pub use platform::PlatformConfig;
pub use rule_bridge::{rule_call_to_attrs, starlark_to_rule_value, RuleValue};
pub use select_evaluator::SelectEvaluator;
pub use token::{Token, TokenType};
pub use work_first_orchestrator::{
    CoreMonitor, CoreStats, LoadPriority, OrchestratorStats, PendingBzlLoad, WorkFirstOrchestrator,
};

use anyhow::Result;

/// Memory profiling utilities
pub mod mem_profile {
    use crate::{Expr, Program, Stmt, Value};
    use std::mem::size_of;

    pub fn print_type_sizes() {
        eprintln!("\n╔════════════════════════════════════════════════════════════════╗");
        eprintln!("║ RUSH MEMORY PROFILING - Type Sizes                             ║");
        eprintln!("╚════════════════════════════════════════════════════════════════╝");

        eprintln!("\n📊 Core Starlark Types:");
        eprintln!(
            "  Value enum:                    {} bytes",
            size_of::<Value>()
        );
        eprintln!(
            "  String:                        {} bytes",
            size_of::<String>()
        );
        eprintln!(
            "  Vec<Value> (empty):            {} bytes",
            size_of::<Vec<Value>>()
        );
        eprintln!(
            "  HashMap<String,Value> (empty): {} bytes",
            size_of::<std::collections::HashMap<String, Value>>()
        );

        eprintln!("\n📊 AST Node Types:");
        eprintln!(
            "  Stmt enum:                     {} bytes",
            size_of::<Stmt>()
        );
        eprintln!(
            "  Expr enum:                     {} bytes",
            size_of::<Expr>()
        );
        eprintln!(
            "  Program:                       {} bytes",
            size_of::<Program>()
        );
        eprintln!(
            "  Vec<Stmt> (empty):             {} bytes",
            size_of::<Vec<Stmt>>()
        );

        eprintln!("\n📊 Memory Analysis:");
        if size_of::<Value>() > 128 {
            eprintln!(
                "  ⚠️  Value enum is LARGE ({}b) - all variants padded to largest!",
                size_of::<Value>()
            );
            eprintln!("      Largest variant wastes space for all other variants.");
            eprintln!("      Consider Box<> for large variants like Function/Dict/List");
        }
        if size_of::<Expr>() > 128 {
            eprintln!("  ⚠️  Expr enum is LARGE ({}b)", size_of::<Expr>());
        }

        eprintln!("\n📊 Estimated .bzl File Memory (100 symbols):");
        let est = estimate_bzl_memory(100, 20);
        eprintln!("  ~{} MB per .bzl file", est / 1024 / 1024);
        eprintln!("  (100 symbols × ~20 char names × Value size + overhead)");

        eprintln!("════════════════════════════════════════════════════════════════\n");
    }

    pub fn estimate_bzl_memory(num_symbols: usize, avg_str_len: usize) -> usize {
        // Rough estimate of memory per .bzl file
        let value_size = size_of::<Value>();
        let string_overhead = size_of::<String>();

        // Each symbol: key String + Value + HashMap overhead
        let per_symbol = string_overhead + avg_str_len + value_size + 32; // +32 for HashMap overhead

        num_symbols * per_symbol
    }
}

/// Parse Starlark source code into an AST
pub fn parse(source: &str) -> Result<Program> {
    let lexer = Lexer::new(source);
    let mut parser = Parser::new(lexer);
    parser.parse()
}

/// Evaluate Starlark source code and return the resulting environment
pub fn eval(source: &str) -> Result<Evaluator> {
    let program = parse(source)?;
    let mut evaluator = Evaluator::new();
    evaluator.eval_program(&program)?;
    Ok(evaluator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_assignment() {
        let source = "x = 42";
        let result = eval(source);
        assert!(result.is_ok());
        let evaluator = result.unwrap();
        assert_eq!(evaluator.get_var("x").unwrap(), Value::Int(42));
    }

    #[test]
    fn test_list_literal() {
        let source = "x = [1, 2, 3]";
        let result = eval(source);
        assert!(result.is_ok());
    }

    #[test]
    fn test_function_def() {
        let root = tempfile::tempdir().unwrap();
        rush_incremental::tracked::test_scope(root.path(), || {
            let source = r#"
def add(a, b):
    return a + b

result = add(10, 20)
"#;
            let result = eval(source);
            assert!(result.is_ok());
            let evaluator = result.unwrap();
            assert_eq!(evaluator.get_var("result").unwrap(), Value::Int(30));
        });
    }

    #[test]
    fn test_list_comprehension_with_if() {
        // Test basic list comprehension with if condition
        let source = "[x for x in [1, 2, 3] if x > 1]";
        let result = parse(source);
        assert!(result.is_ok(), "Failed to parse: {:?}", result.err());
    }
}
