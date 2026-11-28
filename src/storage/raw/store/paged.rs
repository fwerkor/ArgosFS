//! Mountable paged format. The reserved allocator region contains an immutable
//! checksummed membership manifest, not a second recovery authority.
use super::*;
use crate::raw_format::{
    REQUIRED_FEATURE_PAGED_METADATA as ACTIVE, REQUIRED_FEATURE_PAGED_MIGRATION as PENDING,
};
use crate::storage::raw::paged_metadata::{Arena, ImageStore, PAGED_INTEGRITY_SCHEME};

const MANIFEST_MAGIC: &[u8; 16] = b"ARGOSFS-PG-MAN\0\0";
const MAX_MANIFEST: usize = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct Manifest {
    pool: Uuid,
    members: Vec<ManifestMember>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ManifestMember {
    device_uuid: Uuid,
    arena: Arena,
    data_offset: u64,
    data_length: u64,
}

pub(super) fn uses_paged(superblocks: &[RawSuperblock]) -> bool {
    superblocks
        .iter()
        .any(|sb| sb.required_feature_flags & (ACTIVE | PENDING) != 0)
}

pub(super) fn reject_paged(superblocks: &[RawSuperblock]) -> Result<()> {
    if uses_paged(superblocks) {
        return Err(ArgosError::IncompatibleFormat(
            "legacy metadata writer cannot operate on paged pool".into(),
        ));
    }
    Ok(())
}

impl Manifest {
    fn from_superblocks(superblocks: &[RawSuperblock]) -> Result<Self> {
        let first = superblocks
            .first()
            .ok_or_else(|| ArgosError::Invalid("empty paged membership".into()))?;
        let mut members = superblocks
            .iter()
            .map(|sb| ManifestMember {
                device_uuid: sb.device_uuid,
                data_offset: sb.data.offset,
                data_length: sb.data.length,
                arena: Arena {
                    disk_id: sb.disk_id.clone(),
                    offset: sb.metadata.offset,
                    length: sb.metadata.length,
                },
            })
            .collect::<Vec<_>>();
        members.sort_by(|a, b| a.arena.disk_id.cmp(&b.arena.disk_id));
        let manifest = Self {
            pool: first.pool_uuid,
            members,
        };
        manifest.validate(superblocks)?;
        Ok(manifest)
    }

    fn arenas(&self) -> Vec<Arena> {
        self.members
            .iter()
            .map(|member| member.arena.clone())
            .collect()
    }

    fn validate(&self, superblocks: &[RawSuperblock]) -> Result<()> {
        let mut ids = BTreeSet::new();
        let mut uuids = BTreeSet::new();
        if self.members.is_empty()
            || self
                .members
                .iter()
                .any(|m| !ids.insert(&m.arena.disk_id) || !uuids.insert(m.device_uuid))
        {
            return Err(ArgosError::CorruptedMetadata(
                "invalid paged membership".into(),
            ));
        }
        for sb in superblocks {
            if sb.pool_uuid != self.pool
                || sb.required_feature_flags & (ACTIVE | PENDING) == 0
                || !self.members.iter().any(|m| {
                    m.device_uuid == sb.device_uuid
                        && m.arena.disk_id == sb.disk_id
                        && m.data_offset == sb.data.offset
                        && m.data_length == sb.data.length
                        && ((m.arena.offset == sb.metadata.offset
                            && m.arena.length == sb.metadata.length)
                            || (sb.data.offset.checked_add(sb.data.length) == Some(m.arena.offset)
                                && m.arena
                                    .offset
                                    .checked_add(m.arena.length)
                                    .is_some_and(|end| end <= sb.backup_superblock_offset)))
                })
            {
                return Err(ArgosError::IncompatibleFormat(format!(
                    "paged manifest disagrees with superblock {}",
                    sb.disk_id
                )));
            }
        }
        Ok(())
    }
}

fn manifest_bytes(manifest: &Manifest) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(manifest)?;
    if bytes.len() > MAX_MANIFEST {
        return Err(ArgosError::Invalid(
            "paged membership manifest exceeds size limit".into(),
        ));
    }
    let mut out = vec![0; RAW_HEADER_SIZE];
    out[..16].copy_from_slice(MANIFEST_MAGIC);
    put_u64(&mut out, 24, bytes.len() as u64);
    out[32..64].copy_from_slice(&Sha256::digest(&bytes));
    out.extend(bytes);
    Ok(out)
}

