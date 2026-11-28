use super::*;

pub(super) fn initialize_journal_region(
    backend: &dyn StorageBackend,
    device_id: &str,
    sb: &RawSuperblock,
) -> Result<()> {
    let mut header = [0u8; RAW_HEADER_SIZE];
    header[..16].copy_from_slice(JOURNAL_MAGIC);
    put_u32(&mut header, 16, RAW_STORE_VERSION);
    put_u64(&mut header, 24, RAW_HEADER_SIZE as u64);
    backend.write_at(&device_id.to_string(), sb.journal.offset, &header)
}

/// Clear stale journal tails only on members that durably hold the checkpoint.
/// A quorum checkpoint does not imply that every member received its copy.
pub fn reset_journals_after_checkpoint(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    checkpoint: &Metadata,
) -> Result<u64> {
    super::paged::reject_paged(superblocks)?;
    // Metadata contains floating-point telemetry that can change its JSON
    // representation on deserialization. Match the persisted transaction identity
    // and integrity hash rather than requiring byte-for-byte reserialization.
    if checkpoint.integrity.meta_hash.is_empty() {
        return Err(ArgosError::CorruptedMetadata(
            "cannot reset journals without a checkpoint integrity hash".to_string(),
        ));
    }
    let mut reset = 0;
    let mut failures = Vec::new();
    for sb in superblocks {
        let result = (|| -> Result<()> {
            let candidates = read_metadata_candidates(backend, sb)?;
            let confirmed = candidates.into_iter().any(|(candidate, _)| {
                candidate.is_some_and(|candidate| {
                    candidate.txid == checkpoint.txid
                        && candidate.integrity.generation == checkpoint.integrity.generation
                        && candidate.integrity.meta_hash == checkpoint.integrity.meta_hash
                })
            });
            if !confirmed {
                return Err(ArgosError::CorruptedMetadata(format!(
                    "{} has no durable copy of the selected checkpoint",
                    sb.disk_id
                )));
            }
            initialize_journal_region(backend, &sb.disk_id, sb)?;
            backend.flush_device(&sb.disk_id)
        })();
        match result {
            Ok(()) => reset += 1,
            Err(err) => failures.push(format!("{}: {err}", sb.disk_id)),
        }
    }
    if !failures.is_empty() {
        return Err(ArgosError::CorruptedMetadata(format!(
            "reset {reset} journal region(s), {} unresolved: {}",
            failures.len(),
            failures.join("; ")
        )));
    }
    Ok(reset)
}

pub(super) fn append_journal(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_journal_with_previous_validation(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
        false,
    )
}

pub(super) fn build_journal_entry(
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
    trust_integrity: bool,
) -> Result<(Vec<u8>, String)> {
    build_journal_entry_measured(
        metadata,
        previous_metadata,
        action,
        details,
        trust_integrity,
        None,
        None,
    )
}

