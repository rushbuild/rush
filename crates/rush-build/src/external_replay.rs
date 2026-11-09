//! Make-free execution of a validated external analysis snapshot.
//!
//! The analyzer owns graph discovery only. Replay resolves current input
//! values, computes Rush-owned action keys, and sends every action through
//! [`RushActionManager`] so local execution, admission, action caching, output
//! certification, and CAS restoration remain shared with native Rush builds.

use crate::action_manager::{ActionCachePolicy, BeforeOutputMutation, RushActionManager};
use crate::external_action_identity::{external_action_key_from_values, ExternalExecutableWitness};
use crate::external_analysis_snapshot::{
    runtime_directory_checkpoints, runtime_directory_observation_profile,
    RuntimeDirectoryObservationProfile, ExternalAction, ExternalActionGraph, ExternalAnalysisSnapshot,
    ExternalEmptyOutputInput, ExternalInitialInputValue, ExternalInputKind,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use inotify::{Inotify, WatchDescriptor, WatchMask};
use rush_action::action_manifest::ManifestResourceClass;
use rush_action::external_action_key::{
    ExternalArgumentKey, ExternalArgumentPart, ExternalCredentialState, ExternalInputKey,
    ExternalInputValue, ExternalOutputKind, ExternalPathKey, ExternalSignalState,
};
use rush_action::ids::{ActionEnvPolicy, DeclaredActionId, MaterializedCommand, TargetId};
use rush_action::{Action, ActionId, ActionResult, Artifact, Command};
use rush_cache::ActionCache;
use rush_exec::local_action_runner::{
    DefaultLocalActionRunner, ExecutionRequest, LocalActionRunner, RuntimeDirectoryObservation,
    ObservedActionExecutionResult,
};
use rush_exec::local_isolation::{LocalDescriptorTopology, LocalExecutionIsolation};
use rush_exec::ExecutionStrategy;
use rush_resource::{ResourceClass, ResourceManager};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const OUTPUT_TAIL_LIMIT: usize = 1024 * 1024;
static KCONFIG_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static INITIAL_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Result of one Make-free replay pass.
#[derive(Debug)]
pub enum ExternalReplayStatus {
    /// Every action needed for the requested outputs completed.
    Complete {
        /// Number of action-cache or CAS hits.
        cache_hits: usize,
        /// Number of actions that executed.
        executed: usize,
    },
    /// A captured continuation value changed and requires clean recapture.
    AnalysisMismatch {
        /// Zero-based checkpoint that rejected the retained continuation.
        barrier_index: usize,
        /// Number of action-cache or CAS hits before the barrier.
        cache_hits: usize,
        /// Number of actions that executed before the barrier.
        executed: usize,
    },
}

#[derive(Clone)]
struct ResolvedExternalAction {
    logical_id: String,
    mnemonic: String,
    commands: Vec<ResolvedExternalCommand>,
    signal_state: ExternalSignalState,
    file_creation_mask: u32,
    credentials: ExternalCredentialState,
    outputs: Vec<(PathBuf, ExternalOutputKind, u32)>,
    input_preconditions: Vec<ResolvedExternalInputPrecondition>,
    runtime_directory_checkpoints: Vec<ResolvedRuntimeDirectoryCheckpoint>,
    directory_observation_profile: RuntimeDirectoryObservationProfile,
    direct_runtime_directory_opening: Option<RuntimeDirectoryObservation>,
    resource_class: ResourceClass,
    isolation: Option<LocalExecutionIsolation>,
    directory_materialization: bool,
}

#[derive(Clone)]
struct ResolvedRuntimeDirectoryCheckpoint {
    path: PathBuf,
    members: Vec<Vec<u8>>,
    observation_ordinal: u64,
    before_actions: Vec<String>,
    after_actions: Vec<String>,
}

#[derive(Clone)]
struct ResolvedExternalInputPrecondition {
    path: PathBuf,
    kind: ExternalInputKind,
    expected: ExternalInputValue,
}

#[derive(Clone)]
struct ResolvedExternalCommand {
    executable: PathBuf,
    argv0: String,
    argv: Vec<String>,
    environment: Vec<(String, String)>,
    working_directory: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ObservedInputKind {
    File,
    Executable,
    Symlink,
    Missing,
    DirectoryListing,
    Directory,
}

impl ObservedInputKind {
    fn from_external(kind: ExternalInputKind) -> Option<Self> {
        match kind {
            ExternalInputKind::File => Some(Self::File),
            ExternalInputKind::Symlink => Some(Self::Symlink),
            ExternalInputKind::Tree => None,
            ExternalInputKind::Missing => Some(Self::Missing),
            ExternalInputKind::DirectoryListing => Some(Self::DirectoryListing),
            ExternalInputKind::Directory => Some(Self::Directory),
        }
    }
}

/// One replay's memo of leaf input values, guarded by inotify continuity.
///
/// Stable source/host values and generated `obj` values use separate watcher
/// domains. A legitimate action execution, CAS restoration, or mode repair may
/// mutate `obj`; that permanently disables only the generated-value memo while
/// the stable-value fence remains authoritative for the rest of the replay.
struct ExternalObservationSession {
    stable: ExternalObservationMemo,
    generated: ExternalObservationMemo,
    requests: usize,
    reused: usize,
}

struct ExternalObservationMemo {
    watcher: Option<Inotify>,
    watches: Vec<(WatchDescriptor, PathBuf)>,
    observed_paths: Vec<(PathBuf, ObservedInputKind)>,
    roots: Vec<ObservedRoot>,
    cacheable: BTreeMap<ExternalPathKey, BTreeSet<ObservedInputKind>>,
    values: BTreeMap<ExternalPathKey, BTreeMap<ObservedInputKind, ExternalInputValue>>,
    description: &'static str,
}

#[derive(Clone)]
struct ObservedRoot {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl ObservedRoot {
    fn new(path: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;

        let metadata = std::fs::metadata(path).ok()?;
        metadata.file_type().is_dir().then(|| Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn unchanged(&self) -> bool {
        use std::os::unix::fs::MetadataExt;

        std::fs::metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_dir()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        })
    }
}

impl ExternalObservationSession {
    fn new(graph: &ExternalActionGraph) -> Self {
        // Keep references while deduplicating: a real Kbuild graph repeats the
        // same headers many times, so cloning on every occurrence would make
        // watcher setup scale with the expanded edge count.
        let mut stable_candidates = BTreeSet::new();
        let mut generated_candidates = BTreeSet::new();
        for action in &graph.actions {
            for input in &action.normal_inputs {
                let Some(kind) = ObservedInputKind::from_external(input.kind) else {
                    continue;
                };
                if input.path.root == "obj" {
                    generated_candidates.insert((&input.path, kind));
                } else {
                    stable_candidates.insert((&input.path, kind));
                }
            }
            for command in &action.commands {
                if command.executable.root == "obj" {
                    generated_candidates
                        .insert((&command.executable, ObservedInputKind::Executable));
                } else {
                    stable_candidates.insert((&command.executable, ObservedInputKind::Executable));
                }
            }
        }

        let (stable, generated) = std::thread::scope(|scope| {
            let stable = scope.spawn(|| {
                ExternalObservationMemo::new(graph, stable_candidates, "external leaf input")
            });
            let generated = ExternalObservationMemo::new(
                graph,
                generated_candidates,
                "generated external input",
            );
            (
                stable.join().expect("stable observation setup panicked"),
                generated,
            )
        });
        Self {
            stable,
            generated,
            requests: 0,
            reused: 0,
        }
    }

    fn fence(&mut self) -> Result<()> {
        self.stable.fence()?;
        self.generated.fence()
    }

    /// Close the generated observation window before a legitimate mutation.
    ///
    /// Fence both domains first: an earlier foreign write cannot be excused by
    /// a later scheduled writer. Discard generated memo values before writing,
    /// so concurrently completing cache hits cannot reject the producer's own
    /// writes or reuse its pre-mutation values. Stable inputs stay fenced.
    fn before_output_mutation(&mut self) -> Result<()> {
        self.fence()?;
        self.generated.disable();
        Ok(())
    }

    fn observe(
        &mut self,
        graph: &ExternalActionGraph,
        path: &ExternalPathKey,
        kind: ExternalInputKind,
    ) -> Result<ExternalInputValue> {
        self.requests += 1;
        let memo = if path.root == "obj" {
            &mut self.generated
        } else {
            &mut self.stable
        };
        let (value, reused) = memo.observe(graph, path, kind)?;
        self.reused += usize::from(reused);
        Ok(value)
    }

    fn observe_executable(
        &mut self,
        graph: &ExternalActionGraph,
        path: &ExternalPathKey,
    ) -> Result<ExternalInputValue> {
        self.requests += 1;
        let memo = if path.root == "obj" {
            &mut self.generated
        } else {
            &mut self.stable
        };
        let (value, reused) = memo.observe_executable(graph, path)?;
        self.reused += usize::from(reused);
        Ok(value)
    }

    fn report_if_enabled(&self) {
        if std::env::var("RUSH_EXTERNAL_OBSERVATION_STATS").as_deref() == Ok("1") {
            eprintln!(
                "external observations: requests={} reused={} observed={}",
                self.requests,
                self.reused,
                self.requests - self.reused
            );
        }
    }
}

fn lock_observations(
    observations: &Mutex<ExternalObservationSession>,
) -> Result<MutexGuard<'_, ExternalObservationSession>> {
    observations.lock().map_err(|_| anyhow::anyhow!("external observations are poisoned"))
}

impl ExternalObservationMemo {
    fn new<'a>(
        graph: &ExternalActionGraph,
        candidates: BTreeSet<(&'a ExternalPathKey, ObservedInputKind)>,
        description: &'static str,
    ) -> Self {
        let Ok(mut watcher) = Inotify::init() else {
            return Self::disabled(description);
        };
        // CLOSE_WRITE alone is not a value change: a writable open followed by
        // close can emit it without writing. MODIFY covers actual byte changes,
        // while ATTRIB covers the permission bits represented in action keys.
        let mask = WatchMask::MODIFY
            | WatchMask::ATTRIB
            | WatchMask::CREATE
            | WatchMask::DELETE
            | WatchMask::MOVED_FROM
            | WatchMask::MOVED_TO
            | WatchMask::DELETE_SELF
            | WatchMask::MOVE_SELF;
        let mut watched_paths = BTreeMap::new();
        let mut watches = Vec::new();
        let mut observed_paths = Vec::new();
        let mut roots = BTreeMap::<PathBuf, ObservedRoot>::new();
        let mut cacheable = BTreeMap::<ExternalPathKey, BTreeSet<ObservedInputKind>>::new();
        for (path_key, kind) in candidates {
            let Ok(path) = resolve_path(graph, path_key) else {
                continue;
            };
            // procfs values are generated by the kernel and do not provide
            // ordinary inotify change notification. Re-observe the value on
            // every request instead of allowing a watcher-backed memo hit.
            if path == Path::new("/proc/filesystems") {
                continue;
            }
            let Some(root) = graph.roots.get(&path_key.root) else {
                continue;
            };
            let root_identity = if let Some(identity) = roots.get(root) {
                identity.clone()
            } else {
                let Some(identity) = ObservedRoot::new(root) else {
                    continue;
                };
                identity
            };
            let ancestors_watched = arm_observation_ancestor_watches(
                &mut watcher,
                &mut watched_paths,
                &mut watches,
                root,
                &path,
                kind == ObservedInputKind::Missing,
                mask,
            );
            if !ancestors_watched || !observed_kind_matches(&path, kind) {
                continue;
            }
            let leaf_watched = !needs_leaf_watch(kind)
                || add_observation_watch(
                    &mut watcher,
                    &mut watched_paths,
                    &mut watches,
                    &path,
                    mask,
                );
            if leaf_watched {
                cacheable.entry(path_key.clone()).or_default().insert(kind);
                if !observed_paths.contains(&(path.clone(), kind)) {
                    observed_paths.push((path, kind));
                }
                roots.entry(root.clone()).or_insert(root_identity);
            }
        }
        if cacheable.is_empty() {
            Self::disabled(description)
        } else {
            Self {
                watcher: Some(watcher),
                watches,
                observed_paths,
                roots: roots.into_values().collect(),
                cacheable,
                values: BTreeMap::new(),
                description,
            }
        }
    }

    fn disabled(description: &'static str) -> Self {
        Self {
            watcher: None,
            watches: Vec::new(),
            observed_paths: Vec::new(),
            roots: Vec::new(),
            cacheable: BTreeMap::new(),
            values: BTreeMap::new(),
            description,
        }
    }

    fn disable(&mut self) {
        self.watcher = None;
        self.watches.clear();
        self.observed_paths.clear();
        self.roots.clear();
        self.cacheable.clear();
        self.values.clear();
    }

    fn fence(&mut self) -> Result<()> {
        let Some(watcher) = self.watcher.as_mut() else {
            return Ok(());
        };
        if self.roots.iter().any(|root| !root.unchanged()) {
            anyhow::bail!(
                "{} root changed during replay; refusing cached values",
                self.description
            );
        }
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match watcher.read_events(&mut buffer) {
                Ok(mut events) => {
                    for event in &mut events {
                        let watched = self
                            .watches
                            .iter()
                            .find(|(descriptor, _)| descriptor == &event.wd)
                            .map(|(_, path)| path.as_path());
                        let changed = match (watched, event.name) {
                            (Some(path), Some(name)) => Some(path.join(name)),
                            (Some(path), None) => Some(path.to_path_buf()),
                            (None, _) => None,
                        };
                        if !event_changes_observed_path(
                            &self.observed_paths,
                            watched,
                            changed.as_deref(),
                        ) {
                            continue;
                        }
                        anyhow::bail!(
                            "{} changed during replay at {:?} ({:?}); refusing cached values",
                            self.description,
                            changed,
                            event.mask
                        );
                    }
                    if self.roots.iter().any(|root| !root.unchanged()) {
                        anyhow::bail!(
                            "{} root changed during replay; refusing cached values",
                            self.description
                        );
                    }
                    return Ok(());
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if self.roots.iter().any(|root| !root.unchanged()) {
                        anyhow::bail!(
                            "{} root changed during replay; refusing cached values",
                            self.description
                        );
                    }
                    return Ok(());
                }
                Err(error) => {
                    return Err(error).context("drain external replay input watcher");
                }
            }
        }
    }

    fn observe(
        &mut self,
        graph: &ExternalActionGraph,
        path: &ExternalPathKey,
        kind: ExternalInputKind,
    ) -> Result<(ExternalInputValue, bool)> {
        let Some(observed_kind) = ObservedInputKind::from_external(kind) else {
            return Ok((
                observe_resolved_input(&resolve_path(graph, path)?, kind)?,
                false,
            ));
        };
        if self
            .cacheable
            .get(path)
            .is_some_and(|kinds| kinds.contains(&observed_kind))
        {
            if let Some(value) = self
                .values
                .get(path)
                .and_then(|values| values.get(&observed_kind))
            {
                return Ok((value.clone(), true));
            }
            let value = observe_resolved_input(&resolve_path(graph, path)?, kind)?;
            self.values
                .entry(path.clone())
                .or_default()
                .insert(observed_kind, value.clone());
            Ok((value, false))
        } else {
            Ok((
                observe_resolved_input(&resolve_path(graph, path)?, kind)?,
                false,
            ))
        }
    }

    fn observe_executable(
        &mut self,
        graph: &ExternalActionGraph,
        path: &ExternalPathKey,
    ) -> Result<(ExternalInputValue, bool)> {
        let kind = ObservedInputKind::Executable;
        if self
            .cacheable
            .get(path)
            .is_some_and(|kinds| kinds.contains(&kind))
        {
            if let Some(value) = self.values.get(path).and_then(|values| values.get(&kind)) {
                return Ok((value.clone(), true));
            }
            let value = observe_executable_value(&resolve_path(graph, path)?)?;
            self.values
                .entry(path.clone())
                .or_default()
                .insert(kind, value.clone());
            Ok((value, false))
        } else {
            Ok((
                observe_executable_value(&resolve_path(graph, path)?)?,
                false,
            ))
        }
    }
}

fn event_changes_observed_path(
    observed_paths: &[(PathBuf, ObservedInputKind)],
    watched: Option<&Path>,
    changed: Option<&Path>,
) -> bool {
    observed_paths.iter().any(|(observed, kind)| {
        changed.is_some_and(|changed| observed == changed || observed.starts_with(changed))
            || watched.is_some_and(|watched| {
                observed == watched && *kind == ObservedInputKind::DirectoryListing
            })
    })
}

fn arm_observation_ancestor_watches(
    watcher: &mut Inotify,
    watched_paths: &mut BTreeMap<PathBuf, bool>,
    watches: &mut Vec<(WatchDescriptor, PathBuf)>,
    root: &Path,
    path: &Path,
    allow_missing_ancestor: bool,
    mask: WatchMask,
) -> bool {
    // Root identity is checked at every fence. Below it, watch every directory
    // component down to the leaf's parent so replacing an intermediate entry
    // invalidates the memo. Do not memoize through a symlinked intermediate
    // directory: its target ancestry would otherwise escape this watch chain.
    let mut ancestors = if path == root {
        vec![root]
    } else {
        path.ancestors()
            .skip(1)
            .take_while(|ancestor| ancestor.starts_with(root))
            .collect::<Vec<_>>()
    };
    if ancestors.last().copied() != Some(root) {
        return false;
    }
    ancestors.reverse();
    let mut complete = true;
    for ancestor in ancestors {
        if ancestor != root {
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) if metadata.file_type().is_dir() => {}
                Err(error) if allow_missing_ancestor && error.kind() == ErrorKind::NotFound => {
                    // The deepest existing directory is already watched. Any
                    // future resolution of this missing path must first create
                    // its next component there, which generates CREATE or MOVE.
                    break;
                }
                _ => return false,
            }
        }
        complete &= add_observation_watch(watcher, watched_paths, watches, ancestor, mask);
    }
    complete
}

