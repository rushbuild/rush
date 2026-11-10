// Rush Build - owned native external-repository execution namespace
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Local execution layout only. Source admission is the orchestrator's job;
//! this view does not constitute a hermetic REAPI input-root proof. Generation
//! directories are immutable and validated before every retained admission.

use anyhow::{bail, Context, Result};
use rush_incremental::tracked;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const VERSION: u32 = 1;
const OWNER: &[u8] = b"rush-native-execroots-v1\n";
const RECEIPT: &str = "namespace.json";

/// The frontend implements the default external/<repository> execution layout.
/// Missing legacy streams retain that default; malformed supplied streams fail.
pub(crate) fn require_default_layout(command: &rush_command_core::CommandSemantics) -> Result<()> {
    use rush_command_core::build_arguments::{
        from_command, normalize_tokens, EFFECTIVE_BUILD_ARGUMENTS_ENV,
    };
    let arguments = if command
        .getenv(EFFECTIVE_BUILD_ARGUMENTS_ENV.as_bytes())
        .is_some()
    {
        from_command(command).map_err(anyhow::Error::msg)?
    } else {
        // Reuse the request projection's operand boundaries: a copt/action_env
        // value which looks like a flag is still opaque data. Lossy decoding is
        // used only for this ASCII-name gate, never forwarded or used in keys;
        // invalid bytes cannot become an accepted ASCII Boolean spelling.
        let raw: Vec<_> = command
            .argv()
            .iter()
            .map(|argument| String::from_utf8_lossy(argument).into_owned())
            .collect();
        normalize_tokens(&raw).map_err(anyhow::Error::msg)?
    };
    let mut sibling = false;
    let layout_argument = |argument: &str| {
        argument.starts_with("--experimental_sibling_repository_layout")
            || argument.starts_with("--noexperimental_sibling_repository_layout")
    };
    let mut arguments = arguments.into_iter().peekable();
    let mut unknown_operand: Option<String> = None;
    while let Some(argument) = arguments.next() {
        // Unknown bare options may consume the following flag-looking token.
        // Do not grant a layout interpretation when that boundary is unknown.
        if let Some(option) = unknown_operand.take() {
            if argument.starts_with('-')
                && (layout_argument(&argument)
                    || arguments.clone().any(|argument| layout_argument(&argument)))
            {
                bail!("cannot establish native repository-layout option boundary after unsupported option {option:?}; use its joined NAME=VALUE spelling");
            }
        }
        if argument == "--" {
            break;
        }
        // Match the service-operand boundary used by build_arguments::encode.
        if matches!(argument.as_str(), "--daemon" | "--watchfs") {
            arguments.next();
            continue;
        }
        if argument == "--experimental_sibling_repository_layout" {
            sibling = true;
        } else if argument == "--noexperimental_sibling_repository_layout" {
            sibling = false;
        } else if let Some(value) =
            argument.strip_prefix("--experimental_sibling_repository_layout=")
        {
            sibling = rush_starlark::config_defines::parse_bazel_bool_predicate(value)
                .map_err(|error| anyhow::anyhow!("unsupported native repository-layout Boolean argument {argument:?}: {error}"))?;
        } else if argument.starts_with("--experimental_sibling_repository_layout")
            || argument.starts_with("--noexperimental_sibling_repository_layout")
        {
            bail!("unsupported native repository-layout argument: {argument:?}");
        } else if argument.starts_with('-') && !argument.contains('=') {
            unknown_operand = Some(argument);
        }
    }
    if sibling {
        bail!("native builds support the external/<repository> execution layout; --experimental_sibling_repository_layout=true is unsupported");
    }
    Ok(())
}

/// Async analysis/execution may migrate worker threads between polls. Install
/// the command's IO only for each poll, never leave thread-local state across
/// a suspension or leak it into another command on the same worker.
pub(crate) async fn scope_future<F: std::future::Future>(
    io: Option<tracked::SharedTrackedIo>,
    future: F,
) -> F::Output {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|context| match &io {
        Some(io) => tracked::scope(io.clone(), || future.as_mut().poll(context)),
        None => future.as_mut().poll(context),
    })
    .await
}

pub(crate) fn command_cache_root(source: &Path) -> Result<PathBuf> {
    match tracked::getenv(b"RUSH_WORKSPACE_CACHE_ROOT") {
        #[cfg(unix)]
        Some(value) => Ok(path(&value)),
        #[cfg(not(unix))]
        Some(value) => Ok(PathBuf::from(
            std::str::from_utf8(&value).context("native cache root is not UTF-8")?,
        )),
        None => Ok(source.join(".cache/rush")),
    }
}