fn read_manifest(backend: &dyn StorageBackend, sb: &RawSuperblock) -> Result<Manifest> {
    let mut header = [0; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, sb.allocator.offset, &mut header)?;
    if &header[..16] != MANIFEST_MAGIC {
        return Err(ArgosError::CorruptedMetadata(
            "missing paged membership manifest".into(),
        ));
    }
    let len = get_u64(&header, 24)?;
    if len == 0
        || len > MAX_MANIFEST as u64
        || len > sb.allocator.length.saturating_sub(RAW_HEADER_SIZE as u64)
    {
        return Err(ArgosError::CorruptedMetadata(
            "invalid paged membership length".into(),
        ));
    }
    let mut bytes = vec![0; len as usize];
    backend.read_at(
        &sb.disk_id,
        sb.allocator.offset + RAW_HEADER_SIZE as u64,
        &mut bytes,
    )?;
    if Sha256::digest(&bytes).as_slice() != &header[32..64] {
        return Err(ArgosError::Checksum("paged membership manifest".into()));
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if manifest.pool != sb.pool_uuid
        || !manifest
            .members
            .iter()
            .any(|m| m.device_uuid == sb.device_uuid && m.arena.disk_id == sb.disk_id)
    {
        return Err(ArgosError::CorruptedMetadata(
            "paged membership manifest member identity mismatch".into(),
        ));
    }
    Ok(manifest)
}

fn select_manifest(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
) -> Result<Manifest> {
    if !superblocks
        .iter()
        .any(|sb| sb.required_feature_flags & ACTIVE != 0)
    {
        return Err(ArgosError::IncompatibleFormat("paged metadata upgrade interrupted; rerun upgrade-paged-metadata with all members offline".into()));
    }
    let mut chosen = None;
    let mut valid = 0;
    for sb in superblocks {
        if let Ok(manifest) = read_manifest(backend, sb) {
            if chosen.as_ref().is_some_and(|old| old != &manifest) {
                return Err(ArgosError::Conflict(
                    "divergent paged membership manifests".into(),
                ));
            }
            chosen = Some(manifest);
            valid += 1;
        }
    }
    let manifest = chosen.ok_or_else(|| {
        ArgosError::CorruptedMetadata("no valid paged membership manifest".into())
    })?;
    manifest.validate(superblocks)?;
    let need = manifest.members.len() / 2 + 1;
    if valid < need {
        return Err(quorum_unavailable("paged membership manifest", need, valid));
    }
    Ok(manifest)
}

fn validate_image(meta: &Metadata, manifest: &Manifest) -> Result<()> {
    if meta.integrity.hash_scheme != Some(PAGED_INTEGRITY_SCHEME)
        || meta
            .disks
            .values()
            .filter(|disk| disk.status != DiskStatus::Removed)
            .map(|disk| disk.id.as_str())
            .collect::<BTreeSet<_>>()
            != manifest
                .members
                .iter()
                .map(|member| member.arena.disk_id.as_str())
                .collect::<BTreeSet<_>>()
    {
        return Err(ArgosError::CorruptedMetadata(
            "paged metadata membership or integrity scheme mismatch".into(),
        ));
    }
    for member in &manifest.members {
        let alloc = meta
            .raw_pool
            .allocators
            .get(&member.arena.disk_id)
            .ok_or_else(|| {
                ArgosError::CorruptedMetadata("paged member has no data allocator".into())
            })?;
        if alloc.data_start != member.data_offset
            || Some(alloc.data_end) != member.data_offset.checked_add(member.data_length)
            || alloc.next_offset > alloc.data_end
        {
            return Err(ArgosError::CorruptedMetadata(
                "paged data allocator overlaps metadata or disagrees with manifest".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn initialize(
    backend: Arc<dyn StorageBackend>,
    superblocks: &[RawSuperblock],
    metadata: &mut Metadata,
) -> Result<ImageStore> {
    let manifest = Manifest::from_superblocks(superblocks)?;
    metadata.integrity.generation = metadata.txid;
    metadata.integrity.hash_scheme = Some(PAGED_INTEGRITY_SCHEME);
    metadata.integrity.meta_hash.clear();
    validate_image(metadata, &manifest)?;
    stage(backend, superblocks, &manifest, metadata)
}

fn stage(
    backend: Arc<dyn StorageBackend>,
    superblocks: &[RawSuperblock],
    manifest: &Manifest,
    metadata: &mut Metadata,
) -> Result<ImageStore> {
    let bytes = manifest_bytes(manifest)?;
    for sb in superblocks {
        if bytes.len() as u64 > sb.allocator.length {
            return Err(ArgosError::Invalid("paged manifest exceeds region".into()));
        }
    }
    for (sb, member) in superblocks.iter().map(|sb| {
        (
            sb,
            manifest
                .members
                .iter()
                .find(|m| m.arena.disk_id == sb.disk_id)
                .unwrap(),
        )
    }) {
        backend.write_at(&sb.disk_id, member.arena.offset, &[0; RAW_HEADER_SIZE * 4])?;
        backend.write_at(&sb.disk_id, sb.allocator.offset, &bytes)?;
        backend.flush_device(&sb.disk_id)?;
    }
    let mut store = ImageStore::format(backend, manifest.arenas(), metadata)?;
    store.confirm_all(superblocks.len())?;
    Ok(store)
}

pub(super) fn open(
    backend: Arc<dyn StorageBackend>,
    superblocks: &[RawSuperblock],
    write: bool,
) -> Result<ImageStore> {
    let manifest = select_manifest(&*backend, superblocks)?;
    let mut store = ImageStore::open(backend, manifest.pool, manifest.arenas())?;
    validate_image(&store.snapshot(), &manifest)?;
    if write {
        // Repair accessible stale replicas before mutations. A failed repair
        // does not change the selected root or permit a stale base to vote.
        store.repair_stale()?;
    }
    Ok(store)
}

pub(super) fn recover(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
) -> Result<(Metadata, TransactionReport)> {
    let manifest = select_manifest(backend, superblocks)?;
    let metadata = ImageStore::recover(backend, manifest.pool, manifest.arenas())?;
    validate_image(&metadata, &manifest)?;
    let report = report(&metadata);
    Ok((metadata, report))
}

pub(super) fn report(metadata: &Metadata) -> TransactionReport {
    TransactionReport {
        selected_metadata_source: "raw-paged-metadata".into(),
        last_valid_txid: metadata.txid,
        last_valid_generation: metadata.integrity.generation,
        last_valid_record_hash: metadata.integrity.meta_hash.clone(),
        latest_snapshot_txid: metadata.txid,
        latest_snapshot_generation: metadata.integrity.generation,
        ..Default::default()
    }
}

pub(crate) fn commit(
    store: &mut ImageStore,
    meta: &mut Metadata,
    superblocks: &[RawSuperblock],
    metrics: &CommitMetrics,
) -> Result<QuorumWriteReport> {
    let base = store.snapshot();
    let active = |m: &Metadata| {
        m.disks
            .values()
            .filter(|d| d.status != DiskStatus::Removed)
            .map(|d| d.id.clone())
            .collect::<BTreeSet<_>>()
    };
    if active(&base) != active(meta) {
        return Err(ArgosError::Unsupported(
            "paged metadata membership changes require offline migration".into(),
        ));
    }
    let allowed = superblocks
        .iter()
        .map(|sb| sb.disk_id.clone())
        .collect::<BTreeSet<_>>();
    let before = meta.integrity.clone();
    meta.integrity.generation = meta.txid;
    meta.integrity.previous_meta_hash = base.integrity.meta_hash;
    meta.integrity.hash_scheme = Some(PAGED_INTEGRITY_SCHEME);
    let report = match store.commit_allowed(meta, Some(&allowed)) {
        Ok(report) => report,
        Err(err) => {
            meta.integrity = before;
            return Err(err);
        }
    };
    let mut result = QuorumWriteReport::default();
    for io in report.members.values() {
        metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, io.pages_written * RAW_HEADER_SIZE);
    }
    for disk in allowed {
        if !report.members.contains_key(&disk) {
            result.failed_devices.insert(
                disk.clone(),
                report
                    .failures
                    .get(&disk)
                    .cloned()
                    .unwrap_or_else(|| "paged member lacks current root".into()),
            );
            result.unavailable_devices.insert(disk);
        }
    }
    Ok(result)
}

/// One-way offline conversion. Stage the new tree in never-allocated data
/// tails and the manifest in the unused allocator region, leaving both legacy
/// checkpoints and the journal intact until *all* members are fenced. Only
/// then publish ACTIVE. A valid ACTIVE copy on any member proves the old
/// writer was fenced on every member before the new image was exposed.
pub(super) fn upgrade(
    backend: Arc<dyn StorageBackend>,
    superblocks: &[RawSuperblock],
    requested_pool: Option<&str>,
    arena_bytes: Option<u64>,
) -> Result<Metadata> {
    if superblocks.is_empty() {
        return Err(ArgosError::Invalid(
            "no devices supplied for paged metadata upgrade".into(),
        ));
    }
    let pool = superblocks[0].pool_uuid;
    let mut ids = BTreeSet::new();
    let mut device_ids = BTreeSet::new();
    if superblocks.iter().any(|sb| {
        sb.pool_uuid != pool
            || !ids.insert(sb.disk_id.clone())
            || !device_ids.insert(sb.device_uuid)
    }) {
        return Err(ArgosError::Conflict(
            "upgrade requires distinct members of one pool".into(),
        ));
    }
    // Inspect both physical copies: the primary may still be PENDING while
    // an ACTIVE backup already exists following an interrupted publication.
    let mut active = false;
    for sb in superblocks {
        for offset in [PRIMARY_SUPERBLOCK_OFFSET, sb.backup_superblock_offset] {
            let mut bytes = [0; SUPERBLOCK_SIZE];
            backend.read_at(&sb.disk_id, offset, &mut bytes)?;
            if let Ok(copy) = RawSuperblock::decode(&bytes) {
                if copy.pool_uuid != pool
                    || copy.device_uuid != sb.device_uuid
                    || copy.disk_id != sb.disk_id
                {
                    return Err(ArgosError::Conflict(
                        "upgrade superblock copies disagree on identity".into(),
                    ));
                }
                active |= copy.required_feature_flags & ACTIVE != 0;
            }
        }
    }

    let validate_name = |metadata: &Metadata| -> Result<()> {
        if requested_pool
            .is_some_and(|name| name != metadata.uuid && name != metadata.raw_pool.pool_name)
        {
            return Err(ArgosError::Conflict(
                "requested pool identity does not match upgrade devices".into(),
            ));
        }
        let expected = metadata
            .disks
            .values()
            .filter(|disk| disk.status != DiskStatus::Removed)
            .map(|disk| disk.id.clone())
            .collect::<BTreeSet<_>>();
        if expected != ids {
            return Err(ArgosError::Invalid(
                "paged upgrade requires every active member, with no extras".into(),
            ));
        }
        Ok(())
    };

    if active {
        // An ACTIVE copy is irreversible. Recover the new authority, never
        // rebuild from the older legacy image even if some flags are pending.
        let first = read_manifest(&*backend, &superblocks[0])?;
        for sb in superblocks {
            if read_manifest(&*backend, sb)? != first {
                return Err(ArgosError::Conflict("upgrade manifests disagree".into()));
            }
        }
        let mut target = superblocks.to_vec();
        apply_manifest(&first, &mut target)?;
        let mut store = ImageStore::open(backend.clone(), pool, first.arenas())?;
        let metadata = store.snapshot();
        validate_name(&metadata)?;
        validate_image(&metadata, &first)?;
        store.repair_stale()?;
        store.confirm_all(target.len())?;
        publish_flags(&*backend, &target, ACTIVE | PENDING)?;
        return Ok(metadata);
    }

    // Before ACTIVE, no new-format mount was permitted. Re-read the exact
    // legacy recovery winner without writing it, even on PENDING members.
    let legacy = superblocks
        .iter()
        .cloned()
        .map(|mut sb| {
            sb.required_feature_flags &= !(ACTIVE | PENDING);
            sb
        })
        .collect::<Vec<_>>();
    let (mut metadata, _) = super::load_or_recover(&*backend, &legacy, false)?;
    validate_name(&metadata)?;
    if metadata.backend != backend.backend_kind() {
        return Err(ArgosError::IncompatibleFormat(
            "upgrade backend kind mismatch".into(),
        ));
    }

    let mut target = superblocks.to_vec();
    let mut arenas = BTreeMap::new();
    for sb in &mut target {
        let alloc = metadata
            .raw_pool
            .allocators
            .get_mut(&sb.disk_id)
            .ok_or_else(|| {
                ArgosError::CorruptedMetadata(format!("missing allocator for {}", sb.disk_id))
            })?;
        let size = arena_bytes.unwrap_or(sb.metadata.length);
        if !size.is_multiple_of(RAW_HEADER_SIZE as u64) || size < 8 * RAW_HEADER_SIZE as u64 {
            return Err(ArgosError::Invalid(
                "paged upgrade arena must be page aligned and at least 32 KiB".into(),
            ));
        }
        let end = alloc.data_end;
        let start = end.checked_sub(size).ok_or_else(|| ArgosError::DiskFull {
            disk_id: sb.disk_id.clone(),
            required: size,
            available: end.saturating_sub(alloc.next_offset),
        })?;
        if alloc.data_start != sb.data.offset
            || alloc.next_offset < alloc.data_start
            || end > sb.backup_superblock_offset
            || !end.is_multiple_of(RAW_HEADER_SIZE as u64)
            || start <= alloc.data_start
            || alloc.next_offset > start
            || alloc.free_extents.iter().any(|extent| {
                extent.offset < alloc.data_start
                    || extent
                        .offset
                        .checked_add(extent.length)
                        .is_none_or(|end| end > start)
            })
        {
            return Err(ArgosError::DiskFull {
                disk_id: sb.disk_id.clone(),
                required: size,
                available: end.saturating_sub(alloc.next_offset),
            });
        }
        arenas.insert(
            sb.disk_id.clone(),
            Arena {
                disk_id: sb.disk_id.clone(),
                offset: start,
                length: size,
            },
        );
        alloc.data_end = start;
        sb.data.length = start - sb.data.offset;
        sb.required_feature_flags |= PENDING;
    }
    // Independently check every actual shard pointer. A corrupt allocator
    // high-water mark alone cannot justify overwriting a live data extent.
    for inode in metadata.inodes.values() {
        for block in &inode.blocks {
            for shard in &block.shards {
                if let Some(crate::types::ShardLocation::RawExtent(extent)) = &shard.location {
                    if let Some(alloc) = metadata.raw_pool.allocators.get(&extent.disk_id) {
                        if extent.offset < alloc.data_start
                            || extent
                                .offset
                                .checked_add(extent.length)
                                .is_none_or(|end| end > alloc.data_end)
                        {
                            return Err(ArgosError::CorruptedMetadata(format!(
                                "live shard {} overlaps proposed metadata arena",
                                block.stripe_id
                            )));
                        }
                    } else if ids.contains(&extent.disk_id) {
                        return Err(ArgosError::CorruptedMetadata(
                            "live shard has no data allocator".into(),
                        ));
                    }
                } else {
                    return Err(ArgosError::CorruptedMetadata(
                        "raw shard has no physical extent during migration".into(),
                    ));
                }
            }
        }
    }
    let mut manifest = Manifest::from_superblocks(&target)?;
    for member in &mut manifest.members {
        member.arena = arenas.remove(&member.arena.disk_id).unwrap();
    }
    manifest.validate(&target)?;
    metadata.txid = metadata
        .txid
        .checked_add(1)
        .ok_or_else(|| ArgosError::Invalid("metadata transaction ID exhausted".into()))?;
    metadata.updated_at = now_f64();
    metadata.integrity.generation = metadata.txid;
    metadata.integrity.previous_meta_hash = std::mem::take(&mut metadata.integrity.meta_hash);
    metadata.integrity.hash_scheme = Some(PAGED_INTEGRITY_SCHEME);
    validate_image(&metadata, &manifest)?;
    let _store = stage(backend.clone(), &target, &manifest, &mut metadata)?;
    // All new roots exist, but no new-format writer may yet mount. Fence
    // every old primary and backup copy before enabling the new authority.
    publish_flags(&*backend, &target, PENDING)?;
    publish_flags(&*backend, &target, PENDING | ACTIVE)?;
    Ok(metadata)
}

fn apply_manifest(manifest: &Manifest, superblocks: &mut [RawSuperblock]) -> Result<()> {
    for sb in superblocks.iter_mut() {
        let member = manifest
            .members
            .iter()
            .find(|member| {
                member.device_uuid == sb.device_uuid && member.arena.disk_id == sb.disk_id
            })
            .ok_or_else(|| ArgosError::Conflict("upgrade member not in manifest".into()))?;
        if sb.data.offset != member.data_offset {
            return Err(ArgosError::Conflict(
                "upgrade member data region moved".into(),
            ));
        }
        sb.data.length = member.data_length;
        sb.required_feature_flags |= ACTIVE | PENDING;
        RawSuperblock::decode(&sb.encode())?
            .validate_device_capacity(sb.backup_superblock_offset + BACKUP_REGION_SIZE)?;
    }
    manifest.validate(superblocks)
}

fn publish_flags(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    flags: u64,
) -> Result<()> {
    for sb in superblocks {
        let mut next = sb.clone();
        next.required_feature_flags |= flags;
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or_else(|| ArgosError::Invalid("superblock generation exhausted".into()))?;
        next.clean = true;
        let bytes = next.encode();
        // Validate the complete layout before any writes to this device.
        RawSuperblock::decode(&bytes)?.validate_device_capacity(backend.capacity(&sb.disk_id)?)?;
        for offset in [next.backup_superblock_offset, PRIMARY_SUPERBLOCK_OFFSET] {
            backend.write_at(&sb.disk_id, offset, &bytes)?;
            backend.flush_device(&sb.disk_id)?;
            let mut actual = [0; SUPERBLOCK_SIZE];
            backend.read_at(&sb.disk_id, offset, &mut actual)?;
            if RawSuperblock::decode(&actual)? != next {
                return Err(ArgosError::Checksum(format!(
                    "paged upgrade feature readback failed on {}",
                    sb.disk_id
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{BackendCapabilities, BackendDeviceInfo, DeviceId};
    use crate::types::VolumeConfig;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct InterruptedUpgrade {
        inner: Arc<dyn StorageBackend>,
        active: bool,
        tripped: AtomicBool,
    }
    impl StorageBackend for InterruptedUpgrade {
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
            if id == "disk-0001"
                && offset == PRIMARY_SUPERBLOCK_OFFSET
                && RawSuperblock::decode(data).is_ok_and(|sb| {
                    sb.required_feature_flags & PENDING != 0
                        && (sb.required_feature_flags & ACTIVE != 0) == self.active
                })
                && !self.tripped.swap(true, Ordering::Relaxed)
            {
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
    fn paged_migration_resumes_interrupted_pending_and_activation() {
        for active in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = (0..3)
                .map(|i| dir.path().join(format!("{i}.img")))
                .collect::<Vec<_>>();
            let fs = crate::ArgosFs::create_loop(
                &paths,
                VolumeConfig {
                    k: 2,
                    m: 1,
                    chunk_size: 4096,
                    ..Default::default()
                },
                64 * 1024 * 1024,
                "interrupted",
                false,
            )
            .unwrap();
            let data = vec![42; 16_384];
            fs.write_file("/file", &data, 0o600).unwrap();
            fs.mark_clean_unmount().unwrap();
            drop(fs);
            let superblocks = paths
                .iter()
                .map(|path| {
                    inspect_device(BackendKind::LoopBlock, path.clone())
                        .unwrap()
                        .0
                })
                .collect::<Vec<_>>();
            let fault = Arc::new(InterruptedUpgrade {
                inner: Arc::new(FileBlockBackend::open_loop(&paths, true).unwrap()),
                active,
                tripped: AtomicBool::new(false),
            });
            assert!(upgrade(fault.clone(), &superblocks, None, None).is_err());
            assert!(fault.tripped.load(Ordering::Relaxed));
            if active {
                let fs = crate::ArgosFs::open_loop(&paths, true).unwrap();
                fs.rename_path("/file", "/after-activation").unwrap();
                fs.mark_clean_unmount().unwrap();
            } else {
                assert!(crate::ArgosFs::open_loop(&paths, true).is_err());
            }
            super::super::upgrade_paged_metadata(BackendKind::LoopBlock, &paths, None, None)
                .unwrap();
            let fs = crate::ArgosFs::open_loop(&paths, false).unwrap();
            assert_eq!(
                fs.read_file(if active { "/after-activation" } else { "/file" }, false)
                    .unwrap(),
                data
            );
            assert_eq!(
                fs.metadata_snapshot().integrity.hash_scheme,
                Some(PAGED_INTEGRITY_SCHEME)
            );
        }
    }
}
