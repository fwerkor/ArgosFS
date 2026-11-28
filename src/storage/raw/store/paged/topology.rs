//! A configuration change stages a separate tree, fences a majority of
//! the previous configuration, then certifies the new configuration on
//! both majorities. Slots alternate by epoch. A newer prepare is never
//! silently ignored in favor of an older configuration.
use super::*;

const SLOT_BYTES: u64 = 2 * 1024 * 1024;
const CERT_MAGIC: &[u8; 16] = b"ARGOSFS-PG-CERT\0";
const PREP_MAGIC: &[u8; 16] = b"ARGOSFS-PG-PREP\0";
const PAGE: u64 = RAW_HEADER_SIZE as u64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Transition {
    old: Manifest,
    new: Manifest,
    old_hash: String,
    old_txid: u64,
    new_hash: String,
    new_txid: u64,
}

fn member_ids(manifest: &Manifest) -> BTreeSet<String> {
    manifest
        .members
        .iter()
        .map(|m| m.arena.disk_id.clone())
        .collect()
}

fn has_member(manifest: &Manifest, sb: &RawSuperblock) -> bool {
    manifest
        .members
        .iter()
        .any(|m| m.arena.disk_id == sb.disk_id && m.device_uuid == sb.device_uuid)
}

fn matches(transition: &Transition, sb: &RawSuperblock) -> bool {
    transition.new.pool == sb.pool_uuid
        && (has_member(&transition.old, sb) || has_member(&transition.new, sb))
}

fn quorum(manifest: &Manifest, voters: &BTreeSet<String>) -> bool {
    manifest
        .members
        .iter()
        .filter(|m| voters.contains(&m.arena.disk_id))
        .count()
        > manifest.members.len() / 2
}

fn slot_offset(sb: &RawSuperblock, epoch: u64) -> Result<u64> {
    if sb.allocator.length < SLOT_BYTES * 2 {
        return Err(ArgosError::IncompatibleFormat(
            "paged topology requires two manifest slots".into(),
        ));
    }
    Ok(sb.allocator.offset + (epoch % 2) * SLOT_BYTES)
}

pub(crate) fn clear_transition_slots(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
) -> Result<()> {
    for epoch in 0..2 {
        let offset = slot_offset(sb, epoch)?;
        backend.write_at(
            &sb.disk_id,
            offset + SLOT_BYTES - PAGE,
            &[0; RAW_HEADER_SIZE],
        )?;
        backend.write_at(&sb.disk_id, offset, &[0; RAW_HEADER_SIZE])?;
    }
    Ok(())
}

fn descriptor(transition: &Transition) -> Result<(Vec<u8>, [u8; 32])> {
    let bytes = serde_json::to_vec(transition)?;
    if bytes.len() > MAX_MANIFEST || bytes.len() as u64 + PAGE * 2 > SLOT_BYTES {
        return Err(ArgosError::Invalid(
            "paged topology descriptor too large".into(),
        ));
    }
    let digest = Sha256::digest(&bytes).into();
    Ok((bytes, digest))
}

fn certificate(epoch: u64, digest: [u8; 32], device: Uuid) -> [u8; RAW_HEADER_SIZE] {
    let mut page = [0; RAW_HEADER_SIZE];
    page[..16].copy_from_slice(CERT_MAGIC);
    put_u64(&mut page, 16, epoch);
    page[24..56].copy_from_slice(&digest);
    page[56..72].copy_from_slice(device.as_bytes());
    let checksum = Sha256::digest(&page[..RAW_HEADER_SIZE - 32]);
    page[RAW_HEADER_SIZE - 32..].copy_from_slice(&checksum);
    page
}

