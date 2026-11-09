//! Pagination-independent proof for a bounded, private directory-FD lifetime.
//!
//! Raw getdents records and census are never rewritten. Valid but unsupported
//! lifetimes remain unproved; malformed typed records return an error. Consume
//! this result only alongside the caller's complete, validated capture seal.
use crate::*;
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
/// Exact raw getdents locator retained in the normalization partition.
pub struct DirectoryEnumerationPage {
    /// Receipt-local process identity.
    pub process: CapturedProcessId,
    /// Exact raw syscall entry index.
    pub syscall_index: u64,
    /// Resolved directory path, or empty for an unsupported invalid descriptor.
    pub path: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Closed private-FD enumeration whose every returned entry matches its witness.
pub struct CompleteDirectoryEnumeration {
    /// Receipt-local process identity.
    pub process: CapturedProcessId,
    /// Newly opened process-local descriptor.
    pub descriptor: i32,
    /// Successful directory-open boundary.
    pub open_syscall_index: u64,
    /// Exact successful close after end-of-directory.
    pub close_syscall_index: u64,
    /// Every positive page and terminating zero-byte read, in cursor order.
    pub pages: Vec<DirectoryEnumerationPage>,
    /// Stable descriptor object and path across the complete lifetime.
    pub identity: CapturedDescriptorIdentity,
    /// Mount paired with the descriptor identity.
    pub mount_id: u64,
    /// Complete witness, exactly matched by all concatenated returned entries.
    pub directory_value: CapturedDirectoryValue,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
/// Complete proofs and every remaining unsupported raw attempt, without loss.
pub struct DirectoryEnumerationNormalization {
    /// Proved complete lifetimes, with distinct raw page coverage.
    pub complete: Vec<CompleteDirectoryEnumeration>,
    /// Every unsupported, partial, failed, or unwitnessed raw attempt.
    pub unproved: Vec<DirectoryEnumerationPage>,
    /// Original number of complete-witness getdents attempts.
    pub raw_complete_attempts: u64,
    /// Original number of all getdents attempts.
    pub raw_attempts: u64,
}

struct Candidate {
    process: CapturedProcessId,
    descriptor: i32,
    open: u64,
    next: u64,
    identity: CapturedDescriptorIdentity,
    mount: u64,
    fstat: bool,
    eof: bool,
    position: i64,
    pages: Vec<DirectoryEnumerationPage>,
    value: Option<CapturedDirectoryValue>,
    entries: u64,
    names: Vec<Vec<u8>>,
    digest: blake3::Hasher,
}

/// Streaming adapter over already validated typed events. Its conservative
/// profile admits only Open, optional Fstat, contiguous Getdents pages, Close.
/// Other cursor operations cannot disappear: their syscall-index gap prevents
/// a proof. Clone/exec facts additionally establish a private descriptor table.
#[derive(Default)]
pub struct DirectoryEnumerationNormalizer {
    active: BTreeMap<(CapturedProcessId, i32), Candidate>,
    complete: Vec<CompleteDirectoryEnumeration>,
    seen: BTreeMap<(CapturedProcessId, u64), DirectoryEnumerationPage>,
    private_since: BTreeMap<CapturedProcessId, u64>,
    shared: BTreeSet<CapturedProcessId>,
    raw_complete_attempts: u64,
}

impl DirectoryEnumerationNormalizer {
    /// Consume one typed event without changing the raw receipt or census.
    pub fn observe(&mut self, event: &ProcessCaptureStreamEvent<'_>) -> Result<()> {
        match event {
            ProcessCaptureStreamEvent::ExecAttempt { process, attempt } => {
                if matches!(attempt.outcome, CapturedExecOutcome::Loaded(_)) {
                    self.private_since
                        .entry(*process)
                        .or_insert(attempt.syscall_index);
                }
            }
            ProcessCaptureStreamEvent::ProcessCreationAttempt { process, attempt } => {
                crate::validate_captured_process_creation_attempt(attempt)?;
                if let CapturedProcessCreationOutcome::Created(child) = attempt.outcome {
                    let shared = match attempt.request {
                        CapturedProcessCreationRequest::Vfork => false,
                        CapturedProcessCreationRequest::Clone { flags }
                        | CapturedProcessCreationRequest::Clone3 { flags, .. } => {
                            flags & 0x400 != 0
                        }
                    };
                    if shared {
                        // Conservatively exclude both participants even if a
                        // later exec could have split the shared table again.
                        self.shared.extend([*process, child]);
                    } else {
                        self.private_since.entry(child).or_insert(0);
                    }
                }
            }
            ProcessCaptureStreamEvent::OpenAttempt { process, attempt } => {
                self.open(*process, attempt)?
            }
            ProcessCaptureStreamEvent::FstatAttempt { process, attempt } => {
                self.fstat(*process, attempt)?
            }
            ProcessCaptureStreamEvent::Getdents64Attempt { process, attempt } => {
                self.page(*process, attempt)?
            }
            ProcessCaptureStreamEvent::CloseAttempt { process, attempt } => {
                self.close(*process, attempt)?
            }
            ProcessCaptureStreamEvent::ProcessFinished { process, .. } => {
                self.active.retain(|(owner, _), _| owner != process);
            }
            _ => {}
        }
        Ok(())
    }

