use crate::repository_manager::RepositoryManager;
use anyhow::Result;
use rush_parse::{GitRepository, HttpArchive, LocalRepository};
use rush_starlark::{BzlLoader, Evaluator, Lexer, Parser, RuleCall, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct DiscoveredRepositories {
    pub workspace_name: Option<String>,
    pub http_archives: Vec<HttpArchive>,
    pub git_repositories: Vec<GitRepository>,
    pub local_repositories: Vec<LocalRepository>,
}

impl DiscoveredRepositories {
    pub fn new() -> Self {
        Self {
            workspace_name: None,
            http_archives: Vec::new(),
            git_repositories: Vec::new(),
            local_repositories: Vec::new(),
        }
    }
}

pub struct WorkspaceLoader {
    workspace_root: PathBuf,
    repository_manager: Arc<RepositoryManager>,
    global_bzl_cache: rush_starlark::GlobalBzlCache,
}

impl WorkspaceLoader {
    pub fn new(
        workspace_root: PathBuf,
        repository_manager: Arc<RepositoryManager>,
        global_bzl_cache: rush_starlark::GlobalBzlCache,
    ) -> Self {
        Self {
            workspace_root,
            repository_manager,
            global_bzl_cache,
        }
    }

    pub async fn load(
        &self,
        workspace_file: &Path,
        _fetch_dependencies: bool,
    ) -> Result<DiscoveredRepositories> {
        eprintln!(
            "📋 Loading WORKSPACE via Starlark: {}",
            workspace_file.display()
        );

        // Read file content
        let content = std::fs::read_to_string(workspace_file)?;

        // Parse content
        let lexer = Lexer::new(&content);
        let mut parser = Parser::new(lexer);
        let program = parser.parse()?;

        // Create evaluator
        let mut evaluator = Evaluator::new();

        // WORKSPACE repository rules are ordinary top-level callables.  The
        // native namespace alone is not enough for the canonical
        // `workspace(...)` / `local_repository(...)` spellings used by Bazel
        // workspaces, so install the small set this loader records before
        // evaluating the file.
        for rule in [
            "workspace",
            "http_archive",
            "tf_http_archive",
            "git_repository",
            "local_repository",
            "new_local_repository",
        ] {
            evaluator.register_rule_function(rule.to_string());
        }
        evaluator.register_workspace_native_namespace();
        evaluator.set_workspace_repository_context(self.workspace_root.clone());

        // Create BzlLoader for this WORKSPACE evaluation
        let mut bzl_loader = BzlLoader::new_with_global_cache(
            self.workspace_root.clone(),
            self.global_bzl_cache.clone(),
        );
        bzl_loader.set_workspace_repository_context(Some(self.workspace_root.clone()));

        evaluator.register_rule_function("tf_workspace0".to_string());

        // Resolve load() on demand through the genuine repository-rule path.
        // Eagerly fetching every declared archive loads unrelated packages and
        // is unnecessary for a later module load to materialize its own source.
        let bzl_loader_arc = Arc::new(Mutex::new(bzl_loader));
        evaluator.set_bzl_loader(bzl_loader_arc);

        // Evaluate the program.
        //
        // Persistent-daemon TrackedIo migration (evaluator slice): the Starlark
        // evaluator's call dispatch (`eval_call`) and the platform-conditional
        // builtins (`builtin_if_cuda_or_rocm`) read analysis-affecting env
        // (RUSH_ENABLE_ROCM) through the fail-loud `tracked::getenv_str`. WORKSPACE
        // evaluation drives that dispatch (every `workspace()`/`http_archive()`
        // call enters `eval_call`), and it runs at orchestrator setup — before the
        // per-command IO is installed — so without a scope here those reads
        // panic. WORKSPACE content genuinely determines the graph (external repos),
        // so install a real tracked IO and record into it: reuse the already-
        // installed command scope if one is up (nested WORKSPACE under a command),
        // otherwise build one from the process CommandSemantics snapshot — the same
        // real IO `BuildOrchestrator::ensure_tracked_io` constructs, not a fabricated
        // one. The synchronous eval and genuine repository-rule callback share
        // this scope, including tracked patch/overlay/configuration inputs.
        let ws_io: rush_incremental::tracked::SharedTrackedIo =
            match rush_incremental::tracked::current() {
                Some(io) => io,
                None => {
                    let semantics = rush_command_core::CommandSemantics::from_process();
                    Arc::new(rush_incremental::RealTrackedIo::new(
                        semantics,
                        &self.workspace_root,
                    ))
                }
            };
        rush_incremental::tracked::scope(ws_io, || evaluator.eval_program(&program))?;

        // Process collected rule calls
        let mut discovered = DiscoveredRepositories::new();
        for rule_call in evaluator.get_rule_calls() {
            self.process_rule_call(rule_call, &mut discovered)?;
        }

        Ok(discovered)
    }

    fn process_rule_call(
        &self,
        rule_call: &RuleCall,
        discovered: &mut DiscoveredRepositories,
    ) -> Result<()> {
        match rule_call.rule_type.as_str() {
            "workspace" => {
                if let Ok(name) = self.extract_string(&rule_call.kwargs, "name") {
                    discovered.workspace_name = Some(name);
                }
            }
            "http_archive" | "tf_http_archive" => {
                let name = self.extract_string(&rule_call.kwargs, "name")?;
                let urls = self
                    .extract_string_list(&rule_call.kwargs, "urls")
                    .unwrap_or_default();
                let sha256 = self.extract_string(&rule_call.kwargs, "sha256").ok();
                let strip_prefix = self.extract_string(&rule_call.kwargs, "strip_prefix").ok();
                let build_file = self.extract_string(&rule_call.kwargs, "build_file").ok();
                let repo_mapping = self
                    .extract_string_dict(&rule_call.kwargs, "repo_mapping")
                    .unwrap_or_default();

                let archive = HttpArchive {
                    name,
                    urls,
                    sha256,
                    strip_prefix,
                    build_file,
                    build_file_content: None,
                    repo_mapping,
                };

                discovered.http_archives.push(archive);
            }
            "git_repository" => {
                let name = self.extract_string(&rule_call.kwargs, "name")?;
                let remote = self.extract_string(&rule_call.kwargs, "remote")?;
                let commit = self.extract_string(&rule_call.kwargs, "commit").ok();
                let tag = self.extract_string(&rule_call.kwargs, "tag").ok();
                let branch = self.extract_string(&rule_call.kwargs, "branch").ok();
                let repo_mapping = self
                    .extract_string_dict(&rule_call.kwargs, "repo_mapping")
                    .unwrap_or_default();

                let repo = GitRepository {
                    name,
                    remote,
                    commit,
                    tag,
                    branch,
                    repo_mapping,
                };
                discovered.git_repositories.push(repo);
            }
            "local_repository" => {
                let name = self.extract_string(&rule_call.kwargs, "name")?;
                let path = self.extract_string(&rule_call.kwargs, "path")?;
                let repo_mapping = self
                    .extract_string_dict(&rule_call.kwargs, "repo_mapping")
                    .unwrap_or_default();

                let repo = LocalRepository {
                    name,
                    path,
                    repo_mapping,
                };
                discovered.local_repositories.push(repo);
            }
            _ => {
                // Ignore other rules
            }
        }
        Ok(())
    }

    fn extract_string(&self, kwargs: &HashMap<String, Value>, key: &str) -> Result<String> {
        if let Some(Value::String(s)) = kwargs.get(key) {
            Ok(s.clone())
        } else {
            Err(anyhow::anyhow!(
                "Missing or invalid string attribute: {}",
                key
            ))
        }
    }

    fn extract_string_list(
        &self,
        kwargs: &HashMap<String, Value>,
        key: &str,
    ) -> Option<Vec<String>> {
        if let Some(Value::List(items)) = kwargs.get(key) {
            let mut strings = Vec::new();
            for item in items {
                if let Value::String(s) = item {
                    strings.push(s.clone());
                }
            }
            Some(strings)
        } else {
            None
        }
    }

    fn extract_string_dict(
        &self,
        kwargs: &HashMap<String, Value>,
        key: &str,
    ) -> Option<HashMap<String, String>> {
        if let Some(Value::Dict(items)) = kwargs.get(key) {
            let mut dict = HashMap::new();
            for (k, v) in items {
                if let Value::String(s) = v {
                    dict.insert(k.clone(), s.clone());
                }
            }
            Some(dict)
        } else {
            None
        }
    }
}
