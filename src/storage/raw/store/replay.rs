use super::*;

type ReplayKey = (u64, u64, String);

#[derive(Clone)]
struct ReplayProof {
    key: ReplayKey,
    need: usize,
    voting_member: bool,
    present_quorum: bool,
}

impl ReplayProof {
    fn from_metadata(metadata: &Metadata, member: &str, present: &BTreeSet<&str>) -> Result<Self> {
        let need = metadata_quorum_requirement(metadata);
        Ok(Self {
            key: (
                metadata.txid,
                metadata.integrity.generation,
                metadata_hash_for_replay(metadata)?,
            ),
            need,
            voting_member: metadata_has_voting_member(metadata, member),
            present_quorum: metadata
                .disks
                .iter()
                .filter(|(id, disk)| {
                    disk.status != DiskStatus::Removed && present.contains(id.as_str())
                })
                .count()
                >= need,
        })
    }
}

pub(super) struct ReplayScan {
    pub(super) winner: Option<ReplayKey>,
    pub(super) retained: Option<Arc<Metadata>>,
}

pub(super) fn read_latest_journal_metadata(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    report: &mut TransactionReport,
    base_metadata: Option<&Metadata>,
    checkpoint_catalog: &[(String, Option<Arc<Metadata>>, MetadataCandidateReport)],
) -> Result<Option<Arc<Metadata>>> {
    let first = scan_journal_metadata(
        backend,
        superblocks,
        report,
        base_metadata,
        checkpoint_catalog,
        None,
    )?;
    let Some(winner) = first.winner else {
        return Ok(None);
    };
    if let Some(metadata) = first.retained {
        return Ok(Some(metadata));
    }
    // A divergent/uncertified tail can make an older transaction the quorum
    // winner. Materialize that exact authenticated identity in a second pass
    // rather than holding every full historical state in RAM. Recheck the
    // quorum decision; never silently publish a different state if media changed.
    let mut second_report = TransactionReport::default();
    let second = scan_journal_metadata(
        backend,
        superblocks,
        &mut second_report,
        base_metadata,
        checkpoint_catalog,
        Some(&winner),
    )?;
    if second.winner.as_ref() != Some(&winner) || second.retained.is_none() {
        return Err(ArgosError::CorruptedMetadata(
            "raw journal quorum changed during recovery replay".into(),
        ));
    }
    Ok(second.retained)
}

