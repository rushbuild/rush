use rush_starlark::{parse, BzlLoader, Evaluator, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

const MODULE: &[u8] = include_bytes!("fixtures/pybind11_bazel_72cbbf1/build_defs.bzl");
static LOAD_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn pinned_patched_pybind11_module_loads_and_expands_real_extension_rules() {
    let root = tempfile::tempdir().unwrap();
    rush_incremental::tracked::test_scope(root.path(), || {
        let _load = LOAD_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            hex::encode(Sha256::digest(MODULE)),
            "66cc3435282276af092320fb17e28547d7614b4d71cd9852ec49fb127e2262e4"
        );
        assert_eq!(
            hex::encode(Sha256::digest(include_bytes!(
                "fixtures/pybind11_bazel_72cbbf1/upstream_build_defs.bzl"
            ))),
            "9911c481bbaa76fbcaf4fe58fa8bc9c8bc137d7aa38ec11df183b38e0c4cdc54"
        );
        assert_eq!(
            hex::encode(Sha256::digest(include_bytes!(
                "fixtures/pybind11_bazel_72cbbf1/tensorflow.patch"
            ))),
            "308718467be37b41517b8d18d694068c0eb817efbf962beabf0469cffe21d6cb"
        );
        let metadata = root.path().join("third_party/pybind11_bazel");
        let source = root.path().join("fetched/pybind11_bazel");
        std::fs::create_dir_all(&metadata).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            root.path().join("WORKSPACE"),
            "workspace(name = \"module_load_test\")\n",
        )
        .unwrap();
        std::fs::write(metadata.join("BUILD"), "").unwrap();
        std::fs::write(
            metadata.join("workspace.bzl"),
            include_bytes!("fixtures/pybind11_bazel_72cbbf1/workspace.bzl"),
        )
        .unwrap();
        std::fs::write(source.join("build_defs.bzl"), MODULE).unwrap();
        let mut loader = BzlLoader::new(root.path().to_path_buf());
        loader.add_external_repo("pybind11_bazel".to_string(), source.clone());
        let loader = Arc::new(Mutex::new(loader));
        assert_eq!(
            loader
                .lock()
                .unwrap()
                .resolve_bzl_path("@pybind11_bazel//:build_defs.bzl")
                .unwrap(),
            source.join("build_defs.bzl")
        );
        let mut evaluator = Evaluator::new();
        evaluator.register_native_namespace();
        evaluator.set_bzl_loader(loader);
        evaluator.eval_program(&parse(
            "load(\"@pybind11_bazel//:build_defs.bzl\", \"pybind_extension\")\npybind_extension(name = \"extension\", srcs = [\"extension.cc\"], deps = [\":headers\"])\n"
        ).unwrap()).unwrap();
        let calls = evaluator.captured_rule_calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls
                .iter()
                .map(|call| call.rule_type.as_str())
                .collect::<Vec<_>>(),
            vec!["cc_binary", "genrule", "py_library"]
        );
        assert_eq!(
            calls
                .iter()
                .map(|call| call.kwargs["name"].clone())
                .collect::<Vec<_>>(),
            vec![
                Value::String("extension.so".into()),
                Value::String("extension_pyd".into()),
                Value::String("extension".into())
            ]
        );
        assert_eq!(calls[0].kwargs["linkshared"], Value::Int(1));
        let Value::List(deps) = &calls[0].kwargs["deps"] else {
            panic!("missing pybind dependencies")
        };
        assert!(deps.contains(&Value::String("@pybind11".into())));
        assert!(deps.contains(&Value::String(
            "@local_config_python//:python_headers".into()
        )));
        assert_eq!(
            calls[1].kwargs["outs"],
            Value::List(vec![Value::String("extension.pyd".into())])
        );
        assert_eq!(calls[1].kwargs["cmd"], Value::String("cp $< $@".into()));
        let Value::Select(data) = &calls[2].kwargs["data"] else {
            panic!("TensorFlow patch's platform selection was lost")
        };
        assert_eq!(
            data["@platforms//os:windows"],
            Value::List(vec![Value::String(":extension.pyd".into())])
        );
        assert_eq!(
            data["//conditions:default"],
            Value::List(vec![Value::String(":extension.so".into())])
        );
        assert!(!metadata.join("build_defs.bzl").exists());
    });
}
