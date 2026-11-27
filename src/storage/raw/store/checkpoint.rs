use super::*;

pub fn write_metadata_copies(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
) -> Result<()> {
    // Store compact JSON: the metadata checkpoint has half the fixed metadata
    // region, and pretty-printing wastes about a third of that space.
    let checkpoint = prepare_checkpoint_writes(metadata, superblocks)?;
    // Initialization deliberately overwrites both slots; there is no older
    // acknowledged checkpoint to preserve on a freshly formatted device.
    let writes =
        write_metadata_members_parallel(backend, superblocks, metadata, &checkpoint, None, None);
    // Join all in-flight writes even when one member fails.
    for (_, result) in writes {
        result?;
    }
    Ok(())
}

/// The payload is shared across members. Each member writes its own two slots
/// in order, including the flush-before-header protocol, on a separate worker.
///
/// A `protected_targets` entry selects the single slot a member may overwrite.
/// The other slot still holds the previously verified checkpoint, so a partial
/// update can never destroy the last recoverable quorum.
fn write_metadata_members_parallel(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    checkpoint: &MetadataTreeCheckpoint,
    metrics: Option<&CommitMetrics>,
    protected_targets: Option<&BTreeMap<String, u64>>,
) -> Vec<(String, Result<()>)> {
    std::thread::scope(|scope| {
        let jobs: Vec<_> = superblocks
            .iter()
            .map(|sb| {
                scope.spawn(move || {
                    let _member_timer = metrics.map(|metrics| {
                        metrics.member_timer(CommitPhase::CheckpointMemberIo, &sb.disk_id)
                    });
                    (
                        sb.disk_id.clone(),
                        write_metadata_member(
                            backend,
                            sb,
                            metadata,
                            checkpoint,
                            metrics,
                            protected_targets.and_then(|targets| targets.get(&sb.disk_id).copied()),
                        ),
                    )
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().expect("checkpoint member worker panicked"))
            .collect()
    })
}

#[cfg(test)]
pub(crate) fn write_metadata_copies_quorum(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
) -> Result<QuorumWriteReport> {
    write_metadata_copies_quorum_impl(backend, superblocks, metadata, None, false)
}

pub(super) fn write_recovered_metadata_copies_quorum(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
) -> Result<QuorumWriteReport> {
    write_metadata_copies_quorum_impl(backend, superblocks, metadata, None, true)
}

pub(crate) fn write_metadata_copies_quorum_measured(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    metrics: &CommitMetrics,
) -> Result<QuorumWriteReport> {
    write_metadata_copies_quorum_impl(backend, superblocks, metadata, Some(metrics), false)
}

fn write_metadata_copies_quorum_impl(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    metrics: Option<&CommitMetrics>,
    replay_selected: bool,
) -> Result<QuorumWriteReport> {
    let need = metadata_quorum_requirement(metadata);
    if superblocks.len() < need {
        return Err(quorum_unavailable(
            "metadata checkpoint",
            need,
            superblocks.len(),
        ));
    }
    let checkpoint = {
        let _encode_timer = metrics.map(|metrics| metrics.timer(CommitPhase::CheckpointEncode));
        prepare_checkpoint_writes(metadata, superblocks)?
    };
    // Before replacing a checkpoint, keep one physically verified old slot on
    // a quorum of members. Writing both slots on every successful member while
    // other members fail can otherwise leave neither the old nor the new
    // checkpoint with a majority (e.g. one of two members completes while the
    // other keeps only the old slot). The protected subset is also the same
    // quorum rule used for journal rollover.
    // Only the actual recovery path has established the authoritative replay
    // winner. Ordinary checkpoint writes must preserve certified minority bases.
    let (protected_targets, blocked) = protected_explicit_checkpoint_targets(
        backend,
        superblocks,
        replay_selected.then_some(metadata),
    )?;
    let writable = superblocks
        .iter()
        .filter(|sb| !blocked.contains(&sb.disk_id))
        .cloned()
        .collect::<Vec<_>>();
    if writable.len() < need {
        return Err(ArgosError::Conflict(
            "no safe checkpoint targets on a quorum; preserve existing replay bases".to_string(),
        ));
    }
    let mut report = QuorumWriteReport::default();
    let mut durable = 0usize;
    let mut uncertain_write = false;
    for (disk_id, result) in write_metadata_members_parallel(
        backend,
        &writable,
        metadata,
        &checkpoint,
        metrics,
        Some(&protected_targets),
    ) {
        match result {
            Ok(()) => durable += 1,
            Err(err) => {
                // Even a failed flush can have exposed the new slot header.
                uncertain_write |= !matches!(err, ArgosError::DiskFull { .. });
                report.record_failure(&disk_id, &err);
            }
        }
    }
    for disk_id in blocked {
        report.record_failure(
            &disk_id,
            &ArgosError::Conflict("both checkpoint slots hold distinct recovery bases".to_string()),
        );
    }
    if durable < need {
        return Err(if uncertain_write {
            indeterminate_commit("metadata checkpoint", need, durable)
        } else {
            quorum_failure_before_write(&report, "metadata checkpoint", need, durable)
        });
    }
    Ok(report)
}

/// On existing pools, emit the original v1 checkpoint format whenever it
/// fits. Only a checkpoint that would otherwise hit ENOSPC is compressed.
pub(super) struct CheckpointPayload {
    pub(super) bytes: Vec<u8>,
    pub(super) original: Option<(u64, String)>,
    pub(super) has_catalog: bool,
}

pub(super) fn checkpoint_fits(bytes: usize, slot_len: u64) -> bool {
    if bytes == 0 {
        return false;
    }
    let pages = bytes.div_ceil(METADATA_PAGE_SIZE);
    let Some(index_len) = pages.checked_mul(METADATA_INDEX_ENTRY_SIZE) else {
        return false;
    };
    let Some(index_end) = (RAW_HEADER_SIZE as u64).checked_add(index_len as u64) else {
        return false;
    };
    let Some(data_bytes) = (pages as u64).checked_mul(METADATA_PAGE_SIZE as u64) else {
        return false;
    };
    align_up(index_end, METADATA_PAGE_SIZE as u64)
        .checked_add(data_bytes)
        .is_some_and(|end| end <= slot_len)
}

// Retain small checkpoints directly, but do not allocate a full expanded
// JSON image just to decide whether it must be compressed. The probe streams
// the exact legacy serialization into a digest and a bounded prefix buffer.
pub(super) const CHECKPOINT_PROBE_BYTES: usize = 1024 * 1024;

struct CheckpointProbe {
    len: u64,
    hash: Sha256,
    small: Option<Vec<u8>>,
}

impl std::io::Write for CheckpointProbe {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.len = self
            .len
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("checkpoint size overflow"))?;
        self.hash.update(bytes);
        if let Some(small) = &mut self.small {
            if self.len <= CHECKPOINT_PROBE_BYTES as u64 {
                small.extend_from_slice(bytes);
            } else {
                self.small = None;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) const CHECKPOINT_CATALOG_MAGIC: &[u8; 8] = b"ARGCAT01";
pub(super) const CHECKPOINT_CATALOG_OFFSET: usize = 304;

#[derive(Serialize)]
pub(super) struct CheckpointWire<'a> {
    #[serde(flatten)]
    pub(super) metadata: &'a Metadata,
    pub(super) checkpoint_member_catalog: crate::model::inodes::CheckpointMemberCatalog,
}

pub(super) fn prepare_checkpoint_payload(
    metadata: &Metadata,
    superblocks: &[RawSuperblock],
) -> Result<CheckpointPayload> {
    let wire = CheckpointWire {
        metadata,
        checkpoint_member_catalog: metadata.inodes.checkpoint_catalog(),
    };
    // This is an advisory index, not authoritative metadata. Never require
    // additional disk space (or a larger expansion limit) to commit merely
    // because the optional index is large. Preserve the original payload
    // path when the catalog-bearing payload cannot fit the existing slots.
    match encode_checkpoint_payload(&wire, superblocks, true) {
        Ok(payload)
            if superblocks
                .iter()
                .all(|sb| checkpoint_fits(payload.bytes.len(), sb.metadata.length / 2)) =>
        {
            Ok(payload)
        }
        Ok(_) | Err(ArgosError::Invalid(_)) => {
            encode_checkpoint_payload(metadata, superblocks, false)
        }
        Err(err) => Err(err),
    }
}

fn encode_checkpoint_payload(
    wire: &impl Serialize,
    superblocks: &[RawSuperblock],
    has_catalog: bool,
) -> Result<CheckpointPayload> {
    use std::io::Write;
    let mut probe = CheckpointProbe {
        len: 0,
        hash: Sha256::new(),
        small: Some(Vec::new()),
    };
    {
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, &mut probe);
        serde_json::to_writer(&mut writer, wire)?;
        writer.flush()?;
    }
    let len = checked_usize(probe.len, "checkpoint expanded length")?;
    if superblocks
        .iter()
        .all(|sb| checkpoint_fits(len, sb.metadata.length / 2))
    {
        // Large uncompressed images need their exact legacy bytes anyway.
        // Preserve that encoding and COW page reuse instead of forcing zstd.
        return Ok(CheckpointPayload {
            bytes: match probe.small {
                Some(bytes) => bytes,
                None => serde_json::to_vec(wire)?,
            },
            original: None,
            has_catalog,
        });
    }
    if probe.len > METADATA_DECOMPRESSED_MAX {
        return Err(ArgosError::Invalid(format!(
            "metadata checkpoint exceeds {}-byte supported expansion limit",
            METADATA_DECOMPRESSED_MAX
        )));
    }
    let original = (probe.len, hex::encode(probe.hash.finalize()));
    let compressed = if let Some(bytes) = probe.small {
        zstd::stream::encode_all(bytes.as_slice(), CHECKPOINT_ZSTD_LEVEL)?
    } else {
        // Stream the second serialization directly into the compressor. The
        // immutable snapshot and deterministic inode ordering make both passes
        // identical. No expanded JSON Vec is retained for the compressed path.
        let encoder = zstd::stream::write::Encoder::new(Vec::new(), CHECKPOINT_ZSTD_LEVEL)?;
        let mut writer = std::io::BufWriter::with_capacity(64 * 1024, encoder);
        serde_json::to_writer(&mut writer, wire)?;
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|err| err.into_error())?
            .finish()?
    };
    Ok(CheckpointPayload {
        bytes: compressed,
        original: Some(original),
        has_catalog,
    })
}

// Compute the page digests and index once, not once per member and per slot.
pub(super) fn prepare_checkpoint_writes(
    metadata: &Metadata,
    superblocks: &[RawSuperblock],
) -> Result<MetadataTreeCheckpoint> {
    let payload = prepare_checkpoint_payload(metadata, superblocks)?;
    let hash = sha256_hex(&payload.bytes);
    let mut checkpoint = metadata_tree_checkpoint_encoded(
        "checkpoint",
        metadata.txid,
        metadata.integrity.generation,
        payload.bytes,
        &hash,
        u64::MAX,
        payload.original.as_ref(),
    )?;
    if payload.has_catalog {
        checkpoint.header[CHECKPOINT_CATALOG_OFFSET..CHECKPOINT_CATALOG_OFFSET + 8]
            .copy_from_slice(CHECKPOINT_CATALOG_MAGIC);
    }
    Ok(checkpoint)
}

/// Verify an existing tree checkpoint using bounded, contiguous reads.
/// No JSON deserialization or decompression is needed: the authenticated
/// stored payload hash and every indexed page digest are validated. Any
/// unsupported/legacy page layout opts out of single-slot reuse.
fn verify_preserved_checkpoint_pages(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    offset: u64,
    slot_len: u64,
    header: &[u8; RAW_HEADER_SIZE],
) -> Result<()> {
    if !matches!(
        get_u32(header, 20)?,
        METADATA_FORMAT_TREE | METADATA_FORMAT_TREE_ZSTD
    ) {
        return Err(ArgosError::IncompatibleFormat(
            "checkpoint slot does not use a verifiable tree layout".to_string(),
        ));
    }
    let len = checked_usize(get_u64(header, 32)?, "checkpoint length")?;
    if len == 0 || len as u64 > CHECKPOINT_PRESERVE_VALIDATION_MAX_BYTES {
        return Err(ArgosError::CorruptedMetadata(
            "checkpoint size exceeds protected rollover validation bound".to_string(),
        ));
    }
    let page_size = checked_usize(get_u64(header, 144)?, "checkpoint page size")?;
    if page_size != METADATA_PAGE_SIZE {
        return Err(ArgosError::IncompatibleFormat(
            "checkpoint uses an unsupported page size for batch verification".to_string(),
        ));
    }
    let count = len.div_ceil(page_size);
    let index_len = count
        .checked_mul(METADATA_INDEX_ENTRY_SIZE)
        .ok_or_else(|| {
            ArgosError::CorruptedMetadata("checkpoint index size overflow".to_string())
        })?;
    if get_u64(header, 152)? != count as u64 || get_u64(header, 160)? != index_len as u64 {
        return Err(ArgosError::CorruptedMetadata(
            "checkpoint index header inconsistent".to_string(),
        ));
    }
    let index_end = (RAW_HEADER_SIZE as u64)
        .checked_add(index_len as u64)
        .ok_or_else(|| ArgosError::CorruptedMetadata("checkpoint index overflow".to_string()))?;
    let data_start = align_up(index_end, page_size as u64);
    let data_end = data_start
        .checked_add(align_up(len as u64, page_size as u64))
        .ok_or_else(|| ArgosError::CorruptedMetadata("checkpoint data overflow".to_string()))?;
    if data_end > slot_len {
        return Err(ArgosError::CorruptedMetadata(
            "checkpoint data exceeds target slot".to_string(),
        ));
    }
    let mut index = vec![0u8; index_len];
    backend.read_at(&sb.disk_id, offset + RAW_HEADER_SIZE as u64, &mut index)?;
    if sha256_hex(&index) != get_fixed_hex(header, 168, HASH_HEX_LEN)? {
        return Err(ArgosError::Checksum(
            "protected checkpoint index digest mismatch".to_string(),
        ));
    }
    let mut digest = Sha256::new();
    let mut batch = vec![0u8; CHECKPOINT_BATCH_BYTES];
    let pages_per_batch = CHECKPOINT_BATCH_BYTES / page_size;
    for first_page in (0..count).step_by(pages_per_batch) {
        let last_page = (first_page + pages_per_batch).min(count);
        let bytes_begin = first_page * page_size;
        let bytes_end = (last_page * page_size).min(len);
        let batch_len = bytes_end - bytes_begin;
        backend.read_at(
            &sb.disk_id,
            offset + data_start + bytes_begin as u64,
            &mut batch[..batch_len],
        )?;
        for page in first_page..last_page {
            let entry_start = page * METADATA_INDEX_ENTRY_SIZE;
            let entry = &index[entry_start..entry_start + METADATA_INDEX_ENTRY_SIZE];
            let expected_page_offset = data_start + (page * page_size) as u64;
            let expected_len = (len - page * page_size).min(page_size);
            if get_u64(entry, 32)? != expected_page_offset
                || get_u32(entry, 40)? as usize != expected_len
                || Sha256::digest(
                    &batch[(page - first_page) * page_size
                        ..(page - first_page) * page_size + expected_len],
                )
                .as_slice()
                    != &entry[..HASH_BYTES_LEN]
            {
                return Err(ArgosError::Checksum(
                    "protected checkpoint page digest mismatch".to_string(),
                ));
            }
        }
        digest.update(&batch[..batch_len]);
    }
    if hex::encode(digest.finalize()) != get_fixed_hex(header, 64, HASH_HEX_LEN)? {
        return Err(ArgosError::Checksum(
            "protected checkpoint payload digest mismatch".to_string(),
        ));
    }
    Ok(())
}

/// Plan a one-slot rollover only if *a quorum* of members has the same
/// physically verified prior checkpoint. Every member in this map retains
/// its verified checkpoint slot untouched until the new checkpoint is durable.
/// If the selected old checkpoint cannot be independently verified on a
/// quorum, the caller must use the historical two-slot rewrite instead.
/// A newer valid but unquorate checkpoint must *not* be overwritten: it may
/// be the only remaining certified replay base for acknowledged journal deltas.
/// Refuse the rollover until recovery/repair restores a current quorum.
///
/// The validation is intentionally read-only and runs before any checkpoint
/// write or journal truncation. It verifies the stored full payload hash and
/// every on-disk page hash, not just the header's txid; a torn or bitrotted
/// slot must never be treated as a safe recovery anchor.
pub(super) fn protected_rollover_slot_targets(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    need: usize,
) -> Result<Option<BTreeMap<String, u64>>> {
    type CheckpointKey = (u64, u64, String);
    type SlotHeader = (u64, [u8; RAW_HEADER_SIZE]);
    let mut candidates: BTreeMap<CheckpointKey, BTreeMap<String, Vec<SlotHeader>>> =
        BTreeMap::new();
    for sb in superblocks {
        for slot in 0..2u64 {
            let offset = sb.metadata.offset + slot * (sb.metadata.length / 2);
            let mut header = [0u8; RAW_HEADER_SIZE];
            if backend.read_at(&sb.disk_id, offset, &mut header).is_err()
                || &header[..16] != METADATA_MAGIC
            {
                continue;
            }
            let Ok(format) = get_u32(&header, 20) else {
                continue;
            };
            if !matches!(
                format,
                METADATA_FORMAT_LEGACY | METADATA_FORMAT_TREE | METADATA_FORMAT_TREE_ZSTD
            ) {
                continue;
            }
            let (Ok(txid), Ok(generation), Ok(size)) = (
                get_u64(&header, 24),
                get_u64(&header, 136),
                get_u64(&header, 32),
            ) else {
                continue;
            };
            if size == 0 || size > CHECKPOINT_PRESERVE_VALIDATION_MAX_BYTES {
                continue;
            }
            candidates
                .entry((txid, generation, sha256_hex(&header)))
                .or_default()
                .entry(sb.disk_id.clone())
                .or_default()
                .push((slot, header));
        }
    }

    // Prefer the latest checkpoint already replicated on a physical quorum.
    // A lone, physically valid newer checkpoint is not a quorum anchor, but
    // it might be the only surviving certified base for later journal deltas.
    // Never proceed to an older quorum and overwrite that newer copy.
    let mut newer_unquorate_valid = false;
    for (_, members) in candidates.into_iter().rev() {
        // Validate minority checkpoints too. A surviving certified checkpoint
        // may be present on just one member, with an older quorum below it.
        // Overwriting that member and resetting its journal can erase the only
        // replay base even if every other rollover write fails.
        // Each validator has a bounded 1 MiB read buffer. Overlap reads
        // across members so 9 slow devices do not serialize this I/O.
        let targets = std::thread::scope(|scope| {
            let jobs = superblocks
                .iter()
                .map(|sb| {
                    let members = &members;
                    scope.spawn(move || {
                        let slots = members.get(&sb.disk_id)?;
                        let slot_len = sb.metadata.length / 2;
                        for (slot, header) in slots {
                            let offset = sb.metadata.offset + slot * slot_len;
                            if verify_preserved_checkpoint_pages(
                                backend, sb, offset, slot_len, header,
                            )
                            .is_ok()
                            {
                                return Some((sb.disk_id.clone(), 1 - slot));
                            }
                        }
                        None
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .filter_map(|job| job.join().expect("checkpoint verifier panicked"))
                .collect::<BTreeMap<_, _>>()
        });
        if targets.len() >= need {
            if newer_unquorate_valid {
                return Err(ArgosError::Conflict(
                    "refusing journal rollover: a newer valid checkpoint lacks a quorum; overwriting its last replay base could lose acknowledged transactions".to_string(),
                ));
            }
            return Ok(Some(targets));
        }
        newer_unquorate_valid |= !targets.is_empty();
    }
    if newer_unquorate_valid {
        return Err(ArgosError::Conflict(
            "refusing journal rollover: no safe checkpoint quorum exists; preserve the remaining replay bases".to_string(),
        ));
    }
    Ok(None)
}

/// Explicit checkpoint writes do not reset journals. During topology changes
/// the current quorum may be larger than the previous checkpoint's own quorum,
/// and a newly bootstrapped member may carry a newer minority checkpoint.
/// Preserve those bases instead of treating the rollover-only refusal as a
/// permanent inability to checkpoint. Never overwrite two distinct necessary
/// bases on a member before the new checkpoint is durable.
type CheckpointIdentity = (u64, u64, String);
type MemberCheckpointSlots = BTreeMap<String, Vec<(u64, CheckpointIdentity)>>;

fn protected_explicit_checkpoint_targets(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    replay_selected: Option<&Metadata>,
) -> Result<(BTreeMap<String, u64>, BTreeSet<String>)> {
    let decoded = parking_lot::Mutex::new(BTreeMap::new());
    let mut votes = BTreeMap::<CheckpointIdentity, (usize, BTreeSet<String>)>::new();
    let mut members = MemberCheckpointSlots::new();
    let mut catalog = BTreeMap::new();
    // Verify each member independently and share only decoded payloads. An
    // internally valid foreign checkpoint is not a recovery anchor here.
    let results = std::thread::scope(|scope| {
        let jobs = superblocks
            .iter()
            .map(|sb| {
                let decoded = &decoded;
                scope.spawn(move || (sb, read_metadata_candidates_cached(backend, sb, decoded)))
            })
            .collect::<Vec<_>>();
        jobs.into_iter()
            .map(|job| job.join().expect("checkpoint verifier panicked"))
            .collect::<Vec<_>>()
    });
    for (sb, candidates) in results {
        for (slot, (candidate, report)) in candidates?.into_iter().enumerate() {
            let Some(candidate) = candidate else {
                continue;
            };
            if Uuid::parse_str(&candidate.uuid).ok() != Some(sb.pool_uuid) {
                continue;
            }
            if replay_selected.is_some() {
                catalog.insert(
                    (sb.disk_id.clone(), slot as u64),
                    (sb.disk_id.clone(), Some(candidate.clone()), report),
                );
            }
            let key = (
                candidate.txid,
                candidate.integrity.generation,
                super::replay::metadata_hash_for_replay(&candidate)?,
            );
            if metadata_has_voting_member(&candidate, &sb.disk_id) {
                votes
                    .entry(key.clone())
                    .or_insert_with(|| (metadata_quorum_requirement(&candidate), BTreeSet::new()))
                    .1
                    .insert(sb.disk_id.clone());
            }
            members
                .entry(sb.disk_id.clone())
                .or_default()
                .push((slot as u64, key));
        }
    }
    let anchor = votes
        .iter()
        .rev()
        .find(|(_, (required, present))| present.len() >= *required)
        .map(|(key, _)| (key.0, key.1));
    let mut targets = BTreeMap::new();
    let mut blocked = BTreeSet::new();
    for (disk, slots) in &members {
        let important = slots
            .iter()
            .filter(|(_, key)| anchor.is_none_or(|anchor| (key.0, key.1) >= anchor))
            .collect::<Vec<_>>();
        match important.as_slice() {
            [] => {}
            [first, second] if first.1 != second.1 => {
                blocked.insert(disk.clone());
            }
            [first, ..] => {
                targets.insert(disk.clone(), 1 - first.0);
            }
        }
    }
    if let Some(selected) = replay_selected.filter(|_| !blocked.is_empty()) {
        // The ordinary planner protects every plausible replay base. During
        // recovery we know the exact authoritative state, so verify a less
        // restrictive plan using the same recovery algorithm, with the target
        // slots logically absent. No disk writes or journal resets occur here.
        return replay_protected_checkpoint_targets(
            backend,
            superblocks,
            selected,
            catalog,
            &members,
        );
    }
    Ok((targets, blocked))
}

/// Retain a sufficient recovery witness, even if every new checkpoint write
/// fails. Never infer historical membership from currently online members.
fn replay_protected_checkpoint_targets(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    selected: &Metadata,
    mut catalog: BTreeMap<(String, u64), MemberCheckpointCandidate>,
    members: &MemberCheckpointSlots,
) -> Result<(BTreeMap<String, u64>, BTreeSet<String>)> {
    let pool = superblocks.first().map(|sb| sb.pool_uuid);
    let original = catalog.values().cloned().collect::<Vec<_>>();
    let anchor = select_quorum_metadata_candidate_for_pool(&original, pool)?;
    let identity = |m: &Metadata| -> Result<_> {
        Ok((m.txid, m.integrity.generation, metadata_hash_for_replay(m)?))
    };
    let anchor_key = anchor.as_deref().map(identity).transpose()?;
    let selected_key = identity(selected)?;
    let mut targets = BTreeMap::new();
    let mut blocked = BTreeSet::new();
    for (disk, slots) in members {
        let mut choices = slots.iter().collect::<Vec<_>>();
        // Prefer retaining the original quorum anchor, then the most recent
        // state not newer than the replay winner. Try the other slot if needed.
        choices.sort_by_key(|(_, key)| {
            (
                anchor_key.as_ref() == Some(key),
                key.0 <= selected.txid,
                key.0,
                key.1,
            )
        });
        for (keep, _) in choices.into_iter().rev() {
            let target = 1 - keep;
            let slot = (disk.clone(), target);
            let removed = catalog.remove(&slot);
            let safe = if removed.is_none() {
                true
            } else {
                let retained = catalog.values().cloned().collect::<Vec<_>>();
                let base = select_quorum_metadata_candidate_for_pool(&retained, pool)?;
                // Preserve the exact original quorum anchor throughout all
                // partial writes, not merely the same final replay result.
                if base.as_deref().map(identity).transpose()? != anchor_key {
                    false
                } else {
                    let journal = read_latest_journal_metadata(
                        backend,
                        superblocks,
                        &mut TransactionReport::default(),
                        base.as_deref(),
                        &retained,
                    )?;
                    let winner = match (base, journal) {
                        (Some(base), Some(journal)) if journal.txid > base.txid => Some(journal),
                        (Some(base), _) => Some(base),
                        (None, journal) => journal,
                    };
                    winner.as_deref().map(identity).transpose()?.as_ref() == Some(&selected_key)
                }
            };
            if safe {
                targets.insert(disk.clone(), target);
                break;
            }
            if let Some(removed) = removed {
                catalog.insert(slot, removed);
            }
        }
        if !targets.contains_key(disk) {
            blocked.insert(disk.clone());
        }
    }
    Ok((targets, blocked))
}

pub(super) fn write_metadata_member(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    metadata: &Metadata,
    checkpoint: &MetadataTreeCheckpoint,
    metrics: Option<&CommitMetrics>,
    protected_slot: Option<u64>,
) -> Result<()> {
    let slot_len = sb.metadata.length / 2;
    if checkpoint.required_len > slot_len {
        return Err(ArgosError::DiskFull {
            disk_id: sb.disk_id.clone(),
            required: checkpoint.required_len,
            available: slot_len,
        });
    }
    if let Some(slot) = protected_slot {
        // The opposite slot is a physically verified recovery anchor. Elide
        // unchanged pages and publish this slot's header last, but never touch
        // the anchor.
        return write_metadata_slot_differential(
            backend,
            sb,
            sb.metadata.offset + slot * slot_len,
            checkpoint,
            metrics,
        );
    }
    let slot = (metadata.txid % 2) * slot_len;
    write_metadata_slot(backend, sb, sb.metadata.offset + slot, checkpoint, metrics)?;
    let mirror_slot = ((metadata.txid + 1) % 2) * slot_len;
    write_metadata_slot(
        backend,
        sb,
        sb.metadata.offset + mirror_slot,
        checkpoint,
        metrics,
    )
}

pub(super) fn write_metadata_slot(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    offset: u64,
    checkpoint: &MetadataTreeCheckpoint,
    metrics: Option<&CommitMetrics>,
) -> Result<()> {
    for (relative_offset, data) in checkpoint.body_writes() {
        backend.write_at(&sb.disk_id, offset + relative_offset, data)?;
        if let Some(metrics) = metrics {
            metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, data.len());
        }
    }
    backend.flush_device(&sb.disk_id)?;
    backend.write_at(&sb.disk_id, offset, &checkpoint.header)?;
    if let Some(metrics) = metrics {
        metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, checkpoint.header.len());
    }
    backend.flush_device(&sb.disk_id)?;
    Ok(())
}

/// On a quorum-protected rollover the target slot is *not* a recovery
/// anchor: another physically verified slot is retained untouched. Compare
/// target pages with the prepared next checkpoint and rewrite only changed
/// ranges. This reuses unchanged bytes already physically present in the
/// target slot, without changing the existing tree index or disk format.
///
/// The target bytes are read and compared, not trusted from their old header
/// or index. An unreadable range is conservatively rewritten in full.
/// Power loss at any point can corrupt only this unprotected slot; the prior
/// independently verified quorum remains available until new headers flush.
pub(super) fn write_metadata_slot_differential(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    offset: u64,
    checkpoint: &MetadataTreeCheckpoint,
    metrics: Option<&CommitMetrics>,
) -> Result<()> {
    let mut old = vec![0u8; CHECKPOINT_BATCH_BYTES];
    for (relative, data) in checkpoint.body_writes() {
        for (chunk_index, chunk) in data.chunks(CHECKPOINT_BATCH_BYTES).enumerate() {
            let absolute = offset + relative + (chunk_index * CHECKPOINT_BATCH_BYTES) as u64;
            let readable = backend
                .read_at(&sb.disk_id, absolute, &mut old[..chunk.len()])
                .is_ok();
            // Writing one full batch is preferable when at least half its
            // pages changed: it avoids one syscall for every isolated page
            // of a completely different compressed checkpoint.
            if readable {
                if let Some(metrics) = metrics {
                    metrics.record_checkpoint_cow_bytes(chunk.len(), 0);
                }
            }
            let dirty_pages = if readable {
                chunk
                    .chunks(METADATA_PAGE_SIZE)
                    .zip(old[..chunk.len()].chunks(METADATA_PAGE_SIZE))
                    .filter(|(new, previous)| new != previous)
                    .count()
            } else {
                chunk.len().div_ceil(METADATA_PAGE_SIZE)
            };
            if dirty_pages == 0 {
                if let Some(metrics) = metrics {
                    metrics.record_checkpoint_cow_bytes(0, chunk.len());
                }
                continue;
            }
            if !readable || dirty_pages * 2 >= chunk.len().div_ceil(METADATA_PAGE_SIZE) {
                backend.write_at(&sb.disk_id, absolute, chunk)?;
                if let Some(metrics) = metrics {
                    metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, chunk.len());
                }
                continue;
            }
            let mut dirty_start = None;
            let mut written = 0usize;
            for start in (0..chunk.len()).step_by(METADATA_PAGE_SIZE) {
                let end = (start + METADATA_PAGE_SIZE).min(chunk.len());
                if chunk[start..end] != old[start..end] {
                    dirty_start.get_or_insert(start);
                } else if let Some(run_start) = dirty_start.take() {
                    backend.write_at(
                        &sb.disk_id,
                        absolute + run_start as u64,
                        &chunk[run_start..start],
                    )?;
                    written += start - run_start;
                    if let Some(metrics) = metrics {
                        metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, start - run_start);
                    }
                }
            }
            if let Some(run_start) = dirty_start {
                backend.write_at(
                    &sb.disk_id,
                    absolute + run_start as u64,
                    &chunk[run_start..],
                )?;
                written += chunk.len() - run_start;
                if let Some(metrics) = metrics {
                    metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, chunk.len() - run_start);
                }
            }
            if let Some(metrics) = metrics {
                metrics.record_checkpoint_cow_bytes(0, chunk.len() - written);
            }
        }
    }
    // The pre-existing durability protocol must stay unchanged: publish the
    // new slot header only after every retained/written body byte is safe.
    backend.flush_device(&sb.disk_id)?;
    backend.write_at(&sb.disk_id, offset, &checkpoint.header)?;
    if let Some(metrics) = metrics {
        metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, checkpoint.header.len());
    }
    backend.flush_device(&sb.disk_id)
}

