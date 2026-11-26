use super::*;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MemberBalanceReport {
    pub member: String,
    pub moved_shards: u64,
    pub bytes_moved: u64,
    pub used_before: u64,
    pub used_after: u64,
    pub target_bytes: u64,
}

impl ArgosFs {
    /// Return an underutilized raw member to service without decoding files.
    /// Copy verified encoded shards, at most one per stripe, and preserve the
    /// logical block, all other physical references, and inode timestamps.
    pub fn rebalance_member(&self, target: &str, max_shards: usize) -> Result<MemberBalanceReport> {
        self.sync()?;
        let (mut ids, mut report) = {
            let meta = self.meta.read();
            self.ensure_block_backend_writable_locked(&meta)?;
            let disk = self.balance_target(&meta, target)?;
            let ids = meta
                .inodes
                .iter()
                .filter_map(|(id, inode)| (inode.kind == NodeKind::File).then_some(*id))
                .collect::<Vec<_>>();
            (
                ids,
                MemberBalanceReport {
                    member: target.to_string(),
                    used_before: disk.used_bytes,
                    used_after: disk.used_bytes,
                    target_bytes: self.member_balance_goal(&meta, disk),
                    ..Default::default()
                },
            )
        };
        ids.sort_unstable();
        for ino in ids {
            if report.moved_shards >= max_shards as u64 {
                break;
            }
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
                continue;
            };
            let before = meta.clone();
            let pending = self.deferred_commit.lock().pending_reclaims.len();
            let mut blocks = inode.blocks.clone();
            let mut retired = Vec::new();
            let mut moved = 0u64;
            let mut bytes = 0u64;
            let mut donor_debt = BTreeMap::<String, u64>::new();
            let result = (|| -> Result<()> {
                for block in &mut blocks {
                    if report.moved_shards + moved >= max_shards as u64 {
                        break;
                    }
                    let target_disk = self.balance_target(&meta, target)?;
                    if target_disk.used_bytes >= report.target_bytes {
                        break;
                    }
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
                    candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                    let Some((index, _)) = candidates.first().copied() else {
                        continue;
                    };
                    let source = block.shards[index].clone();
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
                    moved += 1;
                    bytes += data.len() as u64;
                }
                Ok(())
            })();
            if let Err(err) = result {
                *meta = before;
                return Err(err);
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
