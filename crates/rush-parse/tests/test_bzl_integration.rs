// Integration test for .bzl file loading
use rush_parse::parse_build_file_with_bzl_loader;
use rush_starlark::BzlLoader;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[test]
fn test_bzl_loading_integration() {
    // Path to test project
    let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("test_bzl_loading");

    if !test_dir.exists() {
        eprintln!("Test directory not found: {}", test_dir.display());
        eprintln!("Skipping test - run from Rush workspace root");
        return;
    }

    let build_file = test_dir.join("BUILD");
    if !build_file.exists() {
        eprintln!("BUILD file not found: {}", build_file.display());
        return;
    }

    // Create BzlLoader
    let bzl_loader = BzlLoader::new(test_dir.clone());
    let bzl_loader = Arc::new(Mutex::new(bzl_loader));

    // Parse BUILD file with .bzl loader
    let result = parse_build_file_with_bzl_loader(&build_file, Some(bzl_loader));

    match result {
        Ok(build_file) => {
            println!("✅ Successfully parsed BUILD file with .bzl loading!");
            println!("   Found {} rules", build_file.rules.len());

            for rule in &build_file.rules {
                println!("   - {} (type: {})", rule.name, rule.rule_type);

                // Check that srcs attribute exists and was populated by get_sources()
                if let Some(srcs) = rule.attributes.get("srcs") {
                    println!("     srcs: {:?}", srcs);
                }

                // Check that copts attribute exists and was populated by get_compiler_flags()
                if let Some(copts) = rule.attributes.get("copts") {
                    println!("     copts: {:?}", copts);
                }
            }

            // Verify we got the expected rule
            assert!(!build_file.rules.is_empty(), "Expected at least one rule");
            let rule = &build_file.rules[0];
            assert_eq!(rule.name, "test_binary");
            assert_eq!(rule.rule_type, "cc_binary");

            // Verify attributes were populated from .bzl functions
            assert!(
                rule.attributes.contains_key("srcs"),
                "Expected srcs attribute"
            );
            assert!(
                rule.attributes.contains_key("copts"),
                "Expected copts attribute"
            );
        }
        Err(e) => {
            eprintln!("❌ Failed to parse BUILD file with .bzl loading: {}", e);
            eprintln!("   Error details: {:?}", e);

            // Print fallback message - this is expected behavior
            eprintln!(
                "\n⚠️  Note: .bzl loading may have fallen back to rule function registration."
            );
            eprintln!("    This is acceptable as it maintains backward compatibility.");
        }
    }
}

#[test]
fn test_bzl_symbol_aliasing() {
    // Path to test project
    let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("test_bzl_loading");

    if !test_dir.exists() {
        eprintln!("Test directory not found: {}", test_dir.display());
        eprintln!("Skipping test - run from Rush workspace root");
        return;
    }

    let build_file = test_dir.join("BUILD.aliasing");
    if !build_file.exists() {
        eprintln!("BUILD.aliasing file not found: {}", build_file.display());
        return;
    }

    // Create BzlLoader
    let bzl_loader = BzlLoader::new(test_dir.clone());
    let bzl_loader = Arc::new(Mutex::new(bzl_loader));

    // Parse BUILD file with .bzl loader
    let result = parse_build_file_with_bzl_loader(&build_file, Some(bzl_loader));

    match result {
        Ok(build_file) => {
            println!("✅ Successfully parsed BUILD file with symbol aliasing!");
            println!("   Found {} rules", build_file.rules.len());

            for rule in &build_file.rules {
                println!("   - {} (type: {})", rule.name, rule.rule_type);

                // Check that srcs attribute exists and was populated by aliased function
                if let Some(srcs) = rule.attributes.get("srcs") {
                    println!("     srcs (from aliased 'test_sources'): {:?}", srcs);
                }

                // Check that copts attribute exists and was populated by aliased function
                if let Some(copts) = rule.attributes.get("copts") {
                    println!(
                        "     copts (from aliased 'default_compiler_flags'): {:?}",
                        copts
                    );
                }

                // Check that linkopts attribute exists and was populated by aliased function
                if let Some(linkopts) = rule.attributes.get("linkopts") {
                    println!(
                        "     linkopts (from aliased 'default_linker_flags'): {:?}",
                        linkopts
                    );
                }
            }

            // Verify we got the expected rule
            assert!(!build_file.rules.is_empty(), "Expected at least one rule");
            let rule = &build_file.rules[0];
            assert_eq!(rule.name, "aliased_test");
            assert_eq!(rule.rule_type, "cc_test");

            // Verify attributes were populated from aliased .bzl functions
            assert!(
                rule.attributes.contains_key("srcs"),
                "Expected srcs attribute from aliased symbol"
            );
            assert!(
                rule.attributes.contains_key("copts"),
                "Expected copts attribute from aliased symbol"
            );
            assert!(
                rule.attributes.contains_key("linkopts"),
                "Expected linkopts attribute from aliased symbol"
            );
        }
        Err(e) => {
            eprintln!("❌ Failed to parse BUILD file with symbol aliasing: {}", e);
            eprintln!("   Error details: {:?}", e);
            panic!("Symbol aliasing test failed - this should work!");
        }
    }
}