pub(super) fn read_metadata_candidates(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
) -> Result<Vec<(Option<Arc<Metadata>>, MetadataCandidateReport)>> {
    read_metadata_candidates_cached(backend, sb, &parking_lot::Mutex::new(BTreeMap::new()))
}

// This cache exists only during one recovery. Its key covers the entire slot
// header, while independently verifying every member's on-disk payload before
// borrowing an already decoded Metadata. It is never used to attest quorum.
pub(super) fn read_metadata_candidates_cached(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    decoded: &parking_lot::Mutex<BTreeMap<String, Arc<Metadata>>>,
) -> Result<Vec<(Option<Arc<Metadata>>, MetadataCandidateReport)>> {
    read_metadata_candidates_from_headers(
        backend,
        sb,
        decoded,
        read_checkpoint_headers(backend, sb),
    )
}

pub(super) type CheckpointHeaders = [Result<Vec<u8>>; 2];

pub(super) fn read_checkpoint_headers(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
) -> CheckpointHeaders {
    std::array::from_fn(|slot| {
        let mut header = vec![0; RAW_HEADER_SIZE];
        backend.read_recovery_at(
            &sb.disk_id,
            sb.metadata.offset + slot as u64 * (sb.metadata.length / 2),
            &mut header,
        )?;
        Ok(header)
    })
}