fn needs_leaf_watch(kind: ObservedInputKind) -> bool {
    matches!(
        kind,
        ObservedInputKind::File
            | ObservedInputKind::Executable
            | ObservedInputKind::DirectoryListing
            | ObservedInputKind::Directory
    )
}

fn add_observation_watch(
    watcher: &mut Inotify,
    watched_paths: &mut BTreeMap<PathBuf, bool>,
    watches: &mut Vec<(WatchDescriptor, PathBuf)>,
    path: &Path,
    mask: WatchMask,
) -> bool {
    if let Some(success) = watched_paths.get(path) {
        return *success;
    }
    let descriptor = watcher.add_watch(path, mask);
    let success = descriptor.is_ok();
    if let Ok(descriptor) = descriptor {
        watches.push((descriptor, path.to_path_buf()));
    }
    watched_paths.insert(path.to_path_buf(), success);
    success
}

fn observed_kind_matches(path: &Path, kind: ObservedInputKind) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => match kind {
            ObservedInputKind::File | ObservedInputKind::Executable => {
                metadata.file_type().is_file()
            }
            ObservedInputKind::Symlink => metadata.file_type().is_symlink(),
            ObservedInputKind::Missing => false,
            ObservedInputKind::DirectoryListing | ObservedInputKind::Directory => {
                metadata.file_type().is_dir()
            }
        },
        Err(error) if error.kind() == ErrorKind::NotFound => kind == ObservedInputKind::Missing,
        Err(_) => false,
    }
}

struct ExternalExecutionStrategy {
    actions: Mutex<HashMap<ActionId, ResolvedExternalAction>>,
    runner: Arc<dyn LocalActionRunner>,
}

impl ExternalExecutionStrategy {
    fn new(runner: Arc<dyn LocalActionRunner>) -> Self {
        Self {
            actions: Mutex::new(HashMap::new()),
            runner,
        }
    }

    fn register(&self, id: ActionId, action: ResolvedExternalAction) -> Result<()> {
        let mut actions = self
            .actions
            .lock()
            .map_err(|_| anyhow::anyhow!("external action registry is poisoned"))?;
        actions.insert(id, action);
        Ok(())
    }
}

#[async_trait]
impl ExecutionStrategy for ExternalExecutionStrategy {
    async fn execute(&self, action: &Action) -> Result<ActionResult> {
        let resolved = self
            .actions
            .lock()
            .map_err(|_| anyhow::anyhow!("external action registry is poisoned"))?
            .get(&action.id)
            .cloned()
            .context("canonical external action was not registered")?;
        let started = Instant::now();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = 0;
        let mut runtime_directory_observations = Vec::new();
        let direct_runtime_checkpoints = direct_runtime_directory_checkpoint_validation(
            &resolved.runtime_directory_checkpoints,
            &resolved.outputs,
            resolved.directory_observation_profile,
        );

        for precondition in &resolved.input_preconditions {
            let actual = observe_resolved_input(&precondition.path, precondition.kind)
                .with_context(|| {
                    format!(
                        "revalidate external input before executing {:?}",
                        resolved.logical_id
                    )
                })?;
            if actual != precondition.expected {
                anyhow::bail!(
                    "captured empty-output input changed before external action {:?}",
                    resolved.logical_id
                );
            }
        }
        if direct_runtime_checkpoints {
            runtime_directory_observations.push(
                resolved
                    .direct_runtime_directory_opening
                    .clone()
                    .context("direct runtime directory opening was not staged")?,
            );
        }

        if resolved.directory_materialization {
            materialize_declared_mkdir(&resolved)?;
        }
        for (index, command) in resolved
            .commands
            .iter()
            .enumerate()
            .filter(|_| !resolved.directory_materialization)
        {
            let request = ExecutionRequest {
                declared_action_id: DeclaredActionId(index as u32),
                mnemonic: resolved.mnemonic.clone(),
                resource_class: Some(resolved.resource_class),
                owner_target: TargetId(0),
                command: MaterializedCommand {
                    executable: command.executable.clone(),
                    argv0: Some(command.argv0.clone()),
                    argv: command.argv.clone(),
                    env: command.environment.clone(),
                    working_dir: Some(command.working_directory.clone()),
                    env_policy: ActionEnvPolicy::ExplicitOnly,
                },
                declared_inputs: Vec::new(),
                declared_outputs: Vec::new(),
                timeout: None,
                log_command_index: Some(index as u32),
                remove_stale_archive_before_execute: false,
                terminate_process_group_on_drop: true,
                stdin_null: true,
                signal_state: Some(resolved.signal_state),
                file_creation_mask: Some(resolved.file_creation_mask),
                credentials: Some(resolved.credentials.clone()),
                isolation: resolved.isolation.clone(),
            };
            let result = if resolved.runtime_directory_checkpoints.is_empty()
                || direct_runtime_checkpoints
            {
                self.runner.execute(request).await
            } else {
                let observed = self.runner.execute_observed(request).await?;
                let (execution, directory_observations) = select_runtime_directory_observations(
                    resolved.directory_observation_profile, observed,
                )?;
                runtime_directory_observations.extend(directory_observations);
                Ok(execution)
            }
            .with_context(|| {
                format!(
                    "execute external action {:?} command {index}",
                    resolved.logical_id
                )
            })?;
            append_tail(&mut stdout, &result.stdout_tail);
            append_tail(&mut stderr, &result.stderr_tail);
            exit_code = result.exit_code;
            if exit_code != 0 {
                break;
            }
        }

        if exit_code == 0 {
            if direct_runtime_checkpoints {
                runtime_directory_observations.push(observe_runtime_directory(
                    &resolved.runtime_directory_checkpoints[1].path,
                )?);
            }
            validate_runtime_directory_observations(
                &resolved.runtime_directory_checkpoints,
                &runtime_directory_observations,
            )
            .with_context(|| {
                format!(
                    "validate runtime directory checkpoints for external action {:?}",
                    resolved.logical_id
                )
            })?;
            normalize_and_validate_outputs(&resolved.outputs)?;
        }
        Ok(ActionResult {
            action_id: action.id.clone(),
            exit_code,
            stdout,
            stderr,
            outputs: action.outputs.clone(),
            output_records: Vec::new(),
            duration: started.elapsed(),
            cached: false,
        })
    }

    fn can_execute(&self, action: &Action) -> bool {
        self.actions
            .lock()
            .map(|actions| actions.contains_key(&action.id))
            .unwrap_or(false)
    }

    fn name(&self) -> &str {
        "external-replay"
    }
}

/// Replays an authenticated action DAG without invoking its analyzer.
pub struct ExternalReplayEngine {
    snapshot: ExternalAnalysisSnapshot,
    strategy: Arc<ExternalExecutionStrategy>,
    manager: Arc<RushActionManager>,
    kconfig_values: BTreeMap<String, Vec<u8>>,
    empty_output_inputs: BTreeMap<ExternalPathKey, ExternalEmptyOutputInput>,
    #[cfg(test)]
    completion_observer: Option<Arc<dyn Fn(&str, bool) + Send + Sync>>,
    #[cfg(test)]
    before_manager_observer: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

impl ExternalReplayEngine {
    /// Build a replay engine around Rush's normal action manager.
    pub fn new(
        snapshot: ExternalAnalysisSnapshot,
        cache: Arc<ActionCache>,
        resource_manager: Arc<ResourceManager>,
    ) -> Result<Self> {
        Self::with_runner(
            snapshot,
            cache,
            resource_manager,
            Arc::new(DefaultLocalActionRunner),
        )
    }

    /// Build a replay engine with an explicit CAS paired to the action cache.
    pub fn with_cas(
        snapshot: ExternalAnalysisSnapshot,
        cache: Arc<ActionCache>,
        resource_manager: Arc<ResourceManager>,
        cas: Arc<rush_cache::cas::CasStore>,
    ) -> Result<Self> {
        Self::with_runner_and_cas(
            snapshot,
            cache,
            resource_manager,
            Arc::new(DefaultLocalActionRunner),
            cas,
        )
    }

    /// Construct with a replaceable local runner for focused tests.
    pub fn with_runner(
        snapshot: ExternalAnalysisSnapshot,
        cache: Arc<ActionCache>,
        resource_manager: Arc<ResourceManager>,
        runner: Arc<dyn LocalActionRunner>,
    ) -> Result<Self> {
        snapshot.validate_structure()?;
        validate_external_action_isolation(&snapshot.graph)?;
        let empty_output_inputs = prepare_initial_state(&snapshot.graph)?;
        let kconfig_values = load_kconfig_values(&snapshot.graph)?;
        let strategy = Arc::new(ExternalExecutionStrategy::new(runner));
        let manager = Arc::new(RushActionManager::new(
            cache,
            strategy.clone(),
            resource_manager,
        ));
        Ok(Self {
            snapshot,
            strategy,
            manager,
            kconfig_values,
            empty_output_inputs,
            #[cfg(test)]
            completion_observer: None,
            #[cfg(test)]
            before_manager_observer: None,
        })
    }

    fn with_runner_and_cas(
        snapshot: ExternalAnalysisSnapshot,
        cache: Arc<ActionCache>,
        resource_manager: Arc<ResourceManager>,
        runner: Arc<dyn LocalActionRunner>,
        cas: Arc<rush_cache::cas::CasStore>,
    ) -> Result<Self> {
        snapshot.validate_structure()?;
        validate_external_action_isolation(&snapshot.graph)?;
        let empty_output_inputs = prepare_initial_state(&snapshot.graph)?;
        let kconfig_values = load_kconfig_values(&snapshot.graph)?;
        let strategy = Arc::new(ExternalExecutionStrategy::new(runner));
        let manager = Arc::new(RushActionManager::new_with_cas(
            cache,
            strategy.clone(),
            resource_manager,
            Some(cas),
        ));
        Ok(Self {
            snapshot,
            strategy,
            manager,
            kconfig_values,
            empty_output_inputs,
            #[cfg(test)]
            completion_observer: None,
            #[cfg(test)]
            before_manager_observer: None,
        })
    }

