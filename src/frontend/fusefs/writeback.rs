use super::*;

#[derive(Clone, Debug)]
pub(super) struct DirtyExtent {
    pub(super) offset: u64,
    pub(super) data: Vec<u8>,
    // Time of the last acknowledged buffered write, not the later disk flush.
    pub(super) written_at: f64,
    // Actual persisted ctime when the latest buffered write was acknowledged.
    // Later metadata mutations (e.g., unlink of an open handle) win over the
    // earlier buffered data write's ctime at both stat and final commit.
    pub(super) seen_ctime: Option<f64>,
}

impl DirtyExtent {
    pub(super) fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.data.len() as u64)
    }

    pub(super) fn merge_bounds(
        &self,
        offset: u64,
        len: usize,
        max_bytes: usize,
    ) -> Option<(u64, usize)> {
        let existing_end = self.end()?;
        let new_end = offset.checked_add(u64::try_from(len).ok()?)?;
        if offset > existing_end || self.offset > new_end {
            return None;
        }
        let merged_start = self.offset.min(offset);
        let merged_end = existing_end.max(new_end);
        let merged_len = usize::try_from(merged_end.checked_sub(merged_start)?).ok()?;
        (merged_len <= max_bytes).then_some((merged_start, merged_len))
    }

    pub(super) fn try_merge(&mut self, offset: u64, data: &[u8], max_bytes: usize) -> bool {
        let Some((merged_start, merged_len)) = self.merge_bounds(offset, data.len(), max_bytes)
        else {
            return false;
        };
        if merged_start == self.offset {
            // Appends and in-place overwrites are the common path. Reuse Vec's
            // amortized capacity instead of copying the whole dirty stripe on
            // every small FUSE write (quadratic cumulative copy volume).
            self.data.resize(merged_len, 0);
            let start = (offset - self.offset) as usize;
            self.data[start..start + data.len()].copy_from_slice(data);
        } else {
            // A prepend still needs to move the existing bytes. The merge
            // bounds guarantee that there is no uninitialised gap.
            let mut merged = vec![0; merged_len];
            let old_start = (self.offset - merged_start) as usize;
            merged[old_start..old_start + self.data.len()].copy_from_slice(&self.data);
            let new_start = (offset - merged_start) as usize;
            merged[new_start..new_start + data.len()].copy_from_slice(data);
            self.offset = merged_start;
            self.data = merged;
        }
        self.written_at = now_f64();
        true
    }
}

#[derive(Default)]
pub(super) struct FuseWriteback {
    pub(super) dirty: BTreeMap<InodeId, Arc<DirtyExtent>>,
    pub(super) reap_after_flush: BTreeSet<InodeId>,
    pub(super) pending_bytes: usize,
}

impl FuseWriteback {
    pub(super) fn insert_dirty(&mut self, ino: InodeId, dirty: DirtyExtent) {
        let old = self.dirty.insert(ino, Arc::new(dirty));
        if let Some(old) = old {
            self.pending_bytes -= old.data.len();
        }
        self.pending_bytes += self.dirty[&ino].data.len();
    }

    pub(super) fn remove_dirty(&mut self, ino: InodeId) -> Option<Arc<DirtyExtent>> {
        let dirty = self.dirty.remove(&ino)?;
        self.pending_bytes -= dirty.data.len();
        Some(dirty)
    }
}

impl ArgosFuseShared {
    // The fixed lock stripes bound lock memory independently of inode count.
    // A stripe protects acknowledged bytes and their persisted generation;
    // the mount-wide writeback mutex protects only map/accounting changes.
    pub(super) fn writeback_lock(&self, ino: InodeId) -> &RwLock<()> {
        &self.writeback_locks[(ino as usize) % WRITEBACK_LOCK_STRIPES]
    }