fn build_journal_entry_measured(
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
    trust_integrity: bool,
    prepared_delta: Option<Vec<journal::MetadataDeltaOp>>,
    metrics: Option<&CommitMetrics>,
) -> Result<(Vec<u8>, String)> {
    let delta_base = match previous_metadata {
        Some(previous)
            if previous.integrity.meta_hash == metadata.integrity.previous_meta_hash
                && (trust_integrity
                    || journal::canonical_metadata_hash(previous)?
                        == metadata.integrity.previous_meta_hash) =>
        {
            Some(previous)
        }
        _ => None,
    };
    // A raw pool already checkpoints into two per-member metadata slots.
    // Periodically embedding the entire namespace in its bounded journal
    // serializes hundreds of MiB only to discover that the entry cannot fit.
    // Use a delta whenever a validated previous image is available.
    if metadata.integrity.hash_scheme == Some(journal::DELTA_INTEGRITY_SCHEME)
        && delta_base.is_none()
    {
        return Err(ArgosError::IncompatibleFormat(
            "raw delta integrity requires a validated previous image".to_string(),
        ));
    }
    let write_checkpoint = delta_base.is_none();
    if prepared_delta.is_some()
        && (!trust_integrity
            || metadata.integrity.hash_scheme != Some(journal::DELTA_INTEGRITY_SCHEME)
            || write_checkpoint)
    {
        return Err(ArgosError::CorruptedMetadata(
            "precomputed delta requires a trusted v2 transaction with a matching base".into(),
        ));
    }
    let metadata_delta = if write_checkpoint {
        None
    } else if let Some(delta) = prepared_delta {
        // The delta was built when preparing v2 integrity, before any I/O.
        // Neither superblock discovery nor the data flush mutates metadata.
        Some(delta)
    } else {
        let _delta_timer = metrics.map(|metrics| metrics.timer(CommitPhase::JournalDelta));
        Some(journal::metadata_delta(
            delta_base.expect("checked above"),
            metadata,
        )?)
    };
    let meta_hash = if trust_integrity && !metadata.integrity.meta_hash.is_empty() {
        metadata.integrity.meta_hash.clone()
    } else {
        journal::canonical_metadata_hash(metadata)?
    };
    let mut record = RawJournalRecord {
        version: RAW_STORE_VERSION,
        time: now_f64(),
        volume_uuid: metadata.uuid.clone(),
        txid: metadata.txid,
        generation: metadata.integrity.generation,
        action: action.to_string(),
        details,
        meta_hash,
        metadata: if metadata_delta.is_some() {
            None
        } else {
            Some(metadata.clone())
        },
        metadata_delta,
        record_hash: String::new(),
    };
    record.record_hash = raw_record_hash(&record)?;
    let logical_record_hash = record.record_hash.clone();
    Ok((encode_journal_record(&record)?, logical_record_hash))
}

// The high bit is reserved for compressed records. All historical journal
// regions are much smaller than 2 GiB, so no valid legacy record used it.
const JOURNAL_COMPRESSED_FLAG: u32 = 1 << 31;
const JOURNAL_MAX_EXPANDED_RECORD: usize = 64 * 1024 * 1024;
const JOURNAL_COMPRESSION_MIN_BYTES: usize = 1024;

fn encode_journal_record(record: &RawJournalRecord) -> Result<Vec<u8>> {
    let record_bytes = serde_json::to_vec(record)?;
    encode_journal_payload(&record_bytes, false)
}

fn encode_journal_payload(bytes: &[u8], compressed: bool) -> Result<Vec<u8>> {
    let record_len = u32::try_from(bytes.len())
        .ok()
        .filter(|len| *len != 0 && *len < JOURNAL_COMPRESSED_FLAG)
        .ok_or_else(|| ArgosError::Invalid("raw journal record is too large".to_string()))?;
    let tagged_len = record_len
        | if compressed {
            JOURNAL_COMPRESSED_FLAG
        } else {
            0
        };
    let mut entry = Vec::with_capacity(36 + bytes.len());
    entry.extend_from_slice(&tagged_len.to_le_bytes());
    entry.extend_from_slice(&Sha256::digest(bytes));
    entry.extend_from_slice(bytes);
    Ok(entry)
}

fn compress_journal_entry(entry: Vec<u8>, enabled: bool) -> Result<Vec<u8>> {
    let bytes = &entry[36..];
    if !enabled
        || !(JOURNAL_COMPRESSION_MIN_BYTES..=JOURNAL_MAX_EXPANDED_RECORD).contains(&bytes.len())
    {
        return Ok(entry);
    }
    // LZ4 is intentionally low latency. Avoid expanding small/incompressible
    // records or paying decompression costs for negligible space savings.
    let compressed = lz4_flex::compress_prepend_size(bytes);
    if compressed.len() <= bytes.len() - bytes.len() / 8 {
        encode_journal_payload(&compressed, true)
    } else {
        Ok(entry)
    }
}