    fn open(&mut self, process: CapturedProcessId, attempt: &CapturedOpenAttempt) -> Result<()> {
        crate::validate_captured_open_attempt(attempt)?;
        let CapturedOpenOutcome::Opened {
            descriptor,
            identity,
            mount_id,
            ..
        } = &attempt.outcome
        else {
            return Ok(());
        };
        let key = (process, *descriptor);
        self.active.remove(&key);
        if identity.kind != CapturedDescriptorKind::Directory
            || attempt.request.flags & libc::O_ACCMODE as u64 != libc::O_RDONLY as u64
        {
            return Ok(());
        }
        let mut digest = blake3::Hasher::new();
        digest.update(crate::COMPLETE_DIRECTORY_DIGEST_DOMAIN);
        self.active.insert(
            key,
            Candidate {
                process,
                descriptor: *descriptor,
                open: attempt.syscall_index,
                next: attempt
                    .syscall_index
                    .checked_add(1)
                    .context("directory open index overflow")?,
                identity: identity.clone(),
                mount: *mount_id,
                fstat: false,
                eof: false,
                position: 0,
                pages: Vec::new(),
                value: None,
                entries: 0,
                names: Vec::new(),
                digest,
            },
        );
        Ok(())
    }

    fn fstat(&mut self, process: CapturedProcessId, attempt: &CapturedFstatAttempt) -> Result<()> {
        crate::validate_captured_fstat_attempt(attempt)?;
        let key = (process, attempt.request.descriptor);
        let Some(candidate) = self.active.get_mut(&key) else {
            return Ok(());
        };
        let exact = matches!(attempt.outcome, CapturedFstatOutcome::Metadata(metadata)
            if metadata.device == candidate.identity.device
                && metadata.inode == candidate.identity.inode
                && metadata.mode & libc::S_IFMT == libc::S_IFDIR);
        if candidate.next != attempt.syscall_index
            || candidate.fstat
            || !candidate.pages.is_empty()
            || attempt.request.identity.as_ref() != Some(&candidate.identity)
            || attempt.request.mount_id != Some(candidate.mount)
            || !exact
        {
            self.active.remove(&key);
            return Ok(());
        }
        candidate.fstat = true;
        candidate.next = attempt
            .syscall_index
            .checked_add(1)
            .context("directory fstat index overflow")?;
        Ok(())
    }

