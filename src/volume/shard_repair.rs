use super::data_plane::assemble_data_shards;
use super::*;

/// Per-volume repair I/O counters. They count only successful repairs; reads are
/// the physically verified shard payloads used to reconstruct the replacement.
#[derive(Default)]
pub(super) struct RepairCounters {
    pub(super) shards: AtomicU64,
    pub(super) bytes_read: AtomicU64,
    pub(super) bytes_written: AtomicU64,
    pub(super) elapsed_ns: AtomicU64,
}

impl RepairCounters {
    pub(super) fn snapshot(&self) -> [u64; 4] {
        [
            self.shards.load(Ordering::Relaxed),
            self.bytes_read.load(Ordering::Relaxed),
            self.bytes_written.load(Ordering::Relaxed),
            self.elapsed_ns.load(Ordering::Relaxed),
        ]
    }

    pub(super) fn add_to_report(&self, report: &mut FsckReport, before: [u64; 4]) {
        let after = self.snapshot();
        report.reconstructed_shards = after[0].saturating_sub(before[0]);
        report.repair_bytes_read = after[1].saturating_sub(before[1]);
        report.repair_bytes_written = after[2].saturating_sub(before[2]);
        report.repair_elapsed_ms = after[3].saturating_sub(before[3]) / 1_000_000;
    }
}

#[derive(Default)]
struct RepairWork {
    shards: u64,
    bytes_read: u64,
    bytes_written: u64,
}

impl ArgosFs {
    /// Repair the original *encoded* shards, leaving unaffected extents, inode
    /// timestamps, logical contents, encryption nonces and layout IDs intact.
    /// The caller holds the metadata write lock (and, for maintenance, the
    /// inode lock). All new extents are COW until a single metadata commit.
    pub(super) fn repair_inode_shards_locked(
        &self,
        meta: &mut Metadata,
        ino: InodeId,
        stripes: Option<&BTreeSet<String>>,
        repair_offline: bool,
        action: &str,
    ) -> Result<u64> {
        self.repair_inode_shards_with_key_locked(meta, ino, stripes, repair_offline, action, None)
    }

