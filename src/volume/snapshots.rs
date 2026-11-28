//! Experimental native V3 filesystem snapshots. The snapshot root and the
//! data pin index are published by a single certified transaction. The
//! materialized volume bridge still enumerates extents when pinning and
//! materializes a historical image on read.
use super::*;
use crate::storage::raw::paged_metadata::V3_INTEGRITY_SCHEME;

impl ArgosFs {
    pub fn create_filesystem_snapshot(&self, name: &str) -> Result<()> {
        self.change_filesystem_snapshot(name, true)
    }

    pub fn delete_filesystem_snapshot(&self, name: &str) -> Result<()> {
        self.change_filesystem_snapshot(name, false)
    }

    pub fn list_filesystem_snapshots(&self) -> Result<Vec<String>> {
        let meta = self.meta.read();
        if meta.integrity.hash_scheme != Some(V3_INTEGRITY_SCHEME) {
            return Err(ArgosError::Unsupported(
                "filesystem snapshots require experimental V3".into(),
            ));
        }
        Ok(meta
            .raw_pool
            .snapshot_state
            .as_ref()
            .map_or_else(Vec::new, |state| state.names.iter().cloned().collect()))
    }

    /// Verify all retained historical placements (including data no live
    /// file references). Deduplicate exact generation/content identities.
    /// The prototype does not relocate damaged historical placements.
    pub(super) fn audit_snapshot_data(&self, report: &mut FsckReport) -> Result<()> {
        let live = self.meta.read();
        let Some(state) = &live.raw_pool.snapshot_state else {
            return Ok(());
        };
        let handle = self
            .paged_metadata
            .as_ref()
            .ok_or_else(|| ArgosError::CorruptedMetadata("missing snapshot store".into()))?;
        let mut seen = BTreeSet::new();
        for name in &state.names {
            let mut historical = handle.lock().snapshot_image(name)?;
            historical.disks = live.disks.clone();
            for shard in historical
                .inodes
                .values()
                .flat_map(|inode| inode.blocks.iter())
                .flat_map(|block| block.shards.iter())
            {
                let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
                    return Err(ArgosError::CorruptedMetadata(
                        "historical shard without extent".into(),
                    ));
                };
                if !seen.insert((
                    extent.disk_id.clone(),
                    extent.offset,
                    extent.generation,
                    shard.sha256.clone(),
                )) {
                    continue;
                }
                report.snapshot_shards_checked += 1;
                if !matches!(
                    self.read_shard_verified_locked(&historical, shard),
                    Ok(Some(_))
                ) {
                    report.snapshot_shards_damaged += 1;
                }
            }
        }
        if report.snapshot_shards_damaged > 0 {
            report.errors.push(format!("{} historical snapshot shards failed verification; snapshot-aware relocation is not yet implemented",report.snapshot_shards_damaged));
        }
        Ok(())
    }

    fn change_filesystem_snapshot(&self, name: &str, create: bool) -> Result<()> {
        if self.meta.read().integrity.hash_scheme != Some(V3_INTEGRITY_SCHEME) {
            return Err(ArgosError::Unsupported(
                "filesystem snapshots require experimental V3".into(),
            ));
        }
        self.sync()?;
        let mut meta = self.meta.write();
        self.ensure_block_backend_writable_locked(&meta)?;
        if meta.integrity.hash_scheme != Some(V3_INTEGRITY_SCHEME) {
            return Err(ArgosError::Unsupported(
                "filesystem snapshots require experimental V3".into(),
            ));
        }
        self.commit_deferred_locked(&mut meta, false)?;
        let handle = self
            .paged_metadata
            .as_ref()
            .ok_or_else(|| ArgosError::CorruptedMetadata("missing V3 metadata store".into()))?;
        let superblocks = self.metadata_superblocks_locked(&meta)?;
        let mut store = handle.lock();
        let base = store.snapshot();
        // Snapshot creation is deliberately conservative until historical
        // stripe repair is integrated. Do not capture an already degraded
        // volume or acknowledge a snapshot of unflushed data.
        if create
            && (meta
                .disks
                .values()
                .any(|disk| !matches!(disk.status, DiskStatus::Online | DiskStatus::Removed))
                || !self.quarantined_devices.lock().is_empty()
                || superblocks.len()
                    != meta
                        .disks
                        .values()
                        .filter(|disk| disk.status != DiskStatus::Removed)
                        .count())
        {
            return Err(ArgosError::Unsupported(
                "snapshot creation requires all active members online".into(),
            ));
        }
        let mut next = meta.clone();
        next.txid = next
            .txid
            .checked_add(1)
            .ok_or_else(|| ArgosError::Invalid("snapshot transaction ID exhausted".into()))?;
        next.updated_at = now_f64();
        next.integrity.generation = next.txid;
        next.integrity.previous_meta_hash = base.integrity.meta_hash;
        let report = match raw_store::commit_paged_snapshot(
            &mut store,
            &mut next,
            &superblocks,
            &self.runtime.commit_metrics,
            name,
            create,
        ) {
            Ok(report) => report,
            Err(err) => {
                self.fence_on_quorum_loss(&err);
                return Err(err);
            }
        };
        drop(store);
        *meta = next;
        let durability = self.apply_quorum_write_report(&meta, report);
        *self.deferred_commit.lock() = DeferredCommitState::new(&meta);
        self.publish_capacity_snapshot_locked(&meta);
        durability
    }

    /// Historical read holds a live metadata read guard until data decoding
    /// finishes, preventing a concurrent snapshot deletion from releasing
    /// its last physical pins. Use current device identities/statuses but
    /// retain the historical encryption and erasure-layout configuration.
    pub fn read_snapshot_range(
        &self,
        name: &str,
        path: &str,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>> {
        let live = self.meta.read();
        if live.integrity.hash_scheme != Some(V3_INTEGRITY_SCHEME) {
            return Err(ArgosError::Unsupported(
                "filesystem snapshots require experimental V3".into(),
            ));
        }
        let store = self
            .paged_metadata
            .as_ref()
            .ok_or_else(|| ArgosError::CorruptedMetadata("missing V3 metadata store".into()))?;
        let mut historical = store.lock().snapshot_image(name)?;
        historical.disks = live.disks.clone();
        let ino = self.resolve_path_locked(&historical, path, true, 40)?;
        let inode = historical
            .inodes
            .get(&ino)
            .ok_or_else(|| ArgosError::NotFound(path.into()))?;
        if inode.kind != NodeKind::File {
            return Err(ArgosError::Unsupported(
                "snapshot range reads require a regular file".into(),
            ));
        }
        let start = usize::try_from(offset.min(inode.size))
            .map_err(|_| ArgosError::Invalid("snapshot offset overflow".into()))?;
        let end = usize::try_from(inode.size.min(offset.saturating_add(size as u64)))
            .map_err(|_| ArgosError::Invalid("snapshot range overflow".into()))?;
        let (data, _) = self.decode_inode_range_with_cache_policy_locked(
            &historical,
            inode,
            start,
            end,
            BlockCachePolicy::Bypass,
        )?;
        Ok(data)
    }
}
