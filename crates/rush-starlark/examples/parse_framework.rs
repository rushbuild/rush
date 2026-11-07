use std::fs;
use std::path::PathBuf;

fn main() {
    // Use TF_PATH env var, or default to ../tensorflow relative to current dir
    let tf_path = std::env::var("TF_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../tensorflow"));

    let build_file = tf_path.join("tensorflow/core/BUILD");

    if !build_file.exists() {
        eprintln!(
            "Error: TensorFlow BUILD file not found at {}",
            build_file.display()
        );
        eprintln!("Set TF_PATH environment variable or clone TensorFlow to ../tensorflow");
        std::process::exit(1);
    }

    let content = fs::read_to_string(&build_file).expect("Failed to read file");

    println!(
        "File size: {} bytes, {} lines",
        content.len(),
        content.lines().count()
    );
    println!("Attempting to parse...\n");

    // Try parsing with rush-starlark
    match rush_starlark::parse(&content) {
        Ok(ast) => {
            println!("✓ Parse successful!");
            println!("AST has {} statements", ast.statements.len());
        }
        Err(e) => {
            println!("✗ Parse failed:");
            println!("\nError: {}", e);

            // Show the full error chain
            println!("\nFull error chain:");
            for (i, cause) in e.chain().enumerate() {
                println!("  {}: {}", i, cause);
            }

            // Show debug representation for more detail
            println!("\nDebug representation:");
            println!("{:#?}", e);
        }
    }
}