    fn page(
        &mut self,
        process: CapturedProcessId,
        attempt: &CapturedGetdents64Attempt,
    ) -> Result<()> {
        crate::validate_captured_getdents64_attempt(attempt)?;
        let page = DirectoryEnumerationPage {
            process,
            syscall_index: attempt.syscall_index,
            path: attempt
                .request
                .identity
                .as_ref()
                .map(|identity| identity.target.clone())
                .unwrap_or_default(),
        };
        if self
            .seen
            .insert((process, attempt.syscall_index), page.clone())
            .is_some()
        {
            anyhow::bail!("directory normalization repeats a raw page");
        }
        let CapturedGetdents64Outcome::Enumerated {
            returned_bytes,
            position_after,
            entries,
            directory_value,
        } = &attempt.outcome
        else {
            self.active.remove(&(process, attempt.request.descriptor));
            return Ok(());
        };
        let CapturedDirectoryValue::Complete { .. } = directory_value else {
            self.active.remove(&(process, attempt.request.descriptor));
            return Ok(());
        };
        self.raw_complete_attempts = self
            .raw_complete_attempts
            .checked_add(1)
            .context("complete directory page count overflow")?;
        let identity = attempt
            .request
            .identity
            .as_ref()
            .context("complete directory page has no identity")?;
        let key = (process, attempt.request.descriptor);
        let Some(candidate) = self.active.get_mut(&key) else {
            return Ok(());
        };
        if candidate.next != attempt.syscall_index
            || candidate.eof
            || identity != &candidate.identity
            || attempt.request.mount_id != Some(candidate.mount)
            || attempt.request.position_before != Some(candidate.position)
            || candidate
                .value
                .as_ref()
                .is_some_and(|value| value != directory_value)
        {
            self.active.remove(&key);
            return Ok(());
        }
        candidate
            .value
            .get_or_insert_with(|| directory_value.clone());
        for entry in entries {
            candidate.entries = candidate
                .entries
                .checked_add(1)
                .context("directory entry count overflow")?;
            candidate.digest.update(&entry.inode.to_le_bytes());
            candidate.digest.update(&entry.next_position.to_le_bytes());
            candidate.digest.update(&[entry.file_type]);
            let length = u32::try_from(entry.name.len()).context("directory name exceeds u32")?;
            candidate.digest.update(&length.to_le_bytes());
            candidate.digest.update(&entry.name);
            if entry.name.as_slice() != b"." && entry.name.as_slice() != b".." {
                candidate.names.push(entry.name.clone());
            }
        }
        candidate.position = *position_after; // Opaque cookie: equality only.
        candidate.eof = *returned_bytes == 0;
        candidate.pages.push(page);
        candidate.next = attempt
            .syscall_index
            .checked_add(1)
            .context("directory page index overflow")?;
        Ok(())
    }

    fn close(&mut self, process: CapturedProcessId, attempt: &CapturedCloseAttempt) -> Result<()> {
        crate::validate_captured_close_attempt(attempt)?;
        let Some(mut candidate) = self.active.remove(&(process, attempt.request.descriptor)) else {
            return Ok(());
        };
        if !candidate.eof
            || candidate.next != attempt.syscall_index
            || !matches!(attempt.outcome, CapturedCloseOutcome::Closed { .. })
            || attempt.request.identity.as_ref() != Some(&candidate.identity)
            || attempt.request.mount_id != Some(candidate.mount)
        {
            return Ok(());
        }
        let Some(CapturedDirectoryValue::Complete {
            entries,
            digest,
            members,
        }) = &candidate.value
        else {
            return Ok(());
        };
        candidate.names.sort();
        if candidate.entries != *entries
            || candidate.digest.finalize().as_bytes() != digest
            || candidate.names != *members
            || candidate.names.windows(2).any(|names| names[0] == names[1])
        {
            return Ok(());
        }
        self.complete.push(CompleteDirectoryEnumeration {
            process: candidate.process,
            descriptor: candidate.descriptor,
            open_syscall_index: candidate.open,
            close_syscall_index: attempt.syscall_index,
            pages: candidate.pages,
            identity: candidate.identity,
            mount_id: candidate.mount,
            directory_value: candidate.value.expect("complete value checked"),
        });
        Ok(())
    }

