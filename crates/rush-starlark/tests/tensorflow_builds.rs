use rush_starlark::parse;
use std::fs;
use std::path::PathBuf;

fn test_file(path: &str) -> Result<usize, String> {
    let source = fs::read_to_string(path).map_err(|e| format!("Failed to read: {}", e))?;

    let program = parse(&source).map_err(|e| format!("Parse error: {}", e))?;

    Ok(program.statements.len())
}

#[test]
#[ignore] // Run with: cargo test -- --ignored
fn test_tensorflow_build_files() {
    // Use TF_PATH env var, or default to ../tensorflow relative to repo root
    let tf_root = std::env::var("TF_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../tensorflow"));

    let files = vec![
        tf_root.join("tensorflow/lite/BUILD"),
        tf_root.join("tensorflow/core/BUILD"),
        tf_root.join("tensorflow/python/BUILD"),
        tf_root.join("tensorflow/BUILD"),
    ];

    let mut passed = 0;
    let mut failed = 0;

    for file in &files {
        let file_str = file.to_str().unwrap();
        match test_file(file_str) {
            Ok(count) => {
                println!("{}: ✓ {} statements", file_str, count);
                passed += 1;
            }
            Err(e) => {
                println!("{}: ✗ {}", file_str, e);
                failed += 1;
            }
        }
    }

    println!("\nResults: {} passed, {} failed", passed, failed);

    // Fail the test if any files failed to parse
    if failed > 0 {
        panic!("{} files failed to parse", failed);
    }
}