    /// Execute the requested-output ancestor closure in stable topological
    /// order. A returned barrier is a request to resume external analysis,
    /// never permission to execute a partially discovered graph.
    pub async fn replay(&self) -> Result<ExternalReplayStatus> {
        let diagnostics = std::env::var("RUSH_EXTERNAL_OBSERVATION_STATS").as_deref() == Ok("1");
        let replay_started = Instant::now();
        let order = requested_topological_order(&self.snapshot.graph)?;
        let observations = Arc::new(Mutex::new(ExternalObservationSession::new(&self.snapshot.graph)));
        let before_output_mutation: Arc<BeforeOutputMutation> = {
            let observations = observations.clone();
            Arc::new(move || lock_observations(&observations)?.before_output_mutation())
        };
        let action_stages = analysis_action_stages(&self.snapshot.graph);
        let mut next_barrier = 0usize;
        let mut cache_hits = 0usize;
        let mut executed = 0usize;
        let mut completed = BTreeSet::new();
        let mut stages = BTreeMap::<usize, Vec<usize>>::new();
        for index in order {
            let external = &self.snapshot.graph.actions[index];
            stages
                .entry(action_stages[external.logical_id.as_str()])
                .or_default()
                .push(index);
        }

        let mut direct_runtime_directory_opening_stages =
            direct_runtime_directory_opening_stages(&self.snapshot.graph, &stages)?;
        let mut direct_runtime_directory_checkpoint_contracts = BTreeMap::new();
        for index in direct_runtime_directory_opening_stages
            .values()
            .flatten()
            .copied()
        {
            direct_runtime_directory_checkpoint_contracts.insert(
                index,
                resolve_runtime_directory_checkpoints(
                    &self.snapshot.graph,
                    &self.snapshot.graph.actions[index],
                )?,
            );
        }
        let mut direct_runtime_directory_openings = BTreeMap::new();
        let mut active_direct_runtime_directory_openings = BTreeSet::new();
        for (stage, mut pending) in stages {
            let stage_started = Instant::now();
            let stage_action_count = pending.len();
            let mut launches = 0usize;
            while next_barrier < stage {
                let matched = {
                    let mut observations = lock_observations(&observations)?;
                    self.analysis_barrier_matches(next_barrier, &mut observations)
                };
                if !matched {
                    lock_observations(&observations)?.report_if_enabled();
                    return Ok(ExternalReplayStatus::AnalysisMismatch {
                        barrier_index: next_barrier,
                        cache_hits,
                        executed,
                    });
                }
                next_barrier += 1;
            }

            let mut running = tokio::task::JoinSet::new();
            active_direct_runtime_directory_openings.extend(
                direct_runtime_directory_opening_stages
                .remove(&stage)
                    .unwrap_or_default(),
            );
            while !pending.is_empty() || !running.is_empty() {
                let openable = active_direct_runtime_directory_openings
                    .iter()
                    .copied()
                    .filter(|index| {
                        direct_runtime_directory_checkpoint_contracts[index][0]
                            .before_actions
                            .iter()
                            .all(|action| completed.contains(action))
                    })
                    .collect::<Vec<_>>();
                for index in openable {
                let external = &self.snapshot.graph.actions[index];
                    let checkpoints = &direct_runtime_directory_checkpoint_contracts[&index];
                    if checkpoints[0]
                        .after_actions
                        .iter()
                        .any(|action| completed.contains(action))
                    {
                        anyhow::bail!(
                            "runtime directory checkpoint successor completed before opening for external action {:?}",
                            external.logical_id,
                        );
                    }
                    let opening = observe_runtime_directory(&checkpoints[0].path)?;
                    validate_runtime_directory_observations(
                        &checkpoints[..1],
                        std::slice::from_ref(&opening),
                    )
                    .with_context(|| {
                        format!(
                            "validate opening runtime directory checkpoint for external action {:?} at observation ordinal {}",
                            external.logical_id,
                            checkpoints[0].observation_ordinal,
                        )
                    })?;
                    direct_runtime_directory_openings.insert(index, opening);
                    active_direct_runtime_directory_openings.remove(&index);
                }
                let ready = pending
                    .iter()
                    .copied()
                    .filter(|index| {
                        let action = &self.snapshot.graph.actions[*index];
                        action
                            .dependencies
                            .iter()
                            .all(|dependency| completed.contains(dependency))
                            && !active_direct_runtime_directory_openings
                                .iter()
                                .any(|observer| {
                                    let checkpoint =
                                        &direct_runtime_directory_checkpoint_contracts[observer][0];
                                    *observer == *index
                                        || checkpoint
                                            .after_actions
                                            .binary_search(&action.logical_id)
                                            .is_ok()
                                })
                    })
                    .collect::<Vec<_>>();
                if ready.is_empty() && running.is_empty() {
                    anyhow::bail!(
                        "external replay stage has no dependency-ready action; pending checkpoint openings={:?}",
                        active_direct_runtime_directory_openings,
                    );
                }
                let mut checkpointed = None;
                for index in &ready {
                    if !runtime_directory_checkpoints(
                        &self.snapshot.graph.actions[*index].execution_properties,
                    )?
                    .is_empty()
                    {
                        checkpointed = Some(*index);
                        break;
                    }
                }
                let ready = if checkpointed.is_some() && !running.is_empty() {
                    Vec::new()
                } else {
                    checkpointed.map_or(ready, |index| vec![index])
                };
                if !ready.is_empty() {
                    let ready_set = ready.iter().copied().collect::<BTreeSet<_>>();
                    pending.retain(|index| !ready_set.contains(index));
                    let preparation_started = Instant::now();
                    for index in &ready {
                        let index = *index;
                        let action_started = Instant::now();
                        let external = &self.snapshot.graph.actions[index];
                        let (action, mut resolved) = {
                            let mut observations = lock_observations(&observations)?;
                            observations.fence().with_context(|| {
                                format!("validate inputs before external action {:?}", external.logical_id)
                            })?;
                            let prepared = self.prepare_action(external, &mut observations)?;
                            observations.fence().with_context(|| {
                                format!("validate inputs prepared for external action {:?}", external.logical_id)
                            })?;
                            prepared
                        };
                        let direct_runtime_checkpoints =
                            direct_runtime_directory_checkpoint_validation(
                                &resolved.runtime_directory_checkpoints,
                                &resolved.outputs,
                                resolved.directory_observation_profile,
                            );
                        if direct_runtime_checkpoints {
                            resolved.direct_runtime_directory_opening = Some(
                                direct_runtime_directory_openings.remove(&index).with_context(
                                    || {
                                        format!(
                                            "external action {:?} has no staged runtime directory opening",
                                            external.logical_id
                                        )
                                    },
                                )?,
                            );
                        }
                        if diagnostics && !resolved.runtime_directory_checkpoints.is_empty() {
                            eprintln!(
                                "external replay runtime checkpoints: index={index} count={} paths={:?}",
                                resolved.runtime_directory_checkpoints.len(),
                                resolved
                                    .runtime_directory_checkpoints
                                    .iter()
                                    .map(|checkpoint| &checkpoint.path)
                                    .collect::<Vec<_>>()
                            );
                        }
                        let resolved_outputs = resolved.outputs.clone();
                        let missing_output_parents = missing_output_parents(
                            &resolved_outputs,
                            self.snapshot
                                .graph
                                .roots
                                .get("obj")
                                .context("external replay has no output root")?,
                            resolved.file_creation_mask,
                        )?;
                        let direct_runtime_directory_expectations = direct_runtime_checkpoints
                            .then(|| {
                                (
                                    resolved.runtime_directory_checkpoints.clone(),
                                    resolved
                                        .direct_runtime_directory_opening
                                        .clone()
                                        .expect("direct runtime directory opening was staged"),
                                )
                            });
                        self.strategy.register(action.id.clone(), resolved)?;
                        let policy = cache_policy(external);
                        let manager = self.manager.clone();
                        let logical_id = external.logical_id.clone();
                        let before_output_mutation = before_output_mutation.clone();
                        #[cfg(test)]
                        let before_manager_observer = self.before_manager_observer.clone();
                        running.spawn(async move {
                            #[cfg(test)]
                            if let Some(observer) = &before_manager_observer {
                                observer(&logical_id);
                            }
                            let result = manager
                                .execute_canonical_external_action(action, policy, before_output_mutation)
                                .await
                                .with_context(|| format!("replay external action {logical_id:?}"));
                            (
                                index,
                                resolved_outputs,
                                missing_output_parents,
                                direct_runtime_directory_expectations,
                                action_started,
                                result,
                            )
                        });
                    }
                    if diagnostics {
                        eprintln!(
                            "external replay launch: stage={stage} launch={launches} actions={} running={} pending={} prepare_us={} indices={ready:?}",
                            ready.len(),
                            running.len(),
                            pending.len(),
                            preparation_started.elapsed().as_micros(),
                        );
                    }
                    launches += 1;
                }

                let completion_started = Instant::now();
                let (
                    index,
                    resolved_outputs,
                    missing_output_parents,
                    direct_runtime_directory_expectations,
                    action_started,
                    result,
                ) = running
                    .join_next()
                    .await
                    .context("external replay scheduler lost a running action")?
                    .context("external replay action task failed")?;
                let wait_elapsed = completion_started.elapsed();
                let finalization_started = Instant::now();
                let external = &self.snapshot.graph.actions[index];
                let result = result?;
                if result.cached {
                    if let Some((checkpoints, opening)) = direct_runtime_directory_expectations {
                        let closing = observe_runtime_directory(&checkpoints[1].path)?;
                        validate_runtime_directory_observations(
                            &checkpoints,
                            &[opening, closing],
                        )
                        .with_context(|| {
                            format!(
                                "validate cached runtime directory checkpoints for external action {:?}",
                                external.logical_id
                            )
                        })?;
                    }
                }
                if result.exit_code != 0 {
                    anyhow::bail!(
                        "external action {:?} exited with status {}: {}",
                        external.logical_id,
                        result.exit_code,
                        String::from_utf8_lossy(&result.stderr)
                    );
                }
                normalize_and_validate_outputs_observed(&resolved_outputs, Some(before_output_mutation.as_ref()))
                    .with_context(|| {
                        format!(
                            "normalize outputs after external action {:?}",
                            external.logical_id
                        )
                    })?;
                if result.cached {
                    normalize_cached_output_parents_observed(&missing_output_parents, Some(before_output_mutation.as_ref())).with_context(|| {
                        format!(
                            "normalize cached output parents after external action {:?}",
                            external.logical_id
                        )
                    })?;
                }
                lock_observations(&observations)?
                    .fence()
                    .with_context(|| {
                        format!(
                            "validate inputs after external action {:?}",
                            external.logical_id
                        )
                    })?;
                if result.cached {
                    cache_hits += 1;
                } else {
                    executed += 1;
                }
                let finalization_elapsed = finalization_started.elapsed();
                if diagnostics {
                    eprintln!(
                        "external replay action: index={index} cached={} manager_us={} total_us={} wait_us={} finalize_us={} running={} pending={} id={}",
                        result.cached,
                        result.duration.as_micros(),
                        action_started.elapsed().as_micros(),
                        wait_elapsed.as_micros(),
                        finalization_elapsed.as_micros(),
                        running.len(),
                        pending.len(),
                        external.logical_id
                    );
                }
                completed.insert(external.logical_id.clone());
                #[cfg(test)]
                if let Some(observer) = &self.completion_observer {
                    observer(&external.logical_id, result.cached);
                }
            }
            if diagnostics {
                eprintln!(
                    "external replay stage: stage={stage} actions={stage_action_count} launches={launches} total_us={}",
                    stage_started.elapsed().as_micros(),
                );
            }
        }
        while next_barrier < self.snapshot.graph.analysis_barriers.len() {
            let matched = {
                let mut observations = lock_observations(&observations)?;
                self.analysis_barrier_matches(next_barrier, &mut observations)
            };
            if !matched {
                lock_observations(&observations)?.report_if_enabled();
                return Ok(ExternalReplayStatus::AnalysisMismatch {
                    barrier_index: next_barrier,
                    cache_hits,
                    executed,
                });
            }
            next_barrier += 1;
        }

        lock_observations(&observations)?.report_if_enabled();
        if diagnostics {
            eprintln!(
                "external replay scheduler: actions={} elapsed_us={}",
                cache_hits + executed,
                replay_started.elapsed().as_micros()
            );
        }
        Ok(ExternalReplayStatus::Complete {
            cache_hits,
            executed,
        })
    }

    fn analysis_barrier_matches(
        &self,
        index: usize,
        observations: &mut ExternalObservationSession,
    ) -> bool {
        let barrier = &self.snapshot.graph.analysis_barriers[index];
        let diagnostics = std::env::var("RUSH_EXTERNAL_OBSERVATION_STATS").as_deref() == Ok("1");
        let started = Instant::now();
        if let Err(error) = observations.fence() {
            if diagnostics {
                eprintln!("external analysis barrier {index} pre-fence failed: {error:#}");
            }
            return false;
        }
        for expected in &barrier.expected_inputs {
            let Some(kind) = input_kind_for_value(&expected.value) else {
                if diagnostics {
                    eprintln!(
                        "external analysis barrier {index} has unsupported expected input {:?}",
                        expected.path
                    );
                }
                return false;
            };
            let actual = match observations.observe(&self.snapshot.graph, &expected.path, kind) {
                Ok(actual) => actual,
                Err(error) => {
                    if diagnostics {
                        eprintln!(
                            "external analysis barrier {index} could not observe {:?}: {error:#}",
                            expected.path
                        );
                    }
                    return false;
                }
            };
            if actual != expected.value {
                if diagnostics {
                    eprintln!(
                        "external analysis barrier {index} input {:?} mismatch: expected {:?}, \
                         actual {:?}",
                        expected.path, expected.value, actual
                    );
                    if kind == ExternalInputKind::DirectoryListing {
                        if let Ok(path) = resolve_path(&self.snapshot.graph, &expected.path) {
                            let members = std::fs::read_dir(path)
                                .ok()
                                .into_iter()
                                .flatten()
                                .filter_map(|entry| entry.ok())
                                .map(|entry| os_bytes(&entry.file_name()))
                                .collect::<Vec<_>>();
                            eprintln!(
                                "external analysis barrier {index} actual directory members: \
                                 {members:?}"
                            );
                        }
                    }
                }
                return false;
            }
        }
        let matched = match observations.fence() {
            Ok(()) => true,
            Err(error) => {
                if diagnostics {
                    eprintln!("external analysis barrier {index} post-fence failed: {error:#}");
                }
                false
            }
        };
        if diagnostics && matched {
            let files = barrier
                .expected_inputs
                .iter()
                .filter(|input| matches!(input.value, ExternalInputValue::File { .. }))
                .count();
            let directories = barrier
                .expected_inputs
                .iter()
                .filter(|input| matches!(input.value, ExternalInputValue::DirectoryListing { .. }))
                .count();
            eprintln!(
                "external analysis barrier: index={index} inputs={} files={files} directories={directories} elapsed_us={}",
                barrier.expected_inputs.len(),
                started.elapsed().as_micros(),
            );
        }
        matched
    }

    fn prepare_action(
        &self,
        external: &ExternalAction,
        observations: &mut ExternalObservationSession,
    ) -> Result<(Action, ResolvedExternalAction)> {
        let commands = external
            .commands
            .iter()
            .map(|command| {
                let executable = resolve_path(&self.snapshot.graph, &command.executable)?;
                let executable_value =
                    observations.observe_executable(&self.snapshot.graph, &command.executable)?;
                let (executable_digest, executable_bit) = match executable_value {
                    ExternalInputValue::File { digest, executable } => (digest, executable),
                    _ => anyhow::bail!("external executable has the wrong shape"),
                };
                Ok((
                    ExternalExecutableWitness {
                        path: command.executable.clone(),
                        value: ExternalInputValue::File {
                            digest: executable_digest,
                            executable: executable_bit,
                        },
                    },
                    ResolvedExternalCommand {
                        executable,
                        argv0: resolve_argument(&self.snapshot.graph, &command.argv0)?,
                        argv: command
                            .argv
                            .iter()
                            .map(|argument| resolve_argument(&self.snapshot.graph, argument))
                            .collect::<Result<Vec<_>>>()?,
                        environment: command
                            .environment
                            .as_ref()
                            .unwrap_or(&external.environment)
                            .iter()
                            .map(|(name, value)| {
                                Ok((
                                    text(name, "environment name")?,
                                    resolve_argument(&self.snapshot.graph, value)?,
                                ))
                            })
                            .collect::<Result<Vec<_>>>()?,
                        working_directory: resolve_path(
                            &self.snapshot.graph,
                            command
                                .working_directory
                                .as_ref()
                                .unwrap_or(&external.working_directory),
                        )?,
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let (executable_values, resolved_commands): (Vec<_>, Vec<_>) = commands.into_iter().unzip();
        let mut input_preconditions = Vec::new();
        let mut inputs = Vec::with_capacity(external.normal_inputs.len());
        for input in &external.normal_inputs {
            let captured = self
                .empty_output_inputs
                .get(&input.path)
                .filter(|captured| captured.kind == input.kind);
            let value = if let Some(captured) = captured {
                input_preconditions.push(ResolvedExternalInputPrecondition {
                    path: resolve_path(&self.snapshot.graph, &input.path)?,
                    kind: captured.kind,
                    expected: captured.value.clone(),
                });
                captured.value.clone()
            } else {
                observations.observe(&self.snapshot.graph, &input.path, input.kind)?
            };
            inputs.push(ExternalInputKey {
                path: input.path.clone(),
                value,
            });
        }
        for symbol in &external.config_deps {
            inputs.push(ExternalInputKey {
                path: ExternalPathKey {
                    root: "kconfig".to_string(),
                    path: symbol.as_bytes().to_vec(),
                },
                value: ExternalInputValue::Virtual {
                    name: symbol.as_bytes().to_vec(),
                    value: self
                        .kconfig_values
                        .get(symbol)
                        .cloned()
                        .unwrap_or_else(|| b"<absent>".to_vec()),
                },
            });
        }
        let key = external_action_key_from_values(external, &executable_values, &inputs)?;
        let id = key.compute_id()?;
        let resolved_outputs = resolve_external_outputs(&self.snapshot.graph, external)?;
        let isolation = external_action_isolation(&self.snapshot.graph, external)?;
        let runtime_directory_checkpoints =
            resolve_runtime_directory_checkpoints(&self.snapshot.graph, external)?;
        let carrier = Action {
            id,
            inputs: external
                .normal_inputs
                .iter()
                .filter(|input| input.kind != ExternalInputKind::Missing)
                .map(|input| resolve_path(&self.snapshot.graph, &input.path))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .map(Artifact::Source)
                .collect(),
            outputs: resolved_outputs
                .iter()
                .map(|(path, _, _)| Artifact::Source(path.clone()))
                .collect(),
            command: Command {
                executable: resolved_commands[0].executable.clone(),
                args: resolved_commands[0].argv.clone(),
                working_dir: Some(resolved_commands[0].working_directory.clone()),
                execution_info: rush_action::ExecutionInfo {
                    requires_network: false,
                    requires_sandbox: external.sandbox,
                    timeout: Duration::from_secs(300),
                },
            },
            env: HashMap::new(),
            mnemonic: external.mnemonic.clone(),
        };
        let resolved = ResolvedExternalAction {
            logical_id: external.logical_id.clone(),
            mnemonic: external.mnemonic.clone(),
            commands: resolved_commands,
            signal_state: external.signal_state,
            file_creation_mask: external.file_creation_mask,
            credentials: external.credentials.clone(),
            outputs: resolved_outputs,
            input_preconditions,
            runtime_directory_checkpoints,
            directory_observation_profile: runtime_directory_observation_profile(&external.execution_properties)?,
            direct_runtime_directory_opening: None,
            resource_class: map_resource_class(external.resource),
            isolation,
            directory_materialization: external
                .execution_properties
                .get("rush.kbuild.directory-materialization")
                .is_some_and(|policy| policy == "declared-mkdir-p-v1"),
        };
        Ok((carrier, resolved))
    }
}

fn materialize_declared_mkdir(action: &ResolvedExternalAction) -> Result<()> {
    let outputs = declared_mkdir_materialization_plan(
        &action.logical_id,
        &action.commands,
        &action.outputs,
        action.runtime_directory_checkpoints.len(),
    )?;
    for output in outputs {
        match std::fs::create_dir(&output) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&output).with_context(|| {
                    format!("inspect existing materialized directory {}", output.display())
                })?;
                if !metadata.file_type().is_dir() {
                    anyhow::bail!(
                        "materialized mkdir output is not a directory: {}",
                        output.display()
                    );
                }
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("materialize declared directory {}", output.display())
                });
            }
        }
    }
    Ok(())
}

fn declared_mkdir_materialization_plan(
    logical_id: &str,
    commands: &[ResolvedExternalCommand],
    outputs: &[(PathBuf, ExternalOutputKind, u32)],
    runtime_directory_checkpoint_count: usize,
) -> Result<Vec<PathBuf>> {
    if runtime_directory_checkpoint_count != 0 {
        anyhow::bail!(
            "directory materialization action {:?} has runtime checkpoints",
            logical_id
        );
    }
    let [command] = commands else {
        anyhow::bail!(
            "directory materialization action {:?} has multiple commands",
            logical_id
        );
    };
    if command.executable != Path::new("/usr/bin/mkdir")
        || command.argv0 != "mkdir"
        || command.argv.len() < 2
        || command.argv[0] != "-p"
    {
        anyhow::bail!(
            "directory materialization action {:?} changed command shape",
            logical_id
        );
    }
    let mut targets = BTreeSet::new();
    for argument in command.argv.iter().skip(1) {
        let path = Path::new(argument);
        if argument.starts_with('-')
            || path.as_os_str().is_empty()
            || path.is_absolute()
            || !path
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_)))
        {
            anyhow::bail!(
                "directory materialization action {:?} has an unsafe target",
                logical_id
            );
        }
        let target = command.working_directory.join(path);
        if !targets.insert(target) {
            anyhow::bail!(
                "directory materialization action {:?} repeats a target",
                logical_id
            );
        }
    }
    if targets.iter().any(|target| {
        !outputs
            .iter()
            .any(|(output, kind, _)| *kind == ExternalOutputKind::Tree && output == target)
    }) || outputs.iter().any(|(output, kind, _)| {
        *kind != ExternalOutputKind::Tree
            || !output.starts_with(&command.working_directory)
            || !targets.iter().any(|target| target.starts_with(output))
    }) {
        anyhow::bail!(
            "directory materialization action {:?} targets undeclared outputs",
            logical_id
        );
    }
    let mut plan = outputs
        .iter()
        .map(|(output, _, _)| output.clone())
        .collect::<Vec<_>>();
    plan.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    Ok(plan)
}

