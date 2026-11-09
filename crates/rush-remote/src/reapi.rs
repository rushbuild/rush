// SPDX-License-Identifier: MIT
//! File-based Remote Execution API actions with verified input and output CAS.

use crate::proto::build::bazel::remote::execution::v2 as re;
use crate::proto::google::{bytestream as bs, longrunning};
use anyhow::{bail, ensure, Context, Result};
use prost::Message;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tonic::transport::Channel;

/// One portable action. Paths are relative to the supplied execution root.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionSpec {
    /// Argument vector; the worker supplies any absolute tool paths.
    pub arguments: Vec<String>,
    /// Complete action environment; no client credentials are inherited.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Files or directories to snapshot, rejecting symbolic links.
    pub inputs: Vec<PathBuf>,
    /// Required regular-file outputs. Directory and symlink outputs are rejected.
    pub outputs: Vec<PathBuf>,
    /// Worker/toolchain selection. These properties participate in the key.
    pub platform: BTreeMap<String, String>,
    /// Worker execution timeout in seconds, separate from the client deadline.
    pub timeout_seconds: u32,
    /// Disable both remote action-cache reads and writes.
    #[serde(default)]
    pub no_cache: bool,
}

/// Verified remote outcome. A failed command never publishes local outputs.
#[derive(Debug, Serialize)]
pub struct Outcome {
    /// SHA256 of the canonical REAPI Action message.
    pub action_digest: String,
    /// Command exit status, including a remote command failure.
    pub exit_code: i32,
    /// Whether the server supplied an action-cache hit.
    pub cached: bool,
    /// Server-reported worker identifier, when supplied.
    pub worker: String,
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}

enum Blob {
    Bytes(Vec<u8>),
    File(PathBuf),
}

fn upload_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .context("generate ByteStream upload UUID")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[derive(Default)]
struct Directory {
    files: BTreeMap<String, re::FileNode>,
    directories: BTreeMap<String, Directory>,
}

/// Standard gRPC client. Endpoints should use TLS or an authenticated tunnel.
pub struct ReapiClient {
    channel: Channel,
    instance: String,
    batch_limit: i64,
}

fn batch_limit(advertised: i64) -> Result<i64> {
    ensure!(advertised >= 0, "invalid CAS batch size capability");
    Ok(if advertised == 0 {
        1024 * 1024
    } else {
        advertised.min(1024 * 1024)
    })
}