/// Upper bound for the encoded payload and index simultaneously held by
/// one member worker. The same headers are used for the subsequent reads.
pub(super) fn checkpoint_payload_budget(sb: &RawSuperblock, headers: &CheckpointHeaders) -> u64 {
    let slot_len = sb.metadata.length / 2;
    headers
        .iter()
        .filter_map(|header| {
            let header = header.as_ref().ok()?;
            if &header[..16] != METADATA_MAGIC {
                return None;
            }
            let len = get_u64(header, 32).ok()?;
            if len == 0 || len > slot_len.saturating_sub(RAW_HEADER_SIZE as u64) {
                return None;
            }
            let index = match get_u32(header, 20).ok()? {
                METADATA_FORMAT_LEGACY => 0,
                METADATA_FORMAT_TREE | METADATA_FORMAT_TREE_ZSTD => get_u64(header, 160).ok()?,
                _ => return None,
            };
            if index > slot_len.saturating_sub(RAW_HEADER_SIZE as u64) {
                return None;
            }
            Some(len.saturating_add(index))
        })
        .max()
        .unwrap_or(0)
}

pub(super) fn read_metadata_candidates_from_headers(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    decoded: &parking_lot::Mutex<BTreeMap<String, Arc<Metadata>>>,
    headers: CheckpointHeaders,
) -> Result<Vec<(Option<Arc<Metadata>>, MetadataCandidateReport)>> {
    let slot_len = sb.metadata.length / 2;
    let mut out = Vec::new();
    for (slot, header) in headers.into_iter().enumerate() {
        let offset = sb.metadata.offset + slot as u64 * slot_len;
        let mut report = MetadataCandidateReport {
            path: format!("{}:metadata-slot-{slot}", sb.disk_id),
            ..MetadataCandidateReport::default()
        };
        let metadata = match header {
            Ok(header) if &header[..16] == METADATA_MAGIC => {
                report.present = true;
                // A matching header is not evidence of valid physical data.
                // read_metadata_payload checks its stored checksum and every
                // page checksum before a member can vote for this checkpoint.
                let candidate = (|| -> Result<Arc<Metadata>> {
                    let bytes = read_metadata_payload(backend, sb, offset, slot_len, &header)?;
                    let key = sha256_hex(&header);
                    // Physical reads and hashing overlap across members. Only
                    // deserialization/cache publication is serialized.
                    let mut decoded = decoded.lock();
                    if let Some(metadata) = decoded.get(&key) {
                        return Ok(metadata.clone());
                    }
                    let metadata = Arc::new(decode_metadata_payload(&header, &bytes)?);
                    decoded.insert(key, Arc::clone(&metadata));
                    Ok(metadata)
                })();
                match candidate {
                    Ok(metadata) => {
                        report.valid = true;
                        report.txid = metadata.txid;
                        report.generation = metadata.integrity.generation;
                        report.meta_hash = metadata.integrity.meta_hash.clone();
                        Some(metadata)
                    }
                    Err(err) => {
                        report.error = Some(err.to_string());
                        None
                    }
                }
            }
            Ok(_) => None,
            Err(err) => {
                report.error = Some(err.to_string());
                None
            }
        };
        out.push((metadata, report));
    }
    Ok(out)
}