fn read_transition(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    slot: u64,
) -> Result<Option<(Transition, bool)>> {
    let offset = slot_offset(sb, slot)?;
    let mut header = [0; RAW_HEADER_SIZE];
    let mut cert = [0; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, offset, &mut header)?;
    backend.read_at(&sb.disk_id, offset + SLOT_BYTES - PAGE, &mut cert)?;
    let valid_cert = &cert[..16] == CERT_MAGIC
        && &cert[56..72] == sb.device_uuid.as_bytes()
        && Sha256::digest(&cert[..RAW_HEADER_SIZE - 32]).as_slice()
            == &cert[RAW_HEADER_SIZE - 32..];
    if header.iter().all(|b| *b == 0) || &header[..16] == MANIFEST_MAGIC {
        if valid_cert {
            return Err(ArgosError::CorruptedMetadata(
                "configuration certificate without descriptor".into(),
            ));
        }
        return Ok(None);
    }
    if &header[..16] != PREP_MAGIC {
        return Err(ArgosError::CorruptedMetadata(
            "torn paged topology descriptor".into(),
        ));
    }
    if &header[64..80] != sb.device_uuid.as_bytes()
        || Sha256::digest(&header[..RAW_HEADER_SIZE - 32]).as_slice()
            != &header[RAW_HEADER_SIZE - 32..]
    {
        return Err(ArgosError::Checksum(
            "paged topology descriptor header".into(),
        ));
    }
    let epoch = get_u64(&header, 16)?;
    let len = get_u64(&header, 24)?;
    if epoch == 0 || epoch % 2 != slot || len == 0 || len > MAX_MANIFEST as u64 {
        return Err(ArgosError::CorruptedMetadata(
            "invalid paged topology descriptor length or epoch".into(),
        ));
    }
    let mut bytes = vec![0; len as usize];
    backend.read_at(&sb.disk_id, offset + PAGE, &mut bytes)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    if digest != header[32..64] {
        return Err(ArgosError::Checksum("paged topology descriptor".into()));
    }
    let transition: Transition = serde_json::from_slice(&bytes)?;
    if transition.new.epoch != epoch
        || transition.old.epoch.checked_add(1) != Some(epoch)
        || transition.old.pool != transition.new.pool
        || !matches(&transition, sb)
        || transition.old_txid.checked_add(1) != Some(transition.new_txid)
    {
        return Err(ArgosError::CorruptedMetadata(
            "invalid paged topology transition".into(),
        ));
    }
    transition.old.validate(&[])?;
    transition.new.validate(&[])?;
    let certified = valid_cert && cert == certificate(epoch, digest, sb.device_uuid);
    if valid_cert && !certified {
        return Err(ArgosError::CorruptedMetadata(
            "configuration certificate disagrees with descriptor".into(),
        ));
    }
    Ok(Some((transition, certified)))
}

fn known_certificate_epoch(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    slot: u64,
) -> Result<Option<u64>> {
    let offset = slot_offset(sb, slot)?;
    let mut cert = [0; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, offset + SLOT_BYTES - PAGE, &mut cert)?;
    if &cert[..16] == CERT_MAGIC
        && &cert[56..72] == sb.device_uuid.as_bytes()
        && Sha256::digest(&cert[..RAW_HEADER_SIZE - 32]).as_slice() == &cert[RAW_HEADER_SIZE - 32..]
    {
        return Ok(Some(get_u64(&cert, 16)?));
    }
    Ok(None)
}

fn write_prepare(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    transition: &Transition,
) -> Result<()> {
    let (bytes, digest) = descriptor(transition)?;
    let offset = slot_offset(sb, transition.new.epoch)?;
    // Clear the old certificate before reusing this slot. The current
    // configuration remains intact in the other slot.
    backend.write_at(
        &sb.disk_id,
        offset + SLOT_BYTES - PAGE,
        &[0; RAW_HEADER_SIZE],
    )?;
    backend.flush_device(&sb.disk_id)?;
    backend.write_at(&sb.disk_id, offset + PAGE, &bytes)?;
    let mut header = [0; RAW_HEADER_SIZE];
    header[..16].copy_from_slice(PREP_MAGIC);
    put_u64(&mut header, 16, transition.new.epoch);
    put_u64(&mut header, 24, bytes.len() as u64);
    header[32..64].copy_from_slice(&digest);
    header[64..80].copy_from_slice(sb.device_uuid.as_bytes());
    let checksum = Sha256::digest(&header[..RAW_HEADER_SIZE - 32]);
    header[RAW_HEADER_SIZE - 32..].copy_from_slice(&checksum);
    backend.flush_device(&sb.disk_id)?;
    backend.write_at(&sb.disk_id, offset, &header)?;
    backend.flush_device(&sb.disk_id)?;
    if !read_transition(backend, sb, transition.new.epoch % 2)?
        .is_some_and(|(actual, cert)| actual == *transition && !cert)
    {
        return Err(ArgosError::Checksum(format!(
            "configuration prepare readback mismatch on {}",
            sb.disk_id
        )));
    }
    Ok(())
}