fn direct_runtime_directory_opening_stages(
    graph: &ExternalActionGraph,
    stages: &BTreeMap<usize, Vec<usize>>,
) -> Result<BTreeMap<usize, Vec<usize>>> {
    let mut action_stages = BTreeMap::new();
    for (stage, indices) in stages {
        for index in indices {
            action_stages.insert(*index, *stage);
        }
    }
    let indices_by_id = graph
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| (action.logical_id.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let mut opening_stages = BTreeMap::<usize, Vec<usize>>::new();
    for (index, action) in graph.actions.iter().enumerate() {
        let checkpoints = resolve_runtime_directory_checkpoints(graph, action)?;
        let outputs = resolve_external_outputs(graph, action)?;
        if !direct_runtime_directory_checkpoint_validation(
            &checkpoints, &outputs, runtime_directory_observation_profile(&action.execution_properties)?,
        ) {
            continue;
        }
        let action_stage = *action_stages
            .get(&index)
            .context("checkpointed external action has no replay stage")?;
        let mut opening_stage = action_stage;
        if checkpoints[0].before_actions.is_empty() {
        let mut dependencies = action
            .dependencies
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        while let Some(dependency) = dependencies.pop() {
            if !visited.insert(dependency) {
                continue;
            }
            let dependency_index = *indices_by_id
                .get(dependency)
                .with_context(|| format!("unknown external dependency {dependency:?}"))?;
            opening_stage = opening_stage.min(
                *action_stages
                    .get(&dependency_index)
                    .context("external dependency has no replay stage")?,
            );
            dependencies.extend(
                graph.actions[dependency_index]
                    .dependencies
                    .iter()
                    .map(String::as_str),
            );
        }
        } else {
            opening_stage = checkpoints[0]
                .before_actions
                .iter()
                .map(|logical_id| {
                    let dependency_index =
                        *indices_by_id.get(logical_id.as_str()).with_context(|| {
                            format!("unknown checkpoint predecessor {logical_id:?}")
                        })?;
                    action_stages
                        .get(&dependency_index)
                        .copied()
                        .context("checkpoint predecessor has no replay stage")
                })
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .max()
                .context("ordered checkpoint has no predecessor stage")?;
            if opening_stage > action_stage {
                anyhow::bail!("runtime directory checkpoint predecessor follows its observer");
            }
            for logical_id in &checkpoints[0].after_actions {
                let successor_index = *indices_by_id
                    .get(logical_id.as_str())
                    .with_context(|| format!("unknown checkpoint successor {logical_id:?}"))?;
                if action_stages
                    .get(&successor_index)
                    .is_some_and(|successor_stage| *successor_stage < opening_stage)
                {
                    anyhow::bail!("runtime directory checkpoint successor precedes its opening");
                }
            }
        }
        opening_stages.entry(opening_stage).or_default().push(index);
    }
    Ok(opening_stages)
}

fn resolve_external_outputs(
    graph: &ExternalActionGraph,
    action: &ExternalAction,
) -> Result<Vec<(PathBuf, ExternalOutputKind, u32)>> {
    action
        .outputs
        .iter()
        .map(|output| Ok((resolve_path(graph, &output.path)?, output.kind, output.mode)))
        .collect()
}

fn resolve_runtime_directory_checkpoints(
    graph: &ExternalActionGraph,
    action: &ExternalAction,
) -> Result<Vec<ResolvedRuntimeDirectoryCheckpoint>> {
    runtime_directory_checkpoints(&action.execution_properties)?
        .into_iter()
        .map(|checkpoint| {
            Ok(ResolvedRuntimeDirectoryCheckpoint {
                path: resolve_path(graph, &checkpoint.path)?,
                members: checkpoint.members,
                observation_ordinal: checkpoint.observation_ordinal,
                before_actions: checkpoint.before_actions,
                after_actions: checkpoint.after_actions,
            })
        })
        .collect()
}

fn direct_runtime_directory_checkpoint_validation(
    checkpoints: &[ResolvedRuntimeDirectoryCheckpoint],
    outputs: &[(PathBuf, ExternalOutputKind, u32)],
    profile: RuntimeDirectoryObservationProfile,
) -> bool {
    profile == RuntimeDirectoryObservationProfile::RawPages
        && checkpoints.len() == 2
        && checkpoints[0].path == checkpoints[1].path
        && checkpoints[0].members == checkpoints[1].members
        && outputs
            .iter()
            .all(|(path, _, _)| path.parent() != Some(checkpoints[0].path.as_path()))
}

fn observe_runtime_directory(path: &Path) -> Result<RuntimeDirectoryObservation> {
    let mut members = std::fs::read_dir(path)
        .with_context(|| {
            format!(
                "read direct runtime directory checkpoint {}",
                path.display()
            )
        })?
        .map(|entry| entry.map(|entry| os_bytes(&entry.file_name())))
        .collect::<std::io::Result<Vec<_>>>()?;
    members.sort();
    Ok(RuntimeDirectoryObservation {
        path: path.to_path_buf(),
        members,
    })
}

/// Select the persisted contract explicitly. Complete-profile proof failures
/// cannot be admitted by the legacy per-page path or a before/after shortcut.
fn select_runtime_directory_observations(
    profile: RuntimeDirectoryObservationProfile,
    observed: ObservedActionExecutionResult,
) -> Result<(rush_exec::local_action_runner::ActionExecutionResult, Vec<RuntimeDirectoryObservation>)> {
    let observations = match profile {
        RuntimeDirectoryObservationProfile::RawPages => observed.directory_observations,
        RuntimeDirectoryObservationProfile::CompletePrivateEnumerations => observed
            .complete_directory_enumerations
            .map_err(|reason| anyhow::anyhow!("runtime complete directory enumeration proof failed: {reason}"))?,
    };
    Ok((observed.execution, observations))
}

fn validate_runtime_directory_observations(
    expected: &[ResolvedRuntimeDirectoryCheckpoint],
    observed: &[RuntimeDirectoryObservation],
) -> Result<()> {
    let expected_paths = expected
        .iter()
        .map(|checkpoint| &checkpoint.path)
        .collect::<BTreeSet<_>>();
    let mut epochs_by_path = BTreeMap::<&PathBuf, Vec<&ResolvedRuntimeDirectoryCheckpoint>>::new();
    for checkpoint in expected {
        epochs_by_path
            .entry(&checkpoint.path)
            .or_default()
            .push(checkpoint);
    }
    for (path, mut epochs) in epochs_by_path {
        if epochs.len() < 2
            || epochs
                .iter()
                .all(|checkpoint| checkpoint.members == epochs[0].members)
        {
            continue;
        }
        epochs.sort_by_key(|checkpoint| checkpoint.observation_ordinal);
        let expected_epochs = epochs
            .iter()
            .map(|checkpoint| &checkpoint.members)
            .collect::<Vec<_>>();
        let observed_epochs = observed
            .iter()
            .filter(|observation| &observation.path == path)
            .map(|observation| &observation.members)
            .collect::<Vec<_>>();
        if expected_epochs != observed_epochs {
            anyhow::bail!(
                "runtime directory checkpoint epochs changed for {}",
                path.display()
            );
        }
    }
    let mut expected = expected
        .iter()
        .map(|checkpoint| (checkpoint.path.clone(), checkpoint.members.clone()))
        .collect::<Vec<_>>();
    let mut observed = observed
        .iter()
        .filter(|observation| expected_paths.contains(&observation.path))
        .map(|observation| (observation.path.clone(), observation.members.clone()))
        .collect::<Vec<_>>();
    expected.sort();
    observed.sort();
    if observed != expected {
        let first_difference = expected
            .iter()
            .zip(&observed)
            .position(|(expected, observed)| expected != observed)
            .unwrap_or_else(|| expected.len().min(observed.len()));
        anyhow::bail!(
            "external runtime directory observations changed: expected {} occurrences, observed {}; first difference {first_difference}: expected {:?}, observed {:?}",
            expected.len(),
            observed.len(),
            expected.get(first_difference),
            observed.get(first_difference),
        );
    }
    Ok(())
}

fn external_action_isolation(
    graph: &ExternalActionGraph,
    action: &ExternalAction,
) -> Result<Option<LocalExecutionIsolation>> {
    if !action.sandbox {
        return Ok(None);
    }
    let output_root = graph
        .roots
        .get("obj")
        .context("sandboxed external action has no output root")?;
    let canonical = output_root
        .canonicalize()
        .context("canonicalize sandboxed external action output root")?;
    if canonical != *output_root || canonical == Path::new("/") {
        anyhow::bail!("sandboxed external action output root is not confined and canonical");
    }
    if !std::fs::metadata(output_root)
        .context("stat sandboxed external action output root")?
        .is_dir()
    {
        anyhow::bail!("sandboxed external action output root is not a directory");
    }
    let isolation = LocalExecutionIsolation::new(vec![output_root.clone()])?;
    if action
        .execution_properties
        .get(crate::external_analysis_snapshot::MANDATORY_EXECUTION_PROPERTY)
        .is_some()
        && isolation.descriptor_topology() != LocalDescriptorTopology::StandardOnlyV1
    {
        anyhow::bail!("mandatory external action has an unsupported descriptor topology");
    }
    isolation
        .validate_credentials(&action.credentials)
        .context("sandboxed external action credentials are not isolatable")?;
    Ok(Some(isolation))
}

fn validate_external_action_isolation(graph: &ExternalActionGraph) -> Result<()> {
    for action in &graph.actions {
        external_action_isolation(graph, action).with_context(|| {
            format!(
                "validate external action {:?} isolation before replay",
                action.logical_id
            )
        })?;
    }
    Ok(())
}

fn cache_policy(action: &ExternalAction) -> ActionCachePolicy {
    if !action.cacheable {
        ActionCachePolicy::UNCACHEABLE
    } else if action.always_run {
        ActionCachePolicy::DURABLE_ALWAYS_RUN
    } else {
        ActionCachePolicy::DURABLE_CACHEABLE
    }
}

fn requested_topological_order(graph: &ExternalActionGraph) -> Result<Vec<usize>> {
    let mut by_id = HashMap::new();
    let mut by_output = BTreeMap::new();
    for (index, action) in graph.actions.iter().enumerate() {
        by_id.insert(action.logical_id.as_str(), index);
        for output in &action.outputs {
            by_output.insert(&output.path, index);
        }
    }
    let mut needed = BTreeSet::new();
    let mut stack = graph
        .requested_outputs
        .iter()
        .filter_map(|output| by_output.get(output).copied())
        .collect::<Vec<_>>();
    for barrier in &graph.analysis_barriers {
        stack.extend(
            barrier
                .producer_closure
                .iter()
                .map(|logical_id| by_id[logical_id.as_str()]),
        );
    }
    while let Some(index) = stack.pop() {
        if needed.insert(index) {
            stack.extend(
                graph.actions[index]
                    .dependencies
                    .iter()
                    .map(|dependency| by_id[dependency.as_str()]),
            );
        }
    }
    let mut indegree = vec![0usize; graph.actions.len()];
    let mut dependents = vec![Vec::new(); graph.actions.len()];
    for &index in &needed {
        for dependency in &graph.actions[index].dependencies {
            let producer = by_id[dependency.as_str()];
            if needed.contains(&producer) {
                indegree[index] += 1;
                dependents[producer].push(index);
            }
        }
    }
    let action_stages = analysis_action_stages(graph);
    let mut ready = BTreeSet::new();
    for &index in &needed {
        if indegree[index] == 0 {
            ready.insert((
                action_stages[graph.actions[index].logical_id.as_str()],
                index,
            ));
        }
    }
    let mut order = Vec::with_capacity(needed.len());
    while let Some((_, index)) = ready.pop_first() {
        order.push(index);
        for &dependent in &dependents[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.insert((
                    action_stages[graph.actions[dependent].logical_id.as_str()],
                    dependent,
                ));
            }
        }
    }
    if order.len() != needed.len() {
        anyhow::bail!("requested external action closure contains a cycle");
    }
    Ok(order)
}

fn analysis_action_stages(graph: &ExternalActionGraph) -> HashMap<&str, usize> {
    let mut stages = graph
        .actions
        .iter()
        .map(|action| (action.logical_id.as_str(), graph.analysis_barriers.len()))
        .collect::<HashMap<_, _>>();
    for (barrier_index, barrier) in graph.analysis_barriers.iter().enumerate() {
        for logical_id in &barrier.producer_closure {
            let stage = stages
                .get_mut(logical_id.as_str())
                .expect("validated barrier action must exist");
            *stage = (*stage).min(barrier_index);
        }
    }
    stages
}

fn input_kind_for_value(value: &ExternalInputValue) -> Option<ExternalInputKind> {
    match value {
        ExternalInputValue::File { .. } => Some(ExternalInputKind::File),
        ExternalInputValue::Symlink { .. } => Some(ExternalInputKind::Symlink),
        ExternalInputValue::Tree { .. } => Some(ExternalInputKind::Tree),
        ExternalInputValue::Missing => Some(ExternalInputKind::Missing),
        ExternalInputValue::DirectoryListing { .. } => Some(ExternalInputKind::DirectoryListing),
        ExternalInputValue::Directory { .. } => Some(ExternalInputKind::Directory),
        ExternalInputValue::Virtual { .. } => None,
    }
}

pub(crate) fn resolve_path(graph: &ExternalActionGraph, path: &ExternalPathKey) -> Result<PathBuf> {
    let root = graph
        .roots
        .get(&path.root)
        .with_context(|| format!("unknown external root {:?}", path.root))?;
    if path.path == b"." {
        return Ok(root.clone());
    }
    Ok(root.join(os_string(&path.path)?))
}

#[cfg(unix)]
fn os_string(value: &[u8]) -> Result<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(value.to_vec()))
}

#[cfg(not(unix))]
fn os_string(value: &[u8]) -> Result<OsString> {
    Ok(OsString::from(text(value, "path")?))
}

#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}

#[cfg(not(unix))]
fn os_bytes(value: &OsStr) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

fn text(value: &[u8], what: &str) -> Result<String> {
    String::from_utf8(value.to_vec()).with_context(|| format!("external {what} is not UTF-8"))
}

fn path_text(path: PathBuf) -> Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("external command path is not UTF-8"))
}

fn resolve_argument(graph: &ExternalActionGraph, argument: &ExternalArgumentKey) -> Result<String> {
    match argument {
        ExternalArgumentKey::Literal(value) => text(value, "command value"),
        ExternalArgumentKey::Path(path) => path_text(resolve_path(graph, path)?),
        ExternalArgumentKey::Joined(parts) => resolve_joined_argument(graph, parts),
    }
}

fn resolve_joined_argument(
    graph: &ExternalActionGraph,
    parts: &[ExternalArgumentPart],
) -> Result<String> {
    let mut value = Vec::new();
    for part in parts {
        match part {
            ExternalArgumentPart::Literal(bytes) => value.extend(bytes),
            ExternalArgumentPart::Path(path) => {
                value.extend(os_bytes(resolve_path(graph, path)?.as_os_str()));
            }
        }
    }
    text(&value, "joined command argument")
}