#[cfg(test)]
pub(super) fn read_metadata_candidate(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    offset: u64,
    slot_len: u64,
    header: &[u8],
) -> Result<Metadata> {
    let bytes = read_metadata_payload(backend, sb, offset, slot_len, header)?;
    decode_metadata_payload(header, &bytes)
}

fn read_metadata_payload(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    offset: u64,
    slot_len: u64,
    header: &[u8],
) -> Result<Vec<u8>> {
    match get_u32(header, 20)? {
        METADATA_FORMAT_LEGACY => {
            read_legacy_metadata_candidate(backend, sb, offset, slot_len, header)
        }
        METADATA_FORMAT_TREE | METADATA_FORMAT_TREE_ZSTD => {
            read_tree_metadata_candidate(backend, sb, offset, slot_len, header)
        }
        format => Err(ArgosError::IncompatibleFormat(format!(
            "unsupported raw metadata checkpoint format {format}"
        ))),
    }
}

pub(super) fn decode_metadata_payload(header: &[u8], bytes: &[u8]) -> Result<Metadata> {
    let capture = header.get(CHECKPOINT_CATALOG_OFFSET..CHECKPOINT_CATALOG_OFFSET + 8)
        == Some(CHECKPOINT_CATALOG_MAGIC.as_slice());
    let _guard = crate::model::inodes::capture_checkpoint_proof(capture);
    let restore = |mut metadata: Metadata| {
        let catalog = metadata.checkpoint_member_catalog.take();
        metadata.inodes.restore_checkpoint_catalog(catalog);
        metadata
    };
    if get_u32(header, 20)? != METADATA_FORMAT_TREE_ZSTD {
        return Ok(restore(serde_json::from_slice(bytes)?));
    }
    let expanded_len = get_u64(header, 232)?;
    if expanded_len == 0 || expanded_len > METADATA_DECOMPRESSED_MAX {
        return Err(ArgosError::CorruptedMetadata(
            "invalid compressed checkpoint expanded length".to_string(),
        ));
    }
    let expanded_hash = get_fixed_hex(header, 240, HASH_HEX_LEN)?;
    // Verify the expanded data *before* interpreting it. Stream the digest
    // rather than retaining the entire expanded JSON alongside the inode
    // table. Keep the legacy checksum-before-parse error ordering.
    let decoder = zstd::stream::read::Decoder::new(bytes)?;
    let mut bounded = decoder.take(expanded_len + 1);
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = bounded.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        digest.update(&chunk[..read]);
    }
    if total != expanded_len || hex::encode(digest.finalize()) != expanded_hash {
        return Err(ArgosError::Checksum(
            "compressed metadata checkpoint content mismatch".to_string(),
        ));
    }
    // A second bounded decompression uses a fixed-size input buffer and
    // deserializes directly into the persistent inode map. It avoids a
    // potentially multi-GiB expanded Vec at recovery time.
    let decoder = zstd::stream::read::Decoder::new(bytes)?;
    let input = std::io::BufReader::new(decoder.take(expanded_len + 1));
    Ok(restore(serde_json::from_reader::<_, Metadata>(input)?))
}

