use super::*;
use crate::backend::StorageBackend;
use crate::storage::guard::GuardedBackend;
use std::sync::Arc;

/// Stitch checked data shards using bounded bulk copies.  Avoids the
/// byte-at-a-time iterator/collect path on verified and reconstructed reads.
pub(super) fn assemble_data_shards<'a>(
    chunks: impl Iterator<Item = &'a [u8]> + Clone,
    compressed_size: usize,
) -> Vec<u8> {
    let available = chunks
        .clone()
        .fold(0usize, |total, chunk| total.saturating_add(chunk.len()));
    let mut compressed = Vec::with_capacity(compressed_size.min(available));
    for chunk in chunks {
        let remaining = compressed_size.saturating_sub(compressed.len());
        if remaining == 0 {
            break;
        }
        compressed.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
    }
    compressed
}

/// Bounded speculative preparation of unencrypted writes. Stripe IDs and
/// placement are allocated only after reacquiring the metadata writer lock.
/// Holding the inode lock keeps same-inode writers serialized.
const PREPARED_PAYLOAD_LIMIT: usize = 8 * 1024 * 1024;
const MAX_PREPARED_WRITES: usize = 4;
static ACTIVE_PREPARATIONS: AtomicUsize = AtomicUsize::new(0);

// Bound extra workers across concurrent FUSE requests, not just per stripe.
// A request always makes progress on its calling thread even if the budget
// is exhausted or the OS refuses to create another worker.
const MAX_SHARD_READ_WORKERS: usize = 32;
static ACTIVE_SHARD_READ_WORKERS: AtomicUsize = AtomicUsize::new(0);

// Range requests may span multiple stripes, including short or sparse
// extents. Share the shard worker budget with stripe-level fanout.
const MAX_RANGE_READ_BATCH: usize = 4;
// A range may refer to stripes as large as MAX_IN_MEMORY_IO_BYTES. Do
// not multiply the previous single-stripe transient memory bound.
const MAX_PARALLEL_RANGE_RAW_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ReadSlice<'a> {
    block: &'a FileBlock,
    source_start: usize,
    destination_start: usize,
    len: usize,
}

struct DecodedSlice {
    bytes: Vec<u8>,
    start: usize,
    damaged: Vec<String>,
}

impl ReadSlice<'_> {
    fn copy_decoded(self, decoded: DecodedSlice, out: &mut [u8], damaged: &mut Vec<String>) {
        out[self.destination_start..self.destination_start + self.len]
            .copy_from_slice(&decoded.bytes[decoded.start..decoded.start + self.len]);
        damaged.extend(decoded.damaged);
    }
}

struct ShardReadPermit;

impl ShardReadPermit {
    fn try_acquire() -> Option<Self> {
        // Keep the MSRV-compatible compare-exchange loop: `fetch_update` was
        // deprecated in newer Rust and `try_update` is not available on the
        // older toolchains this crate supports.
        let mut current = ACTIVE_SHARD_READ_WORKERS.load(Ordering::Acquire);
        while current < MAX_SHARD_READ_WORKERS {
            match ACTIVE_SHARD_READ_WORKERS.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self),
                Err(observed) => current = observed,
            }
        }
        None
    }
}

impl Drop for ShardReadPermit {
    fn drop(&mut self) {
        ACTIVE_SHARD_READ_WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct PreparationPermit(&'static AtomicUsize);
impl PreparationPermit {
    fn acquire(counter: &'static AtomicUsize) -> Option<Self> {
        let mut current = counter.load(Ordering::Acquire);
        while current < MAX_PREPARED_WRITES {
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(counter)),
                Err(observed) => current = observed,
            }
        }
        None
    }
}
impl Drop for PreparationPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EncodingGeometry {
    k: usize,
    m: usize,
    chunk_size: usize,
    compression: Compression,
    level: i32,
}
impl EncodingGeometry {
    fn from_meta(meta: &Metadata) -> Option<Self> {
        if meta.encryption.enabled {
            return None;
        }
        let layout = current_write_layout(meta).ok()?;
        if layout.k == 1 && layout.m == 0 && meta.config.compression == Compression::None {
            return None;
        }
        Some(Self {
            k: layout.k,
            m: layout.m,
            chunk_size: layout.chunk_size,
            compression: meta.config.compression,
            level: meta.config.compression_level,
        })
    }
}

pub(super) struct PreparedStripe {
    pub(super) raw_sha256: String,
    pub(super) codec: Compression,
    pub(super) compressed_size: usize,
    pub(super) shard_size: usize,
    pub(super) encoded: Vec<Vec<u8>>,
    pub(super) shard_hashes: Vec<String>,
}

pub(super) struct PreparedWrite<'a> {
    geometry: EncodingGeometry,
    input: &'a [u8],
    pub(super) stripes: Vec<PreparedStripe>,
    _permit: PreparationPermit,
}

impl<'a> PreparedWrite<'a> {
    fn build(geometry: EncodingGeometry, input: &'a [u8]) -> Option<Self> {
        Self::build_with_budget(geometry, input, &ACTIVE_PREPARATIONS)
    }

    fn build_with_budget(
        geometry: EncodingGeometry,
        input: &'a [u8],
        counter: &'static AtomicUsize,
    ) -> Option<Self> {
        if input.len() <= INLINE_DATA_MAX {
            return None;
        }
        let stripe_size = geometry.chunk_size.checked_mul(geometry.k)?;
        let total = geometry.k.checked_add(geometry.m)?;
        if stripe_size == 0 || stripe_size > PREPARED_PAYLOAD_LIMIT {
            return None;
        }
        // Include worst-case per-stripe padding and descriptor overhead;
        // cap both encoded payload and number of queued stripes.
        let count = input.len().div_ceil(stripe_size);
        let last = input.len() - (count - 1) * stripe_size;
        let bytes = (count - 1)
            .checked_mul(geometry.chunk_size)?
            .checked_add(last.div_ceil(geometry.k))?
            .checked_mul(total)?;
        let overhead = count.checked_mul(total.checked_mul(64)?.checked_add(256)?)?;
        if bytes.checked_add(overhead)? > PREPARED_PAYLOAD_LIMIT {
            return None;
        }
        let permit = PreparationPermit::acquire(counter)?;
        let rs = (geometry.m != 0 || geometry.k != 1)
            .then(|| RsCodec::new(geometry.k, geometry.m))
            .transpose()
            .ok()?;
        let mut stripes = Vec::with_capacity(count);
        for raw in input.chunks(stripe_size) {
            let (payload, codec) =
                transform_stripe(raw, geometry.compression, geometry.level).ok()?;
            let compressed_size = payload.len();
            let (shard_size, encoded) = encode_payload(&payload, geometry.k, rs.as_ref()).ok()?;
            stripes.push(PreparedStripe {
                raw_sha256: content_hash_hex(raw),
                codec,
                compressed_size,
                shard_size,
                shard_hashes: encoded
                    .iter()
                    .map(|shard| content_hash_hex(shard))
                    .collect(),
                encoded,
            });
        }
        Some(Self {
            geometry,
            input,
            stripes,
            _permit: permit,
        })
    }

    pub(super) fn matches(&self, meta: &Metadata, data: &[u8]) -> bool {
        std::ptr::eq(self.input, data)
            && EncodingGeometry::from_meta(meta).as_ref() == Some(&self.geometry)
    }
}

fn transform_stripe(
    raw: &[u8],
    compression: Compression,
    level: i32,
) -> Result<(Vec<u8>, Compression)> {
    let compressed = compress(raw, compression, level)?;
    if compression != Compression::None && compressed.len() >= raw.len() {
        Ok((raw.to_vec(), Compression::None))
    } else {
        Ok((compressed, compression))
    }
}

fn encode_payload(
    payload: &[u8],
    k: usize,
    codec: Option<&RsCodec>,
) -> Result<(usize, Vec<Vec<u8>>)> {
    let Some(codec) = codec else {
        return Ok((payload.len().max(1), vec![payload.to_vec()]));
    };
    let shard_size = payload.len().max(1).div_ceil(k);
    let padded_len = shard_size
        .checked_mul(k)
        .ok_or_else(|| ArgosError::Invalid("encoded shard size overflow".into()))?;
    let mut data_shards = Vec::with_capacity(k);
    for chunk in payload.chunks(shard_size) {
        let mut shard = chunk.to_vec();
        shard.resize(shard_size, 0);
        data_shards.push(shard);
    }
    debug_assert_eq!(padded_len / shard_size, k);
    data_shards.resize_with(k, || vec![0; shard_size]);
    Ok((shard_size, codec.encode_owned(data_shards)?))
}