    /// The optional key was already authenticated against the current pool
    /// configuration. Targeted member rebuild can derive it once per pass;
    /// callers without a cached key retain the previous per-inode behavior.
    pub(super) fn repair_inode_shards_with_key_locked(
        &self,
        meta: &mut Metadata,
        ino: InodeId,
        stripes: Option<&BTreeSet<String>>,
        repair_offline: bool,
        action: &str,
        authenticated_key: Option<&[u8; 32]>,
    ) -> Result<u64> {
        self.ensure_block_backend_writable_locked(meta)?;
        let inode = meta
            .inodes
            .get(&ino)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?
            .clone();
        if inode.kind != NodeKind::File {
            return Err(ArgosError::Unsupported(
                "repair requires a file".to_string(),
            ));
        }

        let start_time = Instant::now();
        // Callers with a pass-scoped authenticated key avoid running the
        // costly Argon2id KDF once per affected inode. Other paths can still
        // derive on demand, preserving their current error semantics.
        let encrypted = inode.blocks.iter().any(|block| block.encrypted);
        let derived_key = if encrypted && authenticated_key.is_none() {
            Some(self.encryption_key_locked(meta)?)
        } else {
            None
        };
        let decrypt_key = encrypted
            .then_some(authenticated_key.or(derived_key.as_ref()))
            .flatten();
        // A raw journal delta must compare against the state *before*
        // allocating any replacement extent. Taking the previous snapshot
        // after physical allocations silently omits allocator changes.
        let before = self
            .commit_previous_with_advisory_locked(meta)
            .unwrap_or_else(|| meta.clone());
        let mut new_blocks = inode.blocks.clone();
        let mut created = Vec::<Shard>::new();
        let mut replaced = Vec::<FileBlock>::new();
        let mut work = RepairWork::default();
        let operation = (|| -> Result<()> {
            for (index, block) in inode.blocks.iter().enumerate() {
                if stripes.is_some_and(|selected| !selected.contains(&block.stripe_id)) {
                    continue;
                }
                let layout = layout_by_id(meta, block_layout_id(block))?;
                let total = layout_total(&layout)?;
                let mut present = vec![None; total];
                let mut failed = BTreeSet::<usize>::new();
                let mut transient = BTreeSet::<usize>::new();
                let mut seen = BTreeSet::new();

                let mut pending = Vec::with_capacity(block.shards.len());
                for shard in &block.shards {
                    if shard.slot >= total || !seen.insert(shard.slot) {
                        return Err(ArgosError::Invalid(format!(
                            "stripe {} has invalid or duplicated shard slot {}",
                            block.stripe_id, shard.slot
                        )));
                    }
                    if shard.size != block.shard_size {
                        return Err(ArgosError::Invalid(format!(
                            "stripe {} shard size mismatch at slot {}",
                            block.stripe_id, shard.slot
                        )));
                    }
                    let disk = meta.disks.get(&shard.disk_id);
                    let unavailable = disk.is_none_or(|disk| disk.status != DiskStatus::Online)
                        || (meta.backend != BackendKind::Host
                            && self.is_device_quarantined(&shard.disk_id));
                    if unavailable {
                        let defer = disk.is_some_and(|disk| {
                            if disk.status == DiskStatus::Offline {
                                return !repair_offline || !offline_member_due_for_repair(disk);
                            }
                            meta.backend != BackendKind::Host
                                && disk.status == DiskStatus::Online
                                && self.is_device_quarantined(&shard.disk_id)
                                && (!repair_offline
                                    || !self.quarantined_member_due_for_repair(&shard.disk_id))
                        });
                        failed.insert(shard.slot);
                        if defer {
                            transient.insert(shard.slot);
                        }
                        continue;
                    }
                    pending.push(shard);
                }
                self.read_verified_shards_batched(meta, &pending, |shard, result| match result {
                    Ok(Some(data)) => {
                        work.bytes_read = work.bytes_read.saturating_add(data.len() as u64);
                        present[shard.slot] = Some(data);
                    }
                    Ok(None) => {
                        failed.insert(shard.slot);
                    }
                    Err(_) => {
                        failed.insert(shard.slot);
                        if meta.backend != BackendKind::Host
                            && self.is_device_quarantined(&shard.disk_id)
                            && (!repair_offline
                                || !self.quarantined_member_due_for_repair(&shard.disk_id))
                        {
                            transient.insert(shard.slot);
                        }
                    }
                });
                for slot in 0..total {
                    if !seen.contains(&slot) {
                        failed.insert(slot);
                    }
                }
                if failed.iter().all(|slot| transient.contains(slot)) {
                    continue;
                }
                if failed.is_empty() {
                    continue;
                }
                let present_count = present.iter().filter(|data| data.is_some()).count();
                if present_count < layout.k {
                    return Err(ArgosError::UnrecoverableStripe {
                        stripe_id: block.stripe_id.clone(),
                        reason: format!(
                            "only {present_count} verified shards available, need {}",
                            layout.k
                        ),
                    });
                }
                let restored = RsCodec::reconstruct_cached(layout.k, layout.m, present)?;

                // Before publishing ANY reconstructed extent, validate the
                // original ciphertext -> plaintext -> uncompressed raw checksum.
                // Re-encrypting would change the nonce/AAD and require replacing
                // healthy shards; reconstruction preserves the exact payload.
                let transformed = assemble_data_shards(
                    restored.iter().take(layout.k).map(Vec::as_slice),
                    block.compressed_size,
                );
                let transformed = if block.encrypted {
                    let key = decrypt_key.ok_or_else(|| {
                        ArgosError::PermissionDenied("missing ArgosFS encryption key".to_string())
                    })?;
                    let nonce = hex::decode(&block.nonce_hex).map_err(|err| {
                        ArgosError::Invalid(format!("invalid encrypted block nonce: {err}"))
                    })?;
                    crypto::decrypt_with_key(
                        key,
                        &nonce,
                        &transformed,
                        &encryption_aad(&meta.uuid, &block.stripe_id),
                    )?
                } else {
                    transformed
                };
                let raw = decompress(&transformed, block.codec)?;
                if raw.len() != block.raw_size || !content_hash_matches(&raw, &block.raw_sha256) {
                    return Err(ArgosError::UnrecoverableStripe {
                        stripe_id: block.stripe_id.clone(),
                        reason: "reconstructed raw checksum mismatch".to_string(),
                    });
                }

                let mut survivors = block
                    .shards
                    .iter()
                    .filter(|shard| {
                        !failed.contains(&shard.slot) || transient.contains(&shard.slot)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let mut excluded = survivors
                    .iter()
                    .map(|shard| shard.disk_id.clone())
                    .collect::<BTreeSet<_>>();
                let old_shards = block
                    .shards
                    .iter()
                    .filter(|shard| {
                        failed.contains(&shard.slot) && !transient.contains(&shard.slot)
                    })
                    .cloned()
                    .collect::<Vec<_>>();

                for slot in failed.difference(&transient) {
                    let bytes = &restored[*slot];
                    let old = block.shards.iter().find(|shard| shard.slot == *slot);
                    if let Some(old) = old {
                        if !content_hash_matches(bytes, &old.sha256) {
                            return Err(ArgosError::UnrecoverableStripe {
                                stripe_id: block.stripe_id.clone(),
                                reason: format!(
                                    "reconstructed shard checksum mismatch at slot {slot}"
                                ),
                            });
                        }
                    }
                    // No destination is ever allowed to share a member with
                    // another live shard of this stripe. The old member may
                    // be reused only when it is online and the new extent/path
                    // is distinct (COW).
                    // Prefer a distinct failure domain when possible. Some
                    // host volumes share the same backing filesystem, so
                    // relax this preference if no distinct domain exists.
                    let used_domains = excluded
                        .iter()
                        .filter_map(|id| meta.disks.get(id))
                        .map(|disk| disk.failure_domain.as_str())
                        .filter(|domain| !domain.is_empty())
                        .collect::<BTreeSet<_>>();
                    let mut preferred_exclusions = excluded.clone();
                    for (id, disk) in &meta.disks {
                        if used_domains.contains(disk.failure_domain.as_str()) {
                            preferred_exclusions.insert(id.clone());
                        }
                    }
                    let choose = |exclusions: &BTreeSet<String>| {
                        self.choose_disks_locked(
                            meta,
                            PlacementRequest {
                                key: &block.stripe_id,
                                count: 1,
                                storage_class: block.storage_class,
                                boot_critical: inode.boot_critical,
                                exclude_disks: exclusions,
                                required_bytes: bytes.len() as u64,
                            },
                        )
                    };
                    let destination = match choose(&preferred_exclusions) {
                        Ok(mut disks) => disks.remove(0),
                        Err(ArgosError::NotEnoughDisks { .. }) => choose(&excluded)?.remove(0),
                        Err(err) => return Err(err),
                    };
                    // Host paths need a fresh physical name for COW. Raw extents
                    // are already unique, and the write verifier must retain
                    // the block's logical stripe ID to find the live extent.
                    let storage_id = if meta.backend == BackendKind::Host {
                        format!("r{}", Uuid::new_v4().simple())
                    } else {
                        block.stripe_id.clone()
                    };
                    let integrity = old.map(|shard| ShardIntegrity {
                        sha256: shard.sha256.clone(),
                        checksum_block_size: shard.checksum_block_size,
                        subblock_sha256: shard.subblock_sha256.clone(),
                    });
                    let new_shard = self.write_shard_locked(
                        meta,
                        &destination,
                        &storage_id,
                        *slot,
                        bytes,
                        integrity.as_ref(),
                    )?;
                    work.shards += 1;
                    work.bytes_written = work.bytes_written.saturating_add(bytes.len() as u64);
                    excluded.insert(destination);
                    created.push(new_shard.clone());
                    survivors.push(new_shard);
                }
                survivors.sort_by_key(|shard| shard.slot);
                new_blocks.edit_at(index, |block| block.shards = survivors);
                if !old_shards.is_empty() {
                    let mut garbage = block.clone();
                    garbage.shards = old_shards;
                    replaced.push(garbage);
                }
            }
            Ok(())
        })();

        if let Err(err) = operation {
            for shard in &created {
                let _ = self.delete_shard_locked(meta, shard);
            }
            *meta = before;
            return Err(err);
        }
        if work.shards == 0 {
            return Ok(0);
        }

        let pending_reclaims = self.deferred_commit.lock().pending_reclaims.len();
        meta.inodes
            .get_mut(&ino)
            .expect("locked inode exists")
            .blocks = new_blocks;
        self.stage_block_reclamation_locked(meta, &replaced);
        if let Err(err) = self.commit_locked_with_previous(
            meta,
            Some(&before),
            action,
            json!({"inode": ino, "shards_reconstructed": work.shards, "bytes_written": work.bytes_written}),
        ) {
            if !Self::transaction_error_is_committed(&err) {
                if meta.backend == BackendKind::Host {
                    for shard in &created {
                        let _ = self.delete_shard_locked(meta, shard);
                    }
                }
                self.deferred_commit.lock().pending_reclaims.truncate(pending_reclaims);
                if !matches!(err, ArgosError::Conflict(_)) {
                    *meta = before;
                }
            }
            return Err(err);
        }
        self.finish_block_reclamation_locked(meta, &replaced);
        self.runtime
            .repair_counters
            .shards
            .fetch_add(work.shards, Ordering::Relaxed);
        self.runtime
            .repair_counters
            .bytes_read
            .fetch_add(work.bytes_read, Ordering::Relaxed);
        self.runtime
            .repair_counters
            .bytes_written
            .fetch_add(work.bytes_written, Ordering::Relaxed);
        self.runtime.repair_counters.elapsed_ns.fetch_add(
            start_time.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        Ok(work.shards)
    }
}