    pub(super) fn queue_writeback(&self, ino: InodeId, offset: u64, data: &[u8]) -> bool {
        if data.len() > FUSE_WRITEBACK_MAX_BYTES {
            return false;
        }
        let _inode_guard = self.writeback_lock(ino).write();
        let seen_ctime = self.volume.attr_inode(ino).ok().map(|attr| attr.ctime);
        let mut writeback = self.writeback.lock();
        if let Some(dirty) = writeback.dirty.get(&ino) {
            let old_len = dirty.data.len();
            let Some((_, merged_len)) =
                dirty.merge_bounds(offset, data.len(), FUSE_WRITEBACK_MAX_BYTES)
            else {
                return false;
            };
            if writeback.pending_bytes - old_len + merged_len > FUSE_WRITEBACK_MAX_MOUNT_BYTES {
                return false;
            }
            let dirty = Arc::make_mut(writeback.dirty.get_mut(&ino).expect("existing dirty inode"));
            if !dirty.try_merge(offset, data, FUSE_WRITEBACK_MAX_BYTES) {
                return false;
            }
            dirty.seen_ctime = seen_ctime;
            writeback.pending_bytes = writeback.pending_bytes - old_len + merged_len;
            return true;
        }
        if writeback.dirty.len() >= FUSE_WRITEBACK_MAX_INODES
            || writeback.pending_bytes + data.len() > FUSE_WRITEBACK_MAX_MOUNT_BYTES
        {
            return false;
        }
        writeback.insert_dirty(
            ino,
            DirtyExtent {
                offset,
                data: data.to_vec(),
                written_at: now_f64(),
                seen_ctime,
            },
        );
        true
    }

    /// When the bounded coalescer fills, persist only whole logical stripes
    /// from a contiguous EOF append. Keep the incomplete last stripe buffered
    /// so the next append cannot force it through another COW rewrite.
    /// The same guard serializes this with reads, attrs, fsync and inode writes.
    /// On error, no acknowledged dirty bytes are discarded.
    pub(super) fn flush_complete_stripes_for_append(
        &self,
        ino: InodeId,
        next_offset: u64,
    ) -> Result<bool> {
        let started = Instant::now();
        let _inode_guard = self.writeback_lock(ino).write();
        let Some(dirty) = self.writeback.lock().dirty.get(&ino).cloned() else {
            return Ok(false);
        };
        // This optimization is only for strictly sequential EOF appends.
        // A gap/overwrite must use the existing ordered flush fallback.
        if dirty.end() != Some(next_offset) {
            return Ok(false);
        }
        let Ok((old_size, stripe_size)) = self.volume.writeback_stripe_geometry(ino) else {
            return Ok(false);
        };
        if stripe_size == 0 || dirty.offset != old_size as u64 {
            return Ok(false);
        }
        let Some(end) = old_size.checked_add(dirty.data.len()) else {
            return Ok(false);
        };
        let complete_end = end / stripe_size * stripe_size;
        if complete_end <= old_size {
            return Ok(false);
        }
        let prefix_len = complete_end - old_size;
        let result = self.volume.write_inode_range_from_open_handle_at(
            ino,
            dirty.offset,
            &dirty.data[..prefix_len],
            dirty.written_at,
            dirty.seen_ctime,
        );
        self.volume.commit_metrics().record_writeback_flush(
            WritebackFlushReason::AppendStage,
            matches!(&result, Ok(written) if *written == prefix_len),
            started.elapsed(),
        );
        match result {
            Ok(written) if written == prefix_len => {
                drop(dirty);
                let mut writeback = self.writeback.lock();
                let dirty =
                    Arc::make_mut(writeback.dirty.get_mut(&ino).expect("locked dirty inode"));
                dirty.offset += prefix_len as u64;
                dirty.data.drain(..prefix_len);
                // Vec::drain retains the original (up to 4 MiB) capacity.
                // Release it so the mount-wide pending-byte budget also
                // bounds retained heap capacity after many partial flushes.
                dirty.data.shrink_to_fit();
                writeback.pending_bytes -= prefix_len;
                if writeback.dirty[&ino].data.is_empty() {
                    writeback.dirty.remove(&ino);
                }
                Ok(true)
            }
            Ok(written) => Err(ArgosError::Invalid(format!(
                "short full-stripe writeback for inode {ino}: wrote {written} of {prefix_len} bytes"
            ))),
            Err(err) => Err(err),
        }
    }

    /// First try bounded in-memory coalescing. If it is full, make space by
    /// writing a whole-stripe prefix, never the incomplete EOF tail.
    pub(super) fn queue_writeback_staged(
        &self,
        ino: InodeId,
        offset: u64,
        data: &[u8],
    ) -> Result<bool> {
        if self.queue_writeback(ino, offset, data) {
            return Ok(true);
        }
        if data.len() <= FUSE_WRITEBACK_MAX_BYTES
            && self.flush_complete_stripes_for_append(ino, offset)?
        {
            return Ok(self.queue_writeback(ino, offset, data));
        }
        Ok(false)
    }