    /// Close the exact partition after the complete stream has been validated.
    pub fn finish(self) -> Result<DirectoryEnumerationNormalization> {
        let raw_attempts =
            u64::try_from(self.seen.len()).context("directory page census exceeds u64")?;
        let raw_complete_attempts = self.raw_complete_attempts;
        let mut unseen = self.seen;
        let mut complete = Vec::new();
        for proof in self.complete {
            if self.shared.contains(&proof.process)
                || !self
                    .private_since
                    .get(&proof.process)
                    .is_some_and(|since| *since <= proof.open_syscall_index)
            {
                continue;
            }
            if proof.pages.is_empty() {
                anyhow::bail!("completed directory proof has no pages");
            }
            for page in &proof.pages {
                if unseen.remove(&(page.process, page.syscall_index)).as_ref() != Some(page)
                    || page.path != proof.identity.target
                    || page.process != proof.process
                {
                    anyhow::bail!("directory normalization page coverage is not bijective");
                }
            }
            complete.push(proof);
        }
        complete.sort_by_key(|proof| (proof.process, proof.open_syscall_index));
        Ok(DirectoryEnumerationNormalization {
            complete,
            unproved: unseen.into_values().collect(),
            raw_complete_attempts,
            raw_attempts,
        })
    }
}

/// Normalize the same typed IR as the streaming adapter, without evaluating any
/// source or inventing global ordering from receipt process-vector order.
pub fn normalize_complete_directory_enumerations(
    receipt: &ProcessCaptureReceipt,
) -> Result<DirectoryEnumerationNormalization> {
    normalize_receipt_directory_enumerations(receipt, DirectoryEnumerationNormalizer::default())
}

// Only the live capture-output method may supply the initial exec fact. This
// fact is absent from the persisted receipt and is never inferred from a PID.
pub(crate) fn normalize_live_capture_directory_enumerations(
    receipt: &ProcessCaptureReceipt,
    root: CapturedProcessId,
) -> Result<DirectoryEnumerationNormalization> {
    if receipt.root() != root {
        anyhow::bail!("live directory normalization root differs from its receipt");
    }
    let mut normalizer = DirectoryEnumerationNormalizer::default();
    normalizer.private_since.insert(root, 0);
    normalize_receipt_directory_enumerations(receipt, normalizer)
}

fn normalize_receipt_directory_enumerations(
    receipt: &ProcessCaptureReceipt,
    mut normalizer: DirectoryEnumerationNormalizer,
) -> Result<DirectoryEnumerationNormalization> {
    receipt.validate_complete()?;
    // Creation/exec facts are checked again at finish. Child creation can be
    // reported after a vfork child's observations, so facts are not guessed
    // from visitor arrival order.
    for process in receipt.processes() {
        for attempt in &process.exec_attempts {
            normalizer.observe(&ProcessCaptureStreamEvent::ExecAttempt {
                process: process.id,
                attempt,
            })?;
        }
        for attempt in &process.process_creation_attempts {
            normalizer.observe(&ProcessCaptureStreamEvent::ProcessCreationAttempt {
                process: process.id,
                attempt,
            })?;
        }
    }
    for process in receipt.processes() {
        let mut events = Vec::new();
        for attempt in &process.open_attempts {
            events.push((
                attempt.syscall_index,
                ProcessCaptureStreamEvent::OpenAttempt {
                    process: process.id,
                    attempt,
                },
            ));
        }
        for attempt in &process.fstat_attempts {
            events.push((
                attempt.syscall_index,
                ProcessCaptureStreamEvent::FstatAttempt {
                    process: process.id,
                    attempt,
                },
            ));
        }
        for attempt in &process.getdents64_attempts {
            events.push((
                attempt.syscall_index,
                ProcessCaptureStreamEvent::Getdents64Attempt {
                    process: process.id,
                    attempt,
                },
            ));
        }
        for attempt in &process.close_attempts {
            events.push((
                attempt.syscall_index,
                ProcessCaptureStreamEvent::CloseAttempt {
                    process: process.id,
                    attempt,
                },
            ));
        }
        events.sort_by_key(|(index, _)| *index);
        if events.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            anyhow::bail!("directory normalization has overlapping syscall records");
        }
        for (_, event) in events {
            normalizer.observe(&event)?;
        }
    }
    normalizer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> CapturedDescriptorIdentity {
        CapturedDescriptorIdentity {
            target: b"/fixture/output".to_vec(),
            device: 1,
            inode: 2,
            kind: CapturedDescriptorKind::Directory,
        }
    }

    fn entries() -> Vec<CapturedDirectoryEntry> {
        [b".".as_slice(), b"..", b"entry"]
            .into_iter()
            .zip([10, 3, 512])
            .map(|(name, next)| CapturedDirectoryEntry {
                inode: 2,
                next_position: next,
                record_length: ((19 + name.len() + 1).div_ceil(8) * 8) as u16,
                file_type: libc::DT_DIR,
                name: name.to_vec(),
            })
            .collect()
    }

