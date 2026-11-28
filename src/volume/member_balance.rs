use super::*;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MemberBalanceReport {
    pub member: String,
    pub moved_shards: u64,
    pub bytes_moved: u64,
    pub data_shards_moved: u64,
    pub parity_shards_moved: u64,
    pub used_before: u64,
    pub used_after: u64,
    pub target_bytes: u64,
    pub scanned_files: usize,
    pub scanned_blocks: usize,
    pub next_cursor: Option<InodeId>,
    pub resume_block: Option<(InodeId, usize)>,
}

#[derive(Clone, Debug, Default)]
pub struct MemberBalanceOptions {
    /// Restrict migration to this file or directory subtree.
    pub path: Option<String>,
    pub min_inode: Option<InodeId>,
    pub max_inode: Option<InodeId>,
    /// Prefer systematic data shards over parity when selecting a donor.
    pub prefer_data: bool,
}

#[derive(Clone, Copy)]
pub(super) struct MemberBalanceWindow {
    pub max_files: usize,
    pub max_blocks: usize,
    pub cursor: Option<InodeId>,
    pub resume_block: Option<(InodeId, usize)>,
}

impl ArgosFs {
    /// Return an underutilized raw member to service without decoding files.
    /// Copy verified encoded shards, at most one per stripe, and preserve the
    /// logical block, all other physical references, and inode timestamps.
    pub fn rebalance_member(&self, target: &str, max_shards: usize) -> Result<MemberBalanceReport> {
        self.rebalance_member_with_options(target, max_shards, &MemberBalanceOptions::default())
    }

    pub fn rebalance_member_with_options(
        &self,
        target: &str,
        max_shards: usize,
        options: &MemberBalanceOptions,
    ) -> Result<MemberBalanceReport> {
        self.rebalance_member_inner(target, max_shards, options, None)
    }

    /// Bound both the namespace work and shard migrations in a background run.
    /// A cursor advances only past fully visited files so partial large-file
    /// moves remain eligible for the next batch.
    pub(super) fn rebalance_member_window(
        &self,
        target: &str,
        max_shards: usize,
        window: MemberBalanceWindow,
    ) -> Result<MemberBalanceReport> {
        self.rebalance_member_inner(
            target,
            max_shards,
            &MemberBalanceOptions::default(),
            Some(window),
        )
    }