fn read_legacy_metadata_candidate(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    offset: u64,
    slot_len: u64,
    header: &[u8],
) -> Result<Vec<u8>> {
    let len = checked_usize(get_u64(header, 32)?, "raw metadata length")?;
    let stored_hash = get_fixed_hex(header, 64, HASH_HEX_LEN)?;
    if len == 0 || len as u64 > slot_len.saturating_sub(RAW_HEADER_SIZE as u64) {
        return Err(ArgosError::CorruptedMetadata(
            "invalid raw metadata length".to_string(),
        ));
    }
    let mut bytes = vec![0u8; len];
    backend.read_recovery_at(&sb.disk_id, offset + RAW_HEADER_SIZE as u64, &mut bytes)?;
    if sha256_hex(&bytes) != stored_hash {
        return Err(ArgosError::Checksum(
            "raw metadata checksum mismatch".to_string(),
        ));
    }
    Ok(bytes)
}

// Keep recovery I/O bounded while avoiding one sysread per metadata page.
pub(super) const METADATA_READ_BATCH_BYTES: usize = 1024 * 1024;

pub(super) fn read_tree_metadata_candidate(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    offset: u64,
    slot_len: u64,
    header: &[u8],
) -> Result<Vec<u8>> {
    let len = checked_usize(get_u64(header, 32)?, "raw metadata length")?;
    let stored_hash = get_fixed_hex(header, 64, HASH_HEX_LEN)?;
    let page_size = checked_usize(get_u64(header, 144)?, "raw metadata page size")?;
    let page_count = checked_usize(get_u64(header, 152)?, "raw metadata page count")?;
    let index_len = checked_usize(get_u64(header, 160)?, "raw metadata index length")?;
    let index_hash = get_fixed_hex(header, 168, HASH_HEX_LEN)?;
    if len == 0 || len as u64 > slot_len.saturating_sub(RAW_HEADER_SIZE as u64) {
        return Err(ArgosError::CorruptedMetadata(
            "invalid raw metadata length".to_string(),
        ));
    }
    if page_size == 0 || page_size > 1024 * 1024 {
        return Err(ArgosError::CorruptedMetadata(
            "invalid raw metadata page size".to_string(),
        ));
    }
    let expected_pages = len.div_ceil(page_size);
    let expected_index_len = page_count
        .checked_mul(METADATA_INDEX_ENTRY_SIZE)
        .ok_or_else(|| ArgosError::CorruptedMetadata("raw metadata index overflows".to_string()))?;
    if page_count != expected_pages || index_len != expected_index_len {
        return Err(ArgosError::CorruptedMetadata(
            "invalid raw metadata page index".to_string(),
        ));
    }
    let index_end = (RAW_HEADER_SIZE as u64)
        .checked_add(index_len as u64)
        .ok_or_else(|| ArgosError::CorruptedMetadata("raw metadata index overflows".to_string()))?;
    if index_end > slot_len {
        return Err(ArgosError::CorruptedMetadata(
            "raw metadata index exceeds slot".to_string(),
        ));
    }
    let mut index = vec![0u8; index_len];
    backend.read_recovery_at(&sb.disk_id, offset + RAW_HEADER_SIZE as u64, &mut index)?;
    if sha256_hex(&index) != index_hash {
        return Err(ArgosError::Checksum(
            "raw metadata index checksum mismatch".to_string(),
        ));
    }
    let data_start = align_up(index_end, page_size as u64);
    let mut bytes = Vec::with_capacity(len);
    let pages_per_batch = (METADATA_READ_BATCH_BYTES / page_size).max(1);
    // The page index format remains unchanged: validate the same individual
    // page offsets/digests, but read physically contiguous pages in a single
    // bounded I/O request. The result buffer is the normal checkpoint payload
    // already required by the decoder; there is no second large allocation.
    for first in (0..page_count).step_by(pages_per_batch) {
        let last = first.saturating_add(pages_per_batch).min(page_count);
        for page_index in first..last {
            let entry_start = page_index * METADATA_INDEX_ENTRY_SIZE;
            let entry = &index[entry_start..entry_start + METADATA_INDEX_ENTRY_SIZE];
            let relative_offset = get_u64(entry, 32)?;
            let page_len = get_u32(entry, 40)? as usize;
            let expected_offset = (page_index as u64)
                .checked_mul(page_size as u64)
                .and_then(|delta| data_start.checked_add(delta))
                .ok_or_else(|| {
                    ArgosError::CorruptedMetadata("raw metadata page offset overflows".to_string())
                })?;
            let end = relative_offset
                .checked_add(page_len as u64)
                .ok_or_else(|| {
                    ArgosError::CorruptedMetadata("raw metadata page overflows".to_string())
                })?;
            if page_len == 0
                || page_len != (len - page_index * page_size).min(page_size)
                || relative_offset != expected_offset
                || end > slot_len
            {
                return Err(ArgosError::CorruptedMetadata(
                    "invalid raw metadata page extent".to_string(),
                ));
            }
        }
        let begin = first * page_size;
        let end = (last * page_size).min(len);
        bytes.resize(end, 0);
        backend.read_recovery_at(
            &sb.disk_id,
            offset + data_start + begin as u64,
            &mut bytes[begin..end],
        )?;
        for page_index in first..last {
            let entry_start = page_index * METADATA_INDEX_ENTRY_SIZE;
            let entry = &index[entry_start..entry_start + HASH_BYTES_LEN];
            let page_begin = page_index * page_size;
            let page_end = (page_begin + page_size).min(len);
            if Sha256::digest(&bytes[page_begin..page_end]).as_slice() != entry {
                return Err(ArgosError::Checksum(format!(
                    "raw metadata page {page_index} checksum mismatch"
                )));
            }
        }
    }
    if bytes.len() != len || sha256_hex(&bytes) != stored_hash {
        return Err(ArgosError::Checksum(
            "raw metadata checksum mismatch".to_string(),
        ));
    }
    Ok(bytes)
}