fn write_certificate(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    transition: &Transition,
) -> Result<()> {
    let (_, digest) = descriptor(transition)?;
    let offset = slot_offset(sb, transition.new.epoch)?;
    backend.write_at(
        &sb.disk_id,
        offset + SLOT_BYTES - PAGE,
        &certificate(transition.new.epoch, digest, sb.device_uuid),
    )?;
    backend.flush_device(&sb.disk_id)?;
    if !read_transition(backend, sb, transition.new.epoch % 2)?
        .is_some_and(|(actual, cert)| actual == *transition && cert)
    {
        return Err(ArgosError::Checksum(format!(
            "configuration certificate readback mismatch on {}",
            sb.disk_id
        )));
    }
    Ok(())
}

fn check_members(manifest: &Manifest, superblocks: &[RawSuperblock]) -> Result<()> {
    let present = superblocks
        .iter()
        .filter(|sb| has_member(manifest, sb))
        .cloned()
        .collect::<Vec<_>>();
    manifest.validate(&present)?;
    if present.len() <= manifest.members.len() / 2 {
        return Err(quorum_unavailable(
            "paged membership",
            manifest.members.len() / 2 + 1,
            present.len(),
        ));
    }
    // A disk ID must not be silently rebound to a different device UUID.
    if superblocks.iter().any(|sb| {
        manifest
            .members
            .iter()
            .any(|m| m.arena.disk_id == sb.disk_id && m.device_uuid != sb.device_uuid)
    }) {
        return Err(ArgosError::Conflict(
            "paged member device UUID changed".into(),
        ));
    }
    Ok(())
}

fn certify_transition(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    transition: &Transition,
    prepared: &BTreeSet<String>,
) -> Result<()> {
    if !quorum(&transition.old, prepared) || !quorum(&transition.new, prepared) {
        return Err(ArgosError::IndeterminateCommit(
            "paged configuration lacks joint prepare quorum".into(),
        ));
    }
    let mut certified = BTreeSet::new();
    for sb in superblocks
        .iter()
        .filter(|sb| prepared.contains(&sb.disk_id))
    {
        if write_certificate(backend, sb, transition).is_ok() {
            certified.insert(sb.disk_id.clone());
        }
    }
    if !quorum(&transition.old, &certified) || !quorum(&transition.new, &certified) {
        return Err(ArgosError::IndeterminateCommit(
            "paged configuration certificate lacks joint quorum".into(),
        ));
    }
    Ok(())
}

fn recover_configuration(
    backend: &dyn StorageBackend,
    manifest: &Manifest,
    v3: bool,
) -> Result<Metadata> {
    if v3 {
        ImageStore::recover_v3(backend, manifest.pool, manifest.arenas()).map(|(meta, _)| meta)
    } else {
        ImageStore::recover(backend, manifest.pool, manifest.arenas())
    }
}