    fn witness(entries: &[CapturedDirectoryEntry]) -> CapturedDirectoryValue {
        let mut digest = blake3::Hasher::new();
        digest.update(crate::COMPLETE_DIRECTORY_DIGEST_DOMAIN);
        let mut members = Vec::new();
        for entry in entries {
            digest.update(&entry.inode.to_le_bytes());
            digest.update(&entry.next_position.to_le_bytes());
            digest.update(&[entry.file_type]);
            digest.update(&(entry.name.len() as u32).to_le_bytes());
            digest.update(&entry.name);
            if entry.name != b"." && entry.name != b".." {
                members.push(entry.name.clone());
            }
        }
        members.sort();
        CapturedDirectoryValue::Complete {
            entries: entries.len() as u64,
            digest: *digest.finalize().as_bytes(),
            members,
        }
    }

    fn open(index: u64) -> CapturedOpenAttempt {
        CapturedOpenAttempt {
            syscall_index: index,
            request: CapturedOpenRequest {
                system_call: CapturedOpenSystemCall::Open,
                path: b"/fixture/output".to_vec(),
                directory: None,
                flags: (libc::O_RDONLY | libc::O_DIRECTORY) as u64,
                mode: 0,
                resolve: 0,
            },
            outcome: CapturedOpenOutcome::Opened {
                descriptor: 3,
                identity: identity(),
                mount_id: 1,
                file_value: None,
            },
        }
    }

    fn close(index: u64) -> CapturedCloseAttempt {
        CapturedCloseAttempt {
            syscall_index: index,
            request: CapturedCloseRequest {
                descriptor: 3,
                identity: Some(identity()),
                mount_id: Some(1),
            },
            outcome: CapturedCloseOutcome::Closed { file_value: None },
        }
    }