pub(super) struct MetadataTreeCheckpoint {
    pub(super) header: [u8; RAW_HEADER_SIZE],
    // Own the encoded payload once. Each member borrows the same bytes in
    // bounded contiguous batches; the index is the only extra full-size
    // allocation (48 bytes per 4 KiB page).
    pub(super) payload: Vec<u8>,
    pub(super) index: Vec<u8>,
    pub(super) data_start: u64,
    pub(super) required_len: u64,
}

impl MetadataTreeCheckpoint {
    pub(super) fn body_writes(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.payload
            .chunks(CHECKPOINT_BATCH_BYTES)
            .enumerate()
            .map(|(batch, bytes)| {
                (
                    self.data_start + (batch * CHECKPOINT_BATCH_BYTES) as u64,
                    bytes,
                )
            })
            .chain(std::iter::once((
                RAW_HEADER_SIZE as u64,
                self.index.as_slice(),
            )))
    }
}

#[cfg(test)]
pub(super) fn metadata_tree_checkpoint(
    disk_id: &str,
    txid: u64,
    generation: u64,
    bytes: &[u8],
    hash: &str,
    slot_len: u64,
) -> Result<MetadataTreeCheckpoint> {
    metadata_tree_checkpoint_encoded(
        disk_id,
        txid,
        generation,
        bytes.to_vec(),
        hash,
        slot_len,
        None,
    )
}