pub(super) fn journal_entry_length(sb: &RawSuperblock, header: &[u8; 36]) -> Result<(usize, bool)> {
    let tagged = u32::from_le_bytes(header[..4].try_into().unwrap());
    let compressed = tagged & JOURNAL_COMPRESSED_FLAG != 0;
    if compressed && sb.required_feature_flags & REQUIRED_FEATURE_JOURNAL_COMPRESSION == 0 {
        return Err(ArgosError::IncompatibleFormat(
            "compressed journal record lacks required feature".into(),
        ));
    }
    Ok(((tagged & !JOURNAL_COMPRESSED_FLAG) as usize, compressed))
}

pub(super) fn decode_journal_payload(bytes: &[u8], compressed: bool) -> Result<RawJournalRecord> {
    if !compressed {
        return Ok(serde_json::from_slice(bytes)?);
    }
    let len = bytes
        .get(..4)
        .map(|prefix| u32::from_le_bytes(prefix.try_into().unwrap()) as usize)
        .filter(|len| *len != 0 && *len <= JOURNAL_MAX_EXPANDED_RECORD)
        .ok_or_else(|| {
            ArgosError::CorruptedMetadata("invalid expanded journal record length".into())
        })?;
    let expanded = lz4_flex::decompress_size_prepended(bytes).map_err(|err| {
        ArgosError::CorruptedMetadata(format!("invalid compressed journal record: {err}"))
    })?;
    if expanded.len() != len {
        return Err(ArgosError::CorruptedMetadata(
            "compressed journal record length mismatch".into(),
        ));
    }
    Ok(serde_json::from_slice(&expanded)?)
}

fn build_quorum_commit_entry(
    metadata: &Metadata,
    target_record_hash: Option<&str>,
) -> Result<Vec<u8>> {
    let meta_hash = metadata_hash_for_replay(metadata)?;
    let mut record = RawJournalRecord {
        version: RAW_STORE_VERSION,
        time: now_f64(),
        volume_uuid: metadata.uuid.clone(),
        txid: metadata.txid,
        generation: metadata.integrity.generation,
        action: RAW_QUORUM_COMMIT_ACTION.to_string(),
        details: serde_json::json!({
            "target_record_hash": target_record_hash,
            "checkpoint": target_record_hash.is_none(),
            "quorum_need": metadata_quorum_requirement(metadata),
        }),
        meta_hash,
        metadata: None,
        metadata_delta: None,
        record_hash: String::new(),
    };
    record.record_hash = raw_record_hash(&record)?;
    encode_journal_record(&record)
}

pub(super) fn journal_append_position(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    entry_len: usize,
) -> Result<(Vec<u8>, u64, u64, bool)> {
    let mut header = vec![0u8; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, sb.journal.offset, &mut header)?;
    if &header[..16] != JOURNAL_MAGIC {
        initialize_journal_region(backend, &sb.disk_id, sb)?;
        backend.read_at(&sb.disk_id, sb.journal.offset, &mut header)?;
    }
    let write_offset = get_u64(&header, 24)?;
    let end = write_offset.checked_add(entry_len as u64);
    let rollover =
        write_offset < RAW_HEADER_SIZE as u64 || end.is_none_or(|end| end > sb.journal.length);
    Ok((header, write_offset, end.unwrap_or_default(), rollover))
}

// Scan the complete journal prefix as before, but fetch bounded contiguous
// ranges rather than two tiny device requests for every record. This preserves
// physical checksum/record hash validation on every append (including stale
// prefixes) and avoids trusting an unverified cached tip.
pub(super) const JOURNAL_TIP_BATCH_BYTES: usize = 256 * 1024;

struct JournalTipReader<'a> {
    backend: &'a dyn StorageBackend,
    sb: &'a RawSuperblock,
    offset: u64,
    end: u64,
    buffer: Vec<u8>,
    consumed: usize,
}