#[cfg(unix)]
fn bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(unix)]
fn path(value: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(value.to_vec()))
}
/// Publication writes are relative to acquired directory descriptors. A named
/// directory replaced after acquisition cannot redirect those writes.
#[cfg(unix)]
struct Directory(fs::File);
#[cfg(unix)]
impl Directory {
    fn name(name: &std::ffi::OsStr) -> Result<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(name);
        if path.components().count() != 1
            || !matches!(path.components().next(), Some(Component::Normal(_)))
        {
            bail!("invalid descriptor-relative native namespace component");
        }
        Ok(std::ffi::CString::new(name.as_bytes())?)
    }
    fn from_open(fd: i32) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Successful openat returns a fresh owned descriptor.
        Ok(Self(unsafe { fs::File::from_raw_fd(fd) }))
    }
    fn open_absolute(path: &Path) -> Result<Self> {
        use nix::libc;
        use std::os::fd::FromRawFd;
        if !path.is_absolute() {
            bail!("native namespace directory anchor is not absolute");
        }
        let root = std::ffi::CString::new("/")?;
        let fd = unsafe {
            libc::open(
                root.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut current = Self(unsafe { fs::File::from_raw_fd(fd) });
        for component in path.components() {
            match component {
                Component::RootDir => (),
                Component::Normal(name) => current = current.open_directory(name)?,
                _ => bail!("non-normal native namespace directory anchor"),
            }
        }
        Ok(current)
    }
    fn open_directory(&self, name: &std::ffi::OsStr) -> Result<Self> {
        use nix::libc;
        use std::os::fd::AsRawFd;
        let name = Self::name(name)?;
        Self::from_open(unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        })
        .map_err(Into::into)
    }
    fn create_directory(&self, name: &std::ffi::OsStr) -> Result<Self> {
        use std::os::fd::AsRawFd;
        let name_c = Self::name(name)?;
        if unsafe { nix::libc::mkdirat(self.0.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        self.open_directory(name)
    }
    fn require_named_directory(&self, name: &std::ffi::OsStr, expected: &Self) -> Result<()> {
        use nix::libc;
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let name = Self::name(name)?;
        let mut actual = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe {
            libc::fstatat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                actual.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let actual = unsafe { actual.assume_init() };
        let expected = expected.0.metadata()?;
        if actual.st_mode & libc::S_IFMT != libc::S_IFDIR
            || actual.st_dev as u64 != expected.dev()
            || actual.st_ino as u64 != expected.ino()
        {
            bail!("native execution namespace directory identity changed during publication");
        }
        Ok(())
    }
    fn write_new(&self, name: &std::ffi::OsStr, bytes: &[u8]) -> Result<()> {
        use nix::libc;
        use std::os::fd::{AsRawFd, FromRawFd};
        let name = Self::name(name)?;
        let fd = unsafe {
            libc::openat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        unsafe { fs::File::from_raw_fd(fd) }.write_all(bytes)?;
        Ok(())
    }
    fn read_regular(&self, name: &std::ffi::OsStr, limit: usize) -> Result<Vec<u8>> {
        use nix::libc;
        use std::io::Read;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::MetadataExt;
        let name = Self::name(name)?;
        let open = || -> Result<fs::File> {
            let fd = unsafe {
                libc::openat(
                    self.0.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(unsafe { fs::File::from_raw_fd(fd) })
        };
        let file = open()?;
        let before = file.metadata()?;
        if !before.is_file() || before.len() >= limit as u64 {
            bail!("native namespace proof is not a regular file of the expected bounded size");
        }
        let stamp = |metadata: &fs::Metadata| {
            (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.len(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            )
        };
        let mut result = Vec::new();
        (&file).take(limit as u64).read_to_end(&mut result)?;
        if stamp(&before) != stamp(&file.metadata()?)
            || stamp(&before) != stamp(&open()?.metadata()?)
        {
            bail!("native namespace proof identity changed during admission");
        }
        Ok(result)
    }
    fn require_path_identity(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let named = fs::symlink_metadata(path)?;
        let opened = self.0.metadata()?;
        if !named.is_dir()
            || named.file_type().is_symlink()
            || (named.dev(), named.ino()) != (opened.dev(), opened.ino())
        {
            bail!("native namespace named directory identity changed during admission");
        }
        Ok(())
    }
    fn symlink(&self, name: &std::ffi::OsStr, target: &Path) -> Result<()> {
        use std::os::fd::AsRawFd;
        let name = Self::name(name)?;
        let target = std::ffi::CString::new(bytes(target))?;
        if unsafe { nix::libc::symlinkat(target.as_ptr(), self.0.as_raw_fd(), name.as_ptr()) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

fn plain_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect execution namespace {}", path.display()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "native execution namespace directory collision: {}",
            path.display()
        );
    }
    Ok(())
}
fn relative(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!(
            "invalid native execution namespace entry: {}",
            path.display()
        );
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    source: Vec<u8>,
    repositories: BTreeMap<String, Vec<u8>>,
    directories: BTreeSet<Vec<u8>>,
    #[serde(with = "ordered_byte_links")]
    links: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// JSON object keys cannot represent arbitrary Unix path bytes. Encode the
/// sorted map as ordered (path bytes, target bytes) pairs without UTF-8 loss.
mod ordered_byte_links {
    use super::*;
    use serde::ser::SerializeSeq;

    pub fn serialize<S: serde::Serializer>(
        links: &BTreeMap<Vec<u8>, Vec<u8>>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(links.len()))?;
        for entry in links {
            sequence.serialize_element(&entry)?;
        }
        sequence.end()
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<BTreeMap<Vec<u8>, Vec<u8>>, D::Error> {
        let entries = Vec::<(Vec<u8>, Vec<u8>)>::deserialize(deserializer)?;
        let mut links = BTreeMap::new();
        for (key, value) in entries {
            if links
                .last_key_value()
                .is_some_and(|(previous, _)| previous >= &key)
            {
                return Err(serde::de::Error::custom(
                    "native namespace links must be strictly sorted and unique",
                ));
            }
            links.insert(key, value);
        }
        Ok(links)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NativeExecroot {
    root: PathBuf,
    generation: PathBuf,
    manifest: Manifest,
    key: String,
}
impl NativeExecroot {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}
#[cfg(not(unix))]
impl NativeExecroot {
    pub(crate) fn prepare(
        _: &Path,
        _: &Path,
        _: &BTreeMap<String, PathBuf>,
        _: &BTreeMap<String, PathBuf>,
    ) -> Result<Self> {
        bail!("native external execution namespaces require Unix symlink semantics");
    }
    pub(crate) fn validate(&self) -> Result<()> {
        bail!("native external execution namespaces require Unix symlink semantics");
    }
}

#[cfg(unix)]
impl NativeExecroot {
    pub(crate) fn prepare(
        source: &Path,
        cache: &Path,
        repositories: &BTreeMap<String, PathBuf>,
        aliases: &BTreeMap<String, PathBuf>,
    ) -> Result<Self> {
        if repositories.is_empty() {
            bail!("native external namespace requires an external build closure");
        }
        if !source.is_absolute()
            || !cache.is_absolute()
            || repositories
                .values()
                .chain(aliases.values())
                .any(|root| !root.is_absolute())
        {
            bail!(
                "native execution namespace requires absolute admitted source/cache/output roots"
            );
        }
        tracked::refresh_filesystem_observations();
        fs::create_dir_all(cache).context("create native namespace cache parent")?;
        let physical_cache =
            tracked::canonicalize(cache).context("resolve native namespace cache")?;
        let physical_source =
            tracked::canonicalize(source).context("resolve main native namespace source")?;
        if physical_source.starts_with(&physical_cache) {
            bail!("native namespace cache must not contain its main source root");
        }
        for output in aliases.values() {
            fs::create_dir_all(output).context("create managed native output root")?;
            if physical_cache.starts_with(fs::canonicalize(output)?) {
                bail!(
                    "native output root contains its execution namespace cache: {}",
                    output.display()
                );
            }
        }
        let mut manifest = Manifest {
            version: VERSION,
            source: bytes(source),
            repositories: repositories
                .iter()
                .map(|(name, root)| (name.clone(), bytes(root)))
                .collect(),
            directories: BTreeSet::from([Vec::new(), bytes(Path::new("external"))]),
            links: BTreeMap::new(),
        };
        let mut builder = Plan {
            manifest: &mut manifest,
            cache: &physical_cache,
            ancestors: BTreeSet::from([physical_source.clone()]),
        };
        builder.source_children(source, Path::new(""), aliases, true)?;
        builder.ancestors.clear();
        for (name, root) in repositories {
            if name.is_empty()
                || name == "."
                || name == ".."
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-+~".contains(&c))
            {
                bail!("invalid canonical repository name for native namespace: {name:?}");
            }
            if tracked::canonicalize(root)? == physical_cache {
                bail!("admitted repository root is the reserved native namespace cache: @{name}");
            }
            let destination = Path::new("external").join(name);
            if let Some(old) = builder.manifest.links.get(&bytes(&destination)) {
                if tracked::canonicalize(&path(old))? != tracked::canonicalize(root)? {
                    bail!("main source external/{name} collides with the admitted repository root");
                }
                builder.manifest.links.remove(&bytes(&destination));
            }
            if builder.manifest.directories.contains(&bytes(&destination)) {
                bail!("main source external/{name} directory collides with repository namespace");
            }
            builder.source_entry(root, &destination)?;
        }
        for (name, target) in aliases {
            let destination = Path::new(name);
            relative(destination)?;
            if destination.components().count() != 1 || name == "external" {
                bail!("invalid generated namespace alias: {name:?}");
            }
            let source_entry = source.join(name);
            if fs::symlink_metadata(&source_entry).is_ok()
                && source_entry != *target
                && tracked::canonicalize(&source_entry)? != fs::canonicalize(target)?
            {
                bail!(
                    "source-owned path collides with native generated alias: {}",
                    source_entry.display()
                );
            }
            builder.link(destination, target)?;
        }
        drop(builder);
        let encoded = serde_json::to_vec(&manifest)?;
        let key = blake3::hash(&encoded).to_hex().to_string();
        let base = cache.join("native-execroots-v1");
        let cache_directory = Directory::open_absolute(&physical_cache)?;
        let base_name = std::ffi::OsStr::new("native-execroots-v1");
        let base_directory = match cache_directory.create_directory(base_name) {
            Ok(directory) => {
                directory.write_new(std::ffi::OsStr::new("owner"), OWNER)?;
                directory
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                let directory = cache_directory.open_directory(base_name)?;
                if directory.read_regular(std::ffi::OsStr::new("owner"), OWNER.len() + 1)? != OWNER
                {
                    bail!(
                        "unowned native execution namespace cache: {}",
                        base.display()
                    );
                }
                directory
            }
            Err(error) => return Err(error).context("create native namespace cache"),
        };
        cache_directory.require_named_directory(base_name, &base_directory)?;
        let generation = base.join(&key);
        let namespace = Self {
            root: generation.join("root"),
            generation,
            manifest,
            key,
        };
        if fs::symlink_metadata(&namespace.generation).is_ok() {
            namespace.validate()?;
            return Ok(namespace);
        }
        namespace.publish_at(&base_directory, &encoded, || Ok(()))?;
        cache_directory.require_named_directory(base_name, &base_directory)?;
        namespace.validate()?;
        Ok(namespace)
    }

    fn publish_at(
        &self,
        base: &Directory,
        encoded: &[u8],
        acquired: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        // Exclusive creation never replaces a raced empty directory/symlink.
        // All later writes are relative to these acquired handles, even when
        // a named generation/root/base is concurrently renamed or replaced.
        let generation = base
            .create_directory(std::ffi::OsStr::new(&self.key))
            .context("exclusively create native execution namespace generation")?;
        let root = generation.create_directory(std::ffi::OsStr::new("root"))?;
        acquired()?;
        let mut directories = BTreeMap::from([(PathBuf::new(), root)]);
        for directory in &self.manifest.directories {
            if directory.is_empty() {
                continue;
            }
            let directory = path(directory);
            relative(&directory)?;
            let parent = directories
                .get(directory.parent().unwrap_or(Path::new("")))
                .context("missing acquired namespace parent")?;
            let child = parent.create_directory(
                directory
                    .file_name()
                    .context("namespace directory has no basename")?,
            )?;
            directories.insert(directory, child);
        }
        for (name, target) in &self.manifest.links {
            let name = path(name);
            relative(&name)?;
            let parent = directories
                .get(name.parent().unwrap_or(Path::new("")))
                .context("missing acquired namespace link parent")?;
            parent.symlink(
                name.file_name().context("namespace link has no basename")?,
                &path(target),
            )?;
        }
        generation.write_new(std::ffi::OsStr::new(RECEIPT), encoded)?;
        for (name, child) in &directories {
            if name.as_os_str().is_empty() {
                continue;
            }
            directories[name.parent().unwrap_or(Path::new(""))]
                .require_named_directory(name.file_name().unwrap(), child)?;
        }
        generation
            .require_named_directory(std::ffi::OsStr::new("root"), &directories[Path::new("")])?;
        base.require_named_directory(std::ffi::OsStr::new(&self.key), &generation)?;
        Ok(())
    }

    #[cfg(test)]
    fn publish(&self, encoded: &[u8]) -> Result<()> {
        let physical_base = fs::canonicalize(self.generation.parent().unwrap())?;
        let base = Directory::open_absolute(&physical_base)?;
        self.publish_at(&base, encoded, || Ok(()))
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.generation.file_name() != Some(std::ffi::OsStr::new(&self.key))
            || blake3::hash(&serde_json::to_vec(&self.manifest)?)
                .to_hex()
                .as_str()
                != self.key
        {
            bail!("native namespace generation identity mismatch");
        }
        plain_directory(&self.generation)?;
        plain_directory(&self.root)?;
        let mut entries = fs::read_dir(&self.generation)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        if entries != [OsString::from(RECEIPT), OsString::from("root")] {
            bail!("unexpected native execution generation entries");
        }
        let generation_directory = Directory::open_absolute(&fs::canonicalize(&self.generation)?)?;
        generation_directory.require_path_identity(&self.generation)?;
        let expected = serde_json::to_vec(&self.manifest)?;
        let limit = expected
            .len()
            .checked_add(1)
            .context("native namespace receipt size overflow")?;
        if generation_directory.read_regular(std::ffi::OsStr::new(RECEIPT), limit)? != expected {
            bail!("native execution namespace receipt disagrees with admitted source map");
        }
        generation_directory.require_path_identity(&self.generation)?;
        let mut children: BTreeMap<PathBuf, BTreeSet<OsString>> = BTreeMap::new();
        for directory in &self.manifest.directories {
            let directory = path(directory);
            plain_directory(&self.root.join(&directory))?;
            children.entry(directory.clone()).or_default();
            if let Some(name) = directory.file_name() {
                children
                    .entry(directory.parent().unwrap_or(Path::new("")).to_owned())
                    .or_default()
                    .insert(name.to_owned());
            }
        }
        for (name, target) in &self.manifest.links {
            let name = path(name);
            relative(&name)?;
            let file = self.root.join(&name);
            if !fs::symlink_metadata(&file)?.file_type().is_symlink()
                || fs::read_link(&file)? != path(target)
            {
                bail!(
                    "foreign or changed native execution namespace link: {}",
                    file.display()
                );
            }
            children
                .entry(name.parent().unwrap_or(Path::new("")).to_owned())
                .or_default()
                .insert(
                    name.file_name()
                        .context("namespace link has no basename")?
                        .to_owned(),
                );
        }
        for (directory, expected) in children {
            let actual = fs::read_dir(self.root.join(directory))?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<BTreeSet<_>>>()?;
            if actual != expected {
                bail!("foreign or missing native execution namespace entries");
            }
        }
        generation_directory.require_path_identity(&self.generation)?;
        Ok(())
    }
}

#[cfg(unix)]
struct Plan<'a> {
    manifest: &'a mut Manifest,
    cache: &'a Path,
    ancestors: BTreeSet<PathBuf>,
}
#[cfg(unix)]
impl Plan<'_> {
    fn link(&mut self, destination: &Path, target: &Path) -> Result<()> {
        relative(destination)?;
        if self.manifest.directories.contains(&bytes(destination))
            || self
                .manifest
                .links
                .insert(bytes(destination), bytes(target))
                .is_some()
        {
            bail!(
                "native execution namespace entry collision: {}",
                destination.display()
            );
        }
        Ok(())
    }
    fn source_entry(&mut self, source: &Path, destination: &Path) -> Result<()> {
        let io = tracked::current_required();
        let mark = io.mark();
        let physical = tracked::canonicalize(source)
            .with_context(|| format!("resolve native namespace source {}", source.display()))?;
        let owned_forest = self.cache.join("native-execroots-v1");
        // Reject intermediate links into an earlier generation as well as a
        // final directory under it. A link through its external/owner entry
        // can otherwise resolve back out to source and hide the forest hop.
        let crosses_owned_forest =
            io.captured_since(mark)
                .iter()
                .any(|dependency| match dependency {
                    rush_incremental::Dependency::FileExistence { path, .. }
                    | rush_incremental::Dependency::Symlink { path, .. } => {
                        path.starts_with(&owned_forest)
                    }
                    _ => false,
                });
        if physical.starts_with(&owned_forest) || crosses_owned_forest {
            bail!(
                "native namespace source points into an owned execution forest: {}",
                source.display()
            );
        }
        if physical == self.cache {
            return Ok(());
        }
        if self.cache.starts_with(&physical) {
            if !self.ancestors.insert(physical.clone()) {
                bail!(
                    "native execution namespace source cycle through a cache ancestor: {}",
                    source.display()
                );
            }
            self.manifest.directories.insert(bytes(destination));
            let result = self.source_children(source, destination, &BTreeMap::new(), false);
            self.ancestors.remove(&physical);
            result
        } else {
            self.link(destination, source)
        }
    }
    fn source_children(
        &mut self,
        source: &Path,
        destination: &Path,
        aliases: &BTreeMap<String, PathBuf>,
        main: bool,
    ) -> Result<()> {
        let mut names = tracked::read_dir_names(source)
            .with_context(|| format!("enumerate native source namespace {}", source.display()))?;
        names.sort();
        for name in names {
            if main && aliases.contains_key(name.to_str().unwrap_or("")) {
                continue;
            }
            let child = source.join(&name);
            let target = destination.join(&name);
            if main && name == "external" {
                if !fs::metadata(&child)?.is_dir() {
                    bail!("source external path collides with native repository namespace");
                }
                let physical = tracked::canonicalize(&child)?;
                if !self.ancestors.insert(physical.clone()) {
                    bail!("native execution namespace source cycle through external/");
                }
                self.source_children(&child, &target, &BTreeMap::new(), false)?;
                self.ancestors.remove(&physical);
            } else {
                self.source_entry(&child, &target)?;
            }
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture() -> Result<(
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        BTreeMap<String, PathBuf>,
        BTreeMap<String, PathBuf>,
    )> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("main");
        let cache = source.join(".cache/rush");
        let repo = temp.path().join("repo");
        fs::create_dir_all(source.join(".cache/legitimate"))?;
        fs::create_dir_all(repo.join("pkg"))?;
        fs::write(source.join(".cache/legitimate/keep"), b"keep")?;
        fs::write(source.join("BUILD"), b"main")?;
        fs::write(repo.join("pkg/header.h"), b"first")?;
        let aliases = BTreeMap::from([
            ("rush-out".to_owned(), source.join("rush-out")),
            (
                "rush-bin".to_owned(),
                source.join("rush-out/k8-fastbuild/bin"),
            ),
        ]);
        Ok((
            temp,
            source,
            cache,
            BTreeMap::from([("owner".to_owned(), repo)]),
            aliases,
        ))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_namespace_scope_installs_io_per_poll_without_leaking() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let io = tracked::test_scope(temp.path(), tracked::current_required);
        assert!(tracked::current().is_none());
        scope_future(Some(io), async {
            assert!(tracked::current().is_some());
            tokio::task::yield_now().await;
            assert!(tracked::current().is_some());
            assert!(tracked::getenv(b"NAMESPACE_SCOPE_TEST_UNSET").is_none());
        })
        .await;
        assert!(tracked::current().is_none());
        Ok(())
    }

    #[test]
    fn native_namespace_preserves_source_authority_and_excludes_only_owned_cache() -> Result<()> {
        let (_temp, source, cache, repos, aliases) = fixture()?;
        tracked::test_scope(&source, || -> Result<()> {
            let first = NativeExecroot::prepare(&source, &cache, &repos, &aliases)?;
            assert_eq!(fs::read(first.root().join("BUILD"))?, b"main");
            assert_eq!(
                fs::read(first.root().join(".cache/legitimate/keep"))?,
                b"keep"
            );
            assert!(!first.root().join(".cache/rush").exists());
            assert_eq!(
                fs::read(first.root().join("external/owner/pkg/header.h"))?,
                b"first"
            );
            fs::write(repos["owner"].join("pkg/header.h"), b"edited")?;
            assert_eq!(
                fs::read(first.root().join("external/owner/pkg/header.h"))?,
                b"edited"
            );
            let second = NativeExecroot::prepare(&source, &cache, &repos, &aliases)?;
            assert_eq!(first.root(), second.root());
            assert_eq!(
                fs::read_link(first.root().join("rush-out"))?,
                aliases["rush-out"]
            );
            assert_eq!(
                fs::read_link(first.root().join("rush-bin"))?,
                aliases["rush-bin"]
            );
            assert!(fs::read_dir(first.root().join(".cache"))?
                .all(|entry| entry.unwrap().file_name() != "rush"));
            Ok(())
        })
    }

    #[test]
    fn native_namespace_byte_pair_receipt_publishes_non_utf8_paths_losslessly() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;
        let (_temp, source, cache, repos, aliases) = fixture()?;
        let name = OsString::from_vec(b"source-\xff.h".to_vec());
        fs::write(source.join(&name), b"arbitrary filename bytes")?;
        tracked::test_scope(&source, || -> Result<()> {
            let namespace = NativeExecroot::prepare(&source, &cache, &repos, &aliases)?;
            assert_eq!(
                fs::read(namespace.root().join(&name))?,
                b"arbitrary filename bytes"
            );
            let encoded = serde_json::to_vec(&namespace.manifest)?;
            let json: serde_json::Value = serde_json::from_slice(&encoded)?;
            assert!(
                json["links"].is_array(),
                "byte paths must not become JSON object keys"
            );
            let decoded: Manifest = serde_json::from_slice(&encoded)?;
            assert_eq!(decoded, namespace.manifest);
            assert_eq!(
                serde_json::to_vec(&decoded)?,
                encoded,
                "canonical byte-pair encoding must remain stable"
            );
            namespace.validate()?;
            assert_eq!(
                NativeExecroot::prepare(&source, &cache, &repos, &aliases)?.root(),
                namespace.root()
            );
            Ok(())
        })
    }

    #[test]
    fn native_namespace_retains_logical_mapping_for_equal_byte_source_retargets() -> Result<()> {
        let (_temp, source, cache, repos, aliases) = fixture()?;
        let logical = source.parent().unwrap().join("logical-repo");
        symlink(&repos["owner"], &logical)?;
        let logical_repos = BTreeMap::from([("owner".to_owned(), logical.clone())]);
        let (first, manifest) = tracked::test_scope(&source, || -> Result<_> {
            let namespace = NativeExecroot::prepare(&source, &cache, &logical_repos, &aliases)?;
            Ok((namespace, tracked::current_required().manifest()))
        })?;
        let replacement = source.parent().unwrap().join("replacement");
        fs::create_dir_all(replacement.join("pkg"))?;
        fs::write(replacement.join("pkg/header.h"), b"first")?;
        fs::remove_file(&logical)?;
        symlink(&replacement, &logical)?;
        let second = tracked::test_scope(&source, || -> Result<_> {
            assert!(!matches!(
                tracked::current_required()
                    .validate_manifest(&manifest)
                    .staleness,
                rush_incremental::Staleness::Fresh
            ));
            NativeExecroot::prepare(&source, &cache, &logical_repos, &aliases)
        })?;
        // Full physical cwd stays keyed. The same logical registration does not
        // manufacture a new cwd for an equal-byte source symlink retarget.
        assert_eq!(first.root(), second.root());
        assert_eq!(
            fs::read_link(second.root().join("external/owner"))?,
            logical
        );
        Ok(())
    }

    #[test]
    fn native_namespace_refuses_foreign_generation_entries_links_and_receipts() -> Result<()> {
        for collision in ["empty", "symlink", "receipt", "extra", "link"] {
            let (_temp, source, cache, repos, aliases) = fixture()?;
            let original = tracked::test_scope(&source, || {
                NativeExecroot::prepare(&source, &cache, &repos, &aliases)
            })?;
            match collision {
                "empty" => {
                    fs::remove_dir_all(&original.generation)?;
                    fs::create_dir(&original.generation)?;
                }
                "symlink" => {
                    fs::remove_dir_all(&original.generation)?;
                    symlink(&source, &original.generation)?;
                }
                "receipt" => fs::write(original.generation.join(RECEIPT), b"{}")?,
                "extra" => fs::write(original.root().join("foreign"), b"private")?,
                "link" => {
                    fs::remove_file(original.root().join("BUILD"))?;
                    symlink(
                        repos["owner"].join("pkg/header.h"),
                        original.root().join("BUILD"),
                    )?;
                }
                _ => unreachable!(),
            }
            assert!(
                tracked::test_scope(&source, || NativeExecroot::prepare(
                    &source, &cache, &repos, &aliases
                ))
                .is_err(),
                "{collision}"
            );
            match collision {
                "empty" => assert_eq!(fs::read_dir(&original.generation)?.count(), 0),
                "symlink" => assert_eq!(fs::read_link(&original.generation)?, source),
                "receipt" => assert_eq!(fs::read(original.generation.join(RECEIPT))?, b"{}"),
                "extra" => assert_eq!(fs::read(original.root().join("foreign"))?, b"private"),
                "link" => assert_eq!(
                    fs::read_link(original.root().join("BUILD"))?,
                    repos["owner"].join("pkg/header.h")
                ),
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    #[test]
    fn native_namespace_publication_never_replaces_a_raced_empty_directory_or_symlink() -> Result<()>
    {
        for symlink_collision in [false, true] {
            let (_temp, source, cache, repos, aliases) = fixture()?;
            let namespace = tracked::test_scope(&source, || {
                NativeExecroot::prepare(&source, &cache, &repos, &aliases)
            })?;
            fs::remove_dir_all(&namespace.generation)?;
            assert!(fs::symlink_metadata(&namespace.generation).is_err());
            // A foreign object arrives between prepare's precheck and its
            // exclusive publication. Neither receipt nor root may be written.
            let generation = namespace.generation.clone();
            let foreign = source.clone();
            std::thread::spawn(move || {
                if symlink_collision {
                    symlink(foreign, generation)
                } else {
                    fs::create_dir(generation)
                }
            })
            .join()
            .unwrap()?;
            assert!(namespace
                .publish(&serde_json::to_vec(&namespace.manifest)?)
                .is_err());
            if symlink_collision {
                assert_eq!(fs::read_link(&namespace.generation)?, source);
            } else {
                assert_eq!(fs::read_dir(&namespace.generation)?.count(), 0);
            }
        }
        Ok(())
    }

    #[test]
    fn native_namespace_descriptor_publication_never_follows_replaced_owned_names() -> Result<()> {
        for replaced in ["base", "generation", "root", "receipt"] {
            let (temp, source, cache, repos, aliases) = fixture()?;
            let namespace = tracked::test_scope(&source, || {
                NativeExecroot::prepare(&source, &cache, &repos, &aliases)
            })?;
            fs::remove_dir_all(&namespace.generation)?;
            let physical_cache = fs::canonicalize(&cache)?;
            let cache_fd = Directory::open_absolute(&physical_cache)?;
            let base_name = std::ffi::OsStr::new("native-execroots-v1");
            let base_fd = cache_fd.open_directory(base_name)?;
            let base_path = cache.join(base_name);
            let foreign = temp.path().join("foreign");
            fs::create_dir(&foreign)?;
            fs::write(foreign.join("victim"), b"untouched")?;
            let result = namespace
                .publish_at(
                    &base_fd,
                    &serde_json::to_vec(&namespace.manifest)?,
                    || -> Result<()> {
                        // Deterministic interleaving after the generation and root FDs
                        // have been acquired, before any population/receipt writes.
                        match replaced {
                            "base" => {
                                fs::rename(&base_path, cache.join("moved-owned-base"))?;
                                symlink(&foreign, &base_path)?;
                            }
                            "generation" => {
                                fs::rename(
                                    &namespace.generation,
                                    base_path.join("moved-owned-generation"),
                                )?;
                                symlink(&foreign, &namespace.generation)?;
                            }
                            "root" => {
                                fs::rename(
                                    &namespace.root,
                                    namespace.generation.join("moved-owned-root"),
                                )?;
                                symlink(&foreign, &namespace.root)?;
                            }
                            "receipt" => {
                                symlink(foreign.join("victim"), namespace.generation.join(RECEIPT))?
                            }
                            _ => unreachable!(),
                        }
                        Ok(())
                    },
                )
                .and_then(|()| cache_fd.require_named_directory(base_name, &base_fd));
            assert!(result.is_err(), "{replaced} replacement must refuse");
            assert_eq!(fs::read(foreign.join("victim"))?, b"untouched");
            assert_eq!(
                fs::read_dir(&foreign)?
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<BTreeSet<_>>>()?,
                BTreeSet::from([OsString::from("victim")])
            );
        }
        Ok(())
    }

    #[test]
    fn native_namespace_receipt_and_owner_corruption_remain_bounded_and_nofollow() -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        for corrupt in [
            "huge-receipt",
            "receipt-symlink",
            "huge-owner",
            "owner-symlink",
        ] {
            let (temp, source, cache, repos, aliases) = fixture()?;
            let namespace = tracked::test_scope(&source, || {
                NativeExecroot::prepare(&source, &cache, &repos, &aliases)
            })?;
            let target = if corrupt.ends_with("receipt") || corrupt == "receipt-symlink" {
                namespace.generation.join(RECEIPT)
            } else {
                cache.join("native-execroots-v1/owner")
            };
            fs::remove_file(&target)?;
            if corrupt.starts_with("huge") {
                // A sparse multi-GiB corrupt proof must be rejected by metadata
                // before allocating or reading its claimed length.
                let foreign = temp.path().join("foreign-huge-proof");
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&foreign)?
                    .set_len(4 * 1024 * 1024 * 1024)?;
                fs::hard_link(&foreign, &target)?;
            } else {
                let foreign = temp.path().join("foreign-proof");
                fs::write(&foreign, b"preserve")?;
                symlink(&foreign, &target)?;
            }
            assert!(
                tracked::test_scope(&source, || NativeExecroot::prepare(
                    &source, &cache, &repos, &aliases
                ))
                .is_err(),
                "{corrupt}"
            );
            if corrupt.starts_with("huge") {
                assert_eq!(fs::metadata(&target)?.len(), 4 * 1024 * 1024 * 1024);
            } else {
                assert_eq!(fs::read(fs::read_link(&target)?)?, b"preserve");
            }
        }
        Ok(())
    }

    #[test]
    fn native_namespace_refuses_cache_ancestor_symlink_cycles() -> Result<()> {
        let (_temp, source, cache, repos, aliases) = fixture()?;
        symlink(&source, source.join("self"))?;
        let error = tracked::test_scope(&source, || {
            NativeExecroot::prepare(&source, &cache, &repos, &aliases)
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("source cycle"));
        assert_eq!(fs::read_link(source.join("self"))?, source);
        assert!(!cache.join("native-execroots-v1").exists());
        Ok(())
    }

    #[test]
    fn native_namespace_refuses_source_links_into_owned_generation_descendants() -> Result<()> {
        for escape_through_repository in [false, true] {
            let (_temp, source, cache, repos, aliases) = fixture()?;
            let original = tracked::test_scope(&source, || {
                NativeExecroot::prepare(&source, &cache, &repos, &aliases)
            })?;
            let target = if escape_through_repository {
                original.root().join("external/owner")
            } else {
                original.root().to_owned()
            };
            symlink(&target, source.join("old-generation"))?;
            let error = tracked::test_scope(&source, || {
                NativeExecroot::prepare(&source, &cache, &repos, &aliases)
            })
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("owned execution forest"),
                "{error:#}"
            );
            assert_eq!(fs::read_link(source.join("old-generation"))?, target);
            original.validate()?;
        }
        Ok(())
    }

    #[test]
    fn native_namespace_rejects_source_alias_collision_without_touching_it() -> Result<()> {
        let (_temp, source, cache, repos, aliases) = fixture()?;
        symlink("missing-foreign-target", source.join("rush-bin"))?;
        assert!(tracked::test_scope(&source, || NativeExecroot::prepare(
            &source, &cache, &repos, &aliases
        ))
        .is_err());
        assert_eq!(
            fs::read_link(source.join("rush-bin"))?,
            PathBuf::from("missing-foreign-target")
        );
        assert!(!cache.join("native-execroots-v1").exists());
        Ok(())
    }
}