pub(super) fn metadata_tree_checkpoint_encoded(
    disk_id: &str,
    txid: u64,
    generation: u64,
    bytes: Vec<u8>,
    hash: &str,
    slot_len: u64,
    original: Option<&(u64, String)>,
) -> Result<MetadataTreeCheckpoint> {
    let page_count = bytes.len().div_ceil(METADATA_PAGE_SIZE);
    let index_len = page_count
        .checked_mul(METADATA_INDEX_ENTRY_SIZE)
        .ok_or_else(|| ArgosError::DiskFull {
            disk_id: disk_id.to_string(),
            required: bytes.len() as u64,
            available: slot_len,
        })?;
    let index_end = (RAW_HEADER_SIZE as u64)
        .checked_add(index_len as u64)
        .ok_or_else(|| ArgosError::DiskFull {
            disk_id: disk_id.to_string(),
            required: bytes.len() as u64,
            available: slot_len,
        })?;
    let data_start = align_up(index_end, METADATA_PAGE_SIZE as u64);
    let data_bytes = (page_count as u64)
        .checked_mul(METADATA_PAGE_SIZE as u64)
        .ok_or_else(|| ArgosError::DiskFull {
            disk_id: disk_id.to_string(),
            required: bytes.len() as u64,
            available: slot_len,
        })?;
    let data_end = data_start
        .checked_add(data_bytes)
        .ok_or_else(|| ArgosError::DiskFull {
            disk_id: disk_id.to_string(),
            required: bytes.len() as u64,
            available: slot_len,
        })?;
    if bytes.is_empty() || data_end > slot_len {
        return Err(ArgosError::DiskFull {
            disk_id: disk_id.to_string(),
            required: data_end.max(bytes.len() as u64),
            available: slot_len,
        });
    }

    let mut index = vec![0u8; index_len];
    for page_index in 0..page_count {
        let start = page_index * METADATA_PAGE_SIZE;
        let end = (start + METADATA_PAGE_SIZE).min(bytes.len());
        let page = &bytes[start..end];
        let page_hash = Sha256::digest(page);
        let entry_start = page_index * METADATA_INDEX_ENTRY_SIZE;
        let entry = &mut index[entry_start..entry_start + METADATA_INDEX_ENTRY_SIZE];
        entry[..HASH_BYTES_LEN].copy_from_slice(&page_hash);
        let relative_offset = (page_index as u64)
            .checked_mul(METADATA_PAGE_SIZE as u64)
            .and_then(|delta| data_start.checked_add(delta))
            .ok_or_else(|| ArgosError::DiskFull {
                disk_id: disk_id.to_string(),
                required: bytes.len() as u64,
                available: slot_len,
            })?;
        put_u64(entry, 32, relative_offset);
        put_u32(entry, 40, page.len() as u32);
    }
    let index_hash = sha256_hex(&index);

    let mut out = [0u8; RAW_HEADER_SIZE];
    out[..16].copy_from_slice(METADATA_MAGIC);
    put_u32(&mut out, 16, RAW_STORE_VERSION);
    put_u32(
        &mut out,
        20,
        if original.is_some() {
            METADATA_FORMAT_TREE_ZSTD
        } else {
            METADATA_FORMAT_TREE
        },
    );
    if let Some((expanded_len, expanded_hash)) = original {
        put_u64(&mut out, 232, *expanded_len);
        put_fixed_str(&mut out, 240, HASH_HEX_LEN, expanded_hash);
    }
    put_u64(&mut out, 24, txid);
    put_u64(&mut out, 32, bytes.len() as u64);
    put_fixed_str(&mut out, 64, 64, hash);
    put_u64(&mut out, 136, generation);
    put_u64(&mut out, 144, METADATA_PAGE_SIZE as u64);
    put_u64(&mut out, 152, page_count as u64);
    put_u64(&mut out, 160, index_len as u64);
    put_fixed_str(&mut out, 168, HASH_HEX_LEN, &index_hash);
    Ok(MetadataTreeCheckpoint {
        header: out,
        payload: bytes,
        index,
        data_start,
        required_len: data_end,
    })
}