pub(super) fn select_manifest(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    write: bool,
) -> Result<Manifest> {
    let v3 = uses_v3(superblocks)?;
    if !superblocks
        .iter()
        .any(|sb| sb.required_feature_flags & ACTIVE != 0)
    {
        return Err(ArgosError::IncompatibleFormat("paged metadata upgrade interrupted; rerun upgrade-paged-metadata with all members offline".into()));
    }
    let mut latest: Option<Transition> = None;
    let mut prepared = BTreeSet::new();
    let mut certified = BTreeSet::new();
    let mut initial = Vec::new();
    let mut unreadable = BTreeSet::new();
    let mut certificate_epoch = 0;
    for sb in superblocks {
        let mut valid = true;
        for slot in 0..2 {
            match read_transition(backend, sb, slot) {
                Ok(Some((transition, cert))) => {
                    if latest
                        .as_ref()
                        .is_none_or(|old| old.new.epoch < transition.new.epoch)
                    {
                        latest = Some(transition.clone());
                        prepared.clear();
                        certified.clear();
                    }
                    if let Some(old) = &latest {
                        if old.new.epoch == transition.new.epoch {
                            if old != &transition {
                                return Err(ArgosError::Conflict(
                                    "conflicting paged configuration descriptors at same epoch"
                                        .into(),
                                ));
                            }
                            prepared.insert(sb.disk_id.clone());
                            if cert {
                                certified.insert(sb.disk_id.clone());
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    valid = false;
                    if let Ok(Some(epoch)) = known_certificate_epoch(backend, sb, slot) {
                        certificate_epoch = certificate_epoch.max(epoch);
                    }
                }
            }
        }
        if valid {
            if let Ok(manifest) = read_manifest(backend, sb) {
                initial.push((sb, manifest));
            }
        } else {
            unreadable.insert(sb.disk_id.clone());
        }
    }
    if certificate_epoch > latest.as_ref().map_or(0, |t| t.new.epoch) {
        return Err(ArgosError::CorruptedMetadata(
            "latest paged configuration certificate has no valid descriptor; refusing rollback"
                .into(),
        ));
    }
    if let Some(transition) = latest {
        // A damaged higher descriptor cannot silently be counted as an old
        // configuration voter. Require a readable configuration majority.
        let readable = superblocks
            .iter()
            .filter(|sb| !unreadable.contains(&sb.disk_id))
            .map(|sb| sb.disk_id.clone())
            .collect::<BTreeSet<_>>();
        check_members(&transition.new, superblocks)?;
        let all_participants = member_ids(&transition.old)
            .union(&member_ids(&transition.new))
            .cloned()
            .collect::<BTreeSet<_>>();
        let supplied = superblocks
            .iter()
            .map(|sb| sb.disk_id.clone())
            .collect::<BTreeSet<_>>();
        let full_recovery = write && certified.is_empty() && all_participants.is_subset(&supplied);
        if !full_recovery && !quorum(&transition.new, &readable) {
            return Err(ArgosError::QuorumUnavailable {
                operation: "readable paged configuration".into(),
                need: transition.new.members.len() / 2 + 1,
                have: transition
                    .new
                    .members
                    .iter()
                    .filter(|m| readable.contains(&m.arena.disk_id))
                    .count(),
            });
        }
        if certified.is_empty() {
            // No certificate is visible. Never roll back: either both
            // prepares are known or recover the interrupted prepare with
            // every old/new member present and an unchanged old root.
            let joint = quorum(&transition.old, &prepared) && quorum(&transition.new, &prepared);
            if !joint {
                if !full_recovery {
                    return Err(ArgosError::IndeterminateCommit(
                        "incomplete paged configuration; reopen writable with every old/new member"
                            .into(),
                    ));
                }
                let old = recover_configuration(backend, &transition.old, v3)?;
                if old.txid != transition.old_txid || old.integrity.meta_hash != transition.old_hash
                {
                    return Err(ArgosError::Conflict("old configuration advanced during interrupted prepare; offline recovery required".into()));
                }
            }
            let new = recover_configuration(backend, &transition.new, v3)?;
            if new.txid != transition.new_txid || new.integrity.meta_hash != transition.new_hash {
                return Err(ArgosError::CorruptedMetadata(
                    "staged paged configuration root mismatch".into(),
                ));
            }
            if write {
                if !joint {
                    for sb in superblocks.iter().filter(|sb| matches(&transition, sb)) {
                        write_prepare(backend, sb, &transition)?;
                        prepared.insert(sb.disk_id.clone());
                    }
                }
                certify_transition(backend, superblocks, &transition, &prepared)?;
            }
        } else if write
            && (!quorum(&transition.old, &certified) || !quorum(&transition.new, &certified))
        {
            // A visible certificate proves the original joint quorum. The
            // previous membership need not still be accessible; restore a
            // certificate majority in the selected new configuration before
            // allowing further writes.
            let mut new_certified = BTreeSet::new();
            for sb in superblocks
                .iter()
                .filter(|sb| has_member(&transition.new, sb))
            {
                if !prepared.contains(&sb.disk_id)
                    && write_prepare(backend, sb, &transition).is_err()
                {
                    continue;
                }
                if write_certificate(backend, sb, &transition).is_ok() {
                    new_certified.insert(sb.disk_id.clone());
                }
            }
            if !quorum(&transition.new, &new_certified) {
                return Err(ArgosError::IndeterminateCommit(
                    "cannot restore paged configuration certificate quorum".into(),
                ));
            }
        }
        return Ok(transition.new);
    }
    let mut chosen = None;
    let mut voters = BTreeSet::new();
    for (sb, manifest) in initial {
        if chosen.as_ref().is_some_and(|old| old != &manifest) {
            return Err(ArgosError::Conflict(
                "divergent initial paged membership".into(),
            ));
        }
        chosen = Some(manifest);
        voters.insert(sb.disk_id.clone());
    }
    let manifest = chosen.ok_or_else(|| {
        ArgosError::CorruptedMetadata("no valid paged membership manifest".into())
    })?;
    check_members(&manifest, superblocks)?;
    if !quorum(&manifest, &voters) {
        return Err(quorum_unavailable(
            "paged membership manifest",
            manifest.members.len() / 2 + 1,
            voters.len(),
        ));
    }
    Ok(manifest)
}

/// Construct a new configuration without overwriting any current tree.
/// All new members must be available for staging; removing an unavailable
/// member is supported provided the old configuration still has a majority.
pub(crate) fn reconfigure(
    backend: Arc<dyn StorageBackend>,
    superblocks: &[RawSuperblock],
    base: &Metadata,
    next: &mut Metadata,
    events: &[crate::types::PagedAuditEvent],
) -> Result<ImageStore> {
    reconfigure_with_growth(backend, superblocks, base, next, events, None)
}

pub(crate) fn reconfigure_with_growth(
    backend: Arc<dyn StorageBackend>,
    superblocks: &[RawSuperblock],
    base: &Metadata,
    next: &mut Metadata,
    events: &[crate::types::PagedAuditEvent],
    requested_arena: Option<u64>,
) -> Result<ImageStore> {
    if base.raw_pool.snapshot_state != next.raw_pool.snapshot_state {
        return Err(ArgosError::Conflict(
            "topology cannot change snapshot ownership".into(),
        ));
    }
    let v3 = uses_v3(superblocks)?;
    if v3 != (base.integrity.hash_scheme == Some(V3_INTEGRITY_SCHEME)) {
        return Err(ArgosError::IncompatibleFormat(
            "topology transition metadata format mismatch".into(),
        ));
    }
    if superblocks
        .iter()
        .any(|sb| sb.required_feature_flags & TOPOLOGY == 0)
    {
        return Err(ArgosError::IncompatibleFormat(
            "paged topology feature is missing; run offline upgrade with all members".into(),
        ));
    }
    let old = select_manifest(&*backend, superblocks, false)?;
    let recovered = recover_configuration(&*backend, &old, v3)?;
    if recovered.txid != base.txid || recovered.integrity.meta_hash != base.integrity.meta_hash {
        return Err(ArgosError::IndeterminateCommit(
            "paged configuration changed since mounted base".into(),
        ));
    }
    let wanted = next
        .disks
        .values()
        .filter(|disk| disk.status != DiskStatus::Removed)
        .map(|disk| disk.id.clone())
        .collect::<BTreeSet<_>>();
    if let Some(state) = &next.raw_pool.snapshot_state {
        if state.pins.keys().any(|id| !wanted.contains(id)) {
            return Err(ArgosError::Unsupported(
                "historical data placement relocation is required before member removal".into(),
            ));
        }
    }
    let previous_pair = old
        .members
        .iter()
        .filter_map(|member| member.alternate_arena.as_ref().map(|arena| arena.length))
        .max();
    if let Some(size) = requested_arena {
        if !v3
            || !size.is_multiple_of(RAW_HEADER_SIZE as u64)
            || size < 8 * RAW_HEADER_SIZE as u64
            || old.members.iter().any(|member| size <= member.arena.length)
            || wanted != member_ids(&old)
            || wanted
                .iter()
                .any(|id| !superblocks.iter().any(|sb| &sb.disk_id == id))
        {
            return Err(ArgosError::Invalid("V3 metadata growth requires a larger aligned arena and all unchanged active members".into()));
        }
    }
    let pair_size = requested_arena.or(previous_pair);
    let mut new = Manifest {
        pool: old.pool,
        epoch: old
            .epoch
            .checked_add(1)
            .ok_or_else(|| ArgosError::Invalid("configuration epoch exhausted".into()))?,
        members: Vec::new(),
    };
    for id in &wanted {
        let sb = superblocks.iter().find(|sb| &sb.disk_id == id);
        let alloc = next.raw_pool.allocators.get_mut(id).ok_or_else(|| {
            ArgosError::CorruptedMetadata("missing data allocator for new configuration".into())
        })?;
        let prior = old.members.iter().find(|m| &m.arena.disk_id == id);
        let Some(sb) = sb else {
            // The absent old member keeps its arena and allocation geometry.
            // It cannot vote until the certified new configuration is
            // selected and its old namespace-bound roots are repaired.
            let prior = prior.ok_or_else(|| ArgosError::MissingDevice(id.clone()))?;
            if alloc.data_start != prior.data_offset
                || alloc.data_end
                    != prior
                        .data_offset
                        .checked_add(prior.data_length)
                        .ok_or_else(|| {
                            ArgosError::CorruptedMetadata("missing member geometry overflow".into())
                        })?
            {
                return Err(ArgosError::CorruptedMetadata(
                    "missing member allocation geometry changed".into(),
                ));
            }
            new.members.push(prior.clone());
            continue;
        };
        if alloc.data_start != sb.data.offset
            || alloc.data_end < alloc.data_start
            || alloc.next_offset < alloc.data_start
            || alloc.next_offset > alloc.data_end
        {
            return Err(ArgosError::CorruptedMetadata(
                "invalid data allocator during reconfiguration".into(),
            ));
        }
        let (arena, alternate_arena) = if let Some(size) = pair_size {
            if let Some(prior) =
                prior.filter(|p| p.alternate_arena.as_ref().is_some_and(|a| a.length >= size))
            {
                (
                    prior.alternate_arena.clone().unwrap(),
                    Some(prior.arena.clone()),
                )
            } else {
                let reserve = size
                    .checked_mul(2)
                    .ok_or_else(|| ArgosError::Invalid("V3 metadata reserve overflow".into()))?;
                let end = alloc.data_end;
                let start = end
                    .checked_sub(reserve)
                    .ok_or_else(|| ArgosError::DiskFull {
                        disk_id: id.clone(),
                        required: reserve,
                        available: end.saturating_sub(alloc.next_offset),
                    })?;
                if start <= alloc.data_start
                    || alloc.next_offset > start
                    || alloc
                        .free_extents
                        .iter()
                        .any(|e| e.offset.checked_add(e.length).is_none_or(|end| end > start))
                {
                    return Err(ArgosError::DiskFull {
                        disk_id: id.clone(),
                        required: reserve,
                        available: end.saturating_sub(alloc.next_offset),
                    });
                }
                alloc.data_end = start;
                (
                    Arena {
                        disk_id: id.clone(),
                        offset: start,
                        length: size,
                    },
                    Some(Arena {
                        disk_id: id.clone(),
                        offset: start + size,
                        length: size,
                    }),
                )
            }
        } else {
            let arena = if let Some(prior) = prior {
                if prior.arena.offset != sb.metadata.offset {
                    // Legacy metadata area or previous alternate arena is free
                    // once the newer configuration is authoritative.
                    Arena {
                        disk_id: id.clone(),
                        offset: sb.metadata.offset,
                        length: sb.metadata.length,
                    }
                } else {
                    // Reuse the reserved tail on later switches. On the first
                    // switch reserve space never reached by the data allocator.
                    let end = alloc.data_end;
                    let prior_tail = sb.backup_superblock_offset.saturating_sub(end);
                    let length = if prior_tail >= sb.metadata.length {
                        prior_tail
                    } else {
                        sb.metadata.length
                    };
                    let start = if prior_tail >= sb.metadata.length {
                        end
                    } else {
                        end.checked_sub(length).ok_or_else(|| {
                            ArgosError::Invalid("no space for alternate metadata arena".into())
                        })?
                    };
                    if start <= alloc.data_start
                        || alloc.next_offset > start
                        || alloc
                            .free_extents
                            .iter()
                            .any(|e| e.offset.checked_add(e.length).is_none_or(|end| end > start))
                    {
                        return Err(ArgosError::DiskFull {
                            disk_id: id.clone(),
                            required: length,
                            available: end.saturating_sub(alloc.next_offset),
                        });
                    }
                    alloc.data_end = start;
                    Arena {
                        disk_id: id.clone(),
                        offset: start,
                        length,
                    }
                }
            } else {
                Arena {
                    disk_id: id.clone(),
                    offset: sb.metadata.offset,
                    length: sb.metadata.length,
                }
            };
            (arena, None)
        };
        new.members.push(ManifestMember {
            device_uuid: sb.device_uuid,
            arena,
            alternate_arena,
            data_offset: alloc.data_start,
            data_length: alloc.data_end - alloc.data_start,
        });
    }
    check_members(&new, superblocks)?;
    // Validate actual referenced extents independently of high water marks.
    for inode in next.inodes.values() {
        for block in &inode.blocks {
            for shard in &block.shards {
                if let Some(crate::types::ShardLocation::RawExtent(extent)) = &shard.location {
                    if !wanted.contains(&extent.disk_id) {
                        return Err(ArgosError::Conflict(
                            "removed member still holds a live shard".into(),
                        ));
                    }
                    let alloc = &next.raw_pool.allocators[&extent.disk_id];
                    if extent.offset < alloc.data_start
                        || extent
                            .offset
                            .checked_add(extent.length)
                            .is_none_or(|end| end > alloc.data_end)
                    {
                        return Err(ArgosError::CorruptedMetadata(
                            "live shard overlaps alternate metadata arena".into(),
                        ));
                    }
                }
            }
        }
    }
    next.txid = base
        .txid
        .checked_add(1)
        .ok_or_else(|| ArgosError::Invalid("transaction ID exhausted".into()))?;
    next.updated_at = now_f64();
    next.integrity.generation = next.txid;
    next.integrity.previous_meta_hash = base.integrity.meta_hash.clone();
    next.integrity.hash_scheme = base.integrity.hash_scheme;
    next.integrity.meta_hash.clear();
    validate_image(next, &new)?;
    // No old root or manifest is overwritten during staging.
    let selected = superblocks
        .iter()
        .filter(|sb| has_member(&new, sb))
        .map(|sb| sb.disk_id.clone())
        .collect::<BTreeSet<_>>();
    for member in new
        .members
        .iter()
        .filter(|m| selected.contains(&m.arena.disk_id))
    {
        if old.members.iter().any(|prior| {
            let a = &member.arena;
            let b = &prior.arena;
            a.disk_id == b.disk_id
                && a.offset < b.offset.saturating_add(b.length)
                && b.offset < a.offset.saturating_add(a.length)
        }) {
            return Err(ArgosError::Conflict(
                "staged metadata arena overlaps current authority".into(),
            ));
        }
    }
    for member in new
        .members
        .iter()
        .filter(|m| selected.contains(&m.arena.disk_id))
    {
        backend.write_at(
            &member.arena.disk_id,
            member.arena.offset,
            &[0; RAW_HEADER_SIZE * 4],
        )?;
        backend.flush_device(&member.arena.disk_id)?;
    }
    let mut events = events.to_vec();
    let sequence = events
        .last()
        .map_or(Some(1), |event| event.sequence.checked_add(1))
        .ok_or_else(|| ArgosError::Invalid("paged event sequence exhausted".into()))?;
    events.push(crate::types::PagedAuditEvent {
        sequence,txid:next.txid,time:now_f64(),action:if requested_arena.is_some(){"grow-metadata"}else{"reconfigure"}.into(),
        details:serde_json::json!({"epoch":new.epoch,"old_members":member_ids(&old),"new_members":member_ids(&new)}),
        advisory:false,
    });
    let mut store = if v3 && base.raw_pool.snapshot_state.is_some() {
        ImageStore::transfer_v3(
            backend.clone(),
            old.arenas(),
            new.arenas(),
            base,
            next,
            &events,
            &selected,
        )?
    } else {
        ImageStore::format_with_events(
            backend.clone(),
            new.arenas(),
            next,
            &events,
            Some(&selected),
        )?
    };
    store.confirm_all(selected.len())?;
    let transition = Transition {
        old,
        new,
        old_hash: base.integrity.meta_hash.clone(),
        old_txid: base.txid,
        new_hash: next.integrity.meta_hash.clone(),
        new_txid: next.txid,
    };
    // From the first attempted prepare onward the outcome is potentially
    // visible to recovery. The caller must fence on any error.
    let mut prepared = BTreeSet::new();
    for sb in superblocks.iter().filter(|sb| matches(&transition, sb)) {
        if write_prepare(&*backend, sb, &transition).is_ok() {
            prepared.insert(sb.disk_id.clone());
        }
    }
    certify_transition(&*backend, superblocks, &transition, &prepared)?;
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendCapabilities, BackendDeviceInfo, DeviceId};
    use crate::types::VolumeConfig;

    struct Interrupt {
        inner: Arc<dyn StorageBackend>,
        mode: u8,
    }

    impl StorageBackend for Interrupt {
        fn backend_kind(&self) -> BackendKind {
            self.inner.backend_kind()
        }
        fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
            self.inner.list_devices()
        }
        fn read_at(&self, id: &DeviceId, offset: u64, data: &mut [u8]) -> Result<()> {
            self.inner.read_at(id, offset, data)
        }
        fn write_at(&self, id: &DeviceId, offset: u64, data: &[u8]) -> Result<()> {
            let fail = (self.mode == 0 && data.starts_with(PREP_MAGIC) && id != "disk-0000")
                || (self.mode == 1 && data.starts_with(CERT_MAGIC))
                || (self.mode == 2 && data.starts_with(CERT_MAGIC) && id != "disk-0000");
            if fail {
                if self.mode == 0 {
                    self.inner.write_at(id, offset, &data[..data.len() / 2])?;
                    self.inner.flush_device(id)?;
                }
                return Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
            }
            self.inner.write_at(id, offset, data)
        }
        fn flush_device(&self, id: &DeviceId) -> Result<()> {
            self.inner.flush_device(id)
        }
        fn flush_all(&self) -> Result<()> {
            self.inner.flush_all()
        }
        fn capacity(&self, id: &DeviceId) -> Result<u64> {
            self.inner.capacity(id)
        }
        fn device_status(&self, id: &DeviceId) -> Result<DiskStatus> {
            self.inner.device_status(id)
        }
        fn capabilities(&self) -> BackendCapabilities {
            self.inner.capabilities()
        }
    }

    #[test]
    fn paged_topology_recovers_partial_prepare_and_certificate_barriers() {
        for mode in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let paths = (0..4)
                .map(|i| dir.path().join(format!("{i}.img")))
                .collect::<Vec<_>>();
            let fs = crate::ArgosFs::create_loop_paged(
                &paths,
                VolumeConfig {
                    k: 2,
                    m: 1,
                    chunk_size: 4096,
                    ..Default::default()
                },
                64 * 1024 * 1024,
                "topology-crash",
                false,
            )
            .unwrap();
            fs.mkdir("/preserved", 0o755).unwrap();
            fs.mark_clean_unmount().unwrap();
            let base = fs.metadata_snapshot();
            drop(fs);
            let superblocks = paths
                .iter()
                .map(|path| {
                    inspect_device(BackendKind::LoopBlock, path.clone())
                        .unwrap()
                        .0
                })
                .collect::<Vec<_>>();
            let backend: Arc<dyn StorageBackend> =
                Arc::new(FileBlockBackend::open_loop(&paths, true).unwrap());
            let fault = Arc::new(Interrupt {
                inner: backend.clone(),
                mode,
            });
            let mut next = base.clone();
            next.disks.get_mut("disk-0003").unwrap().status = DiskStatus::Removed;
            assert!(matches!(
                reconfigure(fault, &superblocks, &base, &mut next, &[]),
                Err(ArgosError::IndeterminateCommit(_))
            ));
            // All members supplied: can repair a partial prepare, finish a
            // joint prepare without certificates, or restore a minority cert.
            let fs = crate::ArgosFs::open_loop(&paths, true).unwrap();
            assert_eq!(
                fs.metadata_snapshot().disks["disk-0003"].status,
                DiskStatus::Removed
            );
            assert!(fs.metadata_snapshot().inodes[&1]
                .entries
                .contains_key("preserved"));
            fs.mkdir("/after-recovery", 0o755).unwrap();
            fs.mark_clean_unmount().unwrap();
            drop(fs);
            let fs = crate::ArgosFs::open_loop(&paths[..3], false).unwrap();
            assert!(fs.metadata_snapshot().inodes[&1]
                .entries
                .contains_key("after-recovery"));
        }
    }
}