    /// Attribute reads observe acknowledged writes without forcing a COW
    /// rewrite. This includes in-place overwrites and sparse extensions:
    /// neither stat nor path lookup is a durability boundary.
    ///
    /// The writeback lock serializes observation with persistence; the inode
    /// cannot publish a stale size or timestamps halfway through a flush.
    pub(super) fn visible_inode_attr(&self, ino: InodeId) -> Result<NodeAttr> {
        let inode_guard = self.writeback_lock(ino).read();
        let dirty = self.writeback.lock().dirty.get(&ino).cloned();
        let mut attr = self.volume.attr_inode(ino)?;
        let Some(dirty) = dirty else {
            return Ok(attr);
        };
        if attr.kind == NodeKind::File {
            if let Some(end) = dirty.end() {
                attr.size = attr.size.max(end);
                attr.blocks = attr.size.div_ceil(512);
                attr.mtime = dirty.written_at;
                if !dirty.seen_ctime.is_some_and(|seen| seen != attr.ctime) {
                    attr.ctime = dirty.written_at;
                }
                attr.mode &= !(libc::S_ISUID | libc::S_ISGID);
                return Ok(attr);
            }
        }
        drop(dirty);
        drop(inode_guard);
        self.flush_inode_writeback_reason(ino, WritebackFlushReason::StatFallback)?;
        self.volume.attr_inode(ino)
    }

    /// Overlay a bounded acknowledged extent over checked persisted bytes.
    /// In-place rewrites can now survive arbitrary reads without premature
    /// decode/encode cycles; sparse gaps between EOF and the write read as
    /// zero. A damaged persisted prefix still fails closed via the normal
    /// decoder, while a failed commit retains the original dirty extent.
    ///
    /// Holding the per-inode writeback lock across metadata/read/overlay is necessary:
    /// a concurrent flush must not mix two inode generations in one reply.
    pub(super) fn read_visible_inode(
        &self,
        ino: InodeId,
        offset: u64,
        size: usize,
    ) -> Result<Vec<u8>> {
        let inode_guard = self.writeback_lock(ino).read();
        let dirty = self.writeback.lock().dirty.get(&ino).cloned();
        let Some(dirty) = dirty else {
            return self.volume.read_inode(ino, offset, size, true);
        };
        let attr = self.volume.attr_inode(ino)?;
        let Some(dirty_end) = dirty.end() else {
            drop(dirty);
            drop(inode_guard);
            self.flush_inode_writeback_reason(ino, WritebackFlushReason::ReadFallback)?;
            return self.volume.read_inode(ino, offset, size, true);
        };
        if attr.kind != NodeKind::File {
            drop(dirty);
            drop(inode_guard);
            self.flush_inode_writeback_reason(ino, WritebackFlushReason::ReadFallback)?;
            return self.volume.read_inode(ino, offset, size, true);
        }

        let logical_end = attr.size.max(dirty_end);
        let read_end = offset.saturating_add(size as u64).min(logical_end);
        if offset >= read_end {
            self.volume.account_buffered_read(ino, 0, false);
            return Ok(Vec::new());
        }

        // Decode and validate every persisted byte in the requested range.
        // Do not mask an unreadable old stripe, even when overwritten by the
        // pending extent: an eventual partial-stripe commit may need it.
        let persisted_end = read_end.min(attr.size);
        let persisted_len = persisted_end.saturating_sub(offset) as usize;
        let mut output = if persisted_len > 0 {
            self.volume.read_inode(ino, offset, persisted_len, true)?
        } else {
            Vec::new()
        };
        if output.len() != persisted_len {
            return Err(ArgosError::Invalid(format!(
                "concurrent inode size change during buffered read of inode {ino}"
            )));
        }

        let visible_len = (read_end - offset) as usize;
        if visible_len > crate::volume::MAX_IN_MEMORY_IO_BYTES {
            return Err(ArgosError::FileTooLarge(format!(
                "buffered read requires {visible_len} bytes, limit is {}",
                crate::volume::MAX_IN_MEMORY_IO_BYTES
            )));
        }
        output
            .try_reserve_exact(visible_len.saturating_sub(output.len()))
            .map_err(|_| ArgosError::Io(std::io::Error::from_raw_os_error(libc::ENOMEM)))?;
        output.resize(visible_len, 0);
        let overlay_start = offset.max(dirty.offset);
        let overlay_end = read_end.min(dirty_end);
        if overlay_end > overlay_start {
            let src_start = (overlay_start - dirty.offset) as usize;
            let dst_start = (overlay_start - offset) as usize;
            let len = (overlay_end - overlay_start) as usize;
            output[dst_start..dst_start + len]
                .copy_from_slice(&dirty.data[src_start..src_start + len]);
        }

        // The persisted prefix was already counted by read_inode. Count the
        // additionally visible sparse gap/dirty tail (or an EOF read) once.
        let buffered_bytes = visible_len - persisted_len;
        if buffered_bytes > 0 || persisted_len == 0 {
            self.volume
                .account_buffered_read(ino, buffered_bytes, persisted_len != 0);
        }
        Ok(output)
    }