    fn pages(split: bool, start: u64) -> Vec<CapturedGetdents64Attempt> {
        let all = entries();
        let value = witness(&all);
        let chunks = if split {
            vec![
                all[..1].to_vec(),
                all[1..2].to_vec(),
                all[2..].to_vec(),
                vec![],
            ]
        } else {
            vec![all, vec![]]
        };
        let mut position = 0;
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, entries)| {
                let after = entries.last().map_or(position, |entry| entry.next_position);
                let attempt = CapturedGetdents64Attempt {
                    syscall_index: start + i as u64,
                    request: CapturedGetdents64Request {
                        descriptor: 3,
                        identity: Some(identity()),
                        mount_id: Some(1),
                        position_before: Some(position),
                        requested_bytes: 32768,
                    },
                    outcome: CapturedGetdents64Outcome::Enumerated {
                        returned_bytes: entries
                            .iter()
                            .map(|entry| u32::from(entry.record_length))
                            .sum(),
                        position_after: after,
                        entries,
                        directory_value: value.clone(),
                    },
                };
                position = after;
                attempt
            })
            .collect()
    }

    fn private(normalizer: &mut DirectoryEnumerationNormalizer) {
        let creation = CapturedProcessCreationAttempt {
            syscall_index: 0,
            request: CapturedProcessCreationRequest::Clone {
                flags: (libc::CLONE_CHILD_CLEARTID | libc::CLONE_CHILD_SETTID | libc::SIGCHLD)
                    as u64,
            },
            outcome: CapturedProcessCreationOutcome::Created(1),
        };
        normalizer
            .observe(&ProcessCaptureStreamEvent::ProcessCreationAttempt {
                process: 100,
                attempt: &creation,
            })
            .unwrap();
    }

    fn record(
        normalizer: &mut DirectoryEnumerationNormalizer,
        opening: u64,
        pages: &[CapturedGetdents64Attempt],
        closing: Option<u64>,
    ) -> Result<()> {
        normalizer.observe(&ProcessCaptureStreamEvent::OpenAttempt {
            process: 1,
            attempt: &open(opening),
        })?;
        for page in pages {
            normalizer.observe(&ProcessCaptureStreamEvent::Getdents64Attempt {
                process: 1,
                attempt: page,
            })?;
        }
        if let Some(index) = closing {
            normalizer.observe(&ProcessCaptureStreamEvent::CloseAttempt {
                process: 1,
                attempt: &close(index),
            })?;
        }
        Ok(())
    }

    #[test]
    fn two_and_four_pages_prove_one_identical_complete_lifetime() {
        let mut results = Vec::new();
        for split in [false, true] {
            let pages = pages(split, 1);
            let mut normalizer = DirectoryEnumerationNormalizer::default();
            private(&mut normalizer);
            record(&mut normalizer, 0, &pages, Some(1 + pages.len() as u64)).unwrap();
            let result = normalizer.finish().unwrap();
            assert_eq!(result.raw_attempts, pages.len() as u64);
            assert_eq!(result.raw_complete_attempts, result.raw_attempts);
            assert_eq!(result.complete.len(), 1);
            assert_eq!(result.complete[0].pages.len(), pages.len());
            assert!(result.unproved.is_empty());
            results.push(result.complete[0].directory_value.clone());
        }
        assert_eq!(results[0], results[1]);
    }

    #[test]
    fn opaque_cookies_and_reused_fd_keep_distinct_lifetimes() {
        let mut normalizer = DirectoryEnumerationNormalizer::default();
        private(&mut normalizer);
        // Cookies go 0 -> 10 -> 3 -> 512: no numeric monotonicity assumption.
        record(&mut normalizer, 0, &pages(true, 1), Some(5)).unwrap();
        record(&mut normalizer, 6, &pages(false, 7), Some(9)).unwrap();
        let result = normalizer.finish().unwrap();
        assert_eq!(result.complete.len(), 2);
        assert_eq!(
            result
                .complete
                .iter()
                .map(|p| p.open_syscall_index)
                .collect::<Vec<_>>(),
            vec![0, 6]
        );
        assert_eq!(result.raw_attempts, 6);
        assert!(result.unproved.is_empty());
    }

    #[test]
    fn identical_witness_does_not_excuse_changed_returned_entries() {
        for variant in 0..4 {
            let mut pages = pages(false, 1);
            if let CapturedGetdents64Outcome::Enumerated {
                entries,
                directory_value,
                ..
            } = &mut pages[0].outcome
            {
                match variant {
                    0 => entries[0].inode += 1,
                    1 => entries[2].name = b"other".to_vec(),
                    2 => {
                        if let CapturedDirectoryValue::Complete { entries, .. } = directory_value {
                            *entries += 1;
                        }
                    }
                    _ => {
                        if let CapturedDirectoryValue::Complete { digest, .. } = directory_value {
                            digest[0] ^= 1;
                        }
                    }
                }
            }
            if variant >= 2 {
                // Keep the forged witness identical across every page: the
                // aggregate digest/count check, not only page consistency,
                // must still reject the returned stream.
                let witness = match &pages[0].outcome {
                    CapturedGetdents64Outcome::Enumerated {
                        directory_value, ..
                    } => directory_value.clone(),
                    _ => unreachable!(),
                };
                if let CapturedGetdents64Outcome::Enumerated {
                    directory_value, ..
                } = &mut pages[1].outcome
                {
                    *directory_value = witness;
                }
            }
            let mut normalizer = DirectoryEnumerationNormalizer::default();
            private(&mut normalizer);
            record(&mut normalizer, 0, &pages, Some(3)).unwrap();
            let result = normalizer.finish().unwrap();
            assert!(result.complete.is_empty(), "variant {variant}");
            assert_eq!(result.unproved.len(), 2);
            assert_eq!(result.raw_attempts, 2);
        }
    }

    #[test]
    fn mutated_witness_or_cursor_interleaving_remains_unproved() {
        for variant in 0..3 {
            let mut pages = pages(true, 1);
            if variant == 0 {
                if let CapturedGetdents64Outcome::Enumerated {
                    directory_value: CapturedDirectoryValue::Complete { digest, .. },
                    ..
                } = &mut pages[1].outcome
                {
                    digest[0] ^= 1;
                }
            } else if variant == 1 {
                pages[1].request.position_before = Some(999);
            } else {
                // Any seek, dup, fork, unknown syscall or interleaved cursor
                // operation introduces an entry gap, even if path/members match.
                for page in &mut pages[1..] {
                    page.syscall_index += 1;
                }
            }
            let mut normalizer = DirectoryEnumerationNormalizer::default();
            private(&mut normalizer);
            record(&mut normalizer, 0, &pages, Some(6)).unwrap();
            let result = normalizer.finish().unwrap();
            assert!(result.complete.is_empty());
            assert_eq!(result.unproved.len(), 4);
        }
    }

    #[test]
    fn missing_eof_close_or_private_open_never_becomes_complete() {
        for variant in 0..4 {
            let mut normalizer = DirectoryEnumerationNormalizer::default();
            if variant != 3 {
                private(&mut normalizer);
            }
            let mut pages = pages(false, 1);
            if variant == 0 {
                pages.pop();
            }
            record(
                &mut normalizer,
                0,
                &pages,
                if variant == 1 {
                    None
                } else {
                    Some(1 + pages.len() as u64)
                },
            )
            .unwrap();
            if variant == 2 {
                let shared = CapturedProcessCreationAttempt {
                    syscall_index: 50,
                    request: CapturedProcessCreationRequest::Clone { flags: 0x50f00 },
                    outcome: CapturedProcessCreationOutcome::Created(2),
                };
                normalizer
                    .observe(&ProcessCaptureStreamEvent::ProcessCreationAttempt {
                        process: 1,
                        attempt: &shared,
                    })
                    .unwrap();
            }
            let result = normalizer.finish().unwrap();
            assert!(result.complete.is_empty(), "variant {variant}");
            assert_eq!(result.unproved.len(), pages.len());
        }
    }

    #[test]
    fn initial_private_exec_fact_does_not_override_observed_shared_table() {
        let mut normalizer = DirectoryEnumerationNormalizer::default();
        // Same seed as the private live-capture entry point, before the root
        // creates another process sharing its descriptor table.
        normalizer.private_since.insert(1, 0);
        record(&mut normalizer, 0, &pages(false, 1), Some(3)).unwrap();
        normalizer
            .observe(&ProcessCaptureStreamEvent::ProcessCreationAttempt {
                process: 1,
                attempt: &CapturedProcessCreationAttempt {
                    syscall_index: 4,
                    request: CapturedProcessCreationRequest::Clone { flags: 0x50f00 },
                    outcome: CapturedProcessCreationOutcome::Created(2),
                },
            })
            .unwrap();
        let proof = normalizer.finish().unwrap();
        assert!(proof.complete.is_empty());
        assert_eq!(proof.raw_attempts, 2);
        assert_eq!(proof.unproved.len(), 2);
    }

    #[test]
    fn failed_terminal_and_unwitnessed_attempts_stay_in_raw_partition() {
        for outcome in [
            CapturedGetdents64Outcome::Failed(libc::EIO),
            CapturedGetdents64Outcome::Terminated,
            CapturedGetdents64Outcome::Enumerated {
                returned_bytes: 0,
                position_after: 0,
                entries: vec![],
                directory_value: CapturedDirectoryValue::Unwitnessed,
            },
        ] {
            let mut normalizer = DirectoryEnumerationNormalizer::default();
            private(&mut normalizer);
            let mut page = pages(false, 1).remove(0);
            page.outcome = outcome;
            record(&mut normalizer, 0, &[page], Some(2)).unwrap();
            let result = normalizer.finish().unwrap();
            assert_eq!(result.raw_attempts, 1);
            assert_eq!(result.raw_complete_attempts, 0);
            assert_eq!(result.unproved.len(), 1);
            assert!(result.complete.is_empty());
        }
    }

    #[test]
    fn malformed_typed_page_is_an_error_not_an_unsupported_profile() {
        let mut page = pages(false, 1).remove(0);
        if let CapturedGetdents64Outcome::Enumerated { returned_bytes, .. } = &mut page.outcome {
            *returned_bytes += 1;
        }
        let mut normalizer = DirectoryEnumerationNormalizer::default();
        private(&mut normalizer);
        assert!(record(&mut normalizer, 0, &[page], Some(2)).is_err());
    }
    #[test]
    fn fork_seek_dup_and_unknown_cursor_gaps_remain_unproved() {
        for operation in 0..4 {
            let mut normalizer = DirectoryEnumerationNormalizer::default();
            private(&mut normalizer);
            normalizer
                .observe(&ProcessCaptureStreamEvent::OpenAttempt {
                    process: 1,
                    attempt: &open(0),
                })
                .unwrap();
            let mut attempts = pages(false, 1);
            normalizer
                .observe(&ProcessCaptureStreamEvent::Getdents64Attempt {
                    process: 1,
                    attempt: &attempts[0],
                })
                .unwrap();
            match operation {
                0 => normalizer
                    .observe(&ProcessCaptureStreamEvent::ProcessCreationAttempt {
                        process: 1,
                        attempt: &CapturedProcessCreationAttempt {
                            syscall_index: 2,
                            request: CapturedProcessCreationRequest::Vfork,
                            outcome: CapturedProcessCreationOutcome::Created(2),
                        },
                    })
                    .unwrap(),
                1 => normalizer
                    .observe(&ProcessCaptureStreamEvent::SeekAttempt {
                        process: 1,
                        attempt: &CapturedSeekAttempt {
                            syscall_index: 2,
                            request: CapturedSeekRequest {
                                descriptor: 3,
                                identity: Some(identity()),
                                mount_id: Some(1),
                                offset: 0,
                                whence: libc::SEEK_SET as u64,
                            },
                            outcome: CapturedSeekOutcome::Positioned(0),
                        },
                    })
                    .unwrap(),
                2 => normalizer
                    .observe(&ProcessCaptureStreamEvent::Dup2Attempt {
                        process: 1,
                        attempt: &CapturedDup2Attempt {
                            syscall_index: 2,
                            request: CapturedDup2Request {
                                system_call: CapturedDescriptorDuplicationSystemCall::Dup2,
                                source_descriptor: 3,
                                source_identity: Some(identity()),
                                source_mount_id: Some(1),
                                target_descriptor: 4,
                                target_identity: None,
                                target_mount_id: None,
                            },
                            outcome: CapturedDup2Outcome::Duplicated {
                                descriptor: 4,
                                identity: identity(),
                                mount_id: 1,
                                replaced_file_value: None,
                            },
                        },
                    })
                    .unwrap(),
                _ => normalizer
                    .observe(&ProcessCaptureStreamEvent::UnmodeledSyscallAttempt {
                        process: 1,
                        attempt: &CapturedUnmodeledSyscallAttempt {
                            syscall_index: 2,
                            number: u64::MAX,
                            arguments: [0; 6],
                            outcome: CapturedUnmodeledSyscallOutcome::Returned {
                                return_value: 0,
                                is_error: false,
                            },
                        },
                    })
                    .unwrap(),
            }
            attempts[1].syscall_index = 3;
            normalizer
                .observe(&ProcessCaptureStreamEvent::Getdents64Attempt {
                    process: 1,
                    attempt: &attempts[1],
                })
                .unwrap();
            normalizer
                .observe(&ProcessCaptureStreamEvent::CloseAttempt {
                    process: 1,
                    attempt: &close(4),
                })
                .unwrap();
            let result = normalizer.finish().unwrap();
            assert!(result.complete.is_empty(), "operation {operation}");
            assert_eq!(result.unproved.len(), 2);
        }
    }

    #[test]
    fn exec_private_table_fact_must_precede_the_new_open() {
        for exec_index in [0, 10] {
            let mut normalizer = DirectoryEnumerationNormalizer::default();
            let exec = CapturedExecAttempt {
                syscall_index: exec_index,
                request: CapturedExecRequest {
                    executable: b"/fixture/tool".to_vec(),
                    directory: None,
                    argv: vec![b"tool".to_vec()],
                    environment: vec![],
                    flags: 0,
                },
                outcome: CapturedExecOutcome::Loaded(0),
            };
            normalizer
                .observe(&ProcessCaptureStreamEvent::ExecAttempt {
                    process: 1,
                    attempt: &exec,
                })
                .unwrap();
            record(&mut normalizer, 1, &pages(false, 2), Some(4)).unwrap();
            let result = normalizer.finish().unwrap();
            assert_eq!(result.complete.len(), usize::from(exec_index == 0));
            assert_eq!(result.unproved.len(), if exec_index == 0 { 0 } else { 2 });
        }
    }
}
