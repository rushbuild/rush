use rush_parse::{starlark_bridge, Value};
use rush_starlark::BzlLoader;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
fn tags(file: &rush_parse::BuildFile) -> Vec<String> {
    match &file
        .rules
        .iter()
        .find(|r| r.name == "probe")
        .unwrap()
        .attributes["tags"]
    {
        Value::List(values) => values
            .iter()
            .map(|value| match value {
                Value::String(value) => value.clone(),
                other => panic!("identity tags must contain exact strings, got {other:?}"),
            })
            .collect(),
        _ => panic!("missing identity tags"),
    }
}

#[test]
fn build_parser_entrypoints_preserve_explicit_repository_caller_and_module_definition_owner() {
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    write(
        &root.path().join("WORKSPACE"),
        "workspace(name=\"named_main\")\n",
    );
    write(
        &root.path().join("main/BUILD"),
        "filegroup(name=\"probe\", tags=[repository_name(),native.repository_name()])\n",
    );
    write(&shared.join("defs.bzl"),"DEFINITION=Label(\":defined\")\ndef declare():\n    native.filegroup(name=\"probe\", tags=[native.repository_name(),str(DEFINITION)])\n");
    write(
        &shared.join("pkg/BUILD"),
        "load(\"//:defs.bzl\",\"declare\")\ndeclare()\n",
    );
    rush_incremental::tracked::test_scope(root.path(), || {
        let plain =
            starlark_bridge::parse_build_file_with_starlark(&root.path().join("main/BUILD"))
                .unwrap();
        assert_eq!(tags(&plain), ["@".to_owned(), "@".to_owned()]);
        let direct_main = starlark_bridge::parse_build_file_with_loader(
            &root.path().join("main/BUILD"),
            BzlLoader::new(root.path().to_owned()),
        )
        .unwrap();
        assert_eq!(tags(&direct_main), ["@".to_owned(), "@".to_owned()]);
        let shared_main = starlark_bridge::parse_build_file_with_bzl_loader(
            &root.path().join("main/BUILD"),
            Some(Arc::new(Mutex::new(BzlLoader::new(root.path().to_owned())))),
        )
        .unwrap();
        assert_eq!(tags(&shared_main), ["@".to_owned(), "@".to_owned()]);
        for shared_api in [false, true] {
            for owner in ["owner_a", "owner_b", "libmod~override"] {
                let mut loader = BzlLoader::new(root.path().to_owned());
                for name in ["owner_a", "owner_b", "libmod~override"] {
                    loader.add_external_repo(name.into(), shared.clone());
                }
                loader.set_current_repo_root(shared.clone());
                loader.set_current_repo_name(Some(owner.into()));
                let path = shared.join("pkg/BUILD");
                let file = if shared_api {
                    starlark_bridge::parse_build_file_with_bzl_loader(
                        &path,
                        Some(Arc::new(Mutex::new(loader))),
                    )
                    .unwrap()
                } else {
                    starlark_bridge::parse_build_file_with_loader(&path, loader).unwrap()
                };
                assert_eq!(
                    tags(&file),
                    [format!("@{owner}"), format!("@@{owner}//:defined")]
                );
            }
        }
    });
}

#[test]
fn build_thread_does_not_authorize_module_initialization_repository_name() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("WORKSPACE"), "");
    // Consume the imported macro so lazy BUILD loading must initialize its module.
    // The initializer must fail before the macro can declare its filegroup.
    write(
        &root.path().join("bad.bzl"),
        "VALUE=native.repository_name()\ndef declare():\n    native.filegroup(name=\"probe\", tags=[VALUE])\n",
    );
    write(
        &root.path().join("BUILD"),
        "load(\":bad.bzl\",\"declare\")\ndeclare()\n",
    );
    rush_incremental::tracked::test_scope(root.path(), || {
        for shared_api in [false, true] {
            let loader = BzlLoader::new(root.path().to_owned());
            let path = root.path().join("BUILD");
            let error = if shared_api {
                starlark_bridge::parse_build_file_with_bzl_loader(
                    &path,
                    Some(Arc::new(Mutex::new(loader))),
                )
                .unwrap_err()
            } else {
                starlark_bridge::parse_build_file_with_loader(&path, loader).unwrap_err()
            };
            assert!(
                format!("{error:#}").contains("requires a BUILD loading context"),
                "{error:#}"
            );
        }
    });
}