fn digest(bytes: &[u8]) -> re::Digest {
    re::Digest {
        hash: format!("{:x}", Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}

fn valid_digest(d: &re::Digest) -> Result<()> {
    ensure!(
        d.size_bytes >= 0
            && d.hash.len() == 64
            && d.hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid SHA256 digest"
    );
    Ok(())
}

fn relative(path: &Path) -> Result<String> {
    ensure!(!path.as_os_str().is_empty(), "empty artifact path");
    ensure!(
        path.components().all(|c| matches!(c, Component::Normal(_))),
        "artifact path must be relative without dot or parent components: {}",
        path.display()
    );
    let s = path.to_str().context("artifact path must be UTF-8")?;
    ensure!(
        s.split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "artifact path must be canonical"
    );
    ensure!(
        !s.contains('\\') && !s.contains('\0'),
        "invalid artifact path"
    );
    Ok(s.to_string())
}

fn safe_path(root: &Path, path: &Path) -> Result<PathBuf> {
    relative(path)?;
    let mut current = root.to_path_buf();
    for part in path.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => ensure!(
                !metadata.file_type().is_symlink(),
                "symbolic link in artifact path: {}",
                path.display()
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(current)
}

fn add_message<M: Message>(
    message: &M,
    blobs: &mut BTreeMap<String, (re::Digest, Blob)>,
) -> re::Digest {
    let data = message.encode_to_vec();
    let d = digest(&data);
    blobs
        .entry(d.hash.clone())
        .or_insert((d.clone(), Blob::Bytes(data)));
    d
}

impl Directory {
    fn insert(&mut self, path: &str, file: re::FileNode) -> Result<()> {
        if let Some((first, rest)) = path.split_once('/') {
            ensure!(
                !self.files.contains_key(first),
                "input file/directory collision"
            );
            self.directories
                .entry(first.to_string())
                .or_default()
                .insert(rest, file)
        } else {
            ensure!(
                !self.directories.contains_key(path),
                "input file/directory collision"
            );
            self.files.insert(
                path.to_string(),
                re::FileNode {
                    name: path.to_string(),
                    ..file
                },
            );
            Ok(())
        }
    }

    fn encode(&self, blobs: &mut BTreeMap<String, (re::Digest, Blob)>) -> re::Digest {
        let directories = self
            .directories
            .iter()
            .map(|(name, dir)| re::DirectoryNode {
                name: name.clone(),
                digest: Some(dir.encode(blobs)),
            })
            .collect();
        add_message(
            &re::Directory {
                files: self.files.values().cloned().collect(),
                directories,
                ..Default::default()
            },
            blobs,
        )
    }
}

fn snapshot(
    root: &Path,
    path: &Path,
    temp: &Path,
    tree: &mut Directory,
    blobs: &mut BTreeMap<String, (re::Digest, Blob)>,
    files: &mut BTreeSet<String>,
) -> Result<()> {
    let name = relative(path)?;
    let source = safe_path(root, path)?;
    let metadata = std::fs::symlink_metadata(&source)?;
    if metadata.is_dir() {
        for entry in std::fs::read_dir(&source)? {
            snapshot(
                root,
                &path.join(entry?.file_name()),
                temp,
                tree,
                blobs,
                files,
            )?;
        }
        // Preserve empty directories too.
        let mut dir = tree;
        for component in name.split('/') {
            ensure!(
                !dir.files.contains_key(component),
                "input file/directory collision"
            );
            dir = dir.directories.entry(component.to_string()).or_default();
        }
    } else {
        ensure!(
            metadata.is_file(),
            "input must be a regular file or directory"
        );
        if !files.insert(name.clone()) {
            return Ok(());
        }
        let dest = temp.join(files.len().to_string());
        std::fs::copy(&source, &dest)?;
        let mut reader = std::fs::File::open(&dest)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 131072];
        let mut len = 0;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            len += n as i64;
        }
        let d = re::Digest {
            hash: format!("{:x}", hasher.finalize()),
            size_bytes: len,
        };
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        tree.insert(
            &name,
            re::FileNode {
                digest: Some(d.clone()),
                is_executable: executable,
                ..Default::default()
            },
        )?;
        blobs.entry(d.hash.clone()).or_insert((d, Blob::File(dest)));
    }
    Ok(())
}

impl ReapiClient {
    /// Connect using tonic's supported HTTP(S) transport; do not inherit auth.
    pub async fn connect(endpoint: String, instance: String) -> Result<Self> {
        ensure!(
            !instance.starts_with('/') && !instance.ends_with('/'),
            "invalid instance name"
        );
        let tls = endpoint.starts_with("https://");
        let mut endpoint = Channel::from_shared(endpoint)?.connect_timeout(Duration::from_secs(10));
        if tls {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())?;
        }
        let channel = endpoint.connect().await?;
        let mut capabilities = re::capabilities_client::CapabilitiesClient::new(channel.clone());
        let caps = tokio::time::timeout(
            Duration::from_secs(10),
            capabilities.get_capabilities(re::GetCapabilitiesRequest {
                instance_name: instance.clone(),
            }),
        )
        .await
        .context("remote capabilities deadline exceeded")??
        .into_inner();
        ensure!(
            caps.cache_capabilities.as_ref().is_some_and(|c| c
                .digest_functions
                .contains(&(re::digest_function::Value::Sha256 as i32))),
            "remote CAS does not advertise SHA256 support"
        );
        ensure!(
            caps.execution_capabilities
                .as_ref()
                .is_some_and(|c| c.exec_enabled),
            "backend does not advertise remote execution"
        );
        let batch_limit = batch_limit(
            caps.cache_capabilities
                .as_ref()
                .unwrap()
                .max_batch_total_size_bytes,
        )?;
        Ok(Self {
            channel,
            instance,
            batch_limit,
        })
    }

    fn resource(&self, suffix: &str) -> String {
        if self.instance.is_empty() {
            suffix.to_string()
        } else {
            format!("{}/{suffix}", self.instance)
        }
    }

    async fn upload(&self, blobs: &BTreeMap<String, (re::Digest, Blob)>) -> Result<()> {
        let mut cas = re::content_addressable_storage_client::ContentAddressableStorageClient::new(
            self.channel.clone(),
        );
        let all: Vec<_> = blobs.values().map(|(d, _)| d.clone()).collect();
        for chunk in all.chunks(256) {
            let missing = cas
                .find_missing_blobs(re::FindMissingBlobsRequest {
                    instance_name: self.instance.clone(),
                    blob_digests: chunk.to_vec(),
                    ..Default::default()
                })
                .await?
                .into_inner()
                .missing_blob_digests;
            for d in missing {
                let (known, blob) = blobs
                    .get(&d.hash)
                    .context("server requested an unknown blob")?;
                ensure!(known == &d, "server requested a mismatched blob size");
                if d.size_bytes <= self.batch_limit {
                    let data = match blob {
                        Blob::Bytes(b) => b.clone(),
                        Blob::File(p) => std::fs::read(p)?,
                    };
                    let replies = cas
                        .batch_update_blobs(re::BatchUpdateBlobsRequest {
                            instance_name: self.instance.clone(),
                            requests: vec![re::batch_update_blobs_request::Request {
                                digest: Some(d.clone()),
                                data,
                                ..Default::default()
                            }],
                            ..Default::default()
                        })
                        .await?
                        .into_inner()
                        .responses;
                    ensure!(
                        replies.len() == 1 && replies[0].digest.as_ref() == Some(&d),
                        "invalid CAS upload acknowledgment"
                    );
                    let status = replies[0]
                        .status
                        .as_ref()
                        .context("missing CAS upload status")?;
                    ensure!(status.code == 0, "CAS upload failed: {}", status.message);
                } else {
                    self.upload_stream(&d, blob).await?;
                }
            }
        }
        Ok(())
    }

    async fn upload_stream(&self, d: &re::Digest, blob: &Blob) -> Result<()> {
        let id = upload_id()?;
        let name = self.resource(&format!("uploads/{id}/blobs/{}/{}", d.hash, d.size_bytes));
        let mut input: Box<dyn tokio::io::AsyncRead + Unpin + Send> = match blob {
            Blob::File(p) => Box::new(tokio::fs::File::open(p).await?),
            Blob::Bytes(data) => Box::new(std::io::Cursor::new(data.clone())),
        };
        let expected = d.size_bytes;
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let producer = tokio::spawn(async move {
            let mut offset = 0;
            loop {
                let mut data = vec![0; 131072];
                let n = input.read(&mut data).await?;
                if n == 0 {
                    break;
                }
                data.truncate(n);
                let next = offset + n as i64;
                tx.send(bs::WriteRequest {
                    resource_name: name.clone(),
                    write_offset: offset,
                    finish_write: next == expected,
                    data,
                })
                .await
                .context("upload stream closed")?;
                offset = next;
            }
            ensure!(offset == expected, "snapshot size changed during upload");
            Ok::<_, anyhow::Error>(())
        });
        let reply = bs::byte_stream_client::ByteStreamClient::new(self.channel.clone())
            .write(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await;
        producer.await??;
        ensure!(
            reply?.into_inner().committed_size == expected,
            "incomplete CAS upload"
        );
        Ok(())
    }

    async fn download(&self, d: &re::Digest, output: &mut impl Write) -> Result<()> {
        valid_digest(d)?;
        let name = self.resource(&format!("blobs/{}/{}", d.hash, d.size_bytes));
        let mut stream = bs::byte_stream_client::ByteStreamClient::new(self.channel.clone())
            .read(bs::ReadRequest {
                resource_name: name,
                read_offset: 0,
                read_limit: 0,
            })
            .await?
            .into_inner();
        let mut hash = Sha256::new();
        let mut size = 0;
        while let Some(part) = stream.message().await? {
            size += part.data.len() as i64;
            ensure!(size <= d.size_bytes, "oversized CAS response");
            hash.update(&part.data);
            output.write_all(&part.data)?;
        }
        ensure!(
            size == d.size_bytes && format!("{:x}", hash.finalize()) == d.hash,
            "CAS content digest mismatch"
        );
        Ok(())
    }

    async fn captured(&self, raw: Vec<u8>, d: Option<re::Digest>) -> Result<Vec<u8>> {
        const LIMIT: i64 = 8 * 1024 * 1024;
        if let Some(d) = d {
            ensure!(d.size_bytes <= LIMIT, "remote log exceeds 8 MiB");
            let mut data = Vec::new();
            self.download(&d, &mut data).await?;
            Ok(data)
        } else {
            ensure!(raw.len() <= LIMIT as usize, "remote log exceeds 8 MiB");
            Ok(raw)
        }
    }

    /// Snapshot declared inputs, execute with a bounded total deadline, verify
    /// every returned byte, then materialize declared regular-file outputs.
    pub async fn execute(
        &self,
        root: &Path,
        spec: &ActionSpec,
        deadline: Duration,
    ) -> Result<Outcome> {
        tokio::time::timeout(deadline, self.execute_inner(root, spec))
            .await
            .context("remote execution client deadline exceeded")?
    }

    async fn execute_inner(&self, root: &Path, spec: &ActionSpec) -> Result<Outcome> {
        ensure!(
            !spec.arguments.is_empty() && !spec.arguments[0].is_empty(),
            "missing command"
        );
        ensure!(
            spec.timeout_seconds > 0,
            "execution timeout must be positive"
        );
        ensure!(
            !spec.platform.is_empty(),
            "declare worker/toolchain platform requirements"
        );
        let root = root.canonicalize()?;
        ensure!(root.is_dir(), "execution root must be a directory");
        let temp = tempfile::tempdir_in(root.parent().context("execution root has no parent")?)?;
        let mut blobs = BTreeMap::new();
        let mut files = BTreeSet::new();
        let mut tree = Directory::default();
        for input in &spec.inputs {
            snapshot(&root, input, temp.path(), &mut tree, &mut blobs, &mut files)?;
        }
        let mut outputs = BTreeSet::new();
        for output in &spec.outputs {
            let name = relative(output)?;
            safe_path(&root, output)?;
            ensure!(!files.contains(&name), "output overwrites a declared input");
            ensure!(outputs.insert(name), "duplicate output path");
        }
        let platform = re::Platform {
            properties: spec
                .platform
                .iter()
                .map(|(name, value)| re::platform::Property {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        };
        let command = re::Command {
            arguments: spec.arguments.clone(),
            environment_variables: spec
                .environment
                .iter()
                .map(|(name, value)| re::command::EnvironmentVariable {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
            output_paths: outputs.iter().cloned().collect(),
            platform: Some(platform.clone()),
            ..Default::default()
        };
        let action = re::Action {
            command_digest: Some(add_message(&command, &mut blobs)),
            input_root_digest: Some(tree.encode(&mut blobs)),
            platform: Some(platform),
            timeout: Some(prost_types::Duration {
                seconds: spec.timeout_seconds as i64,
                nanos: 0,
            }),
            do_not_cache: spec.no_cache,
            ..Default::default()
        };
        let action_digest = add_message(&action, &mut blobs);
        self.upload(&blobs).await?;
        let cached = if spec.no_cache {
            None
        } else {
            match re::action_cache_client::ActionCacheClient::new(self.channel.clone())
                .get_action_result(re::GetActionResultRequest {
                    instance_name: self.instance.clone(),
                    action_digest: Some(action_digest.clone()),
                    ..Default::default()
                })
                .await
            {
                Ok(response) => Some(response.into_inner()),
                Err(error) if error.code() == tonic::Code::NotFound => None,
                Err(error) => return Err(error.into()),
            }
        };
        let response = if let Some(result) = cached {
            re::ExecuteResponse {
                result: Some(result),
                cached_result: true,
                ..Default::default()
            }
        } else {
            let mut execution = re::execution_client::ExecutionClient::new(self.channel.clone());
            let mut stream = execution
                .execute(re::ExecuteRequest {
                    instance_name: self.instance.clone(),
                    action_digest: Some(action_digest.clone()),
                    skip_cache_lookup: spec.no_cache,
                    ..Default::default()
                })
                .await?
                .into_inner();
            loop {
                let operation = stream
                    .message()
                    .await?
                    .context("execution stream ended before completion")?;
                if !operation.done {
                    continue;
                }
                match operation
                    .result
                    .context("completed operation has no result")?
                {
                    longrunning::operation::Result::Error(status) => bail!(
                        "remote operation failed ({}): {}",
                        status.code,
                        status.message
                    ),
                    longrunning::operation::Result::Response(any) => {
                        ensure!(
                            any.type_url
                                .ends_with("/build.bazel.remote.execution.v2.ExecuteResponse"),
                            "unexpected operation response type"
                        );
                        break re::ExecuteResponse::decode(any.value.as_slice())?;
                    }
                }
            }
        };
        if let Some(status) = response.status {
            ensure!(
                status.code == 0,
                "remote execution failed ({}): {}",
                status.code,
                status.message
            );
        }
        let result = response.result.context("missing remote action result")?;
        let stdout = self
            .captured(result.stdout_raw, result.stdout_digest)
            .await?;
        let stderr = self
            .captured(result.stderr_raw, result.stderr_digest)
            .await?;
        if result.exit_code == 0 {
            validate_outputs(&result.output_files, &outputs)?;
            ensure!(
                result.output_directories.is_empty()
                    && result.output_symlinks.is_empty()
                    && result.output_file_symlinks.is_empty()
                    && result.output_directory_symlinks.is_empty(),
                "directory/symlink outputs are unsupported"
            );
            let mut staged = Vec::new();
            for file in &result.output_files {
                let destination = safe_path(&root, Path::new(&file.path))?;
                let mut output = tempfile::NamedTempFile::new_in(&root)?;
                let d = file.digest.as_ref().context("output digest missing")?;
                if file.contents.is_empty() {
                    self.download(d, output.as_file_mut()).await?;
                } else {
                    ensure!(
                        digest(&file.contents) == *d,
                        "inline output digest mismatch"
                    );
                    output.write_all(&file.contents)?;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    output
                        .as_file()
                        .set_permissions(std::fs::Permissions::from_mode(
                            if file.is_executable { 0o755 } else { 0o644 },
                        ))?;
                }
                staged.push((output, destination));
            }
            // Verify all blobs before replacing any destination.
            for (output, destination) in staged {
                safe_path(&root, destination.strip_prefix(&root)?)?;
                std::fs::create_dir_all(destination.parent().context("output has no parent")?)?;
                output
                    .persist(destination)
                    .context("publish verified output")?;
            }
        }
        Ok(Outcome {
            action_digest: action_digest.hash,
            exit_code: result.exit_code,
            cached: response.cached_result,
            worker: result
                .execution_metadata
                .map(|m| m.worker)
                .unwrap_or_default(),
            stdout,
            stderr,
        })
    }
}

fn validate_outputs(files: &[re::OutputFile], expected: &BTreeSet<String>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for file in files {
        relative(Path::new(&file.path))?;
        ensure!(
            expected.contains(&file.path) && seen.insert(file.path.clone()),
            "unexpected or duplicate remote output"
        );
        valid_digest(file.digest.as_ref().context("output digest missing")?)?;
    }
    ensure!(&seen == expected, "remote action omitted a declared output");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytestream_upload_ids_are_uuid_v4() {
        let id = upload_id().unwrap();
        assert_ne!(id, upload_id().unwrap());
        assert_eq!(id.len(), 36);
        assert_eq!(
            id.split('-').map(str::len).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert_eq!(id.as_bytes()[14], b'4');
        assert!(matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
        assert!(id.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit()));
    }

    #[test]
    fn respect_the_server_batch_size_limit() {
        assert_eq!(batch_limit(0).unwrap(), 1024 * 1024);
        assert_eq!(batch_limit(4096).unwrap(), 4096);
        assert_eq!(batch_limit(8 * 1024 * 1024).unwrap(), 1024 * 1024);
        assert!(batch_limit(-1).is_err());
    }

    #[test]
    fn reject_nonportable_artifact_paths() {
        for path in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "a\\b",
        ] {
            assert!(relative(Path::new(path)).is_err(), "accepted {path}");
        }
        assert_eq!(relative(Path::new("a/b.cc")).unwrap(), "a/b.cc");
    }

    #[test]
    fn sha256_is_the_standard_wire_digest() {
        assert_eq!(
            digest(b"abc").hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(digest(b"abc").size_bytes, 3);
        for d in [
            re::Digest {
                hash: "../escape".into(),
                size_bytes: 3,
            },
            re::Digest {
                hash: "a".repeat(64),
                size_bytes: -1,
            },
        ] {
            assert!(valid_digest(&d).is_err());
        }
    }

    #[test]
    fn merkle_order_and_executable_bit() {
        fn make(order: &[&str], executable: bool) -> re::Digest {
            let mut tree = Directory::default();
            for path in order {
                tree.insert(
                    path,
                    re::FileNode {
                        digest: Some(digest(path.as_bytes())),
                        is_executable: executable,
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            tree.encode(&mut BTreeMap::new())
        }
        assert_eq!(
            make(&["src/z", "src/a"], false),
            make(&["src/a", "src/z"], false)
        );
        assert_ne!(make(&["src/z"], false), make(&["src/z"], true));
    }

    #[test]
    fn snapshots_survive_source_changes() {
        let root = tempfile::tempdir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("input"), b"original").unwrap();
        let mut blobs = BTreeMap::new();
        snapshot(
            root.path(),
            Path::new("input"),
            temp.path(),
            &mut Directory::default(),
            &mut blobs,
            &mut BTreeSet::new(),
        )
        .unwrap();
        std::fs::write(root.path().join("input"), b"changed").unwrap();
        let (_, blob) = &blobs[&digest(b"original").hash];
        let Blob::File(path) = blob else {
            panic!("expected disk snapshot")
        };
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    }

    #[cfg(unix)]
    #[test]
    fn reject_symlink_input_and_output_parents() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(safe_path(root.path(), Path::new("escape/output")).is_err());
        assert!(snapshot(
            root.path(),
            Path::new("escape"),
            outside.path(),
            &mut Directory::default(),
            &mut BTreeMap::new(),
            &mut BTreeSet::new()
        )
        .is_err());
    }

    #[test]
    fn reject_missing_duplicate_unrequested_and_traversing_outputs() {
        let expected = BTreeSet::from(["app".to_string()]);
        let file = re::OutputFile {
            path: "app".into(),
            digest: Some(digest(b"app")),
            ..Default::default()
        };
        assert!(validate_outputs(&[file.clone()], &expected).is_ok());
        assert!(validate_outputs(&[], &expected).is_err());
        assert!(validate_outputs(&[file.clone(), file.clone()], &expected).is_err());
        for path in ["../app", "/app", "other"] {
            assert!(validate_outputs(
                &[re::OutputFile {
                    path: path.into(),
                    ..file.clone()
                }],
                &expected
            )
            .is_err());
        }
    }

    #[test]
    fn malformed_action_fields_are_not_ignored() {
        assert!(serde_json::from_str::<ActionSpec>(
            r#"{
            "arguments":["true"],"inputs":[],"outputs":[],"platform":{},
            "timeout_seconds":1,"inherit_credentials":true
        }"#
        )
        .is_err());
    }
}