impl JournalTipReader<'_> {
    fn read_exact(&mut self, mut output: &mut [u8]) -> Result<()> {
        while !output.is_empty() {
            if self.consumed == self.buffer.len() {
                let count = self
                    .end
                    .saturating_sub(self.offset)
                    .min(JOURNAL_TIP_BATCH_BYTES as u64) as usize;
                if count == 0 {
                    return Err(ArgosError::CorruptedMetadata(
                        "truncated raw journal tip".into(),
                    ));
                }
                self.buffer.resize(count, 0);
                self.backend.read_at(
                    &self.sb.disk_id,
                    self.sb.journal.offset + self.offset,
                    &mut self.buffer,
                )?;
                self.offset += count as u64;
                self.consumed = 0;
            }
            let count = output.len().min(self.buffer.len() - self.consumed);
            output[..count].copy_from_slice(&self.buffer[self.consumed..self.consumed + count]);
            self.consumed += count;
            output = &mut output[count..];
        }
        Ok(())
    }
}

pub(super) fn journal_tip_identity(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
) -> Result<Option<(u64, String)>> {
    let mut header = vec![0u8; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, sb.journal.offset, &mut header)?;
    if &header[..16] != JOURNAL_MAGIC {
        return Ok(None);
    }
    let end = get_u64(&header, 24)?;
    if end < RAW_HEADER_SIZE as u64 || end > sb.journal.length {
        return Err(ArgosError::CorruptedMetadata(
            "raw journal end outside region".into(),
        ));
    }
    let mut cursor = RAW_HEADER_SIZE as u64;
    let mut reader = JournalTipReader {
        backend,
        sb,
        offset: cursor,
        end,
        buffer: Vec::new(),
        consumed: 0,
    };
    let mut tip = None;
    while cursor + 36 <= end {
        let mut entry_header = [0u8; 36];
        reader.read_exact(&mut entry_header)?;
        let (len, compressed) = journal_entry_length(sb, &entry_header)?;
        if len == 0 || cursor + 36 + len as u64 > end {
            return Err(ArgosError::CorruptedMetadata(format!(
                "invalid raw journal extent at {}:{}",
                sb.disk_id, cursor
            )));
        }
        let mut bytes = vec![0u8; len];
        reader.read_exact(&mut bytes)?;
        if Sha256::digest(&bytes).as_slice() != &entry_header[4..36] {
            return Err(ArgosError::Checksum(format!(
                "raw journal payload checksum mismatch at {}:{}",
                sb.disk_id, cursor
            )));
        }
        let record = decode_journal_payload(&bytes, compressed)?;
        if raw_record_hash(&record)? != record.record_hash {
            return Err(ArgosError::Checksum(format!(
                "raw journal record hash mismatch at {}:{}",
                sb.disk_id, cursor
            )));
        }
        tip = Some((record.txid, record.meta_hash));
        cursor += 36 + len as u64;
    }
    if cursor != end {
        return Err(ArgosError::CorruptedMetadata(
            "raw journal ends outside a complete record".into(),
        ));
    }
    Ok(tip)
}

fn journal_tip_accepts_transaction(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    metadata: &Metadata,
    allow_same_txid: bool,
) -> Result<bool> {
    let Some((tip_txid, tip_hash)) = journal_tip_identity(backend, sb)? else {
        return Ok(true);
    };
    if tip_txid > metadata.txid {
        return Ok(false);
    }
    if tip_txid == metadata.txid {
        return Ok(allow_same_txid || tip_hash == metadata_hash_for_replay(metadata)?);
    }
    // Older suffixes are safe to supersede: replay starts from the selected
    // durable base and ignores records at/below that base txid. The dangerous
    // case is a divergent record for the transaction txid being appended now,
    // because it would otherwise shadow the later record during replay.
    Ok(true)
}