#[cfg(unix)]
#[test]
fn build_repository_context_tracks_unique_symlink_owner_and_explicit_logical_root() {
    use std::os::unix::fs::symlink;
    let workspace = tempfile::tempdir().unwrap();
    let real = tempfile::tempdir().unwrap();
    write(&workspace.path().join("WORKSPACE"), "");
    write(&real.path().join("defs.bzl"),"DEFINITION=Label(\":defined\")\ndef declare():\n    native.filegroup(name=\"probe\",tags=[native.repository_name(),native.package_name(),str(DEFINITION)])\n");
    write(
        &real.path().join("pkg/BUILD"),
        "load(\"//:defs.bzl\",\"declare\")\ndeclare()\n",
    );
    write(
        &real.path().join("identity/BUILD"),
        "filegroup(name=\"probe\",tags=[repository_name(),native.repository_name()])\n",
    );
    let logical = workspace.path().join("linked");
    symlink(real.path(), &logical).unwrap();
    rush_incremental::tracked::test_scope(workspace.path(), || {
        for shared_api in [false, true] {
            for explicit in [false, true] {
                let mut loader = BzlLoader::new(workspace.path().to_owned());
                if explicit {
                    loader.add_external_repo("selected".into(), logical.clone());
                    loader.add_external_repo("other_owner".into(), real.path().to_owned());
                    loader.set_current_repo_root(logical.clone());
                    loader.set_current_repo_name(Some("selected".into()));
                } else {
                    loader.add_external_repo("selected".into(), real.path().to_owned());
                }
                let path = if explicit {
                    real.path().join("pkg/BUILD")
                } else {
                    logical.join("pkg/BUILD")
                };
                let file = if shared_api {
                    starlark_bridge::parse_build_file_with_bzl_loader(
                        &path,
                        Some(Arc::new(Mutex::new(loader))),
                    )
                    .unwrap()
                } else {
                    starlark_bridge::parse_build_file_with_loader(&path, loader).unwrap()
                };
                assert_eq!(
                    tags(&file),
                    [
                        "@selected".to_owned(),
                        "pkg".to_owned(),
                        "@@selected//:defined".to_owned()
                    ]
                );
            }
            // Without an explicit owner, equal source roots cannot authorize
            // an arbitrary DashMap iteration winner.
            let mut loader = BzlLoader::new(workspace.path().to_owned());
            for owner in ["first", "second"] {
                loader.add_external_repo(owner.into(), real.path().to_owned());
            }
            let path = real.path().join("pkg/BUILD");
            let error = if shared_api {
                starlark_bridge::parse_build_file_with_bzl_loader(
                    &path,
                    Some(Arc::new(Mutex::new(loader))),
                )
                .unwrap_err()
            } else {
                starlark_bridge::parse_build_file_with_loader(&path, loader).unwrap_err()
            };
            assert!(
                format!("{error:#}").contains("multiple registered repository owners"),
                "{error:#}"
            );
        }
        // The loader-free convenience entrypoint has no external owner
        // authority and deliberately remains a main-repository BUILD caller.
        let plain =
            starlark_bridge::parse_build_file_with_starlark(&logical.join("identity/BUILD"))
                .unwrap();
        assert_eq!(tags(&plain), ["@".to_owned(), "@".to_owned()]);
    });
}

#[test]
fn relative_build_paths_read_and_identify_exact_loader_workspace() {
    const CHILD: &str = "RUSH_REPOSITORY_NAME_RELATIVE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let decoy = tempfile::tempdir().unwrap();
        write(
            &decoy.path().join("BUILD"),
            "filegroup(name=\"probe\",tags=[\"decoy\"])\n",
        );
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "relative_build_paths_read_and_identify_exact_loader_workspace",
                "--nocapture",
            ])
            .current_dir(decoy.path())
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    assert_ne!(std::env::current_dir().unwrap(), workspace.path());
    assert!(std::fs::read_to_string("BUILD").unwrap().contains("decoy"));
    write(&workspace.path().join("WORKSPACE"), "");
    write(&workspace.path().join("BUILD"),"filegroup(name=\"probe\",tags=[\"analysis\",repository_name(),native.repository_name()])\n");
    rush_incremental::tracked::test_scope(workspace.path(), || {
        for shared_api in [false, true] {
            let loader = BzlLoader::new(workspace.path().to_owned());
            let file = if shared_api {
                starlark_bridge::parse_build_file_with_bzl_loader(
                    Path::new("BUILD"),
                    Some(Arc::new(Mutex::new(loader))),
                )
                .unwrap()
            } else {
                starlark_bridge::parse_build_file_with_loader(Path::new("BUILD"), loader).unwrap()
            };
            assert_eq!(
                tags(&file),
                ["analysis".to_owned(), "@".to_owned(), "@".to_owned()]
            );
            assert_eq!(
                file.path,
                workspace.path().join("BUILD"),
                "source read and caller context must use one exact path"
            );
        }
        let convenience =
            starlark_bridge::parse_build_file_with_starlark(Path::new("BUILD")).unwrap();
        assert_eq!(
            tags(&convenience),
            ["decoy".to_owned()],
            "a loader-free call retains cwd source authority"
        );
        assert_eq!(
            convenience.path,
            std::env::current_dir().unwrap().join("BUILD")
        );
    });
}