    fn rebalance_member_inner(
        &self,
        target: &str,
        max_shards: usize,
        options: &MemberBalanceOptions,
        window: Option<MemberBalanceWindow>,
    ) -> Result<MemberBalanceReport> {
        if options
            .min_inode
            .zip(options.max_inode)
            .is_some_and(|(min, max)| min > max)
        {
            return Err(ArgosError::Invalid(
                "minimum inode exceeds maximum inode".into(),
            ));
        }
        self.sync()?;
        let (mut ids, mut report) = {
            let meta = self.meta.read();
            self.ensure_block_backend_writable_locked(&meta)?;
            let disk = self.balance_target(&meta, target)?;
            let in_range = |id: InodeId| {
                options.min_inode.is_none_or(|min| id >= min)
                    && options.max_inode.is_none_or(|max| id <= max)
            };
            let ids = if let Some(window) = window {
                if let Some((ino, _)) = window.resume_block {
                    let mut ids = vec![ino];
                    ids.extend(
                        meta.inodes
                            .file_window_ids(Some(ino), window.max_files.saturating_sub(1))
                            .into_iter()
                            .filter(|id| *id != ino),
                    );
                    ids.truncate(window.max_files);
                    ids
                } else {
                    meta.inodes.file_window_ids(window.cursor, window.max_files)
                }
            } else if let Some(path) = &options.path {
                let root = self.resolve_path_locked(&meta, path, true, 40)?;
                let mut stack = vec![root];
                let mut seen = BTreeSet::new();
                let mut files = Vec::new();
                while let Some(id) = stack.pop() {
                    if !seen.insert(id) {
                        continue;
                    }
                    let inode = meta
                        .inodes
                        .get(&id)
                        .ok_or_else(|| ArgosError::NotFound(format!("inode {id}")))?;
                    match inode.kind {
                        NodeKind::Directory => stack.extend(inode.entries.values().copied()),
                        NodeKind::File if in_range(id) => files.push(id),
                        _ => {}
                    }
                }
                files
            } else {
                meta.inodes
                    .iter()
                    .filter_map(|(id, inode)| {
                        (inode.kind == NodeKind::File && in_range(*id)).then_some(*id)
                    })
                    .collect::<Vec<_>>()
            };
            (
                ids,
                MemberBalanceReport {
                    member: target.to_string(),
                    used_before: disk.used_bytes,
                    used_after: disk.used_bytes,
                    target_bytes: self.member_balance_goal(&meta, disk),
                    next_cursor: window.and_then(|window| window.cursor),
                    resume_block: window.and_then(|window| window.resume_block),
                    ..Default::default()
                },
            )
        };
        if window.is_none() {
            ids.sort_unstable();
        }
        let max_blocks = window.map_or(usize::MAX, |w| w.max_blocks);
        for ino in ids {
            if report.moved_shards >= max_shards as u64 || report.scanned_blocks >= max_blocks {
                break;
            }
            report.scanned_files += 1;
            let inode_lock = self.inode_lock(ino);
            let _guard = inode_lock.lock();
            let mut meta = self.meta.write();
            self.ensure_block_backend_writable_locked(&meta)?;
            if meta.config.defer_metadata_commit || bulk_import_enabled() {
                self.commit_deferred_locked(&mut meta, false)?;
            }
            let disk = self.balance_target(&meta, target)?;
            report.target_bytes = self.member_balance_goal(&meta, disk);
            if disk.used_bytes >= report.target_bytes {
                break;
            }
            let Some(inode) = meta
                .inodes
                .get(&ino)
                .filter(|i| i.kind == NodeKind::File)
                .cloned()
            else {
                report.next_cursor = Some(ino);
                report.resume_block = None;
                continue;
            };
            let before = meta.clone();
            let pending = self.deferred_commit.lock().pending_reclaims.len();
            let mut blocks = inode.blocks.clone();
            let mut retired = Vec::new();
            let mut moved = 0u64;
            let mut bytes = 0u64;
            let mut data_moved = 0u64;
            let mut donor_debt = BTreeMap::<String, u64>::new();
            let start = report
                .resume_block
                .filter(|(id, _)| *id == ino)
                .map_or(0, |(_, position)| position.min(blocks.len()));
            report.resume_block = None;
            let result = (|| -> Result<()> {
                for position in start..blocks.len() {
                    if report.moved_shards + moved >= max_shards as u64
                        || report.scanned_blocks >= max_blocks
                    {
                        report.resume_block = Some((ino, position));
                        break;
                    }
                    let target_disk = self.balance_target(&meta, target)?;
                    if target_disk.used_bytes >= report.target_bytes {
                        break;
                    }
                    report.scanned_blocks += 1;
                    let mut block = blocks[position].clone();
                    if block.shards.iter().any(|s| s.disk_id == target) {
                        continue;
                    }
                    // Preserve the original tier and failure-domain diversity.
                    let mut candidates = block
                        .shards
                        .iter()
                        .enumerate()
                        .filter_map(|(index, shard)| {
                            let source = meta.disks.get(&shard.disk_id)?;
                            let used = source
                                .used_bytes
                                .saturating_sub(*donor_debt.get(&shard.disk_id).unwrap_or(&0));
                            let size = shard_accounted_size(shard);
                            if source.status != DiskStatus::Online
                                || self.is_device_quarantined(&shard.disk_id)
                                || source.tier != target_disk.tier
                                || source.capacity_bytes == 0
                                || size == 0
                                || target_disk.used_bytes.saturating_add(size)
                                    > report.target_bytes.saturating_add(size / 2)
                                || !self.disk_has_capacity(&meta, target, target_disk, size)
                                || !matches!(shard.location, Some(ShardLocation::RawExtent(_)))
                                || (!target_disk.failure_domain.is_empty()
                                    && source.failure_domain != target_disk.failure_domain
                                    && block.shards.iter().any(|other| {
                                        other.disk_id != shard.disk_id
                                            && meta.disks.get(&other.disk_id).is_some_and(|d| {
                                                d.failure_domain == target_disk.failure_domain
                                            })
                                    }))
                            {
                                return None;
                            }
                            let donor_after = used.saturating_sub(size) as u128
                                * target_disk.capacity_bytes as u128;
                            let target_after = target_disk.used_bytes.saturating_add(size) as u128
                                * source.capacity_bytes as u128;
                            (donor_after >= target_after)
                                .then_some((index, used as f64 / source.capacity_bytes as f64))
                        })
                        .collect::<Vec<_>>();
                    let k = layout_by_id(&meta, block_layout_id(&block))?.k;
                    candidates.sort_by(|a, b| {
                        let data_order = if options.prefer_data {
                            (block.shards[a.0].slot >= k).cmp(&(block.shards[b.0].slot >= k))
                        } else {
                            std::cmp::Ordering::Equal
                        };
                        data_order
                            .then_with(|| b.1.total_cmp(&a.1))
                            .then_with(|| a.0.cmp(&b.0))
                    });
                    let Some((index, _)) = candidates.first().copied() else {
                        continue;
                    };
                    let source = block.shards[index].clone();
                    data_moved += u64::from(source.slot < k);
                    let Some(data) = self.read_shard_verified_locked(&meta, &source)? else {
                        return Err(ArgosError::CorruptedMetadata(format!(
                            "cannot rebalance unverified shard {}:{}",
                            block.stripe_id, source.slot
                        )));
                    };
                    let integrity = ShardIntegrity {
                        sha256: source.sha256.clone(),
                        checksum_block_size: source.checksum_block_size,
                        subblock_sha256: source.subblock_sha256.clone(),
                    };
                    let replacement = self.write_shard_locked(
                        &mut meta,
                        target,
                        &block.stripe_id,
                        source.slot,
                        &data,
                        Some(&integrity),
                    )?;
                    *donor_debt.entry(source.disk_id.clone()).or_default() +=
                        shard_accounted_size(&source);
                    let mut old = block.clone();
                    old.shards = vec![source];
                    retired.push(old);
                    block.shards[index] = replacement;
                    blocks.edit_at(position, |entry| *entry = block);
                    moved += 1;
                    bytes += data.len() as u64;
                }
                Ok(())
            })();
            if let Err(err) = result {
                *meta = before;
                return Err(err);
            }
            if report.resume_block.is_none() {
                report.next_cursor = Some(ino);
            }
            if moved == 0 {
                continue;
            }
            meta.inodes.get_mut(&ino).expect("inode locked").blocks = blocks;
            self.stage_block_reclamation_locked(&mut meta, &retired);
            if let Err(err) = self.commit_locked_with_previous(
                &mut meta,
                Some(&before),
                "rebalance-member",
                json!({"inode": ino,"member":target,"moved_shards":moved}),
            ) {
                if !Self::transaction_error_is_committed(&err) {
                    self.deferred_commit
                        .lock()
                        .pending_reclaims
                        .truncate(pending);
                    if !matches!(err, ArgosError::Conflict(_)) {
                        *meta = before;
                    }
                }
                return Err(err);
            }
            if meta.config.defer_metadata_commit || bulk_import_enabled() {
                self.commit_deferred_locked(&mut meta, false)?;
            }
            report.moved_shards += moved;
            report.bytes_moved += bytes;
            report.data_shards_moved += data_moved;
            report.parity_shards_moved += moved - data_moved;
        }
        self.sync()?;
        report.used_after = self
            .meta
            .read()
            .disks
            .get(target)
            .ok_or_else(|| ArgosError::MissingDevice(target.into()))?
            .used_bytes;
        Ok(report)
    }