pub(super) struct JournalMemberWriteOptions<'a> {
    pub(super) flush: bool,
    pub(super) metrics: Option<&'a CommitMetrics>,
}

pub(super) fn append_journal_member(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    mut header: Vec<u8>,
    write_offset: u64,
    end: u64,
    entry: &[u8],
    options: JournalMemberWriteOptions<'_>,
) -> Result<()> {
    backend.write_at(&sb.disk_id, sb.journal.offset + write_offset, entry)?;
    if let Some(metrics) = options.metrics {
        metrics.add_raw_write_bytes(RawWriteKind::Journal, entry.len());
    }
    put_u64(&mut header, 24, end);
    backend.write_at(&sb.disk_id, sb.journal.offset, &header)?;
    if let Some(metrics) = options.metrics {
        metrics.add_raw_write_bytes(RawWriteKind::Journal, header.len());
    }
    if options.flush {
        backend.flush_device(&sb.disk_id)?;
    }
    Ok(())
}

/// Device phases are independent, but every phase joins all workers before
/// the next quorum/flush barrier. Preserve input order for error reporting.
fn parallel_journal_members<T: Send, R: Send>(
    items: impl IntoIterator<Item = T>,
    work: impl Fn(T) -> R + Sync,
) -> Vec<R> {
    let items = items.into_iter().collect::<Vec<_>>();
    if items.len() <= 1 {
        return items.into_iter().map(work).collect();
    }
    std::thread::scope(|scope| {
        let work = &work;
        let jobs = items
            .into_iter()
            .map(|item| scope.spawn(move || work(item)))
            .collect::<Vec<_>>();
        jobs.into_iter()
            .map(|job| job.join().expect("journal member worker panicked"))
            .collect()
    })
}

fn append_quorum_commit_certificate(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    eligible: &BTreeSet<String>,
    certificate: &[u8],
    need: usize,
    report: &mut QuorumWriteReport,
    metrics: Option<&CommitMetrics>,
) -> Result<()> {
    let results = parallel_journal_members(
        superblocks
            .iter()
            .filter(|sb| eligible.contains(&sb.disk_id)),
        |sb| {
            let result = (|| -> Result<()> {
                let (header, write_offset, end, rollover) =
                    journal_append_position(backend, sb, certificate.len())?;
                if rollover {
                    return Err(ArgosError::DiskFull {
                        disk_id: sb.disk_id.clone(),
                        required: certificate.len() as u64,
                        available: sb.journal.length,
                    });
                }
                append_journal_member(
                    backend,
                    sb,
                    header,
                    write_offset,
                    end,
                    certificate,
                    JournalMemberWriteOptions {
                        flush: true,
                        metrics,
                    },
                )
            })();
            (sb, result)
        },
    );
    let mut durable = 0usize;
    for (sb, result) in results {
        match result {
            Ok(()) => durable += 1,
            Err(err) => report.record_failure(&sb.disk_id, &err),
        }
    }
    if durable < need {
        return Err(indeterminate_commit(
            "metadata quorum certificate",
            need,
            durable,
        ));
    }
    Ok(())
}