    #[cfg(test)]
    pub(super) fn flush_inode_writeback(&self, ino: InodeId) -> Result<()> {
        self.flush_inode_writeback_reason(ino, WritebackFlushReason::Other)
    }

    pub(super) fn flush_inode_writeback_reason(
        &self,
        ino: InodeId,
        reason: WritebackFlushReason,
    ) -> Result<()> {
        let started = Instant::now();
        let _inode_guard = self.writeback_lock(ino).write();
        // Keep acknowledged bytes in the map and charged to the mount budget
        // until persistence succeeds; failures never need to reinsert them.
        let dirty = self.writeback.lock().dirty.get(&ino).cloned();
        let Some(dirty) = dirty else {
            let reap_after_flush = self.writeback.lock().reap_after_flush.remove(&ino);
            if reap_after_flush {
                if let Err(err) = self.volume.reap_unlinked_inode(ino) {
                    self.writeback.lock().reap_after_flush.insert(ino);
                    return Err(err);
                }
            }
            return Ok(());
        };
        let result = self.volume.write_inode_range_from_open_handle_at(
            ino,
            dirty.offset,
            dirty.data.as_slice(),
            dirty.written_at,
            dirty.seen_ctime,
        );
        self.volume.commit_metrics().record_writeback_flush(
            reason,
            matches!(&result, Ok(written) if *written == dirty.data.len()),
            started.elapsed(),
        );
        let reap_after_flush = match result {
            Ok(written) if written == dirty.data.len() => {
                drop(dirty);
                let mut writeback = self.writeback.lock();
                writeback.remove_dirty(ino);
                writeback.reap_after_flush.remove(&ino)
            }
            Ok(written) => {
                return Err(ArgosError::Invalid(format!(
                    "short writeback flush for inode {ino}: wrote {written} bytes"
                )));
            }
            Err(err) => return Err(err),
        };
        if reap_after_flush {
            if let Err(err) = self.volume.reap_unlinked_inode(ino) {
                self.writeback.lock().reap_after_flush.insert(ino);
                return Err(err);
            }
        }
        Ok(())
    }

    pub(super) fn flush_all_writeback(&self) -> Result<()> {
        loop {
            let next = {
                let writeback = self.writeback.lock();
                writeback
                    .dirty
                    .keys()
                    .chain(writeback.reap_after_flush.iter())
                    .next()
                    .copied()
            };
            let Some(ino) = next else {
                return Ok(());
            };
            self.flush_inode_writeback_reason(ino, WritebackFlushReason::Bulk)?;
        }
    }

    pub(super) fn flush_all_writeback_best_effort(&self) {
        let inodes = {
            let writeback = self.writeback.lock();
            writeback
                .dirty
                .keys()
                .chain(writeback.reap_after_flush.iter())
                .copied()
                .collect::<BTreeSet<_>>()
        };
        for ino in inodes {
            let _ = self.flush_inode_writeback_reason(ino, WritebackFlushReason::Bulk);
        }
    }

    pub(super) fn discard_inode_writeback(&self, ino: InodeId) {
        let _inode_guard = self.writeback_lock(ino).write();
        let mut writeback = self.writeback.lock();
        writeback.remove_dirty(ino);
        writeback.reap_after_flush.remove(&ino);
    }
}