pub(super) fn scan_journal_metadata(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    report: &mut TransactionReport,
    base_metadata: Option<&Metadata>,
    checkpoint_catalog: &[(String, Option<Arc<Metadata>>, MetadataCandidateReport)],
    wanted: Option<&ReplayKey>,
) -> Result<ReplayScan> {
    // Full metadata ownership is bounded by the current member replay tip,
    // the best retained tip, and an optional explicitly requested state.
    // Historical record/candidate indexes contain only identities and votes.
    let expected_uuid = superblocks.first().map(|sb| sb.pool_uuid.to_string());
    let base_metadata = base_metadata.filter(|metadata| {
        expected_uuid
            .as_deref()
            .is_none_or(|uuid| metadata.uuid == uuid)
    });
    let base_snapshot = base_metadata.cloned().map(Arc::new);
    let present = superblocks
        .iter()
        .map(|sb| sb.disk_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut supported = BTreeMap::<ReplayKey, (usize, BTreeSet<String>)>::new();
    let mut certified = BTreeSet::<ReplayKey>::new();
    let mut best_tip: Option<(ReplayKey, Arc<Metadata>)> = None;
    let mut requested = None;
    let mut members = Vec::new();
    for sb in superblocks {
        let valid_before = report.valid_entries;
        let invalid_before = report.invalid_entries;
        let mut member = RawJournalMemberReport {
            disk_id: sb.disk_id.clone(),
            ..RawJournalMemberReport::default()
        };
        let mut latest = base_snapshot.clone();
        let mut member_candidates = BTreeMap::<ReplayKey, usize>::new();
        let mut record_candidates = BTreeMap::<String, ReplayProof>::new();
        let mut checkpoint_candidates = BTreeMap::<String, Arc<Metadata>>::new();
        for (_, candidate, _) in checkpoint_catalog
            .iter()
            .filter(|(disk_id, _, _)| disk_id == &sb.disk_id)
        {
            if let Some(candidate) = candidate {
                if expected_uuid
                    .as_deref()
                    .is_some_and(|uuid| candidate.uuid != uuid)
                {
                    continue;
                }
                checkpoint_candidates
                    .insert(metadata_hash_for_replay(candidate)?, candidate.clone());
            }
        }
        let mut header = vec![0u8; RAW_HEADER_SIZE];
        if let Err(err) = backend.read_at(&sb.disk_id, sb.journal.offset, &mut header) {
            member.error = Some(err.to_string());
            members.push(member);
            continue;
        }
        if &header[..16] != JOURNAL_MAGIC {
            member.error = Some("raw journal header magic mismatch".to_string());
            members.push(member);
            continue;
        }
        member.readable = true;
        let mut cursor = RAW_HEADER_SIZE as u64;
        let declared_end = get_u64(&header, 24)?;
        let end = declared_end.min(sb.journal.length);
        member.journal_end = end;
        while cursor + 36 <= end {
            let mut entry_header = [0u8; 36];
            if backend
                .read_at(&sb.disk_id, sb.journal.offset + cursor, &mut entry_header)
                .is_err()
            {
                report.invalid_entries += 1;
                break;
            }
            let len = u32::from_le_bytes(entry_header[..4].try_into().unwrap()) as usize;
            if len == 0 || cursor + 36 + len as u64 > end {
                report.invalid_entries += 1;
                break;
            }
            let mut bytes = vec![0u8; len];
            if let Err(err) =
                backend.read_at(&sb.disk_id, sb.journal.offset + cursor + 36, &mut bytes)
            {
                member.error = Some(err.to_string());
                report.invalid_entries += 1;
                report.errors.push(format!(
                    "raw journal payload at {}:{} could not be read: {err}",
                    sb.disk_id, cursor
                ));
                break;
            }
            if hex::encode(&entry_header[4..36]) != sha256_hex(&bytes) {
                report.invalid_entries += 1;
                break;
            }
            match serde_json::from_slice::<RawJournalRecord>(&bytes) {
                Ok(record) => {
                    if raw_record_hash(&record)? != record.record_hash {
                        report.invalid_entries += 1;
                        report.errors.push(format!(
                            "raw journal record at {}:{} has an invalid record hash",
                            sb.disk_id, cursor
                        ));
                        break;
                    }
                    // A reused or partially reformatted device can still carry
                    // a valid record from another pool. The record hash
                    // authenticates the UUID it claims, so compare that claim
                    // with the pool the superblocks belong to before replaying.
                    if expected_uuid
                        .as_deref()
                        .is_some_and(|uuid| record.volume_uuid != uuid)
                    {
                        report.invalid_entries += 1;
                        report.errors.push(format!(
                            "raw journal record at {}:{} belongs to pool {}, not {}",
                            sb.disk_id,
                            cursor,
                            record.volume_uuid,
                            expected_uuid.as_deref().unwrap_or("")
                        ));
                        break;
                    }
                    report.valid_entries += 1;
                    report.last_valid_txid = report.last_valid_txid.max(record.txid);
                    report.last_valid_generation =
                        report.last_valid_generation.max(record.generation);
                    member.last_valid_txid = record.txid;
                    member.last_valid_generation = record.generation;
                    member.last_valid_record_hash = record.record_hash.clone();
                    if record.action == RAW_QUORUM_COMMIT_ACTION {
                        if base_metadata.is_some_and(|base| record.txid <= base.txid) {
                            cursor += 36 + len as u64;
                            continue;
                        }
                        let target_record_hash = record
                            .details
                            .get("target_record_hash")
                            .and_then(serde_json::Value::as_str);
                        let checkpoint = record
                            .details
                            .get("checkpoint")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        let physical_checkpoint = checkpoint
                            .then(|| checkpoint_candidates.get(&record.meta_hash))
                            .flatten();
                        let proof = if checkpoint {
                            physical_checkpoint
                                .map(|candidate| {
                                    ReplayProof::from_metadata(candidate, &sb.disk_id, &present)
                                })
                                .transpose()?
                        } else {
                            target_record_hash
                                .and_then(|hash| record_candidates.get(hash))
                                .cloned()
                        };
                        let Some(proof) = proof else {
                            report.invalid_entries += 1;
                            report.errors.push(format!(
                                "raw quorum certificate at {}:{} has no local target state",
                                sb.disk_id, cursor
                            ));
                            break;
                        };
                        let claimed_need = record
                            .details
                            .get("quorum_need")
                            .and_then(serde_json::Value::as_u64)
                            .and_then(|value| usize::try_from(value).ok());
                        if proof.key.0 != record.txid
                            || proof.key.1 != record.generation
                            || proof.key.2 != record.meta_hash
                            || claimed_need != Some(proof.need)
                            || !proof.voting_member
                        {
                            report.invalid_entries += 1;
                            report.errors.push(format!(
                                "raw quorum certificate at {}:{} does not match its target state",
                                sb.disk_id, cursor
                            ));
                            break;
                        }
                        if proof.present_quorum {
                            certified.insert(proof.key.clone());
                        }
                        if let Some(candidate) = physical_checkpoint {
                            if wanted == Some(&proof.key) {
                                requested = Some(candidate.clone());
                            }
                            // A certified physical checkpoint may be newer than
                            // the selected quorum checkpoint after rollover.
                            if latest
                                .as_ref()
                                .is_none_or(|base| candidate.txid > base.txid)
                            {
                                latest = Some(candidate.clone());
                            }
                        }
                        cursor += 36 + len as u64;
                        continue;
                    }
                    let candidate = if let Some(metadata) = record.metadata {
                        if metadata.integrity.hash_scheme == Some(journal::DELTA_INTEGRITY_SCHEME) {
                            report.invalid_entries += 1;
                            report.errors.push(format!(
                                "raw journal {}:{} contains an unverified v2 full metadata snapshot",
                                sb.disk_id, cursor
                            ));
                            break;
                        }
                        let metadata_hash = journal::canonical_metadata_hash(&metadata)?;
                        if metadata_hash != record.meta_hash {
                            report.invalid_entries += 1;
                            report.errors.push(format!(
                                "raw journal metadata at {}:{} has a metadata hash mismatch",
                                sb.disk_id, cursor
                            ));
                            break;
                        }
                        if !metadata.integrity.meta_hash.is_empty()
                            && metadata.integrity.meta_hash != metadata_hash
                        {
                            report.invalid_entries += 1;
                            report.errors.push(format!(
                                "raw journal metadata at {}:{} has an integrity hash mismatch",
                                sb.disk_id, cursor
                            ));
                            break;
                        }
                        if metadata.txid != record.txid
                            || metadata.integrity.generation != record.generation
                        {
                            report.invalid_entries += 1;
                            report.errors.push(format!(
                                "raw journal metadata at {}:{} has inconsistent txid/generation",
                                sb.disk_id, cursor
                            ));
                            break;
                        }
                        Some(metadata)
                    } else if let Some(delta) = record.metadata_delta.as_ref() {
                        let Some(base) = latest.as_ref() else {
                            report.invalid_entries += 1;
                            report.errors.push(format!(
                                "raw journal record at {}:{} lacks a metadata checkpoint base",
                                sb.disk_id, cursor
                            ));
                            break;
                        };
                        if record.txid <= base.txid {
                            cursor += 36 + len as u64;
                            continue;
                        }
                        let base_hash = metadata_hash_for_replay(base)?;
                        if let Some(previous_hash) = raw_record_previous_meta_hash(&record) {
                            if previous_hash != base_hash {
                                report.invalid_entries += 1;
                                report.errors.push(format!(
                                    "raw journal delta at {}:{} does not chain from selected metadata: previous_meta_hash={} selected_meta_hash={}",
                                    sb.disk_id, cursor, previous_hash, base_hash
                                ));
                                break;
                            }
                        }
                        match journal::apply_metadata_delta(base, delta) {
                            Ok(metadata) => {
                                let metadata_hash = if metadata.integrity.hash_scheme
                                    == Some(journal::DELTA_INTEGRITY_SCHEME)
                                {
                                    metadata.integrity.meta_hash.clone()
                                } else {
                                    journal::canonical_metadata_hash(&metadata)?
                                };
                                if metadata_hash != record.meta_hash {
                                    report.invalid_entries += 1;
                                    report.errors.push(format!(
                                        "raw journal delta at {}:{} has a metadata hash mismatch",
                                        sb.disk_id, cursor
                                    ));
                                    break;
                                }
                                if metadata.txid != record.txid
                                    || metadata.integrity.generation != record.generation
                                {
                                    report.invalid_entries += 1;
                                    report.errors.push(format!(
                                        "raw journal delta at {}:{} has inconsistent txid/generation",
                                        sb.disk_id, cursor
                                    ));
                                    break;
                                }
                                Some(metadata)
                            }
                            Err(err) => {
                                report.invalid_entries += 1;
                                report.errors.push(format!(
                                    "raw journal delta at {}:{} is invalid: {err}",
                                    sb.disk_id, cursor
                                ));
                                break;
                            }
                        }
                    } else {
                        report.invalid_entries += 1;
                        report.errors.push(format!(
                            "raw journal record at {}:{} lacks a metadata checkpoint base",
                            sb.disk_id, cursor
                        ));
                        break;
                    };
                    let Some(candidate) = candidate else {
                        break;
                    };
                    // Validate both full snapshots and legacy delta results.
                    // A matching record UUID is not enough when a delta can
                    // modify the embedded metadata identity.
                    if expected_uuid
                        .as_deref()
                        .is_some_and(|uuid| candidate.uuid != uuid)
                    {
                        report.invalid_entries += 1;
                        report.errors.push(format!(
                            "raw journal metadata at {}:{} belongs to pool {}, not {}",
                            sb.disk_id,
                            cursor,
                            candidate.uuid,
                            expected_uuid.as_deref().unwrap_or("")
                        ));
                        break;
                    }
                    let candidate = Arc::new(candidate);
                    let proof = ReplayProof::from_metadata(&candidate, &sb.disk_id, &present)?;
                    if wanted == Some(&proof.key) {
                        requested = Some(candidate.clone());
                    }
                    record_candidates.insert(record.record_hash.clone(), proof.clone());
                    if latest
                        .as_ref()
                        .is_some_and(|metadata| candidate.txid <= metadata.txid)
                    {
                        cursor += 36 + len as u64;
                        continue;
                    }
                    if proof.voting_member {
                        member_candidates.insert(proof.key, proof.need);
                    }
                    latest = Some(candidate);
                }
                Err(err) => {
                    report.invalid_entries += 1;
                    report.errors.push(format!(
                        "raw journal record at {}:{} is invalid: {err}",
                        sb.disk_id, cursor
                    ));
                    break;
                }
            }
            cursor += 36 + len as u64;
        }
        // A short trailing fragment (or out-of-range end pointer) must
        // not be accepted as an appendable, fully verified replay chain.
        if report.invalid_entries == invalid_before && (declared_end != end || cursor != end) {
            report.invalid_entries += 1;
            member.error = Some("raw journal ends outside a complete record".into());
        }
        member.valid_entries = report.valid_entries.saturating_sub(valid_before);
        member.invalid_entries = report.invalid_entries.saturating_sub(invalid_before);
        if member.invalid_entries == 0 {
            for (key, need) in member_candidates {
                supported
                    .entry(key)
                    .or_insert_with(|| (need, BTreeSet::new()))
                    .1
                    .insert(sb.disk_id.clone());
            }
        }
        if let Some(metadata) = latest {
            let key = (
                metadata.txid,
                metadata.integrity.generation,
                metadata_hash_for_replay(&metadata)?,
            );
            if best_tip.as_ref().is_none_or(|(old, _)| key > *old) {
                best_tip = Some((key, metadata));
            }
        }
        members.push(member);
    }
    let best_supported = supported
        .into_iter()
        .filter(|(_, (need, ids))| ids.len() >= *need)
        .max_by_key(|((txid, generation, _), _)| (*txid, *generation))
        .map(|(key, _)| key);
    let best_certified = certified
        .into_iter()
        .max_by_key(|(txid, generation, _)| (*txid, *generation));
    let best = match (best_supported, best_certified) {
        (Some(left), Some(right)) => {
            if (right.0, right.1) > (left.0, left.1) {
                Some(right)
            } else {
                Some(left)
            }
        }
        (Some(key), None) | (None, Some(key)) => Some(key),
        (None, None) => None,
    };
    let total_members = base_metadata
        .map(|metadata| {
            metadata
                .disks
                .values()
                .filter(|disk| disk.status != DiskStatus::Removed)
                .count()
        })
        .unwrap_or(superblocks.len());
    report.raw_journal_quorum = Some(raw_journal_quorum(&members, total_members) || best.is_some());
    report.raw_journal_members = members;
    let retained = requested.or_else(|| {
        best_tip
            .filter(|(key, _)| Some(key) == best.as_ref())
            .map(|(_, metadata)| metadata)
    });
    Ok(ReplayScan {
        winner: best,
        retained,
    })
}

pub(super) fn raw_journal_quorum(members: &[RawJournalMemberReport], total_members: usize) -> bool {
    if total_members == 0 {
        return true;
    }
    let required = total_members / 2 + 1;
    let mut counts: BTreeMap<(u64, u64, String), usize> = BTreeMap::new();
    for member in members {
        if !member.readable || member.invalid_entries > 0 {
            continue;
        }
        let key = (
            member.last_valid_txid,
            member.last_valid_generation,
            member.last_valid_record_hash.clone(),
        );
        let count = counts.entry(key).or_default();
        *count += 1;
        if *count >= required {
            return true;
        }
    }
    false
}

pub(super) fn raw_record_previous_meta_hash(record: &RawJournalRecord) -> Option<&str> {
    record
        .details
        .get("previous_meta_hash")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
}

pub(super) fn metadata_hash_for_replay(metadata: &Metadata) -> Result<String> {
    if metadata.integrity.meta_hash.is_empty() {
        journal::canonical_metadata_hash(metadata)
    } else {
        Ok(metadata.integrity.meta_hash.clone())
    }
}