fn digest_file(path: &Path) -> Result<[u8; 32]> {
    let mut file =
        File::open(path).with_context(|| format!("open external input {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("read external input {}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn observe_executable_value(path: &Path) -> Result<ExternalInputValue> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("stat external executable {}", path.display()))?;
    if !metadata.file_type().is_file() {
        anyhow::bail!(
            "external executable has the wrong shape: {}",
            path.display()
        );
    }
    Ok(ExternalInputValue::File {
        digest: digest_file(path)?,
        executable: is_executable(&metadata),
    })
}

fn observe_resolved_input(path: &Path, kind: ExternalInputKind) -> Result<ExternalInputValue> {
    match kind {
        ExternalInputKind::File => {
            let metadata = std::fs::symlink_metadata(path)
                .with_context(|| format!("stat external input {}", path.display()))?;
            if !metadata.file_type().is_file() {
                anyhow::bail!(
                    "external file input has the wrong shape: {}",
                    path.display()
                );
            }
            Ok(ExternalInputValue::File {
                digest: digest_file(path)?,
                executable: is_executable(&metadata),
            })
        }
        ExternalInputKind::Symlink => {
            let metadata = std::fs::symlink_metadata(path)
                .with_context(|| format!("stat external input {}", path.display()))?;
            if !metadata.file_type().is_symlink() {
                anyhow::bail!(
                    "external symlink input has the wrong shape: {}",
                    path.display()
                );
            }
            Ok(ExternalInputValue::Symlink {
                target: os_bytes(
                    std::fs::read_link(path)
                        .with_context(|| format!("read link {}", path.display()))?
                        .as_os_str(),
                ),
            })
        }
        ExternalInputKind::Tree => Ok(ExternalInputValue::Tree {
            digest: digest_tree(path)?,
        }),
        ExternalInputKind::Missing => match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ExternalInputValue::Missing)
            }
            Err(error) => Err(error).with_context(|| format!("probe {}", path.display())),
            Ok(_) => anyhow::bail!("external negative input now exists: {}", path.display()),
        },
        ExternalInputKind::DirectoryListing => Ok(ExternalInputValue::DirectoryListing {
            digest: digest_directory_listing(path)?,
        }),
        ExternalInputKind::Directory => {
            let metadata = std::fs::symlink_metadata(path)
                .with_context(|| format!("stat directory input {}", path.display()))?;
            if !metadata.file_type().is_dir() {
                anyhow::bail!(
                    "external directory input has the wrong shape: {}",
                    path.display()
                );
            }
            Ok(ExternalInputValue::Directory {
                mode: permission_mode(&metadata),
            })
        }
    }
}

#[cfg(unix)]
fn permission_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn permission_mode(metadata: &std::fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    false
}

pub(crate) fn digest_directory_listing(path: &Path) -> Result<[u8; 32]> {
    let mut members = std::fs::read_dir(path)
        .with_context(|| format!("read directory input {}", path.display()))?
        .map(|entry| entry.map(|entry| os_bytes(&entry.file_name())))
        .collect::<std::io::Result<Vec<_>>>()?;
    members.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.external-directory-listing.v1\n");
    for member in members {
        hash_bytes(&mut hasher, &member);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn digest_tree(path: &Path) -> Result<[u8; 32]> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat tree input {}", path.display()))?;
    if !metadata.is_dir() {
        anyhow::bail!("external tree input is not a directory: {}", path.display());
    }
    let mut entries = walkdir::WalkDir::new(path)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    entries.remove(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.external-tree.v1\n");
    for entry in entries {
        let relative = entry.path().strip_prefix(path)?;
        hash_bytes(&mut hasher, &os_bytes(relative.as_os_str()));
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            hasher.update(&[1]);
            hash_bytes(
                &mut hasher,
                &os_bytes(std::fs::read_link(entry.path())?.as_os_str()),
            );
        } else if metadata.is_dir() {
            hasher.update(&[2]);
        } else if metadata.is_file() {
            hasher.update(&[3, u8::from(is_executable(&metadata))]);
            hasher.update(&digest_file(entry.path())?);
        } else {
            anyhow::bail!(
                "unsupported entry in external tree: {}",
                entry.path().display()
            );
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn prepare_initial_state(
    graph: &ExternalActionGraph,
) -> Result<BTreeMap<ExternalPathKey, ExternalEmptyOutputInput>> {
    let output_root = graph
        .roots
        .get("obj")
        .context("external graph has no obj root")?;
    let started_empty = output_root_started_empty(output_root)?;
    materialize_initial_inputs(graph)?;
    if std::env::var("RUSH_EXTERNAL_OBSERVATION_STATS").as_deref() == Ok("1") {
        let symlinks = graph
            .initial_inputs
            .iter()
            .filter(|input| matches!(input.value, ExternalInitialInputValue::Symlink { .. }))
            .collect::<Vec<_>>();
        let materialized_symlinks = symlinks
            .iter()
            .filter(|input| {
                resolve_path(graph, &input.path)
                    .ok()
                    .and_then(|path| std::fs::symlink_metadata(path).ok())
                    .is_some_and(|metadata| metadata.file_type().is_symlink())
            })
            .count();
        eprintln!(
            "external initial inputs: total={} symlinks={} materialized_symlinks={}",
            graph.initial_inputs.len(),
            symlinks.len(),
            materialized_symlinks
        );
    }
    if !started_empty {
        return Ok(BTreeMap::new());
    }
    Ok(graph
        .empty_output_inputs
        .iter()
        .cloned()
        .map(|input| (input.path.clone(), input))
        .collect())
}

fn output_root_started_empty(output_root: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(output_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => anyhow::bail!("external output root is not a directory"),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            std::fs::create_dir_all(output_root).with_context(|| {
                format!("create external output root {}", output_root.display())
            })?;
            return Ok(true);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("stat external output root {}", output_root.display()));
        }
    }
    Ok(std::fs::read_dir(output_root)
        .with_context(|| format!("read external output root {}", output_root.display()))?
        .next()
        .transpose()?
        .is_none())
}

fn materialize_initial_inputs(graph: &ExternalActionGraph) -> Result<()> {
    let output_root = graph
        .roots
        .get("obj")
        .context("external graph has no obj root")?;
    let mut directories = graph
        .initial_inputs
        .iter()
        .filter_map(|input| match input.value {
            ExternalInitialInputValue::Directory { mode } => Some((input, mode)),
            _ => None,
        })
        .collect::<Vec<_>>();
    directories.sort_by_key(|(input, _)| path_depth(&input.path.path));
    let mut created_directories = Vec::new();
    for (input, mode) in directories {
        let path = resolve_path(graph, &input.path)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => continue,
            Ok(_) => anyhow::bail!("existing initial directory has the wrong shape"),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("stat initial directory {}", path.display()));
            }
        }
        validate_materialization_parent(output_root, &path)?;
        match std::fs::create_dir(&path) {
            Ok(()) => {
                set_permission_mode(&path, 0o700)?;
                created_directories.push((path, mode));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create initial directory {}", path.display()));
            }
        }
    }

    for input in &graph.initial_inputs {
        let path = resolve_path(graph, &input.path)?;
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                let shape_matches = match &input.value {
                    ExternalInitialInputValue::File { .. } => metadata.file_type().is_file(),
                    ExternalInitialInputValue::Directory { .. } => metadata.file_type().is_dir(),
                    ExternalInitialInputValue::Symlink { .. } => metadata.file_type().is_symlink(),
                };
                if !shape_matches {
                    anyhow::bail!("existing initial input has the wrong shape");
                }
                continue;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("stat initial input {}", path.display()));
            }
        }
        validate_materialization_parent(output_root, &path)?;
        match &input.value {
            ExternalInitialInputValue::File { bytes, mode } => {
                materialize_initial_file(&path, bytes, *mode)?;
            }
            ExternalInitialInputValue::Symlink { target } => {
                materialize_initial_symlink(&path, target)?;
            }
            ExternalInitialInputValue::Directory { .. } => {}
        }
    }
    created_directories.sort_by_key(|(path, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode) in created_directories {
        set_permission_mode(&path, mode)?;
    }
    Ok(())
}

fn path_depth(path: &[u8]) -> usize {
    path.iter().filter(|byte| **byte == b'/').count()
}

fn validate_materialization_parent(output_root: &Path, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("external initial input has no parent")?;
    let metadata = std::fs::symlink_metadata(parent)
        .with_context(|| format!("stat initial input parent {}", parent.display()))?;
    if !metadata.file_type().is_dir() {
        anyhow::bail!("external initial input parent is not a directory");
    }
    let canonical_root = output_root.canonicalize().with_context(|| {
        format!(
            "canonicalize external output root {}",
            output_root.display()
        )
    })?;
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("canonicalize initial input parent {}", parent.display()))?;
    if canonical_parent != parent || !canonical_parent.starts_with(canonical_root) {
        anyhow::bail!("external initial input parent crosses a symbolic link");
    }
    Ok(())
}

fn materialize_initial_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .context("external initial file has no parent")?;
    let temporary = parent.join(format!(
        ".rush-initial-{}-{}.tmp",
        std::process::id(),
        INITIAL_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create initial input temporary {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write initial input temporary {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync initial input temporary {}", temporary.display()))?;
        set_permission_mode(&temporary, mode)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    match std::fs::hard_link(&temporary, path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&temporary);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error).with_context(|| format!("publish initial input {}", path.display()))
        }
    }
}

#[cfg(unix)]
fn materialize_initial_symlink(path: &Path, target: &[u8]) -> Result<()> {
    use std::os::unix::fs::symlink;
    match symlink(os_string(target)?, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("materialize initial symlink {}", path.display()))
        }
    }
}

#[cfg(not(unix))]
fn materialize_initial_symlink(_path: &Path, _target: &[u8]) -> Result<()> {
    anyhow::bail!("external initial symlinks are unsupported on this platform")
}

#[cfg(unix)]
fn set_permission_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("set initial input mode on {}", path.display()))
}

#[cfg(not(unix))]
fn set_permission_mode(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(mode != 0);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("set initial input mode on {}", path.display()))
}

fn load_kconfig_values(graph: &ExternalActionGraph) -> Result<BTreeMap<String, Vec<u8>>> {
    let Some(source) = &graph.kconfig else {
        return Ok(BTreeMap::new());
    };
    let path = resolve_path(graph, &source.path)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            materialize_captured_kconfig(&path, &source.captured_bytes)?
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read Kconfig value source {}", path.display()));
        }
    };
    let mut values = BTreeMap::new();
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Some(rest) = line.strip_prefix(b"CONFIG_") {
            if let Some(equal) = rest.iter().position(|byte| *byte == b'=') {
                let name = format!("CONFIG_{}", text(&rest[..equal], "Kconfig symbol")?);
                values.insert(name, rest[equal + 1..].to_vec());
            }
        } else if let Some(symbol) = line
            .strip_prefix(b"# ")
            .and_then(|line| line.strip_suffix(b" is not set"))
        {
            if symbol.starts_with(b"CONFIG_") {
                values.insert(text(symbol, "Kconfig symbol")?, b"n".to_vec());
            }
        }
    }
    Ok(values)
}

fn materialize_captured_kconfig(path: &Path, captured: &[u8]) -> Result<Vec<u8>> {
    let parent = path
        .parent()
        .context("Kconfig value source has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create Kconfig output directory {}", parent.display()))?;
    let temporary = parent.join(format!(
        ".rush-kconfig-{}-{}.tmp",
        std::process::id(),
        KCONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| {
                format!("create captured Kconfig temporary {}", temporary.display())
            })?;
        file.write_all(captured)
            .with_context(|| format!("write captured Kconfig temporary {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync captured Kconfig temporary {}", temporary.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    match std::fs::hard_link(&temporary, path) {
        Ok(()) => {
            std::fs::remove_file(&temporary).with_context(|| {
                format!("remove captured Kconfig temporary {}", temporary.display())
            })?;
            Ok(captured.to_vec())
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&temporary);
            std::fs::read(path)
                .with_context(|| format!("read concurrent Kconfig source {}", path.display()))
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error).with_context(|| {
                format!("publish captured Kconfig value source {}", path.display())
            })
        }
    }
}

pub(crate) fn normalize_and_validate_outputs(
    outputs: &[(PathBuf, ExternalOutputKind, u32)],
) -> Result<bool> {
    normalize_and_validate_outputs_observed(outputs, None)
}

fn normalize_and_validate_outputs_observed(
    outputs: &[(PathBuf, ExternalOutputKind, u32)],
    before_output_mutation: Option<&BeforeOutputMutation>,
) -> Result<bool> {
    let mut repaired_mode = false;
    for (path, kind, mode) in outputs {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("external action did not create {}", path.display()))?;
        let matches = match kind {
            ExternalOutputKind::File => metadata.file_type().is_file(),
            ExternalOutputKind::Symlink => metadata.file_type().is_symlink(),
            ExternalOutputKind::Tree => metadata.file_type().is_dir(),
        };
        if !matches {
            anyhow::bail!(
                "external action created the wrong output shape: {}",
                path.display()
            );
        }
        let actual_mode = permission_mode(&metadata);
        if actual_mode != *mode {
            if *kind != ExternalOutputKind::Symlink {
                if let Some(before) = before_output_mutation {
                    before()?;
                }
            }
            set_output_mode(path, *kind, *mode)?;
            repaired_mode = true;
        }
        let normalized_mode = if actual_mode == *mode {
            actual_mode
        } else {
            permission_mode(
                &std::fs::symlink_metadata(path)
                    .with_context(|| format!("restat external output {}", path.display()))?,
            )
        };
        if normalized_mode != *mode {
            anyhow::bail!(
                "external action output has the wrong permission mode: {}",
                path.display()
            );
        }
    }
    Ok(repaired_mode)
}

fn missing_output_parents(
    outputs: &[(PathBuf, ExternalOutputKind, u32)],
    output_root: &Path,
    file_creation_mask: u32,
) -> Result<Vec<(PathBuf, u32)>> {
    let mode = 0o777 & !file_creation_mask;
    let mut missing = BTreeSet::new();
    for (output, _, _) in outputs {
        let mut parent = output.parent();
        while let Some(path) = parent {
            if path == output_root {
                break;
            }
            if !path.starts_with(output_root) {
                anyhow::bail!(
                    "external output parent escaped the output root: {}",
                    output.display()
                );
            }
            match std::fs::symlink_metadata(path) {
                Ok(metadata) => {
                    if !metadata.file_type().is_dir() {
                        anyhow::bail!(
                            "external output parent is not a directory: {}",
                            path.display()
                        );
                    }
                    break;
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    missing.insert(path.to_path_buf());
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect external output parent {}", path.display())
                    });
                }
            }
            parent = path.parent();
        }
    }
    Ok(missing.into_iter().map(|path| (path, mode)).collect())
}

fn normalize_cached_output_parents(parents: &[(PathBuf, u32)]) -> Result<bool> {
    normalize_cached_output_parents_observed(parents, None)
}

fn normalize_cached_output_parents_observed(
    parents: &[(PathBuf, u32)],
    before_output_mutation: Option<&BeforeOutputMutation>,
) -> Result<bool> {
    let mut repaired = false;
    for (path, mode) in parents {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspect cached output parent {}", path.display()))?;
        if !metadata.file_type().is_dir() {
            anyhow::bail!("cached output parent is not a directory: {}", path.display());
        }
        if permission_mode(&metadata) != *mode {
            if let Some(before) = before_output_mutation {
                before()?;
            }
            set_output_mode(path, ExternalOutputKind::Tree, *mode)?;
            repaired = true;
        }
        if permission_mode(
            &std::fs::symlink_metadata(path)
                .with_context(|| format!("restat cached output parent {}", path.display()))?,
        ) != *mode
        {
            anyhow::bail!("cached output parent has the wrong mode: {}", path.display());
        }
    }
    Ok(repaired)
}