fn append_journal_with_previous_validation(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
    trust_integrity: bool,
) -> Result<()> {
    super::paged::reject_paged(superblocks)?;
    let (entry, _) = build_journal_entry(
        metadata,
        previous_metadata,
        action,
        details,
        trust_integrity,
    )?;
    let mut append_positions = Vec::with_capacity(superblocks.len());
    let mut rollover = false;
    for sb in superblocks {
        let position = journal_append_position(backend, sb, entry.len())?;
        rollover |= position.3;
        append_positions.push(position);
    }
    if rollover {
        let checkpoint = prepare_checkpoint_writes(metadata, superblocks)?;
        for (_, result) in checkpoint_members_and_reset_parallel(
            backend,
            superblocks,
            metadata,
            &checkpoint,
            None,
            None,
        ) {
            result?;
        }
        return Ok(());
    }

    let mut rollback_headers: Vec<(String, u64, Vec<u8>)> = Vec::new();
    let result = (|| -> Result<()> {
        for (index, (sb, (header, write_offset, end, _))) in
            superblocks.iter().zip(append_positions).enumerate()
        {
            rollback_headers.push((sb.disk_id.clone(), sb.journal.offset, header.clone()));
            append_journal_member(
                backend,
                sb,
                header,
                write_offset,
                end,
                &entry,
                JournalMemberWriteOptions {
                    flush: !metadata.config.defer_journal_flush,
                    metrics: None,
                },
            )?;
            if index + 1 < superblocks.len() {
                journal::inject_crash(FaultPoint::AfterPartialJournalFanout.as_str())?;
            }
        }
        Ok(())
    })();

    if result.is_err() {
        for (disk_id, journal_offset, header) in rollback_headers.into_iter().rev() {
            let _ = backend.write_at(&disk_id, journal_offset, &header);
            if !metadata.config.defer_journal_flush {
                let _ = backend.flush_device(&disk_id);
            }
        }
    }
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JournalQuorumMode {
    Transaction,
    TrustedTransaction,
    Event,
}

pub(super) struct JournalQuorumOptions<'a> {
    pub(super) mode: JournalQuorumMode,
    pub(super) metrics: Option<&'a CommitMetrics>,
    pub(super) prepared_delta: Option<Vec<journal::MetadataDeltaOp>>,
}