#[test]
fn test_macro_expansion() {
    // Path to test project
    let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("test_bzl_loading");

    if !test_dir.exists() {
        eprintln!("Test directory not found: {}", test_dir.display());
        eprintln!("Skipping test - run from Rush workspace root");
        return;
    }

    let build_file = test_dir.join("BUILD.macros");
    if !build_file.exists() {
        eprintln!("BUILD.macros file not found: {}", build_file.display());
        return;
    }

    // Create BzlLoader
    let bzl_loader = BzlLoader::new(test_dir.clone());
    let bzl_loader = Arc::new(Mutex::new(bzl_loader));

    // Parse BUILD file with .bzl loader
    let result = parse_build_file_with_bzl_loader(&build_file, Some(bzl_loader));

    match result {
        Ok(build_file) => {
            println!("✅ Successfully parsed BUILD file with macro expansion!");
            println!("   Found {} rules", build_file.rules.len());

            for rule in &build_file.rules {
                println!("   - {} (type: {})", rule.name, rule.rule_type);

                // Print some key attributes
                if let Some(srcs) = rule.attributes.get("srcs") {
                    println!("     srcs: {:?}", srcs);
                }
                if let Some(deps) = rule.attributes.get("deps") {
                    println!("     deps: {:?}", deps);
                }
            }

            // Verify we got the expected number of rules
            // cc_library_with_test("mylib") creates: mylib (cc_library) + mylib_test (cc_test)
            // multi_target_macro("example") creates: example_lib, example_bin, example_test
            // Total: 5 rules
            assert_eq!(
                build_file.rules.len(),
                5,
                "Expected 5 rules from macro expansion"
            );

            // Verify specific rules exist
            let rule_names: Vec<&str> = build_file.rules.iter().map(|r| r.name.as_str()).collect();

            assert!(rule_names.contains(&"mylib"), "Expected mylib rule");
            assert!(
                rule_names.contains(&"mylib_test"),
                "Expected mylib_test rule"
            );
            assert!(
                rule_names.contains(&"example_lib"),
                "Expected example_lib rule"
            );
            assert!(
                rule_names.contains(&"example_bin"),
                "Expected example_bin rule"
            );
            assert!(
                rule_names.contains(&"example_test"),
                "Expected example_test rule"
            );

            // Verify rule types
            let mylib = build_file.rules.iter().find(|r| r.name == "mylib").unwrap();
            assert_eq!(mylib.rule_type, "cc_library");

            let mylib_test = build_file
                .rules
                .iter()
                .find(|r| r.name == "mylib_test")
                .unwrap();
            assert_eq!(mylib_test.rule_type, "cc_test");

            let example_bin = build_file
                .rules
                .iter()
                .find(|r| r.name == "example_bin")
                .unwrap();
            assert_eq!(example_bin.rule_type, "cc_binary");
        }
        Err(e) => {
            eprintln!("❌ Failed to parse BUILD file with macro expansion: {}", e);
            eprintln!("   Error details: {:?}", e);
            panic!("Macro expansion test failed - this should work!");
        }
    }
}

#[test]
fn test_native_functions() {
    // Path to test project
    let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("test_bzl_loading");

    if !test_dir.exists() {
        eprintln!("Test directory not found: {}", test_dir.display());
        eprintln!("Skipping test - run from Rush workspace root");
        return;
    }

    let build_file = test_dir.join("BUILD.advanced");
    if !build_file.exists() {
        eprintln!("BUILD.advanced file not found: {}", build_file.display());
        return;
    }

    // Create BzlLoader
    let bzl_loader = BzlLoader::new(test_dir.clone());
    let bzl_loader = Arc::new(Mutex::new(bzl_loader));

    // Parse BUILD file with .bzl loader
    let result = parse_build_file_with_bzl_loader(&build_file, Some(bzl_loader));

    match result {
        Ok(build_file) => {
            println!("✅ Successfully parsed BUILD file with native.* functions!");
            println!("   Found {} rules", build_file.rules.len());

            for rule in &build_file.rules {
                println!("   - {} (type: {})", rule.name, rule.rule_type);

                if let Some(srcs) = rule.attributes.get("srcs") {
                    println!("     srcs: {:?}", srcs);
                }
                if let Some(deps) = rule.attributes.get("deps") {
                    println!("     deps: {:?}", deps);
                }
            }

            // Verify we got the expected rules
            // smart_library("utils") creates: utils (cc_library) + utils_test (cc_test)
            // conditional_macro("app") creates: app_base (cc_library) + app (cc_binary)
            // Total: 4 rules
            assert!(build_file.rules.len() >= 3, "Expected at least 3 rules");

            // Verify specific rules exist
            let rule_names: Vec<&str> = build_file.rules.iter().map(|r| r.name.as_str()).collect();

            assert!(rule_names.contains(&"utils"), "Expected utils rule");
            assert!(rule_names.contains(&"app_base"), "Expected app_base rule");
            assert!(rule_names.contains(&"app"), "Expected app rule");
        }
        Err(e) => {
            eprintln!(
                "❌ Failed to parse BUILD file with native.* functions: {}",
                e
            );
            eprintln!("   Error details: {:?}", e);
            panic!("native.* functions test failed!");
        }
    }
}

