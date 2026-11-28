//! Bounded raw write reservations: independent inode I/O can overlap, while
//! publishing allocator and namespace changes remains an ordered transaction.
use super::data_plane::PreparedWrite;
use super::metadata_lock::ReservationSpec;
use super::*;

struct RawAppendPlan<'a> {
    blocks: Vec<FileBlock>,
    writes: BTreeMap<String, Vec<(u64, &'a [u8])>>,
}

// Bound the number and total size of ranges per guarded worker operation.
// A single large shard still follows its original per-operation deadline.
const MAX_RAW_BATCH_BYTES: usize = 4 * 1024 * 1024;
const MAX_RAW_BATCH_RANGES: usize = 64;

fn raw_write_batch_len(writes: &[(u64, &[u8])]) -> usize {
    let mut bytes = 0usize;
    let mut count = 0;
    for (_, data) in writes.iter().take(MAX_RAW_BATCH_RANGES) {
        if count > 0 && bytes.saturating_add(data.len()) > MAX_RAW_BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(data.len());
        count += 1;
    }
    count
}

impl ArgosFs {
    /// Called under the caller's exclusive inode lock for appends, or its
    /// shared lock for aligned in-place logical replacements. None means the ordinary path
    /// must be used; Some(Err) is an actual write/commit failure.
    pub(super) fn try_parallel_raw_append(
        &self,
        ino: InodeId,
        offset: usize,
        data: &[u8],
        clear_setid: bool,
        write_time: Option<BufferedWriteTime>,
        prepared: Option<&PreparedWrite<'_>>,
    ) -> Option<Result<()>> {
        self.try_parallel_raw_range(ino, offset, data, clear_setid, write_time, prepared, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn try_parallel_raw_range(
        &self,
        ino: InodeId,
        offset: usize,
        data: &[u8],
        clear_setid: bool,
        write_time: Option<BufferedWriteTime>,
        prepared: Option<&PreparedWrite<'_>>,
        overwrite: bool,
    ) -> Option<Result<()>> {
        let prepared = prepared?;
        if journal::crash_injection_configured() || bulk_import_enabled() {
            return None;
        }
        let lane = self.meta.parallel()?;
        let mut plan = None;
        let (reservation, snapshot) = {
            let meta = lane.image();
            if !self.parallel_range_eligible(&meta, ino, offset, data, prepared, overwrite) {
                return None;
            }
            if let Err(err) = self.ensure_block_backend_writable_locked(&meta) {
                return Some(Err(err));
            }
            let target = if overwrite {
                meta.inodes.get(&ino)?.size
            } else {
                offset.checked_add(data.len())? as u64
            };
            if let Err(err) = self.ensure_resize_within_quotas_locked(&meta, ino, target) {
                return Some(Err(err));
            }
            let result = lane.reserve(&meta, |private| {
                let (spec, prepared_plan) =
                    self.plan_raw_append(private, ino, offset, data, prepared)?;
                plan = Some(prepared_plan);
                Ok(spec)
            });
            match result {
                Ok(result) => result,
                Err(_) => return None,
            }
        };
        let mut plan = plan.expect("reserved append has a plan");
        // Each member gets one independent worker and a single flush after
        // its payload writes. Nothing is visible in the live image yet.
        let writes = std::thread::scope(|scope| {
            let jobs = plan
                .writes
                .iter()
                .map(|(disk, writes)| {
                    let snapshot = &snapshot;
                    scope.spawn(move || -> Result<(String, u64, f64)> {
                        let start = Instant::now();
                        let mut bytes = 0u64;
                        let mut remaining = writes.as_slice();
                        while !remaining.is_empty() {
                            let count = raw_write_batch_len(remaining);
                            let (batch, rest) = remaining.split_at(count);
                            self.backend_write_batch_locked(snapshot, disk, batch)?;
                            for (_, payload) in batch {
                                self.runtime.commit_metrics.add_data_plane_bytes(
                                    DataPlaneByteKind::ShardPayloadWritten,
                                    payload.len(),
                                );
                                bytes = bytes.saturating_add(payload.len() as u64);
                            }
                            remaining = rest;
                        }
                        self.backend_flush_locked(snapshot, disk)?;
                        Ok((disk.clone(), bytes, start.elapsed().as_secs_f64()))
                    })
                })
                .collect::<Vec<_>>();
            // Join every worker before any failed reservation is released.
            jobs.into_iter()
                .map(|job| job.join().expect("raw append worker panicked"))
                .collect::<Vec<_>>()
        })
        .into_iter()
        .collect::<Result<Vec<_>>>();
        let timings = match writes {
            Ok(timings) => timings,
            Err(err) => return Some(Err(err)),
        };
        reservation.wait_turn();
        let mut meta = lane.image();
        if !self.parallel_range_eligible(&meta, ino, offset, data, prepared, overwrite) {
            return None;
        }
        if let Err(err) = self.ensure_block_backend_writable_locked(&meta) {
            return Some(Err(err));
        }
        let target = if overwrite {
            meta.inodes.get(&ino)?.size
        } else {
            offset.checked_add(data.len())? as u64
        };
        if let Err(err) = self.ensure_resize_within_quotas_locked(&meta, ino, target) {
            return Some(Err(err));
        }
        // If an earlier writer failed, allocator/stripe choices might differ.
        // Retry through the regular path only after this reservation is gone.
        let mut staged = meta.clone();
        if reservation.spec.apply(&mut staged).is_err() {
            return None;
        }
        let rollback = self.commit_previous_with_advisory_locked(&mut meta);
        meta.raw_pool.allocators = staged.raw_pool.allocators;
        meta.next_stripe = staged.next_stripe;
        let generation = meta.txid.saturating_add(1);
        for block in &mut plan.blocks {
            for shard in &mut block.shards {
                let Some(ShardLocation::RawExtent(extent)) = &mut shard.location else {
                    unreachable!()
                };
                extent.generation = generation;
                if let Some(disk) = meta.disks.get_mut(&shard.disk_id) {
                    disk.used_bytes = disk.used_bytes.saturating_add(extent.length);
                }
            }
        }
        for (disk, bytes, elapsed) in timings {
            self.update_write_latency_locked(&mut meta, &disk, bytes, elapsed);
        }
        let now = write_time.map_or_else(now_f64, |time| time.at);
        let inode = meta.inodes.get_mut(&ino).expect("validated range inode");
        let replaced = if overwrite {
            inode.blocks.remove_window(
                offset as u64,
                offset.checked_add(data.len())? as u64,
                target,
            )
        } else {
            Vec::new()
        };
        inode.blocks.insert_sorted(plan.blocks.iter().cloned());
        set_inline_payload(inode, None);
        inode.size = target;
        inode.write_count = inode.write_count.saturating_add(1);
        inode.write_bytes = inode.write_bytes.saturating_add(data.len() as u64);
        inode.last_written_at = now;
        inode.workload_score = inode.workload_score * 0.90 + 2.0;
        inode.mtime = now;
        inode.ctime = write_time.map_or(now, |time| time.persisted_ctime(inode.ctime));
        if clear_setid {
            inode.mode &= !(libc::S_ISUID | libc::S_ISGID);
        }
        self.stage_block_reclamation_locked(&mut meta, &replaced);
        let rewrite = if overwrite {
            "parallel-full-stripe-overwrite"
        } else {
            "parallel-aligned-eof-append"
        };
        if let Err(err) = self.commit_locked_with_previous(
            &mut meta,
            rollback.as_ref(),
            "write-range",
            json!({"inode":ino,"offset":offset,"bytes":data.len(),"rewrite":rewrite}),
        ) {
            if !Self::transaction_error_is_committed(&err)
                && !matches!(err, ArgosError::Conflict(_))
            {
                if let Some(rollback) = rollback {
                    *meta = rollback;
                }
            }
            return Some(Err(err));
        }
        self.finish_block_reclamation_locked(&mut meta, &replaced);
        if overwrite {
            self.runtime
                .commit_metrics
                .add_data_plane_bytes(DataPlaneByteKind::RangeRewriteInput, data.len());
        } else {
            self.adjust_quota_usage_for_resize(ino, offset as u64, target);
        }
        if let Some(verify) = &self.verify_written {
            for block in &plan.blocks {
                for shard in &block.shards {
                    let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
                        unreachable!()
                    };
                    verify.enqueue(
                        &shard.disk_id,
                        extent.offset,
                        extent.generation,
                        shard.size,
                        &shard.sha256,
                        &block.stripe_id,
                    );
                }
            }
        }
        // Drop while the image remains locked: another reserver must not
        // replay this now-published allocation a second time.
        drop(reservation);
        Some(Ok(()))
    }

    fn parallel_range_eligible(
        &self,
        meta: &Metadata,
        ino: InodeId,
        offset: usize,
        data: &[u8],
        prepared: &PreparedWrite<'_>,
        overwrite: bool,
    ) -> bool {
        if meta.backend == BackendKind::Host
            || meta.config.defer_metadata_commit
            || meta.config.defer_data_flush
            || !prepared.matches(meta, data)
        {
            return false;
        }
        let Ok((old_size, stripe_size)) = self.range_update_geometry_locked(meta, ino) else {
            return false;
        };
        (if overwrite {
            self.parallel_overwrite_geometry(meta, ino, offset, data.len())
        } else {
            old_size == offset && offset.is_multiple_of(stripe_size)
        }) && meta
            .inodes
            .get(&ino)
            .is_some_and(|inode| inode.inline_data.is_none())
            && prepared
                .stripes
                .iter()
                .all(|stripe| stripe.encoded.len() > 1)
    }

    // Full stripes are independent of old payload contents. Never admit
    // partial stripes, EOF extension, inline data or a mixed-layout block
    // crossing the write boundary to the shared-inode reservation lane.
    pub(super) fn parallel_overwrite_geometry(
        &self,
        meta: &Metadata,
        ino: InodeId,
        offset: usize,
        len: usize,
    ) -> bool {
        if meta.backend == BackendKind::Host
            || meta.config.defer_metadata_commit
            || meta.config.defer_data_flush
            || len == 0
        {
            return false;
        }
        let Ok((size, stripe_size)) = self.range_update_geometry_locked(meta, ino) else {
            return false;
        };
        let Some(end) = offset.checked_add(len) else {
            return false;
        };
        if end > size || !offset.is_multiple_of(stripe_size) || !len.is_multiple_of(stripe_size) {
            return false;
        }
        let Some(inode) = meta.inodes.get(&ino) else {
            return false;
        };
        if inode.inline_data.is_some() {
            return false;
        }
        // FileBlocks uses a persistent indexed sequence. Inspect only the
        // affected window in healthy files instead of scanning a huge inode.
        let mut blocks = inode.blocks.clone();
        blocks
            .remove_window(offset as u64, end as u64, size as u64)
            .iter()
            .all(|block| {
                block.raw_offset >= offset as u64
                    && block
                        .raw_offset
                        .checked_add(block.raw_size as u64)
                        .is_some_and(|block_end| block_end <= end as u64)
            })
    }

    fn plan_raw_append<'a>(
        &self,
        private: &mut Metadata,
        ino: InodeId,
        offset: usize,
        data: &[u8],
        prepared: &'a PreparedWrite<'_>,
    ) -> Result<(ReservationSpec, RawAppendPlan<'a>)> {
        let layout = current_write_layout(private)?;
        let stripe_size = layout_stripe_raw_size(&layout)?;
        let inode = private
            .inodes
            .get(&ino)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
        let storage_class = inode.storage_class;
        let boot_critical = inode.boot_critical;
        let first_stripe = private.next_stripe;
        let mut extents = Vec::new();
        let mut blocks = Vec::new();
        let mut writes = BTreeMap::<String, Vec<(u64, &[u8])>>::new();
        for (index, (stripe, raw)) in prepared
            .stripes
            .iter()
            .zip(data.chunks(stripe_size))
            .enumerate()
        {
            let stripe_id = format!("s{:016x}", private.next_stripe);
            private.next_stripe = private
                .next_stripe
                .checked_add(1)
                .ok_or_else(|| ArgosError::Invalid("stripe id overflow".into()))?;
            let placements = self.choose_disks_locked(
                private,
                PlacementRequest {
                    key: &stripe_id,
                    count: stripe.encoded.len(),
                    storage_class,
                    boot_critical,
                    exclude_disks: &BTreeSet::new(),
                    required_bytes: stripe.shard_size as u64,
                },
            )?;
            let mut shards = Vec::new();
            for (slot, (disk, payload)) in placements.iter().zip(&stripe.encoded).enumerate() {
                self.ensure_disk_capacity_locked(private, disk, payload.len() as u64)?;
                let state = private
                    .raw_pool
                    .allocators
                    .get_mut(disk)
                    .ok_or_else(|| ArgosError::MissingDevice(disk.clone()))?;
                let extent =
                    allocator::allocate(state, disk, payload.len() as u64, private.txid + 1)?;
                private
                    .disks
                    .get_mut(disk)
                    .expect("placement member")
                    .used_bytes = private.disks[disk].used_bytes.saturating_add(extent.length);
                writes
                    .entry(disk.clone())
                    .or_default()
                    .push((extent.offset, payload));
                shards.push(Shard {
                    slot,
                    disk_id: disk.clone(),
                    location: Some(ShardLocation::RawExtent(extent.clone())),
                    relpath: PathBuf::new(),
                    sha256: stripe.shard_hashes[slot].clone(),
                    checksum_block_size: 0,
                    subblock_sha256: Vec::new(),
                    size: payload.len(),
                });
                extents.push(extent);
            }
            blocks.push(FileBlock {
                layout_id: layout.id.clone(),
                stripe_id,
                raw_offset: (offset + index * stripe_size) as u64,
                raw_size: raw.len(),
                raw_sha256: stripe.raw_sha256.clone(),
                codec: stripe.codec,
                encrypted: false,
                nonce_hex: String::new(),
                compressed_size: stripe.compressed_size,
                shard_size: stripe.shard_size,
                shards,
                storage_class,
            });
        }
        Ok((
            ReservationSpec {
                first_stripe,
                next_stripe: private.next_stripe,
                extents,
            },
            RawAppendPlan { blocks, writes },
        ))
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    #[test]
    fn batch_limits_count_bytes_and_always_make_progress() {
        let data = vec![0; 128 * 1024];
        let writes = vec![(0, data.as_slice()); 80];
        assert_eq!(raw_write_batch_len(&writes), 32);
        let tiny = vec![(0, b"x".as_slice()); 80];
        assert_eq!(raw_write_batch_len(&tiny), 64);
        let large = vec![0; MAX_RAW_BATCH_BYTES + 1];
        assert_eq!(raw_write_batch_len(&[(0, &large), (0, b"x")]), 1);
        assert_eq!(raw_write_batch_len(&[]), 0);
    }
}