impl ArgosFs {
    pub(super) fn prepare_write<'a>(
        &self,
        ino: InodeId,
        data: &'a [u8],
        aligned_append: Option<usize>,
    ) -> Option<PreparedWrite<'a>> {
        let geometry = {
            let meta = self.meta.read();
            let inode = meta.inodes.get(&ino)?;
            if inode.kind != NodeKind::File {
                return None;
            }
            if let Some(offset) = aligned_append {
                let (old_size, stripe_size) = self.range_update_geometry_locked(&meta, ino).ok()?;
                if old_size != offset || offset % stripe_size != 0 || inode.inline_data.is_some() {
                    return None;
                }
            }
            EncodingGeometry::from_meta(&meta)?
        };
        // No metadata guard remains during hashing/compression/RS encoding.
        PreparedWrite::build(geometry, data)
    }

    pub(super) fn replace_inode_data(
        &self,
        ino: InodeId,
        data: &[u8],
        action: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        let inode_lock = self.inode_lock(ino);
        let _inode_guard = inode_lock.lock();
        let prepared = self.prepare_write(ino, data, None);
        let mut meta = self.meta.write();
        self.replace_inode_data_prepared_locked(
            &mut meta,
            ino,
            data,
            action,
            details,
            false,
            &BTreeSet::new(),
            prepared,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn replace_inode_data_locked(
        &self,
        meta: &mut Metadata,
        ino: InodeId,
        data: &[u8],
        action: &str,
        details: serde_json::Value,
        preserve_mtime: bool,
        exclude_disks: &BTreeSet<String>,
    ) -> Result<()> {
        self.replace_inode_data_prepared_locked(
            meta,
            ino,
            data,
            action,
            details,
            preserve_mtime,
            exclude_disks,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn replace_inode_data_prepared_locked(
        &self,
        meta: &mut Metadata,
        ino: InodeId,
        data: &[u8],
        action: &str,
        details: serde_json::Value,
        preserve_mtime: bool,
        exclude_disks: &BTreeSet<String>,
        prepared: Option<PreparedWrite<'_>>,
    ) -> Result<()> {
        self.ensure_block_backend_writable_locked(meta)?;
        let rollback = self.commit_previous_with_advisory_locked(meta);
        let (storage_class, boot_critical, old_size) = {
            let inode = meta
                .inodes
                .get(&ino)
                .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
            match inode.kind {
                NodeKind::File => {}
                NodeKind::Directory => {
                    return Err(ArgosError::IsDirectory(format!("inode {ino}")));
                }
                NodeKind::Symlink | NodeKind::Special => {
                    return Err(ArgosError::Unsupported("not a regular file".to_string()));
                }
            }
            (inode.storage_class, inode.boot_critical, inode.size)
        };
        self.ensure_resize_within_quotas_locked(meta, ino, data.len() as u64)?;
        let old_blocks = meta.inodes.get(&ino).unwrap().blocks.clone();
        let inline_payload = inline_payload_for(meta, data);
        let new_blocks = if inline_payload.is_some() {
            Vec::new()
        } else {
            self.encode_data_prepared_locked(
                meta,
                data,
                0,
                storage_class,
                boot_critical,
                exclude_disks,
                prepared,
            )?
        };
        let new_blocks_for_cleanup = new_blocks.clone();
        let now = now_f64();
        let inode = meta.inodes.get_mut(&ino).unwrap();
        inode.blocks = new_blocks.into();
        set_inline_payload(inode, inline_payload);
        inode.size = data.len() as u64;
        inode.write_count = inode.write_count.saturating_add(1);
        inode.write_bytes = inode.write_bytes.saturating_add(data.len() as u64);
        inode.last_written_at = now;
        inode.workload_score = inode.workload_score * 0.90 + 2.0;
        if !preserve_mtime {
            inode.mtime = now;
        }
        inode.ctime = now;
        self.stage_block_reclamation_locked(meta, &old_blocks);
        if let Err(err) = self.commit_locked_with_previous(meta, rollback.as_ref(), action, details)
        {
            if !Self::transaction_error_is_committed(&err) {
                if matches!(err, ArgosError::Conflict(_)) {
                    if meta.backend == BackendKind::Host {
                        self.delete_blocks_locked(meta, &new_blocks_for_cleanup);
                    }
                } else {
                    self.delete_blocks_locked(meta, &new_blocks_for_cleanup);
                    if let Some(rollback) = rollback {
                        *meta = rollback;
                    }
                }
            }
            return Err(err);
        }
        self.finish_block_reclamation_locked(meta, &old_blocks);
        self.adjust_quota_usage_for_resize(ino, old_size, data.len() as u64);
        Ok(())
    }

    pub(super) fn range_update_geometry_locked(
        &self,
        meta: &Metadata,
        ino: InodeId,
    ) -> Result<(usize, usize)> {
        let inode = meta
            .inodes
            .get(&ino)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
        if inode.kind != NodeKind::File {
            return Err(ArgosError::Unsupported(
                "range updates require a regular file".to_string(),
            ));
        }
        let old_size = usize::try_from(inode.size)
            .map_err(|_| ArgosError::Invalid("inode size is too large".to_string()))?;
        let layout = current_write_layout(meta)?;
        let stripe_raw_size = layout_stripe_raw_size(&layout)?;
        Ok((old_size, stripe_raw_size))
    }

    pub(super) fn decode_inode_window_locked(
        &self,
        meta: &mut Metadata,
        ino: InodeId,
        start: usize,
        end: usize,
    ) -> Result<Vec<u8>> {
        if end <= start {
            return Ok(Vec::new());
        }
        let inode = meta
            .inodes
            .get(&ino)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?
            .clone();
        if inode.kind != NodeKind::File {
            return Err(ArgosError::Unsupported(
                "range updates require a regular file".to_string(),
            ));
        }
        if let Some(inline) = decode_inline_data(&inode)? {
            let mut out = zeroed_io_buffer(end - start, "inline read window")?;
            let copy_end = end.min(inline.len());
            if copy_end > start {
                out[..copy_end - start].copy_from_slice(&inline[start..copy_end]);
            }
            return Ok(out);
        }

        self.decode_inode_range_from_inode_locked(meta, &inode, start, end)
            .map(|(data, _)| data)
    }

    pub(super) fn decode_inode_range_from_inode_locked(
        &self,
        meta: &mut Metadata,
        inode: &Inode,
        start: usize,
        end: usize,
    ) -> Result<(Vec<u8>, Vec<String>)> {
        self.decode_inode_range_with_cache_policy_locked(
            meta,
            inode,
            start,
            end,
            BlockCachePolicy::Normal,
        )
    }

    pub(super) fn decode_inode_range_with_cache_policy_locked(
        &self,
        meta: &Metadata,
        inode: &Inode,
        start: usize,
        end: usize,
        cache_policy: BlockCachePolicy,
    ) -> Result<(Vec<u8>, Vec<String>)> {
        if end <= start {
            return Ok((Vec::new(), Vec::new()));
        }
        if let Some(inline) = decode_inline_data(inode)? {
            if end > inline.len() {
                return Err(ArgosError::Invalid(format!(
                    "inline inode {} size is smaller than requested range",
                    inode.id
                )));
            }
            return Ok((inline[start..end].to_vec(), Vec::new()));
        }
        let mut out = zeroed_io_buffer(end - start, "inode read window")?;
        let decrypt_key = if inode.blocks.iter().any(|block| block.encrypted) {
            Some(self.encryption_key_locked(meta)?)
        } else {
            None
        };
        let mut selected = Vec::new();
        for block in &inode.blocks {
            let block_start = usize::try_from(block.raw_offset).map_err(|_| {
                ArgosError::Invalid(format!("block {} raw offset is too large", block.stripe_id))
            })?;
            let block_end = block_start.checked_add(block.raw_size).ok_or_else(|| {
                ArgosError::Invalid(format!("block {} raw range overflow", block.stripe_id))
            })?;
            let copy_start = block_start.max(start);
            let copy_end = block_end.min(end);
            if copy_end > copy_start {
                selected.push(ReadSlice {
                    block,
                    source_start: copy_start - block_start,
                    destination_start: copy_start - start,
                    len: copy_end - copy_start,
                });
            }
        }
        let mut damaged = Vec::new();
        // Preserve the old ordered overwrite semantics for noncanonical
        // overlapping/out-of-order metadata, and keep physical maintenance
        // verification on its existing serial path.
        let ordered = selected
            .windows(2)
            .all(|pair| pair[0].destination_start + pair[0].len <= pair[1].destination_start);
        if cache_policy != BlockCachePolicy::Normal || !ordered {
            for slice in selected {
                if !self.copy_cached_slice(meta, slice, cache_policy, &mut out) {
                    let decoded =
                        self.decode_read_slice(meta, slice, decrypt_key.as_ref(), cache_policy)?;
                    slice.copy_decoded(decoded, &mut out, &mut damaged);
                }
            }
            return Ok((out, damaged));
        }

        // Warm ranges retain the direct-copy path and do not spawn workers.
        // Only independent cold stripe misses overlap, in bounded batches.
        selected.retain(|slice| !self.copy_cached_slice(meta, *slice, cache_policy, &mut out));
        for batch in selected.chunks(MAX_RANGE_READ_BATCH) {
            if batch.len() == 1
                || batch.iter().fold(0usize, |sum, slice| {
                    sum.saturating_add(slice.block.raw_size)
                }) > MAX_PARALLEL_RANGE_RAW_BYTES
            {
                for slice in batch {
                    let decoded =
                        self.decode_read_slice(meta, *slice, decrypt_key.as_ref(), cache_policy)?;
                    slice.copy_decoded(decoded, &mut out, &mut damaged);
                }
                continue;
            }
            let decoded = std::thread::scope(|scope| {
                let mut tasks = Vec::with_capacity(batch.len());
                for (index, &slice) in batch.iter().enumerate() {
                    let key = decrypt_key.as_ref();
                    let task = if index + 1 < batch.len() {
                        ShardReadPermit::try_acquire().and_then(|permit| {
                            std::thread::Builder::new()
                                .spawn_scoped(scope, move || {
                                    let _permit = permit;
                                    self.decode_read_slice(meta, slice, key, cache_policy)
                                })
                                .ok()
                        })
                    } else {
                        None
                    };
                    let inline = if task.is_none() {
                        Some(self.decode_read_slice(meta, slice, key, cache_policy))
                    } else {
                        None
                    };
                    tasks.push((slice, task, inline));
                }
                tasks
                    .into_iter()
                    .map(|(slice, task, inline)| {
                        let result = match task {
                            Some(task) => task.join().expect("parallel stripe reader panicked"),
                            None => inline.expect("inline stripe read completed"),
                        };
                        (slice, result)
                    })
                    .collect::<Vec<_>>()
            });
            // All workers have completed before propagating any error.
            // Apply damage and bytes in the original logical block order.
            for (slice, result) in decoded {
                slice.copy_decoded(result?, &mut out, &mut damaged);
            }
        }
        Ok((out, damaged))
    }

    fn copy_cached_slice(
        &self,
        meta: &Metadata,
        slice: ReadSlice<'_>,
        cache_policy: BlockCachePolicy,
        out: &mut [u8],
    ) -> bool {
        let block = slice.block;
        // The 1+0 uncompressed direct-range path must verify physical
        // subblocks even after a full-block cache admission.
        if cache_policy != BlockCachePolicy::Normal
            || meta.encryption.enabled
            || block.encrypted
            || (block.shards.len() <= 1 && block.codec == Compression::None)
        {
            return false;
        }
        let key = format!("{}:{}:{}", meta.uuid, block.stripe_id, block.raw_sha256);
        self.cache.copy_range(
            &key,
            Some(&block.raw_sha256),
            block.raw_size,
            slice.source_start,
            &mut out[slice.destination_start..slice.destination_start + slice.len],
        )
    }

    fn decode_read_slice(
        &self,
        meta: &Metadata,
        slice: ReadSlice<'_>,
        decrypt_key: Option<&[u8; 32]>,
        cache_policy: BlockCachePolicy,
    ) -> Result<DecodedSlice> {
        let mut damaged = Vec::new();
        if let Some(bytes) = self.decode_block_range_locked(
            meta,
            slice.block,
            slice.source_start,
            slice.source_start + slice.len,
            &mut damaged,
        )? {
            return Ok(DecodedSlice {
                bytes,
                start: 0,
                damaged,
            });
        }
        let bytes = self.decode_block_with_cache_policy_locked(
            meta,
            slice.block,
            decrypt_key,
            &mut damaged,
            cache_policy,
        )?;
        Ok(DecodedSlice {
            bytes,
            start: slice.source_start,
            damaged,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn rewrite_inode_window_locked(
        &self,
        meta: &mut Metadata,
        ino: InodeId,
        affected_start: usize,
        affected_end: usize,
        new_size: usize,
        window: &[u8],
        logical_write_bytes: u64,
        clear_setid: bool,
        preserve_mtime: bool,
        action: &str,
        details: serde_json::Value,
        write_time: Option<BufferedWriteTime>,
    ) -> Result<()> {
        let rollback = self.commit_previous_with_advisory_locked(meta);
        let (storage_class, boot_critical, old_size, old_blocks) = {
            let inode = meta
                .inodes
                .get(&ino)
                .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
            if inode.kind != NodeKind::File {
                return Err(ArgosError::Unsupported(
                    "range updates require a regular file".to_string(),
                ));
            }
            (
                inode.storage_class,
                inode.boot_critical,
                inode.size,
                inode.blocks.clone(),
            )
        };

        self.ensure_resize_within_quotas_locked(meta, ino, new_size as u64)?;

        let mut merged = old_blocks;
        let replaced =
            merged.remove_window(affected_start as u64, affected_end as u64, new_size as u64);

        let inline_payload = if affected_start == 0 && window.len() == new_size {
            inline_payload_for(meta, window)
        } else {
            None
        };
        let written_blocks = if inline_payload.is_some() {
            Vec::new()
        } else if !window.is_empty() {
            self.encode_data_locked(
                meta,
                window,
                affected_start as u64,
                storage_class,
                boot_critical,
                &BTreeSet::new(),
            )?
        } else {
            Vec::new()
        };
        merged.insert_sorted(written_blocks.iter().cloned());

        let now = write_time.map_or_else(now_f64, |time| time.at);
        let inode = meta.inodes.get_mut(&ino).unwrap();
        inode.blocks = merged;
        set_inline_payload(inode, inline_payload);
        inode.size = new_size as u64;
        inode.write_count = inode.write_count.saturating_add(1);
        inode.write_bytes = inode.write_bytes.saturating_add(logical_write_bytes);
        inode.last_written_at = now;
        inode.workload_score = inode.workload_score * 0.90 + 2.0;
        if !preserve_mtime {
            inode.mtime = now;
        }
        inode.ctime = write_time.map_or(now, |time| time.persisted_ctime(inode.ctime));
        if clear_setid {
            inode.mode &= !(libc::S_ISUID | libc::S_ISGID);
        }

        self.stage_block_reclamation_locked(meta, &replaced);
        if let Err(err) = self.commit_locked_with_previous(meta, rollback.as_ref(), action, details)
        {
            if !Self::transaction_error_is_committed(&err) {
                if matches!(err, ArgosError::Conflict(_)) {
                    if meta.backend == BackendKind::Host {
                        self.delete_blocks_locked(meta, &written_blocks);
                    }
                } else {
                    self.delete_blocks_locked(meta, &written_blocks);
                    if let Some(rollback) = rollback {
                        *meta = rollback;
                    }
                }
            }
            return Err(err);
        }
        self.finish_block_reclamation_locked(meta, &replaced);
        // Inline-only updates write no physical shard. Account only for
        // successful range-write windows that actually reached the encoder;
        // truncate and fsck-repair are not foreground range writes.
        if action == "write-range" && !written_blocks.is_empty() {
            self.runtime
                .commit_metrics
                .add_data_plane_bytes(DataPlaneByteKind::RangeRewriteInput, window.len());
        }
        self.adjust_quota_usage_for_resize(ino, old_size, new_size as u64);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn decode_inode_data_locked(
        &self,
        meta: &mut Metadata,
        inode: &Inode,
    ) -> Result<(Vec<u8>, Vec<String>)> {
        self.decode_inode_data_with_cache_policy_locked(meta, inode, BlockCachePolicy::Normal)
    }

    #[cfg(test)]
    pub(super) fn decode_inode_data_with_cache_policy_locked(
        &self,
        meta: &Metadata,
        inode: &Inode,
        cache_policy: BlockCachePolicy,
    ) -> Result<(Vec<u8>, Vec<String>)> {
        let logical_size = usize::try_from(inode.size)
            .map_err(|_| ArgosError::Invalid("inode logical size is too large".to_string()))?;
        if let Some(inline) = decode_inline_data(inode)? {
            if inline.len() != logical_size {
                return Err(ArgosError::Invalid(format!(
                    "inline inode {} size mismatch",
                    inode.id
                )));
            }
            return Ok((inline, Vec::new()));
        }
        let mut out = zeroed_io_buffer(logical_size, "full inode decode")?;
        let mut damaged = Vec::new();
        let decrypt_key = if inode.blocks.iter().any(|block| block.encrypted) {
            Some(self.encryption_key_locked(meta)?)
        } else {
            None
        };
        for block in &inode.blocks {
            let raw = self.decode_block_with_cache_policy_locked(
                meta,
                block,
                decrypt_key.as_ref(),
                &mut damaged,
                cache_policy,
            )?;
            let block_start = usize::try_from(block.raw_offset).map_err(|_| {
                ArgosError::Invalid(format!("block {} raw offset is too large", block.stripe_id))
            })?;
            let block_end = block_start.checked_add(raw.len()).ok_or_else(|| {
                ArgosError::Invalid(format!("block {} raw range overflow", block.stripe_id))
            })?;
            if block_end > logical_size {
                return Err(ArgosError::Invalid(format!(
                    "block {} extends past inode size",
                    block.stripe_id
                )));
            }
            out[block_start..block_end].copy_from_slice(&raw);
        }
        Ok((out, damaged))
    }

    pub(super) fn decode_block_locked(
        &self,
        meta: &mut Metadata,
        block: &FileBlock,
        decrypt_key: Option<&[u8; 32]>,
        damaged: &mut Vec<String>,
    ) -> Result<Vec<u8>> {
        self.decode_block_with_cache_policy_locked(
            meta,
            block,
            decrypt_key,
            damaged,
            BlockCachePolicy::Normal,
        )
    }

    /// Read the shards of one stripe concurrently.  Independent members have
    /// independent backend queues and storage devices, so serial reads make a
    /// 4+4 stripe pay four full device latencies.  Results are returned in the
    /// caller's slot order so damage reporting stays deterministic.
    fn read_block_shards_parallel(
        &self,
        meta: &Metadata,
        pending: Vec<(usize, &Shard)>,
        shards: &mut [Option<Vec<u8>>],
        damaged: &mut Vec<String>,
    ) {
        // Keep one read on the caller and only spawn for the remaining
        // independent members. Preserve the slot order when consuming results.
        let results = std::thread::scope(|scope| {
            let mut reads = Vec::with_capacity(pending.len());
            let mut pending = pending.into_iter().peekable();
            while let Some((slot, shard)) = pending.next() {
                let worker = if pending.peek().is_some() {
                    ShardReadPermit::try_acquire().and_then(|permit| {
                        std::thread::Builder::new()
                            .spawn_scoped(scope, move || {
                                let _permit = permit;
                                self.read_shard_verified_locked(meta, shard)
                            })
                            .ok()
                    })
                } else {
                    None
                };
                let inline = if worker.is_none() {
                    Some(self.read_shard_verified_locked(meta, shard))
                } else {
                    None
                };
                reads.push((slot, shard, worker, inline));
            }
            reads
                .into_iter()
                .map(|(slot, shard, worker, inline)| {
                    let result = match worker {
                        Some(worker) => worker.join().expect("parallel shard reader panicked"),
                        None => inline.expect("inline shard read completed"),
                    };
                    (slot, shard, result)
                })
                .collect::<Vec<_>>()
        });
        for (slot, shard, result) in results {
            match result {
                Ok(Some(data)) => {
                    shards[slot] = Some(data);
                }
                Ok(None) => {
                    damaged.push(format!("{}:checksum:{}", shard.disk_id, slot));
                }
                Err(_) => {
                    damaged.push(format!("{}:missing:{}", shard.disk_id, slot));
                }
            }
        }
    }

    fn decode_block_with_cache_policy_locked(
        &self,
        meta: &Metadata,
        block: &FileBlock,
        decrypt_key: Option<&[u8; 32]>,
        damaged: &mut Vec<String>,
        cache_policy: BlockCachePolicy,
    ) -> Result<Vec<u8>> {
        let cache_key = format!("{}:{}:{}", meta.uuid, block.stripe_id, block.raw_sha256);
        if block.encrypted || meta.encryption.enabled {
            self.cache.remove(&cache_key);
        } else if cache_policy == BlockCachePolicy::Normal {
            let mut flight_damage = Vec::new();
            let decoded = self.runtime.decode_flights.run(&cache_key, || {
                if let Some(raw) = self.cache.get(&cache_key, Some(&block.raw_sha256)) {
                    if raw.len() == block.raw_size {
                        return Ok((raw, Vec::new()));
                    }
                    self.cache.remove(&cache_key);
                }
                let raw = self.decode_uncached_block_locked(
                    meta,
                    block,
                    decrypt_key,
                    &mut flight_damage,
                    cache_policy,
                    &cache_key,
                )?;
                Ok((raw, std::mem::take(&mut flight_damage)))
            });
            return match decoded {
                Ok((raw, flight_damage)) => {
                    damaged.extend(flight_damage);
                    Ok(raw)
                }
                Err(err) => {
                    damaged.extend(flight_damage);
                    Err(err)
                }
            };
        }
        self.decode_uncached_block_locked(
            meta,
            block,
            decrypt_key,
            damaged,
            cache_policy,
            &cache_key,
        )
    }

    fn decode_uncached_block_locked(
        &self,
        meta: &Metadata,
        block: &FileBlock,
        decrypt_key: Option<&[u8; 32]>,
        damaged: &mut Vec<String>,
        cache_policy: BlockCachePolicy,
        cache_key: &str,
    ) -> Result<Vec<u8>> {
        let _decode = self
            .runtime
            .commit_metrics
            .read_timer(crate::commit_metrics::ReadPhase::StripeDecode);
        let layout = layout_by_id(meta, block_layout_id(block))?;
        if layout.k == 1 && layout.m == 0 && !block.encrypted && block.codec == Compression::None {
            return self.decode_single_shard_block_with_cache_policy_locked(
                meta,
                block,
                damaged,
                cache_key,
                cache_policy,
            );
        }
        let mut shards: Vec<Option<Vec<u8>>> = vec![None; layout_total(&layout)?];
        // Erasure shards 0..k are systematic data. For normal reads, fetching
        // their verified bytes is sufficient: neither parity I/O nor an RS
        // matrix operation is needed when all data members are healthy.
        // Scrub and repair explicitly bypass the reconstructed cache and
        // continue to read *every* parity member for physical verification.
        // If any data shard fails, fetch parity before reconstructing.
        for parity_pass in [false, true] {
            if parity_pass
                && cache_policy == BlockCachePolicy::Normal
                && shards[..layout.k].iter().all(Option::is_some)
            {
                break;
            }
            let mut pending = Vec::new();
            for shard in &block.shards {
                if (shard.slot >= layout.k) != parity_pass {
                    continue;
                }
                if shard.slot >= shards.len() {
                    damaged.push(format!("{}:invalid-slot:{}", shard.disk_id, shard.slot));
                    continue;
                }
                let Some(disk) = meta.disks.get(&shard.disk_id) else {
                    damaged.push(format!("{}:missing-disk", shard.disk_id));
                    continue;
                };
                if matches!(
                    disk.status,
                    DiskStatus::Failed | DiskStatus::Offline | DiskStatus::Removed
                ) {
                    damaged.push(format!("{}:unavailable", shard.disk_id));
                    continue;
                }
                pending.push((shard.slot, shard));
            }
            self.read_block_shards_parallel(meta, pending, &mut shards, damaged);
        }
        let compressed: Vec<u8> = if shards[..layout.k].iter().all(Option::is_some) {
            // Each data shard has already passed its individual hash. The
            // reconstructed block is checked against raw_sha256 below too.
            assemble_data_shards(
                shards
                    .iter()
                    .take(layout.k)
                    .map(|shard| shard.as_deref().unwrap()),
                block.compressed_size,
            )
        } else {
            let present = shards.iter().filter(|shard| shard.is_some()).count();
            if present < layout.k {
                return Err(ArgosError::UnrecoverableStripe {
                    stripe_id: block.stripe_id.clone(),
                    reason: format!("only {present} shards available, need {}", layout.k),
                });
            }
            let reconstructed = RsCodec::reconstruct_data_cached(layout.k, layout.m, shards)?;
            assemble_data_shards(
                reconstructed.iter().take(layout.k).map(Vec::as_slice),
                block.compressed_size,
            )
        };
        let compressed = if block.encrypted {
            let nonce = hex::decode(&block.nonce_hex).map_err(|err| {
                ArgosError::Invalid(format!("invalid encrypted block nonce: {err}"))
            })?;
            let key = decrypt_key.ok_or_else(|| {
                ArgosError::PermissionDenied("missing ArgosFS encryption key".to_string())
            })?;
            crypto::decrypt_with_key(
                key,
                &nonce,
                &compressed,
                &encryption_aad(&meta.uuid, &block.stripe_id),
            )?
        } else {
            compressed
        };
        // Plain blocks are already assembled/decrypted into an owned buffer.
        // Decompress(None) would allocate and copy the entire stripe again.
        let raw = if block.codec == Compression::None {
            compressed
        } else {
            decompress(&compressed, block.codec)?
        };
        if raw.len() != block.raw_size || !content_hash_matches(&raw, &block.raw_sha256) {
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "raw checksum mismatch".to_string(),
            });
        }
        if !meta.encryption.enabled && !block.encrypted && cache_policy == BlockCachePolicy::Normal
        {
            self.cache.put(cache_key, &raw)?;
        }
        Ok(raw)
    }

    pub(super) fn decode_block_range_locked(
        &self,
        meta: &Metadata,
        block: &FileBlock,
        start: usize,
        end: usize,
        damaged: &mut Vec<String>,
    ) -> Result<Option<Vec<u8>>> {
        if start >= end {
            return Ok(Some(Vec::new()));
        }
        if block.encrypted || block.codec != Compression::None || end > block.raw_size {
            return Ok(None);
        }
        let layout = layout_by_id(meta, block_layout_id(block))?;
        if layout.k != 1 || layout.m != 0 {
            return Ok(None);
        }
        let Some(shard) = block.shards.iter().find(|shard| shard.slot == 0) else {
            damaged.push("single-shard:missing-slot-0".to_string());
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "single-device block has no shard 0".to_string(),
            });
        };
        if shard.size != block.raw_size
            || shard.sha256 != block.raw_sha256
            || shard.checksum_block_size == 0
            || shard.subblock_sha256.is_empty()
        {
            return Ok(None);
        }
        let checksum_block_size = shard.checksum_block_size;
        let expected_checksums = shard.size.div_ceil(checksum_block_size);
        if shard.subblock_sha256.len() != expected_checksums {
            return Ok(None);
        }
        let Some(disk) = meta.disks.get(&shard.disk_id) else {
            damaged.push(format!("{}:missing-disk", shard.disk_id));
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "single-device block references a missing disk".to_string(),
            });
        };
        if matches!(
            disk.status,
            DiskStatus::Failed | DiskStatus::Offline | DiskStatus::Removed
        ) {
            damaged.push(format!("{}:unavailable", shard.disk_id));
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "single-device shard is unavailable".to_string(),
            });
        }

        let verify_start = (start / checksum_block_size) * checksum_block_size;
        let verify_end = end.div_ceil(checksum_block_size) * checksum_block_size;
        let verify_end = verify_end.min(shard.size);
        let start_time = std::time::Instant::now();
        let data = match self.read_shard_range_locked(
            meta,
            shard,
            verify_start,
            verify_end.saturating_sub(verify_start),
        ) {
            Ok(data) => data,
            Err(err) => {
                if let Some(disk) = meta.disks.get(&shard.disk_id) {
                    self.runtime
                        .read_advisory
                        .disk(disk, 0, start_time.elapsed().as_secs_f64());
                }
                damaged.push(format!("{}:missing-range:{}", shard.disk_id, shard.slot));
                return Err(err);
            }
        };
        if let Some(disk) = meta.disks.get(&shard.disk_id) {
            self.runtime.read_advisory.disk(
                disk,
                data.len() as u64,
                start_time.elapsed().as_secs_f64(),
            );
        }

        let first_checksum = verify_start / checksum_block_size;
        let last_checksum = verify_end.div_ceil(checksum_block_size);
        for checksum_index in first_checksum..last_checksum {
            let absolute_start = checksum_index * checksum_block_size;
            let absolute_end = absolute_start
                .saturating_add(checksum_block_size)
                .min(shard.size);
            let relative_start = absolute_start.saturating_sub(verify_start);
            let relative_end = absolute_end.saturating_sub(verify_start);
            if relative_end > data.len()
                || !content_hash_matches(
                    &data[relative_start..relative_end],
                    &shard.subblock_sha256[checksum_index],
                )
            {
                damaged.push(format!(
                    "{}:subblock-checksum:{}:{}",
                    shard.disk_id, shard.slot, checksum_index
                ));
                return Err(ArgosError::UnrecoverableStripe {
                    stripe_id: block.stripe_id.clone(),
                    reason: "single-device subblock checksum mismatch".to_string(),
                });
            }
        }

        let local_start = start - verify_start;
        let local_end = end - verify_start;
        Ok(Some(data[local_start..local_end].to_vec()))
    }

    #[cfg(test)]
    pub(super) fn decode_single_shard_block_locked(
        &self,
        meta: &mut Metadata,
        block: &FileBlock,
        damaged: &mut Vec<String>,
        cache_key: &str,
    ) -> Result<Vec<u8>> {
        self.decode_single_shard_block_with_cache_policy_locked(
            meta,
            block,
            damaged,
            cache_key,
            BlockCachePolicy::Normal,
        )
    }

    fn decode_single_shard_block_with_cache_policy_locked(
        &self,
        meta: &Metadata,
        block: &FileBlock,
        damaged: &mut Vec<String>,
        cache_key: &str,
        cache_policy: BlockCachePolicy,
    ) -> Result<Vec<u8>> {
        let Some(shard) = block.shards.iter().find(|shard| shard.slot == 0) else {
            damaged.push("single-shard:missing-slot-0".to_string());
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "single-device block has no shard 0".to_string(),
            });
        };
        let Some(disk) = meta.disks.get(&shard.disk_id) else {
            damaged.push(format!("{}:missing-disk", shard.disk_id));
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "single-device block references a missing disk".to_string(),
            });
        };
        if matches!(
            disk.status,
            DiskStatus::Failed | DiskStatus::Offline | DiskStatus::Removed
        ) {
            damaged.push(format!("{}:unavailable", shard.disk_id));
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "single-device shard is unavailable".to_string(),
            });
        }
        let data = match self.read_shard_verified_locked(meta, shard) {
            Ok(Some(data)) => data,
            Ok(None) => {
                damaged.push(format!("{}:checksum:{}", shard.disk_id, shard.slot));
                return Err(ArgosError::UnrecoverableStripe {
                    stripe_id: block.stripe_id.clone(),
                    reason: "single-device raw checksum mismatch".to_string(),
                });
            }
            Err(err) => {
                damaged.push(format!("{}:missing:{}", shard.disk_id, shard.slot));
                return Err(err);
            }
        };
        if data.len() != block.raw_size || !content_hash_matches(&data, &block.raw_sha256) {
            damaged.push(format!("{}:checksum:{}", shard.disk_id, shard.slot));
            return Err(ArgosError::UnrecoverableStripe {
                stripe_id: block.stripe_id.clone(),
                reason: "single-device raw checksum mismatch".to_string(),
            });
        }
        if !meta.encryption.enabled && cache_policy == BlockCachePolicy::Normal {
            self.cache.put(cache_key, &data)?;
        }
        Ok(data)
    }

    pub(super) fn encode_data_locked(
        &self,
        meta: &mut Metadata,
        data: &[u8],
        base_offset: u64,
        storage_class: StorageTier,
        boot_critical: bool,
        exclude_disks: &BTreeSet<String>,
    ) -> Result<Vec<FileBlock>> {
        self.encode_data_prepared_locked(
            meta,
            data,
            base_offset,
            storage_class,
            boot_critical,
            exclude_disks,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn encode_data_prepared_locked(
        &self,
        meta: &mut Metadata,
        data: &[u8],
        base_offset: u64,
        storage_class: StorageTier,
        boot_critical: bool,
        exclude_disks: &BTreeSet<String>,
        mut prepared: Option<PreparedWrite<'_>>,
    ) -> Result<Vec<FileBlock>> {
        if prepared.as_ref().is_some_and(|p| !p.matches(meta, data)) {
            prepared = None;
        }
        let rollback = meta.clone();
        let mut created_shards = Vec::new();
        match self.encode_data_locked_inner(
            meta,
            data,
            base_offset,
            storage_class,
            boot_critical,
            exclude_disks,
            &mut created_shards,
            prepared,
        ) {
            Ok(blocks) => Ok(blocks),
            Err(err) => {
                for shard in &created_shards {
                    let _ = self.delete_shard_locked(meta, shard);
                }
                *meta = rollback;
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn encode_data_locked_inner(
        &self,
        meta: &mut Metadata,
        data: &[u8],
        base_offset: u64,
        storage_class: StorageTier,
        boot_critical: bool,
        exclude_disks: &BTreeSet<String>,
        created_shards: &mut Vec<Shard>,
        prepared: Option<PreparedWrite<'_>>,
    ) -> Result<Vec<FileBlock>> {
        let mut blocks = Vec::new();
        let layout = current_write_layout(meta)?;
        let stripe_raw_size = layout_stripe_raw_size(&layout)?;
        if data.is_empty() {
            return Ok(blocks);
        }
        let encrypt_key = if meta.encryption.enabled {
            Some(self.encryption_key_locked(meta)?)
        } else {
            None
        };
        let mut prepared = prepared;
        // Keep the permit live while draining the pre-encoded stripes.
        let mut prepared_stripes = prepared
            .as_mut()
            .map(|p| std::mem::take(&mut p.stripes).into_iter());
        let rs = (layout.k != 1 || layout.m != 0)
            .then(|| RsCodec::new(layout.k, layout.m))
            .transpose()?;
        for (index, raw) in data.chunks(stripe_raw_size).enumerate() {
            let stripe_id = format!("s{:016x}", meta.next_stripe);
            meta.next_stripe = meta
                .next_stripe
                .checked_add(1)
                .ok_or_else(|| ArgosError::Invalid("stripe id overflow".to_string()))?;
            let preencoded = prepared_stripes.as_mut().and_then(Iterator::next);
            let raw_sha256 = preencoded
                .as_ref()
                .map(|p| p.raw_sha256.clone())
                .unwrap_or_else(|| content_hash_hex(raw));
            if layout.k == 1
                && layout.m == 0
                && encrypt_key.is_none()
                && meta.config.compression == Compression::None
            {
                let shard_size = raw.len().max(1);
                let placements = self.choose_disks_locked(
                    meta,
                    PlacementRequest {
                        key: &stripe_id,
                        count: 1,
                        storage_class,
                        boot_critical,
                        exclude_disks,
                        required_bytes: shard_size as u64,
                    },
                )?;
                let integrity = ShardIntegrity {
                    sha256: raw_sha256.clone(),
                    checksum_block_size: SHARD_CHECKSUM_BLOCK_SIZE,
                    subblock_sha256: shard_subblock_hashes(raw, &raw_sha256),
                };
                let shard = self.write_shard_locked(
                    meta,
                    &placements[0],
                    &stripe_id,
                    0,
                    raw,
                    Some(&integrity),
                )?;
                created_shards.push(shard.clone());
                let raw_offset = index
                    .checked_mul(stripe_raw_size)
                    .and_then(|offset| u64::try_from(offset).ok())
                    .and_then(|offset| base_offset.checked_add(offset))
                    .ok_or_else(|| ArgosError::Invalid("raw block offset overflow".to_string()))?;
                blocks.push(FileBlock {
                    layout_id: layout.id.clone(),
                    stripe_id,
                    raw_offset,
                    raw_size: raw.len(),
                    raw_sha256,
                    codec: Compression::None,
                    encrypted: false,
                    nonce_hex: String::new(),
                    compressed_size: raw.len(),
                    shard_size,
                    shards: vec![shard],
                    storage_class,
                });
                continue;
            }
            let (codec, encrypted, nonce_hex, compressed_size, shard_size, encoded) =
                if let Some(p) = preencoded {
                    (
                        p.codec,
                        false,
                        String::new(),
                        p.compressed_size,
                        p.shard_size,
                        p.encoded,
                    )
                } else {
                    let (transformed, codec) = transform_stripe(
                        raw,
                        meta.config.compression,
                        meta.config.compression_level,
                    )?;
                    let (payload, encrypted, nonce_hex) = if let Some(key) = encrypt_key.as_ref() {
                        let (nonce, ciphertext) = crypto::encrypt_with_key(
                            key,
                            &transformed,
                            &encryption_aad(&meta.uuid, &stripe_id),
                        )?;
                        (ciphertext, true, hex::encode(nonce))
                    } else {
                        (transformed, false, String::new())
                    };
                    let (shard_size, encoded) = encode_payload(&payload, layout.k, rs.as_ref())?;
                    (
                        codec,
                        encrypted,
                        nonce_hex,
                        payload.len(),
                        shard_size,
                        encoded,
                    )
                };
            let single_raw_shard_integrity =
                if layout.k == 1 && layout.m == 0 && !encrypted && codec == Compression::None {
                    Some(ShardIntegrity {
                        sha256: raw_sha256.clone(),
                        checksum_block_size: SHARD_CHECKSUM_BLOCK_SIZE,
                        subblock_sha256: shard_subblock_hashes(raw, &raw_sha256),
                    })
                } else {
                    None
                };
            let placements = self.choose_disks_locked(
                meta,
                PlacementRequest {
                    key: &stripe_id,
                    count: layout_total(&layout)?,
                    storage_class,
                    boot_critical,
                    exclude_disks,
                    required_bytes: shard_size as u64,
                },
            )?;
            let shards = if meta.backend != BackendKind::Host
                && encoded.len() > 1
                && !journal::crash_injection_configured()
                && placements.iter().collect::<BTreeSet<_>>().len() == placements.len()
            {
                let shards =
                    self.write_raw_stripe_locked(meta, &placements, &stripe_id, &encoded)?;
                created_shards.extend(shards.iter().cloned());
                shards
            } else {
                let mut shards = Vec::new();
                for (slot, shard_data) in encoded.iter().enumerate() {
                    let integrity = if slot == 0 {
                        single_raw_shard_integrity.as_ref()
                    } else {
                        None
                    };
                    let shard = self.write_shard_locked(
                        meta,
                        &placements[slot],
                        &stripe_id,
                        slot,
                        shard_data,
                        integrity,
                    )?;
                    created_shards.push(shard.clone());
                    shards.push(shard);
                }
                shards
            };
            let raw_offset = index
                .checked_mul(stripe_raw_size)
                .and_then(|offset| u64::try_from(offset).ok())
                .and_then(|offset| base_offset.checked_add(offset))
                .ok_or_else(|| ArgosError::Invalid("raw block offset overflow".to_string()))?;
            blocks.push(FileBlock {
                layout_id: layout.id.clone(),
                stripe_id,
                raw_offset,
                raw_size: raw.len(),
                raw_sha256,
                codec,
                encrypted,
                nonce_hex,
                compressed_size,
                shard_size,
                shards,
                storage_class,
            });
        }
        Ok(blocks)
    }

    /// Reserve COW extents under the metadata lock, write independent members
    /// concurrently, and publish shard descriptors only after every worker
    /// has completed. encode_data_locked owns the pre-allocation rollback
    /// snapshot; on error no worker can still write a reclaimed extent.
    fn write_raw_stripe_locked(
        &self,
        meta: &mut Metadata,
        placements: &[String],
        stripe_id: &str,
        encoded: &[Vec<u8>],
    ) -> Result<Vec<Shard>> {
        self.ensure_block_backend_writable_locked(meta)?;
        let mut reservations = Vec::with_capacity(encoded.len());
        for (slot, (disk_id, data)) in placements.iter().zip(encoded).enumerate() {
            self.ensure_disk_capacity_locked(meta, disk_id, data.len() as u64)?;
            let allocator = meta
                .raw_pool
                .allocators
                .get_mut(disk_id)
                .ok_or_else(|| ArgosError::MissingDevice(disk_id.clone()))?;
            let extent = allocator::allocate(allocator, disk_id, data.len() as u64, meta.txid + 1)?;
            reservations.push((slot, extent, data));
        }
        let flush = !bulk_import_enabled() && !meta.config.defer_data_flush;
        let snapshot = &*meta;
        let writes = std::thread::scope(|scope| {
            let jobs = reservations
                .into_iter()
                .map(|(slot, extent, data)| {
                    scope.spawn(move || -> Result<(Shard, f64)> {
                        let sha256 = content_hash_hex(data);
                        let start = Instant::now();
                        self.backend_write_at_locked(
                            snapshot,
                            &extent.disk_id,
                            extent.offset,
                            data,
                        )?;
                        if flush {
                            self.backend_flush_locked(snapshot, &extent.disk_id)?;
                        }
                        let elapsed = start.elapsed().as_secs_f64();
                        self.runtime.commit_metrics.add_data_plane_bytes(
                            DataPlaneByteKind::ShardPayloadWritten,
                            data.len(),
                        );
                        Ok((
                            Shard {
                                slot,
                                disk_id: extent.disk_id.clone(),
                                location: Some(ShardLocation::RawExtent(extent)),
                                relpath: PathBuf::new(),
                                sha256,
                                checksum_block_size: 0,
                                subblock_sha256: Vec::new(),
                                size: data.len(),
                            },
                            elapsed,
                        ))
                    })
                })
                .collect::<Vec<_>>();
            // Join all writers even if one failed, before rollback can reuse
            // any reserved extent. Return the first error in slot order.
            jobs.into_iter()
                .map(|job| job.join().expect("raw shard writer panicked"))
                .collect::<Vec<_>>()
        });
        let written = writes.into_iter().collect::<Result<Vec<_>>>()?;
        let mut shards = Vec::with_capacity(written.len());
        for (shard, elapsed) in written {
            self.record_raw_shard_locked(meta, stripe_id, &shard, elapsed);
            shards.push(shard);
        }
        Ok(shards)
    }

    fn record_raw_shard_locked(
        &self,
        meta: &mut Metadata,
        stripe_id: &str,
        shard: &Shard,
        elapsed: f64,
    ) {
        let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
            unreachable!("raw shard required")
        };
        if let Some(disk) = meta.disks.get_mut(&shard.disk_id) {
            disk.used_bytes = disk.used_bytes.saturating_add(extent.length);
        }
        self.update_write_latency_locked(meta, &shard.disk_id, shard.size as u64, elapsed);
        if let Some(verify) = &self.verify_written {
            verify.enqueue(
                &shard.disk_id,
                extent.offset,
                extent.generation,
                shard.size,
                &shard.sha256,
                stripe_id,
            );
        }
    }

    pub(super) fn write_shard_locked(
        &self,
        meta: &mut Metadata,
        disk_id: &str,
        stripe_id: &str,
        slot: usize,
        data: &[u8],
        integrity: Option<&ShardIntegrity>,
    ) -> Result<Shard> {
        let sha256 = integrity
            .map(|integrity| integrity.sha256.clone())
            .unwrap_or_else(|| content_hash_hex(data));
        let checksum_block_size = integrity
            .map(|integrity| integrity.checksum_block_size)
            .unwrap_or_default();
        let subblock_sha256 = integrity
            .map(|integrity| integrity.subblock_sha256.clone())
            .unwrap_or_default();
        if meta.backend != BackendKind::Host {
            self.ensure_block_backend_writable_locked(meta)?;
            self.ensure_disk_capacity_locked(meta, disk_id, data.len() as u64)?;
            let allocator = meta
                .raw_pool
                .allocators
                .get_mut(disk_id)
                .ok_or_else(|| ArgosError::MissingDevice(disk_id.to_string()))?;
            let extent = allocator::allocate(allocator, disk_id, data.len() as u64, meta.txid + 1)?;
            let start = std::time::Instant::now();
            let write_result = (|| -> Result<()> {
                journal::inject_crash(FaultPoint::BeforeDataWrite.as_str())?;
                self.backend_write_at_locked(meta, disk_id, extent.offset, data)?;
                journal::inject_crash(FaultPoint::AfterDataWriteBeforeFlush.as_str())?;
                if !bulk_import_enabled() && !meta.config.defer_data_flush {
                    self.backend_flush_locked(meta, disk_id)?;
                    journal::inject_crash(FaultPoint::AfterDataFlushBeforeJournalCommit.as_str())?;
                }
                Ok(())
            })();
            if let Err(err) = write_result {
                if let Some(allocator) = meta.raw_pool.allocators.get_mut(disk_id) {
                    if extent.offset.saturating_add(extent.length) == allocator.next_offset {
                        allocator.next_offset = extent.offset;
                    } else {
                        let _ = allocator::free(allocator, &extent);
                    }
                }
                return Err(err);
            }
            let shard = Shard {
                slot,
                disk_id: disk_id.to_string(),
                location: Some(ShardLocation::RawExtent(extent)),
                relpath: PathBuf::new(),
                sha256,
                checksum_block_size,
                subblock_sha256,
                size: data.len(),
            };
            self.record_raw_shard_locked(meta, stripe_id, &shard, start.elapsed().as_secs_f64());
            self.runtime
                .commit_metrics
                .add_data_plane_bytes(DataPlaneByteKind::ShardPayloadWritten, data.len());
            return Ok(shard);
        }
        let subdir = &stripe_id[stripe_id.len().saturating_sub(2)..];
        let relpath = PathBuf::from(format!("shards/{subdir}/{stripe_id}.{slot:03}.blk"));
        let path = self.shard_path_locked(meta, disk_id, &relpath);
        if let Some(parent) = path.parent() {
            ensure_private_dir(parent)?;
        }
        self.ensure_disk_capacity_locked(meta, disk_id, data.len() as u64)?;
        let start = std::time::Instant::now();
        if let Err(err) = advanced_io::write_all(&path, data, meta.config.io_mode) {
            let _ = fs::remove_file(&path);
            return Err(err);
        }
        self.mark_host_shard_dirty(path.clone());
        if let Some(parent) = path.parent() {
            sync_directory(parent);
            if let Some(grandparent) = parent.parent() {
                sync_directory(grandparent);
            }
        }
        if let Some(disk) = meta.disks.get_mut(disk_id) {
            disk.used_bytes = disk.used_bytes.saturating_add(data.len() as u64);
        }
        self.update_write_latency_locked(
            meta,
            disk_id,
            data.len() as u64,
            start.elapsed().as_secs_f64(),
        );
        self.runtime
            .commit_metrics
            .add_data_plane_bytes(DataPlaneByteKind::ShardPayloadWritten, data.len());
        Ok(Shard {
            slot,
            disk_id: disk_id.to_string(),
            location: Some(ShardLocation::HostPath {
                disk_id: disk_id.to_string(),
                relpath: relpath.clone(),
            }),
            relpath,
            sha256,
            checksum_block_size,
            subblock_sha256,
            size: data.len(),
        })
    }

    pub(super) fn choose_disks_locked(
        &self,
        meta: &Metadata,
        request: PlacementRequest<'_>,
    ) -> Result<Vec<String>> {
        let quarantined = self.quarantined_devices.lock().clone();
        if request.count == 1 {
            let mut only = None;
            let mut eligible = 0usize;
            for (disk_id, disk) in &meta.disks {
                if request.exclude_disks.contains(disk_id)
                    || quarantined.contains(disk_id)
                    || disk.status != DiskStatus::Online
                {
                    continue;
                }
                if meta.backend == BackendKind::Host {
                    let disk_path = relative_or_absolute(&self.root, &disk.path);
                    if !disk_path.join("shards").exists() {
                        continue;
                    }
                }
                if !self.disk_has_capacity(meta, disk_id, disk, request.required_bytes) {
                    continue;
                }
                eligible += 1;
                only = Some(disk_id.clone());
                if eligible > 1 {
                    break;
                }
            }
            if eligible == 1 {
                return Ok(vec![only.expect("eligible disk id")]);
            }
        }
        let mut scored = Vec::new();
        let local_numa = if bulk_import_enabled() {
            None
        } else {
            meta.config
                .numa_aware
                .then(advanced_io::current_numa_node)
                .flatten()
        };
        for (disk_id, disk) in &meta.disks {
            if request.exclude_disks.contains(disk_id)
                || quarantined.contains(disk_id)
                || disk.status != DiskStatus::Online
            {
                continue;
            }
            if meta.backend == BackendKind::Host {
                let disk_path = relative_or_absolute(&self.root, &disk.path);
                if !disk_path.join("shards").exists() {
                    continue;
                }
            }
            if !self.disk_has_capacity(meta, disk_id, disk, request.required_bytes) {
                continue;
            }
            let tier_bonus = match (request.storage_class, disk.tier) {
                (StorageTier::Hot, StorageTier::Hot) => 2.5,
                (StorageTier::Hot, StorageTier::Cold) => 0.45,
                (StorageTier::Cold, StorageTier::Cold) => 2.2,
                (StorageTier::Cold, StorageTier::Hot) => 0.55,
                _ => 1.0,
            };
            let u = stable_u01(&[&meta.uuid, request.key, disk_id]);
            let latency_penalty = 1.0
                + ((disk.read_latency_ewma_ms + disk.write_latency_ewma_ms) / 2.0 / 20.0).min(4.0);
            let mut score = (-u.ln() * latency_penalty) / (disk.weight.max(0.01) * tier_bonus);
            if let (Some(local), Some(remote)) = (local_numa, disk.numa_node) {
                if local == remote {
                    score *= 0.90;
                } else {
                    score *= 1.10;
                }
            }
            if disk.capacity_bytes > 0 {
                let used = self.effective_used_bytes_locked(meta, disk);
                let capacity = self.effective_capacity_bytes_locked(meta, disk);
                if capacity > 0 {
                    score += (used as f64 / capacity as f64).min(2.0);
                }
            }
            if request.boot_critical && disk.tier == StorageTier::Cold {
                score *= 1.35;
            }
            scored.push((score, disk_id.clone()));
        }
        if scored.len() < request.count {
            return Err(ArgosError::NotEnoughDisks {
                need: request.count,
                have: scored.len(),
            });
        }
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        let mut selected = Vec::new();
        let mut domains = BTreeSet::new();
        let mut reserved_by_capacity_group = BTreeMap::<String, u64>::new();

        let capacity_group = |disk: &Disk| -> String {
            if disk.capacity_source == CapacitySource::AutoProbe {
                if let Some(fs_id) = disk.backing_fs_id.as_deref() {
                    return format!("fs:{fs_id}");
                }
            }
            format!("disk:{}", disk.id)
        };

        let can_reserve = |reservations: &BTreeMap<String, u64>, disk: &Disk| -> bool {
            let capacity = self.effective_capacity_bytes_locked(meta, disk);
            if capacity == 0 {
                return true;
            }
            let used = self.effective_used_bytes_locked(meta, disk);
            let reserved = reservations
                .get(&capacity_group(disk))
                .copied()
                .unwrap_or(0);
            used.saturating_add(reserved)
                .saturating_add(request.required_bytes)
                <= capacity
        };

        let reserve = |reservations: &mut BTreeMap<String, u64>, disk: &Disk| {
            let capacity = self.effective_capacity_bytes_locked(meta, disk);
            if capacity != 0 {
                let group = capacity_group(disk);
                let current = reservations.get(&group).copied().unwrap_or(0);
                reservations.insert(group, current.saturating_add(request.required_bytes));
            }
        };

        for (_, id) in &scored {
            let Some(disk) = meta.disks.get(id) else {
                continue;
            };
            if !can_reserve(&reserved_by_capacity_group, disk) {
                continue;
            }
            let domain = disk.failure_domain.as_str();
            if domains.insert(domain.to_string()) {
                selected.push(id.clone());
                reserve(&mut reserved_by_capacity_group, disk);
                if selected.len() == request.count {
                    return Ok(selected);
                }
            }
        }

        for (_, id) in scored {
            if selected.contains(&id) {
                continue;
            }
            let Some(disk) = meta.disks.get(&id) else {
                continue;
            };
            if !can_reserve(&reserved_by_capacity_group, disk) {
                continue;
            }
            selected.push(id);
            reserve(&mut reserved_by_capacity_group, disk);
            if selected.len() == request.count {
                return Ok(selected);
            }
        }

        Err(ArgosError::NotEnoughDisks {
            need: request.count,
            have: selected.len(),
        })
    }

    pub(super) fn disk_has_capacity(
        &self,
        meta: &Metadata,
        _disk_id: &str,
        disk: &Disk,
        required_bytes: u64,
    ) -> bool {
        let capacity = self.effective_capacity_bytes_locked(meta, disk);
        if capacity == 0 {
            return true;
        }
        let used = self.effective_used_bytes_locked(meta, disk);
        used.saturating_add(required_bytes) <= capacity
    }

    pub(super) fn ensure_disk_capacity_locked(
        &self,
        meta: &Metadata,
        disk_id: &str,
        required_bytes: u64,
    ) -> Result<()> {
        let disk = meta
            .disks
            .get(disk_id)
            .ok_or_else(|| ArgosError::NotFound(disk_id.to_string()))?;
        let capacity = self.effective_capacity_bytes_locked(meta, disk);
        if capacity == 0 {
            return Ok(());
        }
        let used = self.effective_used_bytes_locked(meta, disk);
        if used.saturating_add(required_bytes) > capacity {
            return Err(ArgosError::DiskFull {
                disk_id: disk_id.to_string(),
                required: required_bytes,
                available: capacity.saturating_sub(used),
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn update_read_latency_locked(
        &self,
        meta: &mut Metadata,
        disk_id: &str,
        bytes: u64,
        seconds: f64,
    ) {
        if meta.backend != BackendKind::Host {
            self.deferred_commit.lock().raw_advisory_metadata_dirty = true;
        }
        if let Some(disk) = meta.disks.get_mut(disk_id) {
            update_latency_ewma(
                &mut disk.read_latency_ewma_ms,
                &mut disk.observed_read_mib_s,
                seconds,
                bytes,
            );
            disk.io_samples = disk.io_samples.saturating_add(1);
            disk.health.latency_ms = ((disk.read_latency_ewma_ms + disk.write_latency_ewma_ms)
                / 2.0)
                .max(disk.health.latency_ms);
        }
    }

    pub(super) fn update_write_latency_locked(
        &self,
        meta: &mut Metadata,
        disk_id: &str,
        bytes: u64,
        seconds: f64,
    ) {
        if meta.config.defer_metadata_commit && bytes < SHARD_CHECKSUM_BLOCK_SIZE as u64 {
            return;
        }
        if let Some(disk) = meta.disks.get_mut(disk_id) {
            update_latency_ewma(
                &mut disk.write_latency_ewma_ms,
                &mut disk.observed_write_mib_s,
                seconds,
                bytes,
            );
            disk.io_samples = disk.io_samples.saturating_add(1);
            disk.health.latency_ms = ((disk.read_latency_ewma_ms + disk.write_latency_ewma_ms)
                / 2.0)
                .max(disk.health.latency_ms);
        }
    }

    pub(super) fn stage_block_reclamation_locked<'a>(
        &self,
        meta: &mut Metadata,
        blocks: impl IntoIterator<Item = &'a FileBlock> + Copy,
    ) {
        if meta.backend != BackendKind::Host
            && (bulk_import_enabled() || meta.config.defer_metadata_commit)
        {
            self.deferred_commit
                .lock()
                .pending_reclaims
                .extend(blocks.into_iter().cloned());
            return;
        }
        self.account_blocks_locked(meta, blocks, false);
        if meta.backend != BackendKind::Host {
            self.delete_blocks_locked(meta, blocks);
        }
    }

    pub(super) fn finish_block_reclamation_locked<'a>(
        &self,
        meta: &mut Metadata,
        blocks: impl IntoIterator<Item = &'a FileBlock>,
    ) {
        if meta.backend == BackendKind::Host {
            self.delete_blocks_locked(meta, blocks);
        }
    }

    pub(super) fn delete_blocks_locked<'a>(
        &self,
        meta: &mut Metadata,
        blocks: impl IntoIterator<Item = &'a FileBlock>,
    ) {
        for block in blocks {
            self.cache.remove(&format!(
                "{}:{}:{}",
                meta.uuid, block.stripe_id, block.raw_sha256
            ));
            for shard in &block.shards {
                let _ = self.delete_shard_locked(meta, shard);
            }
        }
    }

    pub(super) fn read_shard_locked(&self, meta: &Metadata, shard: &Shard) -> Result<Vec<u8>> {
        match shard.location.as_ref() {
            Some(ShardLocation::RawExtent(extent)) => {
                let data = zeroed_io_buffer(shard.size, "raw shard read")?;
                self.backend_read_owned_locked(meta, &extent.disk_id, extent.offset, data)
            }
            Some(ShardLocation::HostPath { disk_id, relpath }) => advanced_io::read_all(
                &self.shard_path_locked(meta, disk_id, relpath),
                shard.size,
                meta.config.io_mode,
                meta.config.zero_copy,
            ),
            None => advanced_io::read_all(
                &self.shard_path_locked(meta, &shard.disk_id, &shard.relpath),
                shard.size,
                meta.config.io_mode,
                meta.config.zero_copy,
            ),
        }
    }

    /// Read one shard, retrying a checksum or size mismatch.
    ///
    /// A USB card or its controller can occasionally return a bad block once
    /// and the correct data on the next read.  Retrying before declaring the
    /// shard damaged keeps scrub from rewriting a file for a transmission
    /// glitch.  I/O errors are returned immediately: the guarded backend has
    /// already fenced the device for those.
    pub(super) fn read_shard_verified_locked(
        &self,
        meta: &Metadata,
        shard: &Shard,
    ) -> Result<Option<Vec<u8>>> {
        let (result, attempts) = retry_verified(
            1 + self.runtime.read_retries,
            |_| {
                let start = std::time::Instant::now();
                match self.read_shard_locked(meta, shard) {
                    Ok(data) => {
                        if let Some(disk) = meta.disks.get(&shard.disk_id) {
                            self.runtime.read_advisory.disk(
                                disk,
                                data.len() as u64,
                                start.elapsed().as_secs_f64(),
                            );
                        }
                        Ok(data)
                    }
                    Err(err) => {
                        if let Some(disk) = meta.disks.get(&shard.disk_id) {
                            self.runtime
                                .read_advisory
                                .disk(disk, 0, start.elapsed().as_secs_f64());
                        }
                        Err(err)
                    }
                }
            },
            |data| data.len() == shard.size && content_hash_matches(data, &shard.sha256),
        )?;
        if attempts > 1 {
            self.runtime
                .read_retries_seen
                .fetch_add((attempts - 1) as u64, std::sync::atomic::Ordering::Relaxed);
            if result.is_some() {
                self.runtime
                    .read_recovered
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(result)
    }

    pub(super) fn read_shard_range_locked(
        &self,
        meta: &Metadata,
        shard: &Shard,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| ArgosError::Invalid("shard read range overflow".to_string()))?;
        if end > shard.size {
            return Err(ArgosError::Invalid(format!(
                "shard range {offset}..{end} exceeds shard size {}",
                shard.size
            )));
        }
        let mut data = zeroed_io_buffer(len, "shard range read")?;
        match shard.location.as_ref() {
            Some(ShardLocation::RawExtent(extent)) => {
                let absolute = extent
                    .offset
                    .checked_add(offset as u64)
                    .ok_or_else(|| ArgosError::Invalid("raw extent read overflow".to_string()))?;
                return self.backend_read_owned_locked(meta, &extent.disk_id, absolute, data);
            }
            Some(ShardLocation::HostPath { disk_id, relpath }) => {
                read_path_range_exact(
                    &self.shard_path_locked(meta, disk_id, relpath),
                    offset as u64,
                    &mut data,
                )?;
            }
            None => {
                read_path_range_exact(
                    &self.shard_path_locked(meta, &shard.disk_id, &shard.relpath),
                    offset as u64,
                    &mut data,
                )?;
            }
        }
        Ok(data)
    }

    pub(super) fn delete_shard_locked(&self, meta: &mut Metadata, shard: &Shard) -> Result<()> {
        match shard.location.as_ref() {
            Some(ShardLocation::RawExtent(extent)) => {
                if let Some(verify) = &self.verify_written {
                    verify.invalidate(&extent.disk_id, extent.offset);
                }
                if let Some(allocator) = meta.raw_pool.allocators.get_mut(&extent.disk_id) {
                    allocator::free(allocator, extent)?;
                }
                Ok(())
            }
            Some(ShardLocation::HostPath { disk_id, relpath }) => {
                if let Some(path) = self.shard_path_if_disk_exists_locked(meta, disk_id, relpath) {
                    let _ = fs::remove_file(path);
                }
                Ok(())
            }
            None => {
                if let Some(path) =
                    self.shard_path_if_disk_exists_locked(meta, &shard.disk_id, &shard.relpath)
                {
                    let _ = fs::remove_file(path);
                }
                Ok(())
            }
        }
    }

    fn backend_read_owned_locked(
        &self,
        meta: &Metadata,
        disk_id: &str,
        offset: u64,
        buf: Vec<u8>,
    ) -> Result<Vec<u8>> {
        if self.is_device_quarantined(disk_id) {
            return Err(ArgosError::MissingDevice(disk_id.to_string()));
        }
        let length = buf.len();
        let result = match self
            .backend
            .read_shard_owned(&disk_id.to_string(), offset, buf)
        {
            Err(ArgosError::MissingDevice(_)) if meta.backend != BackendKind::Host => {
                let backend = self.single_device_backend_locked(meta, disk_id, false)?;
                backend.read_shard_owned(
                    &disk_id.to_string(),
                    offset,
                    zeroed_io_buffer(length, "raw shard retry")?,
                )
            }
            other => other,
        };
        if let Err(err) = &result {
            if Self::device_error_should_quarantine(err) && meta.backend != BackendKind::Host {
                self.quarantine_device(disk_id);
            }
        }
        result
    }

    pub(super) fn backend_write_at_locked(
        &self,
        meta: &Metadata,
        disk_id: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        if self.is_device_quarantined(disk_id) {
            return Err(ArgosError::MissingDevice(disk_id.to_string()));
        }
        let result = match self.backend.write_at(&disk_id.to_string(), offset, data) {
            Err(ArgosError::MissingDevice(_)) if meta.backend != BackendKind::Host => {
                let backend = self.single_device_backend_locked(meta, disk_id, true)?;
                backend.write_at(&disk_id.to_string(), offset, data)
            }
            other => other,
        };
        if let Err(err) = &result {
            if Self::device_error_should_quarantine(err) && meta.backend != BackendKind::Host {
                self.quarantine_device(disk_id);
            }
        }
        result
    }

    pub(super) fn backend_write_batch_locked(
        &self,
        meta: &Metadata,
        disk_id: &str,
        writes: &[(u64, &[u8])],
    ) -> Result<()> {
        if self.is_device_quarantined(disk_id) {
            return Err(ArgosError::MissingDevice(disk_id.to_string()));
        }
        let result = match self.backend.write_batch(&disk_id.to_string(), writes) {
            Err(ArgosError::MissingDevice(_)) if meta.backend != BackendKind::Host => {
                let backend = self.single_device_backend_locked(meta, disk_id, true)?;
                backend.write_batch(&disk_id.to_string(), writes)
            }
            other => other,
        };
        if let Err(err) = &result {
            if Self::device_error_should_quarantine(err) && meta.backend != BackendKind::Host {
                self.quarantine_device(disk_id);
            }
        }
        result
    }

    pub(super) fn backend_flush_locked(&self, meta: &Metadata, disk_id: &str) -> Result<()> {
        if self.is_device_quarantined(disk_id) {
            return Err(ArgosError::MissingDevice(disk_id.to_string()));
        }
        let result = match self.backend.flush_device(&disk_id.to_string()) {
            Err(ArgosError::MissingDevice(_)) if meta.backend != BackendKind::Host => {
                let backend = self.single_device_backend_locked(meta, disk_id, true)?;
                backend.flush_device(&disk_id.to_string())
            }
            other => other,
        };
        if let Err(err) = &result {
            if Self::device_error_should_quarantine(err) && meta.backend != BackendKind::Host {
                self.quarantine_device(disk_id);
            }
        }
        result
    }

    pub(super) fn single_device_backend_locked(
        &self,
        meta: &Metadata,
        disk_id: &str,
        write: bool,
    ) -> Result<Arc<dyn StorageBackend>> {
        let disk = meta
            .disks
            .get(disk_id)
            .ok_or_else(|| ArgosError::MissingDevice(disk_id.to_string()))?;
        let inner: Arc<dyn StorageBackend> = Arc::new(FileBlockBackend::open_with_ids(
            meta.backend,
            vec![(disk_id.to_string(), disk.path.clone())],
            write,
        )?);
        Ok(Arc::new(GuardedBackend::new(inner)))
    }

    pub(super) fn account_blocks_locked<'a>(
        &self,
        meta: &mut Metadata,
        blocks: impl IntoIterator<Item = &'a FileBlock>,
        add: bool,
    ) {
        for shard in blocks.into_iter().flat_map(|block| block.shards.iter()) {
            if let Some(disk) = meta.disks.get_mut(&shard.disk_id) {
                let accounted = shard_accounted_size(shard);
                if add {
                    disk.used_bytes = disk.used_bytes.saturating_add(accounted);
                } else {
                    disk.used_bytes = disk.used_bytes.saturating_sub(accounted);
                }
            }
        }
    }

    pub(super) fn effective_capacity_bytes_locked(&self, meta: &Metadata, disk: &Disk) -> u64 {
        if disk.capacity_source == CapacitySource::UserOverride {
            return disk.capacity_bytes;
        }
        let Some(fs_id) = disk.backing_fs_id.as_deref() else {
            return disk.capacity_bytes;
        };
        meta.disks
            .values()
            .filter(|candidate| {
                candidate.capacity_source == CapacitySource::AutoProbe
                    && candidate.backing_fs_id.as_deref() == Some(fs_id)
            })
            .map(|candidate| candidate.capacity_bytes)
            .max()
            .unwrap_or(disk.capacity_bytes)
    }

    pub(super) fn effective_used_bytes_locked(&self, meta: &Metadata, disk: &Disk) -> u64 {
        if disk.capacity_source == CapacitySource::UserOverride {
            return disk.used_bytes;
        }
        let Some(fs_id) = disk.backing_fs_id.as_deref() else {
            return disk.used_bytes;
        };
        meta.disks
            .values()
            .filter(|candidate| {
                candidate.capacity_source == CapacitySource::AutoProbe
                    && candidate.backing_fs_id.as_deref() == Some(fs_id)
            })
            .map(|candidate| candidate.used_bytes)
            .sum()
    }
}

/// True when an unavailable damage marker should be left for a member that
/// may still return.
///
/// `decode_block_locked` records `"<disk>:unavailable"` for a shard whose
/// member is Offline, Failed or Removed and reconstructs the block from the
/// remaining shards.  An `Offline` member may simply be missing from this
/// scan (a reader was power-cycled, a card is out of its slot) and can return
/// on the next open.  `repair_offline` is false for ordinary reads, which
/// must never rewrite a file for it.  Scrub passes true and waits for the
/// configured grace period before rebuilding the shards.  A `Failed` or
/// `Removed` member is a different case: the device is present but failed, or
/// the operator has explicitly retired it, so the normal repair path still
/// rebuilds the file.
pub(super) fn damage_is_transient_offline_member(
    fs: &ArgosFs,
    meta: &Metadata,
    entry: &str,
    repair_offline: bool,
) -> bool {
    let disk_id = entry
        .strip_suffix(":unavailable")
        .or_else(|| entry.split_once(":missing:").map(|(disk_id, _)| disk_id));
    disk_id.is_some_and(|disk_id| {
        meta.disks.get(disk_id).is_some_and(|disk| {
            if disk.status == DiskStatus::Offline {
                return !(repair_offline && offline_member_due_for_repair(disk));
            }
            // During an active mount a disappearing USB device is quarantined
            // before the next open scan can mark it Offline. The first fatal
            // I/O failure starts the same grace period for scrub and reads.
            meta.backend != BackendKind::Host
                && disk.status != DiskStatus::Failed
                && disk.status != DiskStatus::Removed
                && fs.is_device_quarantined(disk_id)
                && !(repair_offline && fs.quarantined_member_due_for_repair(disk_id))
        })
    })
}

#[cfg(test)]
#[path = "data_plane_tests.rs"]
mod tests;