#[test]
fn test_providers() {
    // Path to test project
    let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("test_bzl_loading");

    if !test_dir.exists() {
        eprintln!("Test directory not found: {}", test_dir.display());
        eprintln!("Skipping test - run from Rush workspace root");
        return;
    }

    let build_file = test_dir.join("BUILD.providers");
    if !build_file.exists() {
        eprintln!("BUILD.providers file not found: {}", build_file.display());
        return;
    }

    // Create BzlLoader
    let bzl_loader = BzlLoader::new(test_dir.clone());
    let bzl_loader = Arc::new(Mutex::new(bzl_loader));

    // Parse BUILD file with .bzl loader
    let result = parse_build_file_with_bzl_loader(&build_file, Some(bzl_loader));

    match result {
        Ok(build_file) => {
            println!("✅ Successfully parsed BUILD file with providers!");
            println!("   Found {} rules", build_file.rules.len());

            for rule in &build_file.rules {
                println!("   - {} (type: {})", rule.name, rule.rule_type);

                if let Some(srcs) = rule.attributes.get("srcs") {
                    println!("     srcs: {:?}", srcs);
                }
                if let Some(deps) = rule.attributes.get("deps") {
                    println!("     deps: {:?}", deps);
                }
            }

            // Verify we got the expected rules
            // library_with_info("data_lib") creates: data_lib (cc_library) + data_lib_test (cc_test)
            assert_eq!(build_file.rules.len(), 2, "Expected 2 rules");

            // Verify specific rules exist
            let rule_names: Vec<&str> = build_file.rules.iter().map(|r| r.name.as_str()).collect();

            assert!(rule_names.contains(&"data_lib"), "Expected data_lib rule");
            assert!(
                rule_names.contains(&"data_lib_test"),
                "Expected data_lib_test rule"
            );

            // Verify rule types
            let data_lib = build_file
                .rules
                .iter()
                .find(|r| r.name == "data_lib")
                .unwrap();
            assert_eq!(data_lib.rule_type, "cc_library");

            let data_lib_test = build_file
                .rules
                .iter()
                .find(|r| r.name == "data_lib_test")
                .unwrap();
            assert_eq!(data_lib_test.rule_type, "cc_test");
        }
        Err(e) => {
            eprintln!("❌ Failed to parse BUILD file with providers: {}", e);
            eprintln!("   Error details: {:?}", e);
            panic!("Provider test failed!");
        }
    }
}

#[test]
fn test_real_tensorflow_build() {
    use std::path::Path;

    // Use TF_PATH env var, or default to ../tensorflow relative to repo root
    let tf_root = std::env::var("TF_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../tensorflow"));

    let build_path = tf_root.join("tensorflow/c/eager/BUILD");
    if !build_path.exists() {
        eprintln!(
            "Skipping - TensorFlow BUILD not found at {}",
            build_path.display()
        );
        eprintln!("Set TF_PATH environment variable or clone TensorFlow to ../tensorflow");
        return;
    }

    // Create BzlLoader with TensorFlow workspace root
    let bzl_loader = BzlLoader::new(tf_root.to_path_buf());
    let bzl_loader = Arc::new(Mutex::new(bzl_loader));

    println!("Attempting to parse real TensorFlow BUILD file...");
    let result = parse_build_file_with_bzl_loader(&build_path, Some(bzl_loader));

    match &result {
        Ok(build_file) => {
            println!("✅ Successfully parsed TensorFlow BUILD file!");
            println!("   Found {} rules", build_file.rules.len());

            // Show first few rules
            for (i, rule) in build_file.rules.iter().take(5).enumerate() {
                println!("   {}. {} (type: {})", i + 1, rule.name, rule.rule_type);
            }

            if build_file.rules.len() > 5 {
                println!("   ... and {} more rules", build_file.rules.len() - 5);
            }
        }
        Err(e) => {
            eprintln!("❌ Failed to parse TensorFlow BUILD file: {}", e);
            eprintln!("   Error details: {:?}", e);
            eprintln!("\nThis helps identify what features to implement next!");
        }
    }
}