pub(super) fn append_journal_quorum(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
    options: JournalQuorumOptions<'_>,
) -> Result<QuorumWriteReport> {
    super::paged::reject_paged(superblocks)?;
    let JournalQuorumOptions {
        mode,
        metrics,
        prepared_delta,
    } = options;
    let trust_integrity = matches!(mode, JournalQuorumMode::TrustedTransaction);
    let allow_same_txid = matches!(mode, JournalQuorumMode::Event);
    let need = metadata_quorum_requirement(metadata);
    if superblocks.len() < need {
        return Err(quorum_unavailable(
            "metadata journal",
            need,
            superblocks.len(),
        ));
    }
    // Do not introduce a new encoding during a partial feature upgrade or
    // degraded mount: every active member must have been fenced first.
    let active = metadata
        .disks
        .values()
        .filter(|disk| disk.status != crate::types::DiskStatus::Removed)
        .count();
    let splice_enabled = superblocks.len() == active && array_splice_feature_enabled(superblocks);
    if !splice_enabled
        && prepared_delta.as_ref().is_some_and(|delta| {
            delta
                .iter()
                .any(|op| matches!(op, journal::MetadataDeltaOp::Splice { .. }))
        })
    {
        return Err(ArgosError::IncompatibleFormat(
            "array splice journal delta requires all active members to be upgraded".into(),
        ));
    }
    let _splice_guard = journal::allow_array_splice(splice_enabled);
    let (entry, target_record_hash) = build_journal_entry_measured(
        metadata,
        previous_metadata,
        action,
        details,
        trust_integrity,
        prepared_delta,
        metrics,
    )?;
    // Compression is enabled only after all active members advertise the
    // required bit, including when a pool was only partially upgraded.
    let compression_enabled = superblocks.len() == active
        && superblocks
            .iter()
            .all(|sb| sb.required_feature_flags & REQUIRED_FEATURE_JOURNAL_COMPRESSION != 0);
    let entry = compress_journal_entry(entry, compression_enabled)?;
    let journal_certificate =
        build_quorum_commit_entry(metadata, Some(target_record_hash.as_str()))?;
    let mut report = QuorumWriteReport::default();
    let mut positions = Vec::with_capacity(superblocks.len());
    let mut rollover = false;
    let discovered = parallel_journal_members(superblocks, |sb| {
        let tip_accepts = {
            let _tip_timer = metrics.map(|metrics| metrics.timer(CommitPhase::JournalTip));
            journal_tip_accepts_transaction(backend, sb, metadata, allow_same_txid)
        };
        let position = matches!(&tip_accepts, Ok(true))
            .then(|| journal_append_position(backend, sb, entry.len()));
        (sb, tip_accepts, position)
    });
    for (sb, tip_accepts, position) in discovered {
        match tip_accepts {
            Ok(true) => match position.expect("accepted member has append position") {
                Ok(position) => {
                    let certificate_end = position.2.checked_add(journal_certificate.len() as u64);
                    let member_rollover =
                        position.3 || certificate_end.is_none_or(|end| end > sb.journal.length);
                    rollover |= member_rollover;
                    positions.push((sb, position));
                }
                Err(err) => report.record_failure(&sb.disk_id, &err),
            },
            Ok(false) => report.record_failure(
                &sb.disk_id,
                &ArgosError::Conflict(format!(
                    "raw journal tip on {} does not chain from the selected durable metadata",
                    sb.disk_id
                )),
            ),
            Err(err) => report.record_failure(&sb.disk_id, &err),
        }
    }
    if positions.len() < need {
        return Err(quorum_failure_before_write(
            &report,
            "metadata journal",
            need,
            positions.len(),
        ));
    }

    if rollover {
        let mut durable = BTreeSet::new();
        let mut uncertain_write = false;
        let eligible: Vec<_> = positions.into_iter().map(|(sb, _)| sb.clone()).collect();
        // All members receive the exact same checkpoint bytes and hash.
        // Prepare once before touching any journal or metadata slot.
        let checkpoint = {
            let _encode_timer = metrics.map(|metrics| metrics.timer(CommitPhase::CheckpointEncode));
            prepare_checkpoint_writes(metadata, &eligible)?
        };
        if let Some(metrics) = metrics {
            metrics.record_checkpoint_reason(CheckpointReason::JournalRollover);
        }
        let _rollover_timer = metrics.map(|metrics| metrics.timer(CommitPhase::JournalRollover));
        let protected_targets = protected_rollover_slot_targets(backend, &eligible, need)?;
        if let Some(metrics) = metrics {
            let single = protected_targets.as_ref().map_or(0, BTreeMap::len);
            metrics.record_rollover_member_strategy(single, eligible.len() - single);
        }
        for (disk_id, result) in checkpoint_members_and_reset_parallel(
            backend,
            &eligible,
            metadata,
            &checkpoint,
            metrics,
            protected_targets.as_ref(),
        ) {
            match result {
                Ok(()) => {
                    durable.insert(disk_id);
                }
                Err(err) => {
                    uncertain_write |= !matches!(err, ArgosError::DiskFull { .. });
                    report.record_failure(&disk_id, &err);
                }
            }
        }
        if durable.len() < need {
            return Err(if uncertain_write {
                indeterminate_commit("metadata journal rollover", need, durable.len())
            } else {
                quorum_failure_before_write(
                    &report,
                    "metadata journal rollover",
                    need,
                    durable.len(),
                )
            });
        }
        let checkpoint_certificate = build_quorum_commit_entry(metadata, None)?;
        {
            let _quorum_timer = metrics.map(|metrics| metrics.timer(CommitPhase::JournalQuorum));
            append_quorum_commit_certificate(
                backend,
                superblocks,
                &durable,
                &checkpoint_certificate,
                need,
                &mut report,
                metrics,
            )?;
        }
        return Ok(report);
    }

    let mut appended = BTreeSet::new();
    let append =
        |(sb, (header, write_offset, end, _)): (&RawSuperblock, (Vec<u8>, u64, u64, bool))| {
            let _member_timer = metrics
                .map(|metrics| metrics.member_timer(CommitPhase::JournalMemberIo, &sb.disk_id));
            let result = append_journal_member(
                backend,
                sb,
                header,
                write_offset,
                end,
                &entry,
                JournalMemberWriteOptions {
                    flush: !metadata.config.defer_journal_flush,
                    metrics,
                },
            );
            (sb.disk_id.clone(), result)
        };
    if journal::crash_point_requested(FaultPoint::AfterPartialJournalFanout.as_str()) {
        // Fault injection deliberately stops below quorum. Keep this path
        // serial so a test/process abort cannot accidentally publish quorum.
        let position_count = positions.len();
        for (index, position) in positions.into_iter().enumerate() {
            let (disk_id, result) = append(position);
            match result {
                Ok(()) => {
                    appended.insert(disk_id);
                    if appended.len() < need && index + 1 < position_count {
                        journal::inject_crash(FaultPoint::AfterPartialJournalFanout.as_str())?;
                    }
                }
                Err(err) => report.record_failure(&disk_id, &err),
            }
        }
    } else {
        for (disk_id, result) in parallel_journal_members(positions, append) {
            match result {
                Ok(()) => {
                    appended.insert(disk_id);
                }
                Err(err) => report.record_failure(&disk_id, &err),
            }
        }
    }
    let durable = if metadata.config.defer_journal_flush {
        let mut durable = BTreeSet::new();
        let flushes = parallel_journal_members(appended, |disk_id| {
            let _flush_timer = metrics.map(|metrics| metrics.timer(CommitPhase::JournalFlush));
            let result = backend.flush_device(&disk_id);
            (disk_id, result)
        });
        for (disk_id, result) in flushes {
            match result {
                Ok(()) => {
                    durable.insert(disk_id);
                }
                Err(err) => report.record_failure(&disk_id, &err),
            }
        }
        durable
    } else {
        appended
    };
    if durable.len() < need {
        // Every candidate member has already been offered the new record/header.
        // A write or flush error cannot prove that those bytes will disappear
        // across reboot, so returning an ordinary uncommitted quorum error would
        // allow caller rollback followed by recovery resurrecting the mutation.
        return Err(indeterminate_commit(
            "metadata journal",
            need,
            durable.len(),
        ));
    }
    {
        let _quorum_timer = metrics.map(|metrics| metrics.timer(CommitPhase::JournalQuorum));
        append_quorum_commit_certificate(
            backend,
            superblocks,
            &durable,
            &journal_certificate,
            need,
            &mut report,
            metrics,
        )?;
    }
    Ok(report)
}