#[cfg(unix)]
fn set_output_mode(path: &Path, kind: ExternalOutputKind, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if kind != ExternalOutputKind::Symlink {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("set external output mode {}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_output_mode(path: &Path, kind: ExternalOutputKind, mode: u32) -> Result<()> {
    if kind != ExternalOutputKind::Symlink {
        let mut permissions = std::fs::metadata(path)?.permissions();
        permissions.set_readonly(mode != 0);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

fn append_tail(destination: &mut Vec<u8>, source: &[u8]) {
    destination.extend_from_slice(source);
    if destination.len() > OUTPUT_TAIL_LIMIT {
        destination.drain(..destination.len() - OUTPUT_TAIL_LIMIT);
    }
}

fn map_resource_class(class: ManifestResourceClass) -> ResourceClass {
    match class {
        ManifestResourceClass::Compile => ResourceClass::CppCompile,
        ManifestResourceClass::KernelCompile => ResourceClass::KernelCompile,
        ManifestResourceClass::Link => ResourceClass::Link,
        ManifestResourceClass::KernelLink => ResourceClass::KernelLink,
        ManifestResourceClass::Archive => ResourceClass::Archive,
        ManifestResourceClass::Codegen => ResourceClass::CodegenProtoc,
        ManifestResourceClass::KernelCodegen => ResourceClass::KernelCodegen,
        ManifestResourceClass::Other => ResourceClass::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_analysis_snapshot::{
        ExternalAnalysisBarrier, ExternalAnalyzerIdentity, ExternalCommand,
        ExternalEmptyOutputInput, ExternalInitialInput, ExternalInitialInputValue, ExternalInput,
        ExternalKconfigSource, ExternalOutput,
    };

    #[cfg(unix)]
    #[test]
    fn output_normalization_does_not_mutate_an_already_normalized_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        std::fs::write(&output, b"value\n").unwrap();
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o640)).unwrap();
        let before = std::fs::symlink_metadata(&output).unwrap();

        normalize_and_validate_outputs(&[(output.clone(), ExternalOutputKind::File, 0o640)])
            .unwrap();

        let after = std::fs::symlink_metadata(&output).unwrap();
        assert_eq!(
            (before.ctime(), before.ctime_nsec()),
            (after.ctime(), after.ctime_nsec()),
            "an already-correct output must not receive chmod"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_and_parent_mode_repairs_fence_before_writing() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::AtomicUsize;

        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("output");
        std::fs::write(&output, b"value\n").unwrap();
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o640)).unwrap();
        let outputs = [(output.clone(), ExternalOutputKind::File, 0o600)];
        let refused: Arc<BeforeOutputMutation> = Arc::new(|| anyhow::bail!("input fence rejected"));
        assert!(normalize_and_validate_outputs_observed(&outputs, Some(refused.as_ref())).is_err());
        assert_eq!(permission_mode(&std::fs::metadata(&output).unwrap()), 0o640);

        let callbacks = Arc::new(AtomicUsize::new(0));
        let before: Arc<BeforeOutputMutation> = {
            let output = output.clone();
            let callbacks = callbacks.clone();
            Arc::new(move || {
                assert_eq!(permission_mode(&std::fs::metadata(&output)?), 0o640);
                callbacks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        assert!(normalize_and_validate_outputs_observed(&outputs, Some(before.as_ref())).unwrap());
        assert!(!normalize_and_validate_outputs_observed(&outputs, Some(before.as_ref())).unwrap());
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);

        let parent = temp.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o750)).unwrap();
        let parents = [(parent.clone(), 0o700)];
        assert!(normalize_cached_output_parents_observed(&parents, Some(refused.as_ref())).is_err());
        assert_eq!(permission_mode(&std::fs::metadata(&parent).unwrap()), 0o750);
        let before: Arc<BeforeOutputMutation> = {
            let callbacks = callbacks.clone();
            Arc::new(move || {
                assert_eq!(permission_mode(&std::fs::metadata(&parent)?), 0o750);
                callbacks.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        assert!(normalize_cached_output_parents_observed(&parents, Some(before.as_ref())).unwrap());
        assert!(!normalize_cached_output_parents_observed(&parents, Some(before.as_ref())).unwrap());
        assert_eq!(callbacks.load(Ordering::SeqCst), 2);
    }

    #[cfg(unix)]
    #[test]
    fn cached_output_parent_normalization_uses_the_captured_umask() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let output_root = temp.path().join("obj");
        std::fs::create_dir(&output_root).unwrap();
        let output = output_root.join("include/generated/uapi/linux/version.h");
        let missing = missing_output_parents(
            &[(output.clone(), ExternalOutputKind::File, 0o600)],
            &output_root,
            0o077,
        )
        .unwrap();
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, b"value\n").unwrap();
        for (path, _) in &missing {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o775)).unwrap();
        }

        assert!(normalize_cached_output_parents(&missing).unwrap());
        for (path, mode) in missing {
            assert_eq!(permission_mode(&std::fs::symlink_metadata(path).unwrap()), mode);
        }
    }

    #[test]
    fn declared_mkdir_plan_is_safe_and_parent_first() {
        let temp = tempfile::tempdir().unwrap();
        let output_root = temp.path().join("obj");
        std::fs::create_dir(&output_root).unwrap();
        let parent = output_root.join("scripts");
        let child = parent.join("basic");
        let command = ResolvedExternalCommand {
            executable: PathBuf::from("/usr/bin/mkdir"),
            argv0: "mkdir".to_string(),
            argv: vec!["-p".to_string(), "scripts/basic/".to_string()],
            environment: Vec::new(),
            working_directory: output_root.clone(),
        };
        let outputs = vec![
            (child.clone(), ExternalOutputKind::Tree, 0o700),
            (parent.clone(), ExternalOutputKind::Tree, 0o700),
        ];

        let plan = declared_mkdir_materialization_plan(
            "mkdir scripts/basic",
            std::slice::from_ref(&command),
            &outputs,
            0,
        )
        .unwrap();
        assert_eq!(plan, vec![parent.clone(), child.clone()]);
        for output in plan {
            std::fs::create_dir(output).unwrap();
        }
        assert!(parent.is_dir());
        assert!(child.is_dir());

        let mut unsafe_command = command.clone();
        unsafe_command.argv[1] = "../escape".to_string();
        assert!(declared_mkdir_materialization_plan(
            "unsafe mkdir",
            &[unsafe_command],
            &outputs,
            0,
        )
        .is_err());

        let mut undeclared_target_command = command;
        undeclared_target_command
            .argv
            .push("scripts/undeclared".to_string());
        assert!(declared_mkdir_materialization_plan(
            "mkdir with undeclared target",
            &[undeclared_target_command],
            &outputs,
            0,
        )
        .is_err());
    }
    use rush_cache::cas::CasStore;
    use rush_cache::CacheConfig;
    use rush_exec::local_action_runner::ActionExecutionResult;
    use rush_incremental::{content_digest_of_regular_file, Dependency, DependencyManifest};

    #[derive(Default)]
    struct RecordingRunner {
        // One atomic record binds every request field to its action label.
        // Independent ready actions may enter the runner in either order.
        requests: Mutex<Vec<(String, ExecutionRequest)>>,
    }

    impl RecordingRunner {
        fn calls(&self) -> Vec<String> {
            self.requests.lock().unwrap().iter().map(|(label, _)| label.clone()).collect()
        }

        fn requests(&self) -> Vec<(String, ExecutionRequest)> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn assert_producers_precede_link(calls: &[String]) {
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2], "link");
        assert_eq!(calls[..2].iter().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(["a", "b"]));
    }

    #[async_trait]
    impl LocalActionRunner for RecordingRunner {
        async fn execute(&self, request: ExecutionRequest) -> Result<ActionExecutionResult> {
            let label = request.command.argv[0].clone();
            self.requests.lock().unwrap().push((label.clone(), request.clone()));
            let started = Instant::now();
            match label.as_str() {
                "a" | "b" => {
                    let bytes = std::fs::read(&request.command.argv[1])?;
                    std::fs::write(&request.command.argv[2], bytes)?;
                }
                "child" => {
                    std::fs::create_dir_all(&request.command.argv[1])?;
                    std::fs::write(&request.command.argv[2], b"child\n")?;
                }
                "parent" => {
                    let bytes = std::fs::read(&request.command.argv[2])?;
                    std::fs::remove_dir_all(&request.command.argv[1])?;
                    std::fs::write(&request.command.argv[3], bytes)?;
                }
                "link" => {
                    let mut bytes = std::fs::read(&request.command.argv[1])?;
                    bytes.extend(std::fs::read(&request.command.argv[2])?);
                    std::fs::write(&request.command.argv[3], bytes)?;
                }
                other => anyhow::bail!("unknown test action {other}"),
            }
            Ok(ActionExecutionResult {
                declared_action_id: request.declared_action_id,
                exit_code: 0,
                stdout_tail: Vec::new(),
                stderr_tail: Vec::new(),
                duration: started.elapsed(),
                peak_rss_kb: 0,
            })
        }
    }

    #[test]
    fn complete_directory_profile_requires_proof_and_preserves_enumeration_occurrences() {
        let path = PathBuf::from("/tmp/generated");
        let members = vec![b"member".to_vec()];
        let observation = RuntimeDirectoryObservation { path: path.clone(), members: members.clone() };
        let expected = [ResolvedRuntimeDirectoryCheckpoint {
            path: path.clone(), members, observation_ordinal: 1,
            before_actions: Vec::new(), after_actions: Vec::new(),
        }];
        let observed = |proof| ObservedActionExecutionResult {
            execution: ActionExecutionResult {
                declared_action_id: DeclaredActionId(0), exit_code: 0,
                stdout_tail: Vec::new(), stderr_tail: Vec::new(),
                duration: Duration::ZERO, peak_rss_kb: 0,
            },
            // Four raw pages can prove one enumeration. The proof helper, not
            // this selector, establishes the private lifetime/full-entry cover.
            directory_observations: vec![observation.clone(); 4],
            complete_directory_enumerations: proof,
        };
        let (_, raw) = select_runtime_directory_observations(
            RuntimeDirectoryObservationProfile::RawPages, observed(Err("unsupported private lifecycle".into())),
        ).unwrap();
        assert_eq!(raw.len(), 4);
        assert!(validate_runtime_directory_observations(&expected, &raw).is_err());
        let error = select_runtime_directory_observations(
            RuntimeDirectoryObservationProfile::CompletePrivateEnumerations,
            observed(Err("unproved page".into())),
        ).unwrap_err();
        assert!(error.to_string().contains("unproved page"));
        let (_, complete) = select_runtime_directory_observations(
            RuntimeDirectoryObservationProfile::CompletePrivateEnumerations,
            observed(Ok(vec![observation.clone()])),
        ).unwrap();
        validate_runtime_directory_observations(&expected, &complete).unwrap();
        // Two independently completed opens remain two occurrences; equality
        // of their values cannot turn them into a single enumeration.
        assert!(validate_runtime_directory_observations(&expected,
            &[observation.clone(), observation.clone()]).is_err());
        assert!(validate_runtime_directory_observations(&expected, &[]).is_err());
        assert!(validate_runtime_directory_observations(&expected, &[RuntimeDirectoryObservation {
            path, members: vec![b"changed".to_vec()],
        }]).is_err());
    }

    #[test]
    fn complete_directory_profile_never_uses_direct_before_after_shortcut() {
        let path = PathBuf::from("/tmp/generated");
        let checkpoint = ResolvedRuntimeDirectoryCheckpoint {
            path, members: vec![b"member".to_vec()], observation_ordinal: 0,
            before_actions: Vec::new(), after_actions: Vec::new(),
        };
        let checkpoints = [checkpoint.clone(), checkpoint];
        assert!(direct_runtime_directory_checkpoint_validation(&checkpoints, &[], RuntimeDirectoryObservationProfile::RawPages));
        assert!(!direct_runtime_directory_checkpoint_validation(&checkpoints, &[], RuntimeDirectoryObservationProfile::CompletePrivateEnumerations));
    }

    #[test]
    fn runtime_directory_validation_is_occurrence_exact() {
        let path = PathBuf::from("/tmp/generated");
        let members = vec![b"first".to_vec(), b"second".to_vec()];
        let expected = vec![
            ResolvedRuntimeDirectoryCheckpoint {
                path: path.clone(),
                members: members.clone(),
                observation_ordinal: 0,
                before_actions: Vec::new(),
                after_actions: Vec::new(),
            },
            ResolvedRuntimeDirectoryCheckpoint {
                path: path.clone(),
                members: members.clone(),
                observation_ordinal: 1,
                before_actions: Vec::new(),
                after_actions: Vec::new(),
            },
        ];
        let mut observed = vec![
            RuntimeDirectoryObservation {
                path: path.clone(),
                members: members.clone(),
            },
            RuntimeDirectoryObservation {
                path: path.clone(),
                members: members.clone(),
            },
            RuntimeDirectoryObservation {
                path: PathBuf::from("/tmp/unrelated"),
                members: Vec::new(),
            },
        ];

        validate_runtime_directory_observations(&expected, &observed).unwrap();
        observed.pop();
        observed.pop();
        assert!(validate_runtime_directory_observations(&expected, &observed).is_err());
        observed.push(RuntimeDirectoryObservation {
            path,
            members: vec![b"changed".to_vec()],
        });
        assert!(validate_runtime_directory_observations(&expected, &observed).is_err());
    }

    #[test]
    fn runtime_directory_validation_orders_distinct_same_path_epochs() {
        let path = PathBuf::from("/tmp/generated");
        let early = vec![b"early".to_vec()];
        let late = vec![b"early".to_vec(), b"late".to_vec()];
        let expected = vec![
            ResolvedRuntimeDirectoryCheckpoint {
                path: path.clone(),
                members: early.clone(),
                observation_ordinal: 10,
                before_actions: Vec::new(),
                after_actions: Vec::new(),
            },
            ResolvedRuntimeDirectoryCheckpoint {
                path: path.clone(),
                members: late.clone(),
                observation_ordinal: 20,
                before_actions: Vec::new(),
                after_actions: Vec::new(),
            },
        ];
        let forward = vec![
            RuntimeDirectoryObservation {
                path: path.clone(),
                members: early,
            },
            RuntimeDirectoryObservation {
                path: path.clone(),
                members: late,
            },
        ];
        let mut reversed = forward.clone();
        reversed.reverse();

        validate_runtime_directory_observations(&expected, &forward).unwrap();
        assert!(validate_runtime_directory_observations(&expected, &reversed).is_err());
    }

    fn rooted(root: &str, path: &str) -> ExternalPathKey {
        ExternalPathKey {
            root: root.to_string(),
            path: path.as_bytes().to_vec(),
        }
    }

    fn literal(value: &str) -> ExternalArgumentKey {
        ExternalArgumentKey::Literal(value.as_bytes().to_vec())
    }

    fn path(root: &str, value: &str) -> ExternalArgumentKey {
        ExternalArgumentKey::Path(rooted(root, value))
    }

    #[test]
    fn root_sentinel_resolves_without_changing_command_bytes() {
        let graph = ExternalActionGraph {
            roots: BTreeMap::from([("src".to_string(), PathBuf::from("/source"))]),
            kconfig: None,
            initial_inputs: Vec::new(),
            empty_output_inputs: Vec::new(),
            actions: Vec::new(),
            analysis_barriers: Vec::new(),
            requested_outputs: Vec::new(),
        };
        let root = rooted("src", ".");
        assert_eq!(resolve_path(&graph, &root).unwrap(), Path::new("/source"));
        assert_eq!(
            resolve_argument(&graph, &ExternalArgumentKey::Path(root)).unwrap(),
            "/source"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_reuses_only_watched_leaf_values() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let source = &graph.actions[0].normal_inputs[0];
        let mut session = ExternalObservationSession::new(graph);
        assert!(session.stable.watcher.is_some());

        session.fence().unwrap();
        let first = session.observe(graph, &source.path, source.kind).unwrap();
        let second = session.observe(graph, &source.path, source.kind).unwrap();
        assert_eq!(first, second);
        assert_eq!(session.requests, 2);
        assert_eq!(session.reused, 1);

        std::fs::write(graph.roots["src"].join("a.in"), b"changed\n").unwrap();
        let error = session.fence().unwrap_err().to_string();
        assert!(error.contains("changed during replay"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_never_memoizes_proc_filesystems() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = snapshot(temp.path());
        snapshot
            .graph
            .roots
            .insert("host".to_string(), PathBuf::from("/"));
        snapshot.graph.actions[0].normal_inputs.push(ExternalInput {
            path: rooted("host", "proc/filesystems"),
            kind: ExternalInputKind::File,
        });
        let graph = &snapshot.graph;
        let input = graph.actions[0].normal_inputs.last().unwrap();
        let mut session = ExternalObservationSession::new(graph);

        let first = session.observe(graph, &input.path, input.kind).unwrap();
        let second = session.observe(graph, &input.path, input.kind).unwrap();
        assert_eq!(first, second);
        assert_eq!(session.requests, 2);
        assert_eq!(session.reused, 0);
        assert!(!session.stable.cacheable.contains_key(&input.path));

        let mut restarted = ExternalObservationSession::new(graph);
        assert_eq!(
            restarted.observe(graph, &input.path, input.kind).unwrap(),
            first
        );
        assert_eq!(restarted.reused, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_reuses_missing_path_below_missing_parent() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = snapshot(temp.path());
        snapshot.graph.actions[0].normal_inputs[0] = ExternalInput {
            path: rooted("src", "absent/child"),
            kind: ExternalInputKind::Missing,
        };
        let graph = &snapshot.graph;
        let missing = &graph.actions[0].normal_inputs[0];
        let mut session = ExternalObservationSession::new(graph);

        let first = session.observe(graph, &missing.path, missing.kind).unwrap();
        let second = session.observe(graph, &missing.path, missing.kind).unwrap();
        assert_eq!(first, ExternalInputValue::Missing);
        assert_eq!(second, ExternalInputValue::Missing);
        assert_eq!(session.reused, 1);

        std::fs::create_dir(graph.roots["src"].join("absent")).unwrap();
        let error = session.fence().unwrap_err().to_string();
        assert!(error.contains("changed during replay"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_detects_hard_link_writes() {
        let temp = tempfile::tempdir().unwrap();
        let alias_dir = temp.path().join("alias");
        std::fs::create_dir(&alias_dir).unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let source = &graph.actions[0].normal_inputs[0];
        std::fs::hard_link(graph.roots["src"].join("a.in"), alias_dir.join("a.in")).unwrap();
        let mut session = ExternalObservationSession::new(graph);

        session.observe(graph, &source.path, source.kind).unwrap();
        std::fs::write(alias_dir.join("a.in"), b"changed through alias\n").unwrap();

        let error = session.fence().unwrap_err().to_string();
        assert!(error.contains("changed during replay"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_ignores_write_open_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let source = &graph.actions[0].normal_inputs[0];
        let source_path = resolve_path(graph, &source.path).unwrap();
        let mut session = ExternalObservationSession::new(graph);

        let first = session.observe(graph, &source.path, source.kind).unwrap();
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .open(source_path)
                .unwrap(),
        );
        session.fence().unwrap();
        let second = session.observe(graph, &source.path, source.kind).unwrap();

        assert_eq!(first, second);
        assert_eq!(session.reused, 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_detects_ancestor_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let source = &graph.actions[0].normal_inputs[0];
        let source_root = graph.roots["src"].clone();
        let mut session = ExternalObservationSession::new(graph);

        session.observe(graph, &source.path, source.kind).unwrap();
        std::fs::rename(&source_root, temp.path().join("old-src")).unwrap();
        std::fs::create_dir(&source_root).unwrap();
        std::fs::write(source_root.join("a.in"), b"replacement\n").unwrap();

        let error = session.fence().unwrap_err().to_string();
        assert!(error.contains("changed during replay"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_reuses_generated_values_until_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let generated = &graph.actions[2].normal_inputs[0];
        let generated_path = resolve_path(graph, &generated.path).unwrap();
        std::fs::write(&generated_path, b"first\n").unwrap();
        let mut session = ExternalObservationSession::new(graph);

        let first = session
            .observe(graph, &generated.path, generated.kind)
            .unwrap();
        let second = session
            .observe(graph, &generated.path, generated.kind)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(session.requests, 2);
        assert_eq!(session.reused, 1);

        std::fs::write(&generated_path, b"second\n").unwrap();
        let error = session.fence().unwrap_err().to_string();
        assert!(error.contains("generated external input changed"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_disables_generated_memo_after_planned_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let generated = &graph.actions[2].normal_inputs[0];
        let generated_path = resolve_path(graph, &generated.path).unwrap();
        std::fs::write(&generated_path, b"first\n").unwrap();
        let mut session = ExternalObservationSession::new(graph);

        let first = session
            .observe(graph, &generated.path, generated.kind)
            .unwrap();
        session.before_output_mutation().unwrap();
        std::fs::write(&generated_path, b"second\n").unwrap();
        let second = session
            .observe(graph, &generated.path, generated.kind)
            .unwrap();

        assert_ne!(first, second);
        assert_eq!(session.requests, 2);
        assert_eq!(session.reused, 0);
        assert!(session.generated.watcher.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn planned_output_mutation_does_not_excuse_an_earlier_foreign_generated_write() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let generated = &graph.actions[2].normal_inputs[0];
        let path = resolve_path(graph, &generated.path).unwrap();
        std::fs::write(&path, b"before\n").unwrap();
        let mut session = ExternalObservationSession::new(graph);
        session.observe(graph, &generated.path, generated.kind).unwrap();
        std::fs::write(&path, b"foreign\n").unwrap();
        let error = session.before_output_mutation().unwrap_err().to_string();
        assert!(error.contains("generated external input changed"), "{error}");
        assert!(session.generated.watcher.is_some(), "a failed fence must not disable the guard");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn planned_output_mutation_does_not_disable_stable_input_fence() {
        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let source = &graph.actions[0].normal_inputs[0];
        let mut session = ExternalObservationSession::new(graph);

        session.observe(graph, &source.path, source.kind).unwrap();
        std::fs::write(graph.roots["src"].join("a.in"), b"changed\n").unwrap();

        let error = session.before_output_mutation().unwrap_err().to_string();
        assert!(error.contains("external leaf input changed"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_session_rehashes_symlinked_executables() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let snapshot = snapshot(temp.path());
        let graph = &snapshot.graph;
        let executable = &graph.actions[0].commands[0].executable;
        let executable_path = resolve_path(graph, executable).unwrap();
        let target = graph.roots["src"].join("real-tool");
        std::fs::write(&target, b"first tool\n").unwrap();
        std::fs::remove_file(&executable_path).unwrap();
        symlink(&target, &executable_path).unwrap();
        let mut session = ExternalObservationSession::new(graph);

        let first = session.observe_executable(graph, executable).unwrap();
        std::fs::write(&target, b"second tool\n").unwrap();
        let second = session.observe_executable(graph, executable).unwrap();

        assert_ne!(first, second);
        assert_eq!(session.requests, 2);
        assert_eq!(session.reused, 0);
    }

    fn external_action(
        logical_id: &str,
        argv: Vec<ExternalArgumentKey>,
        inputs: Vec<ExternalInput>,
        output: &str,
        dependencies: Vec<&str>,
        config_deps: Vec<&str>,
    ) -> ExternalAction {
        ExternalAction {
            logical_id: logical_id.to_string(),
            mnemonic: "ExternalTest".to_string(),
            commands: vec![ExternalCommand {
                executable: rooted("src", "tool"),
                argv0: ExternalArgumentKey::Literal(format!("captured-{logical_id}").into_bytes()),
                argv,
                environment: None,
                working_directory: None,
            }],
            working_directory: rooted("obj", "."),
            environment: BTreeMap::new(),
            signal_state: match logical_id {
                "a" => ExternalSignalState {
                    blocked: 1_u64 << (10 - 1),
                    ignored: 0,
                },
                "b" => ExternalSignalState {
                    blocked: 0,
                    ignored: 1_u64 << (12 - 1),
                },
                _ => ExternalSignalState::default(),
            },
            file_creation_mask: match logical_id {
                "a" => 0o077,
                "b" => 0o027,
                _ => 0o022,
            },
            credentials: ExternalCredentialState {
                supplementary_groups: match logical_id {
                    "a" => vec![10],
                    "b" => vec![20],
                    _ => Vec::new(),
                },
                ..ExternalCredentialState::default()
            },
            normal_inputs: inputs,
            order_only_inputs: Vec::new(),
            outputs: vec![ExternalOutput {
                path: rooted("obj", output),
                kind: ExternalOutputKind::File,
                mode: 0o640,
            }],
            dependencies: dependencies.into_iter().map(str::to_string).collect(),
            config_deps: config_deps.into_iter().map(str::to_string).collect(),
            depfile: None,
            restat: true,
            always_run: false,
            cacheable: true,
            local_only: true,
            sandbox: false,
            resource: ManifestResourceClass::Other,
            execution_properties: BTreeMap::new(),
        }
    }

    fn snapshot(root: &Path) -> ExternalAnalysisSnapshot {
        let src = root.join("src");
        let obj = root.join("obj");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&obj).unwrap();
        std::fs::write(src.join("tool"), b"test tool bytes").unwrap();
        std::fs::write(src.join("a.in"), b"a1\n").unwrap();
        std::fs::write(src.join("b.in"), b"b1\n").unwrap();
        std::fs::write(src.join("Makefile"), b"all:\n").unwrap();
        std::fs::write(obj.join(".config"), b"CONFIG_FEATURE=y\n").unwrap();
        let analyzer = ExternalAnalyzerIdentity {
            kind: "test-analyzer".to_string(),
            executable_digest: [7; 32],
            argv: vec![b"analyzer".to_vec()],
            working_directory: src.clone(),
            environment: BTreeMap::new(),
            build_options: BTreeMap::new(),
            platform: BTreeMap::new(),
        };
        let key = crate::external_analysis_snapshot::ExternalAnalysisKey::new(
            src.clone(),
            obj.clone(),
            &analyzer,
            vec![b"all".to_vec()],
        );
        let makefile = src.join("Makefile");
        let manifest = DependencyManifest::from_deps(vec![Dependency::FileContent {
            path: makefile.clone(),
            digest: content_digest_of_regular_file(&makefile, 1 << 20).unwrap(),
        }]);
        ExternalAnalysisSnapshot::new(
            key,
            analyzer,
            manifest,
            ExternalActionGraph {
                roots: BTreeMap::from([("src".to_string(), src), ("obj".to_string(), obj)]),
                kconfig: Some(ExternalKconfigSource {
                    path: rooted("obj", ".config"),
                    captured_bytes: b"CONFIG_FEATURE=y\n".to_vec(),
                }),
                initial_inputs: Vec::new(),
                empty_output_inputs: Vec::new(),
                actions: vec![
                    external_action(
                        "a",
                        vec![literal("a"), path("src", "a.in"), path("obj", "a.o")],
                        vec![ExternalInput {
                            path: rooted("src", "a.in"),
                            kind: ExternalInputKind::File,
                        }],
                        "a.o",
                        Vec::new(),
                        vec!["CONFIG_FEATURE"],
                    ),
                    external_action(
                        "b",
                        vec![literal("b"), path("src", "b.in"), path("obj", "b.o")],
                        vec![ExternalInput {
                            path: rooted("src", "b.in"),
                            kind: ExternalInputKind::File,
                        }],
                        "b.o",
                        Vec::new(),
                        Vec::new(),
                    ),
                    external_action(
                        "link",
                        vec![
                            literal("link"),
                            path("obj", "a.o"),
                            path("obj", "b.o"),
                            path("obj", "final"),
                        ],
                        vec![
                            ExternalInput {
                                path: rooted("obj", "a.o"),
                                kind: ExternalInputKind::File,
                            },
                            ExternalInput {
                                path: rooted("obj", "b.o"),
                                kind: ExternalInputKind::File,
                            },
                        ],
                        "final",
                        vec!["a", "b"],
                        Vec::new(),
                    ),
                ],
                analysis_barriers: Vec::new(),
                requested_outputs: vec![rooted("obj", "final")],
            },
        )
        .unwrap()
    }

    fn disk_cache(root: &Path) -> Arc<ActionCache> {
        Arc::new(
            ActionCache::with_disk(CacheConfig {
                cache_root: root.join("actions"),
                max_size: 0,
                ttl_seconds: 0,
            })
            .unwrap(),
        )
    }

    async fn replay(
        snapshot: ExternalAnalysisSnapshot,
        cache_root: &Path,
        runner: Arc<RecordingRunner>,
    ) -> ExternalReplayStatus {
        let engine = ExternalReplayEngine::with_runner_and_cas(
            snapshot,
            disk_cache(cache_root),
            ResourceManager::new().unwrap(),
            runner,
            Arc::new(CasStore::new(cache_root).unwrap()),
        )
        .unwrap();
        engine.replay().await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_runtime_directory_checkpoint_brackets_stage_dependencies() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = snapshot(temp.path());
        let obj = snapshot.key.output_root.clone();
        std::fs::create_dir(obj.join("result")).unwrap();
        snapshot.graph.initial_inputs.push(ExternalInitialInput {
            path: rooted("obj", "result"),
            value: ExternalInitialInputValue::Directory { mode: 0o755 },
        });

        let child = external_action(
            "child",
            vec![
                literal("child"),
                path("obj", "transient"),
                path("obj", "transient/child"),
            ],
            Vec::new(),
            "transient/child",
            Vec::new(),
            Vec::new(),
        );
        let mut parent = external_action(
            "parent",
            vec![
                literal("parent"),
                path("obj", "transient"),
                path("obj", "transient/child"),
                path("obj", "result/final"),
            ],
            vec![ExternalInput {
                path: rooted("obj", "transient/child"),
                kind: ExternalInputKind::File,
            }],
            "result/final",
            vec!["child"],
            Vec::new(),
        );
        parent.cacheable = false;
        parent.sandbox = true;
        parent.execution_properties.insert(
            crate::external_analysis_snapshot::MANDATORY_EXECUTION_PROPERTY.to_string(),
            crate::external_analysis_snapshot::PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION.to_string(),
        );
        let members = vec![b".config".to_vec(), b"result".to_vec()];
        for index in 0..2 {
            let (name, value) =
                crate::external_analysis_snapshot::encode_runtime_directory_checkpoint(
                    index,
                    &rooted("obj", "."),
                    &members,
                )
                .unwrap();
            parent.execution_properties.insert(name, value);
        }
        snapshot.graph.actions = vec![child, parent];
        snapshot.graph.requested_outputs = vec![rooted("obj", "result/final")];
        snapshot
            .graph
            .analysis_barriers
            .push(ExternalAnalysisBarrier {
            producer_closure: vec!["child".to_string()],
            expected_inputs: vec![ExternalInputKey {
                path: rooted("obj", "transient/child"),
                value: ExternalInputValue::File {
                    digest: *blake3::hash(b"child\n").as_bytes(),
                    executable: false,
                },
            }],
            blocked_actions: vec!["parent".to_string()],
        });
        snapshot.validate_structure().unwrap();

        let runner = Arc::new(RecordingRunner::default());
        assert_eq!(
            counts(replay(snapshot, &temp.path().join("cache"), runner).await),
            (0, 2)
        );
        assert_eq!(std::fs::read(obj.join("result/final")).unwrap(), b"child\n");
        assert!(!obj.join("transient").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn direct_runtime_directory_checkpoint_waits_for_retained_member_producer() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = snapshot(temp.path());
        let obj = snapshot.key.output_root.clone();
        std::fs::create_dir(obj.join("result")).unwrap();
        snapshot.graph.initial_inputs.push(ExternalInitialInput {
            path: rooted("obj", "result"),
            value: ExternalInitialInputValue::Directory { mode: 0o755 },
        });

        let producer = external_action(
            "child",
            vec![
                literal("child"),
                path("obj", "result/scratch"),
                path("obj", "retained"),
            ],
            Vec::new(),
            "retained",
            Vec::new(),
            Vec::new(),
        );
        let mut observer = external_action(
            "parent",
            vec![
                literal("parent"),
                path("obj", "result/scratch"),
                path("obj", "retained"),
                path("obj", "result/final"),
            ],
            vec![ExternalInput {
                path: rooted("obj", "retained"),
                kind: ExternalInputKind::File,
            }],
            "result/final",
            vec!["child"],
            Vec::new(),
        );
        observer.cacheable = false;
        observer.sandbox = true;
        observer.execution_properties.insert(
            crate::external_analysis_snapshot::MANDATORY_EXECUTION_PROPERTY.to_string(),
            crate::external_analysis_snapshot::PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION.to_string(),
        );
        let members = vec![
            b".config".to_vec(),
            b"result".to_vec(),
            b"retained".to_vec(),
        ];
        for index in 0..2 {
            let (name, value) =
                crate::external_analysis_snapshot::encode_ordered_runtime_directory_checkpoint(
                    index,
                    &rooted("obj", "."),
                    &members,
                    u64::try_from(index).unwrap(),
                    &["child".to_string()],
                    &[],
                )
                .unwrap();
            observer.execution_properties.insert(name, value);
        }
        snapshot.graph.actions = vec![producer, observer];
        snapshot.graph.requested_outputs = vec![rooted("obj", "result/final")];
        snapshot.validate_structure().unwrap();

        let runner = Arc::new(RecordingRunner::default());
        assert_eq!(
            counts(replay(snapshot, &temp.path().join("cache"), runner).await),
            (0, 2)
        );
        assert_eq!(std::fs::read(obj.join("result/final")).unwrap(), b"child\n");
    }

    fn counts(status: ExternalReplayStatus) -> (usize, usize) {
        match status {
            ExternalReplayStatus::Complete {
                cache_hits,
                executed,
            } => (cache_hits, executed),
            ExternalReplayStatus::AnalysisMismatch { .. } => {
                panic!("unexpected analysis mismatch")
            }
        }
    }

    fn continuation_barrier(digest: [u8; 32]) -> ExternalAnalysisBarrier {
        ExternalAnalysisBarrier {
            producer_closure: vec!["a".to_string()],
            expected_inputs: vec![ExternalInputKey {
                path: rooted("obj", "a.o"),
                value: ExternalInputValue::File {
                    digest,
                    executable: false,
                },
            }],
            blocked_actions: vec!["b".to_string(), "link".to_string()],
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn matching_continuation_replays_the_blocked_graph_without_analyzer() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = snapshot(temp.path());
        snapshot
            .graph
            .analysis_barriers
            .push(continuation_barrier(*blake3::hash(b"a1\n").as_bytes()));
        snapshot.validate_structure().unwrap();
        let runner = Arc::new(RecordingRunner::default());

        assert_eq!(
            counts(replay(snapshot, &temp.path().join("cache"), runner.clone()).await),
            (0, 3)
        );
        assert_eq!(runner.calls(), ["a", "b", "link"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mismatched_continuation_stops_before_blocked_actions() {
        let temp = tempfile::tempdir().unwrap();
        let mut snapshot = snapshot(temp.path());
        snapshot
            .graph
            .analysis_barriers
            .push(continuation_barrier(*blake3::hash(b"wrong\n").as_bytes()));
        snapshot.validate_structure().unwrap();
        let runner = Arc::new(RecordingRunner::default());

        match replay(snapshot, &temp.path().join("cache"), runner.clone()).await {
            ExternalReplayStatus::AnalysisMismatch {
                barrier_index,
                cache_hits,
                executed,
            } => {
                assert_eq!(barrier_index, 0);
                assert_eq!(cache_hits, 0);
                assert_eq!(executed, 1);
            }
            status => panic!("unexpected replay status: {status:?}"),
        }
        assert_eq!(runner.calls(), ["a"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restart_noop_cas_restore_and_dirty_closure_are_make_free() {
        let temp = tempfile::tempdir().unwrap();
        let primary = snapshot(temp.path());
        let cache_root = temp.path().join("cache");
        let runner = Arc::new(RecordingRunner::default());

        assert_eq!(
            counts(replay(primary.clone(), &cache_root, runner.clone()).await),
            (0, 3)
        );
        assert_producers_precede_link(&runner.calls());
        let recorded = runner.requests();
        assert_eq!(recorded.len(), 3);
        for (label, request) in recorded {
            let expected = primary.graph.actions.iter().find(|action| action.logical_id == label).unwrap();
            assert_eq!(request.signal_state, Some(expected.signal_state), "{label}");
            assert_eq!(request.file_creation_mask, Some(expected.file_creation_mask), "{label}");
            assert_eq!(request.credentials.as_ref(), Some(&expected.credentials), "{label}");
            let argv0 = format!("captured-{label}");
            assert_eq!(request.command.argv0.as_deref(), Some(argv0.as_str()), "{label}");
            assert_eq!(request.command.executable, primary.key.workspace_root.join("tool"), "{label}");
            assert_eq!(request.command.argv, expected.commands[0].argv.iter()
                .map(|argument| resolve_argument(&primary.graph, argument).unwrap()).collect::<Vec<_>>(), "{label}");
            assert_eq!(request.command.working_dir.as_ref(), Some(&primary.key.output_root), "{label}");
            assert!(request.command.env.is_empty(), "{label}");
            assert_eq!(request.command.env_policy, ActionEnvPolicy::ExplicitOnly, "{label}");
            assert_eq!(request.mnemonic, expected.mnemonic, "{label}");
            assert_eq!(request.resource_class, Some(map_resource_class(expected.resource)), "{label}");
            assert_eq!(request.declared_action_id, DeclaredActionId(0), "{label}");
            assert_eq!(request.owner_target, TargetId(0), "{label}");
            assert!(request.declared_inputs.is_empty() && request.declared_outputs.is_empty(), "{label}");
            assert_eq!(request.timeout, None, "{label}");
            assert_eq!(request.log_command_index, Some(0), "{label}");
            assert!(!request.remove_stale_archive_before_execute, "{label}");
            assert!(request.terminate_process_group_on_drop && request.stdin_null, "{label}");
            assert_eq!(request.isolation, None, "{label}");
        }
        for output in ["a.o", "b.o", "final"] {
            assert_eq!(
                permission_mode(&std::fs::metadata(primary.key.output_root.join(output)).unwrap()),
                0o640
            );
        }

        let relocated = snapshot(&temp.path().join("relocated"));
        assert_eq!(
            counts(replay(relocated.clone(), &cache_root, runner.clone()).await),
            (3, 0)
        );
        assert_eq!(
            std::fs::read(relocated.key.output_root.join("final")).unwrap(),
            b"a1\nb1\n"
        );
        assert_eq!(
            permission_mode(&std::fs::metadata(relocated.key.output_root.join("final")).unwrap()),
            0o640
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                primary.key.output_root.join("final"),
                std::fs::Permissions::from_mode(0o664),
            )
            .unwrap();
        }
        assert_eq!(
            counts(replay(primary.clone(), &cache_root, runner.clone()).await),
            (3, 0)
        );
        assert_eq!(
            permission_mode(&std::fs::metadata(primary.key.output_root.join("final")).unwrap()),
            0o640
        );

        let obj = &primary.key.output_root;
        std::fs::remove_file(obj.join("a.o")).unwrap();
        std::fs::remove_file(obj.join("b.o")).unwrap();
        std::fs::remove_file(obj.join("final")).unwrap();
        std::fs::remove_file(obj.join(".config")).unwrap();
        assert_eq!(
            counts(replay(primary.clone(), &cache_root, runner.clone()).await),
            (3, 0)
        );
        assert_eq!(std::fs::read(obj.join("final")).unwrap(), b"a1\nb1\n");
        for output in ["a.o", "b.o", "final"] {
            assert_eq!(
                permission_mode(&std::fs::metadata(obj.join(output)).unwrap()),
                0o640
            );
        }
        assert_eq!(
            std::fs::read(obj.join(".config")).unwrap(),
            b"CONFIG_FEATURE=y\n"
        );

        std::fs::write(primary.key.workspace_root.join("a.in"), b"a2\n").unwrap();
        assert_eq!(
            counts(replay(primary.clone(), &cache_root, runner.clone()).await),
            (1, 2)
        );
        assert_eq!(std::fs::read(obj.join("final")).unwrap(), b"a2\nb1\n");

        std::fs::write(obj.join(".config"), b"# CONFIG_FEATURE is not set\n").unwrap();
        assert_eq!(
            counts(replay(primary, &cache_root, runner.clone()).await),
            (2, 1)
        );
        assert_eq!(runner.calls().len(), 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn controller_owned_seed_can_be_replaced_by_its_output_producer() {
        let temp = tempfile::tempdir().unwrap();
        let mut primary = snapshot(temp.path());
        primary.graph.initial_inputs.push(ExternalInitialInput {
            path: rooted("obj", "a.o"),
            value: ExternalInitialInputValue::File { bytes: b"seed\n".to_vec(), mode: 0o640 },
        });
        let cache_root = temp.path().join("cache");
        let runner = Arc::new(RecordingRunner::default());
        let engine = ExternalReplayEngine::with_runner_and_cas(
            primary.clone(), disk_cache(&cache_root), ResourceManager::new().unwrap(),
            runner.clone(), Arc::new(CasStore::new(&cache_root).unwrap()),
        ).unwrap();
        assert_eq!(std::fs::read(primary.key.output_root.join("a.o")).unwrap(), b"seed\n");
        assert_eq!(counts(engine.replay().await.unwrap()), (0, 3));
        assert_eq!(std::fs::read(primary.key.output_root.join("final")).unwrap(), b"a1\nb1\n");
        // A restart must preserve the produced value rather than reseeding it.
        assert_eq!(counts(replay(primary, &cache_root, runner).await), (3, 0));
    }

    struct ParkAfterPublishingRunner {
        recording: RecordingRunner,
        published: tokio::sync::Notify,
        release: tokio::sync::Notify,
        published_gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    #[async_trait]
    impl LocalActionRunner for ParkAfterPublishingRunner {
        async fn execute(&self, request: ExecutionRequest) -> Result<ActionExecutionResult> {
            let park = request.command.argv[0] == "a";
            let result = self.recording.execute(request).await?;
            if park {
                *self.published_gate.0.lock().unwrap() = true;
                self.published_gate.1.notify_all();
                self.published.notify_one();
                self.release.notified().await;
            }
            Ok(result)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cached_sibling_completes_while_dirty_producer_is_still_running() {
        assert_cached_sibling_with_parked_producer(false).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cached_sibling_completes_while_uncacheable_producer_is_still_running() {
        assert_cached_sibling_with_parked_producer(true).await;
    }

    async fn assert_cached_sibling_with_parked_producer(uncacheable: bool) {
        let temp = tempfile::tempdir().unwrap();
        let mut primary = snapshot(temp.path());
        if uncacheable {
            let action = &mut primary.graph.actions[0];
            action.cacheable = false;
            action.sandbox = true;
            action.credentials = ExternalCredentialState::default();
            action.execution_properties.insert(
                crate::external_analysis_snapshot::MANDATORY_EXECUTION_PROPERTY.to_string(),
                crate::external_analysis_snapshot::PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION.to_string(),
            );
        }
        let cache_root = temp.path().join("cache");
        assert_eq!(counts(replay(primary.clone(), &cache_root, Arc::new(RecordingRunner::default())).await), (0, 3));
        if !uncacheable {
            std::fs::write(primary.key.workspace_root.join("a.in"), b"a2\n").unwrap();
        }
        let runner = Arc::new(ParkAfterPublishingRunner {
            recording: RecordingRunner::default(),
            published: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            published_gate: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        });
        let cached_sibling = Arc::new(tokio::sync::Notify::new());
        let mut engine = ExternalReplayEngine::with_runner_and_cas(
            primary.clone(), disk_cache(&cache_root), ResourceManager::new().unwrap(),
            runner.clone(), Arc::new(CasStore::new(&cache_root).unwrap()),
        ).unwrap();
        engine.before_manager_observer = Some({
            let gate = runner.published_gate.clone();
            Arc::new(move |logical_id| {
                if logical_id == "b" {
                    // Force b's cache-hit validation to occur after a wrote
                    // a.o, while a is parked before returning its result.
                    let (published, _) = gate.1.wait_timeout_while(
                        gate.0.lock().unwrap(), Duration::from_secs(10), |published| !*published,
                    ).unwrap();
                    assert!(*published, "producer did not publish before cached sibling");
                }
            })
        });
        engine.completion_observer = Some({
            let cached_sibling = cached_sibling.clone();
            Arc::new(move |logical_id, cached| {
                if logical_id == "b" && cached {
                    cached_sibling.notify_one();
                }
            })
        });
        let replay_task = tokio::spawn(async move { engine.replay().await });
        tokio::time::timeout(Duration::from_secs(10), runner.published.notified()).await.unwrap();
        // The producer has already written a.o but cannot complete. Require
        // the actual cached-sibling completion fence to pass in this window.
        tokio::time::timeout(Duration::from_secs(10), cached_sibling.notified()).await.unwrap();
        assert!(!replay_task.is_finished());
        runner.release.notify_one();
        let result = replay_task.await.unwrap().unwrap();
        assert_eq!(counts(result), if uncacheable { (2, 1) } else { (1, 2) });
        assert_eq!(std::fs::read(primary.key.output_root.join("final")).unwrap(),
            if uncacheable { &b"a1\nb1\n"[..] } else { &b"a2\nb1\n"[..] });
        let expected_calls = if uncacheable {
            vec!["a".to_string()]
        } else {
            vec!["a".to_string(), "link".to_string()]
        };
        assert_eq!(runner.recording.calls(), expected_calls);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sandboxed_uncacheable_action_executes_on_every_replay() {
        let temp = tempfile::tempdir().unwrap();
        let mut primary = snapshot(temp.path());
        primary.graph.actions[0].cacheable = false;
        primary.graph.actions[0].sandbox = true;
        primary.graph.actions[0].credentials = ExternalCredentialState::default();
        primary.graph.actions[0].execution_properties.insert(
            crate::external_analysis_snapshot::MANDATORY_EXECUTION_PROPERTY.to_string(),
            crate::external_analysis_snapshot::PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION.to_string(),
        );
        primary.validate_structure().unwrap();
        let canonical = primary.clone();
        let output_root = primary.key.output_root.clone();
        let cache_root = temp.path().join("cache");
        let runner = Arc::new(RecordingRunner::default());

        assert_eq!(
            counts(replay(primary.clone(), &cache_root, runner.clone()).await),
            (0, 3)
        );
        assert_eq!(
            counts(replay(primary, &cache_root, runner.clone()).await),
            (2, 1)
        );
        let calls = runner.calls();
        assert_producers_precede_link(&calls[..3]);
        assert_eq!(calls[3..], ["a"]);
        let requests = runner.requests();
        assert_eq!(requests.len(), 4);
        for (label, request) in requests {
            if label == "a" {
                assert_eq!(request.isolation.as_ref().unwrap().writable_roots(), [output_root.as_path()]);
            } else {
                assert!(request.isolation.is_none(), "{label}");
            }
        }

        let mut invalid = Vec::new();
        let mut changed = canonical.clone();
        changed.graph.actions[0].cacheable = true;
        invalid.push(changed);
        let mut changed = canonical.clone();
        changed.graph.actions[0].always_run = true;
        invalid.push(changed);
        let mut changed = canonical.clone();
        changed.graph.actions[0].local_only = false;
        invalid.push(changed);
        let mut changed = canonical.clone();
        changed.graph.actions[0].sandbox = false;
        invalid.push(changed);
        let mut changed = canonical;
        changed.graph.actions[0].execution_properties.insert(
            crate::external_analysis_snapshot::MANDATORY_EXECUTION_PROPERTY.to_string(),
            "unknown-policy".to_string(),
        );
        invalid.push(changed);
        for snapshot in invalid {
            assert!(snapshot
                .validate_structure()
                .unwrap_err()
                .to_string()
                .contains("policy"));
        }
    }

    #[test]
    fn sandboxed_action_rejects_unconfined_root_and_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let mut primary = snapshot(temp.path());
        primary.graph.actions[0].sandbox = true;
        primary.graph.actions[0].credentials = ExternalCredentialState::default();
        let action = primary.graph.actions[0].clone();
        let noncanonical = primary.key.output_root.join("..").join("obj");
        primary.graph.roots.insert("obj".to_string(), noncanonical);
        assert!(external_action_isolation(&primary.graph, &action)
            .unwrap_err()
            .to_string()
            .contains("not confined and canonical"));

        let primary = snapshot(&temp.path().join("credentials"));
        let mut action = primary.graph.actions[0].clone();
        action.sandbox = true;
        action.credentials = ExternalCredentialState {
            effective_user: 1,
            ..ExternalCredentialState::default()
        };
        assert!(external_action_isolation(&primary.graph, &action)
            .unwrap_err()
            .to_string()
            .contains("credentials are not isolatable"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn empty_output_bootstrap_hits_and_cache_miss_fails_before_execution() {
        let temp = tempfile::tempdir().unwrap();
        let primary = snapshot(temp.path());
        let obj = primary.key.output_root.clone();
        std::fs::create_dir(obj.join("bootstrap")).unwrap();
        std::fs::write(obj.join("bootstrap/state"), b"captured\n").unwrap();
        std::fs::write(obj.join("final"), b"preexisting output\n").unwrap();

        let mut primary = primary;
        primary.graph.initial_inputs = vec![
            ExternalInitialInput {
                path: rooted("obj", "bootstrap"),
                value: ExternalInitialInputValue::Directory { mode: 0o750 },
            },
            ExternalInitialInput {
                path: rooted("obj", "bootstrap/state"),
                value: ExternalInitialInputValue::File {
                    bytes: b"captured\n".to_vec(),
                    mode: 0o640,
                },
            },
        ];
        primary.graph.actions[0].normal_inputs.push(ExternalInput {
            path: rooted("obj", "."),
            kind: ExternalInputKind::DirectoryListing,
        });
        primary.graph.empty_output_inputs = vec![ExternalEmptyOutputInput {
            path: rooted("obj", "."),
            kind: ExternalInputKind::DirectoryListing,
            value: ExternalInputValue::DirectoryListing {
                digest: digest_directory_listing(&obj).unwrap(),
            },
        }];
        primary.validate_structure().unwrap();

        let cache_root = temp.path().join("cache");
        let runner = Arc::new(RecordingRunner::default());
        assert_eq!(
            counts(replay(primary.clone(), &cache_root, runner.clone()).await),
            (0, 3)
        );

        std::fs::remove_dir_all(&obj).unwrap();
        std::fs::create_dir(&obj).unwrap();
        assert_eq!(
            counts(replay(primary.clone(), &cache_root, runner.clone()).await),
            (3, 0)
        );
        assert_eq!(
            std::fs::read(obj.join("bootstrap/state")).unwrap(),
            b"captured\n"
        );
        assert_eq!(
            permission_mode(&std::fs::metadata(obj.join("bootstrap")).unwrap()),
            0o750
        );
        assert_eq!(
            permission_mode(&std::fs::metadata(obj.join("bootstrap/state")).unwrap()),
            0o640
        );

        std::fs::remove_dir_all(&obj).unwrap();
        std::fs::create_dir(&obj).unwrap();
        std::fs::write(primary.key.workspace_root.join("a.in"), b"changed\n").unwrap();
        let engine = ExternalReplayEngine::with_runner_and_cas(
            primary,
            disk_cache(&cache_root),
            ResourceManager::new().unwrap(),
            runner.clone(),
            Arc::new(CasStore::new(&cache_root).unwrap()),
        )
        .unwrap();
        let error = engine.replay().await.unwrap_err().to_string();
        assert!(error.contains("replay external action \"a\""));
        assert_eq!(runner.calls().len(), 3);
    }
}