    pub(super) fn autopilot_member_target(
        &self,
        report: &HealthReport,
        min_skew: f64,
        critical_risk: f64,
    ) -> Option<String> {
        let meta = self.meta.read();
        if meta.backend == BackendKind::Host
            || meta.disks.iter().any(|(id, disk)| {
                disk.status != DiskStatus::Removed
                    && (disk.status != DiskStatus::Online || self.is_device_quarantined(id))
            })
        {
            return None;
        }
        report
            .disks
            .iter()
            .filter_map(|health| {
                if health.predicted_failure || health.risk_score >= critical_risk {
                    return None;
                }
                let disk = self.balance_target(&meta, &health.id).ok()?;
                let goal = self.member_balance_goal(&meta, disk);
                if goal == 0 || disk.used_bytes >= goal {
                    return None;
                }
                let deficit = (goal - disk.used_bytes) as f64 / goal as f64;
                (deficit >= min_skew).then_some((deficit, &health.id))
            })
            .max_by(|a, b| a.0.total_cmp(&b.0).then_with(|| b.1.cmp(a.1)))
            .map(|(_, id)| id.clone())
    }

    fn balance_target<'a>(&self, meta: &'a Metadata, id: &str) -> Result<&'a Disk> {
        if meta.backend == BackendKind::Host {
            return Err(ArgosError::Unsupported(
                "member rebalance requires raw or loop storage".into(),
            ));
        }
        let disk = meta
            .disks
            .get(id)
            .ok_or_else(|| ArgosError::MissingDevice(id.into()))?;
        if disk.status != DiskStatus::Online
            || self.is_device_quarantined(id)
            || disk.capacity_bytes == 0
        {
            return Err(ArgosError::Invalid(format!(
                "member {id} is not an online, usable destination"
            )));
        }
        Ok(disk)
    }

    fn member_balance_goal(&self, meta: &Metadata, target: &Disk) -> u64 {
        let (used, capacity) = meta
            .disks
            .iter()
            .filter(|(id, d)| {
                d.status == DiskStatus::Online
                    && d.tier == target.tier
                    && !self.is_device_quarantined(id)
            })
            .fold((0u128, 0u128), |(u, c), (_, d)| {
                (u + d.used_bytes as u128, c + d.capacity_bytes as u128)
            });
        (used * target.capacity_bytes as u128 / capacity.max(1)).min(u64::MAX as u128) as u64
    }
}

#[cfg(test)]
#[path = "member_balance_tests.rs"]
mod tests;