fn checkpoint_members_and_reset_parallel(
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
                    let result = (|| {
                        if let Some(&slot) =
                            protected_targets.and_then(|targets| targets.get(&sb.disk_id))
                        {
                            // Never touch the quorum-verified recovery slot.
                            if checkpoint.required_len > sb.metadata.length / 2 {
                                return Err(ArgosError::DiskFull {
                                    disk_id: sb.disk_id.clone(),
                                    required: checkpoint.required_len,
                                    available: sb.metadata.length / 2,
                                });
                            }
                            write_metadata_slot_differential(
                                backend,
                                sb,
                                sb.metadata.offset + slot * (sb.metadata.length / 2),
                                checkpoint,
                                metrics,
                            )?;
                        } else {
                            // Missing/unverified members still receive both
                            // copies, as before. The protected subset by
                            // itself must contain a quorum.
                            write_metadata_member(
                                backend, sb, metadata, checkpoint, metrics, None,
                            )?;
                        }
                        // Reset this member's journal only after its new
                        // checkpoint is physically flushed and published.
                        initialize_journal_region(backend, &sb.disk_id, sb)?;
                        if let Some(metrics) = metrics {
                            metrics.add_raw_write_bytes(RawWriteKind::Journal, RAW_HEADER_SIZE);
                        }
                        backend.flush_device(&sb.disk_id)
                    })();
                    (sb.disk_id.clone(), result)
                })
            })
            .collect();
        jobs.into_iter()
            .map(|job| job.join().expect("journal rollover worker panicked"))
            .collect()
    })
}
