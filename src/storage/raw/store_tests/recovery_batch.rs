use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingRecoveryReads {
    inner: FileBlockBackend,
    device: String,
    first_body_byte: u64,
    last_body_byte: u64,
    body_calls: AtomicUsize,
    overlap: Option<(AtomicUsize, AtomicUsize)>,
}

impl StorageBackend for CountingRecoveryReads {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, device: &String, offset: u64, bytes: &mut [u8]) -> Result<()> {
        if device == &self.device && offset >= self.first_body_byte && offset < self.last_body_byte
        {
            self.body_calls.fetch_add(1, Ordering::Relaxed);
        }
        if let Some((active, peak)) = &self.overlap {
            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(count, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(15));
            let result = self.inner.read_at(device, offset, bytes);
            active.fetch_sub(1, Ordering::SeqCst);
            return result;
        }
        self.inner.read_at(device, offset, bytes)
    }
    fn write_at(&self, device: &String, offset: u64, bytes: &[u8]) -> Result<()> {
        self.inner.write_at(device, offset, bytes)
    }
    fn flush_device(&self, device: &String) -> Result<()> {
        self.inner.flush_device(device)
    }
    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }
    fn capacity(&self, device: &String) -> Result<u64> {
        self.inner.capacity(device)
    }
    fn device_status(&self, device: &String) -> Result<DiskStatus> {
        self.inner.device_status(device)
    }
    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

#[test]
fn recovery_materializes_older_quorum_winner_without_retaining_full_history() {
    let (_dir, images, base, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    // Both voting members support a committed prefix; a lone newer tail
    // must not displace it. No commit certificates are needed for this case.
    let mut previous = base.clone();
    for i in 0..12 {
        let next = next_metadata(&previous, &format!("history-{i}"));
        let (entry, _) = build_journal_entry(
            &next,
            Some(&previous),
            "history",
            serde_json::json!({}),
            true,
        )
        .unwrap();
        for sb in &superblocks[..2] {
            let (header, offset, end, rollover) =
                journal_append_position(&backend, sb, entry.len()).unwrap();
            assert!(!rollover);
            append_journal_member(
                &backend,
                sb,
                header,
                offset,
                end,
                &entry,
                JournalMemberWriteOptions {
                    flush: true,
                    metrics: None,
                },
            )
            .unwrap();
        }
        previous = next;
    }
    let winner = previous;
    let tail = next_metadata(&winner, "uncommitted-tail");
    let (entry, _) =
        build_journal_entry(&tail, Some(&winner), "history", serde_json::json!({}), true).unwrap();
    let sb = &superblocks[0];
    let (header, offset, end, rollover) =
        journal_append_position(&backend, sb, entry.len()).unwrap();
    assert!(!rollover);
    append_journal_member(
        &backend,
        sb,
        header,
        offset,
        end,
        &entry,
        JournalMemberWriteOptions {
            flush: true,
            metrics: None,
        },
    )
    .unwrap();
    let mut report = TransactionReport::default();
    let scan =
        scan_journal_metadata(&backend, &superblocks, &mut report, Some(&base), &[], None).unwrap();
    assert_eq!(
        scan.winner,
        Some((
            winner.txid,
            winner.integrity.generation,
            winner.integrity.meta_hash.clone()
        ))
    );
    assert!(
        scan.retained.is_none(),
        "older winner should be reconstructed, not held in the history map"
    );
    let expected_valid = report.valid_entries;
    let mut report = TransactionReport::default();
    let recovered =
        read_latest_journal_metadata(&backend, &superblocks, &mut report, Some(&base), &[])
            .unwrap()
            .unwrap();
    assert_eq!(recovered.txid, winner.txid);
    assert_eq!(recovered.integrity.meta_hash, winner.integrity.meta_hash);
    assert_eq!(
        report.valid_entries, expected_valid,
        "second pass must not double-count report entries"
    );
    let actual = recover_metadata(&backend, &superblocks).unwrap();
    assert_eq!(actual.integrity.meta_hash, winner.integrity.meta_hash);
}

#[test]
fn recovery_reads_checkpoint_in_large_batches_and_verifies_each_page() {
    let (_dir, images, _, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let sb = &superblocks[0];
    let bytes = (0..2 * METADATA_READ_BATCH_BYTES + 17)
        .map(|i| (i as u64 * 137 % 251) as u8)
        .collect::<Vec<_>>();
    let checkpoint = metadata_tree_checkpoint(
        &sb.disk_id,
        20,
        20,
        &bytes,
        &sha256_hex(&bytes),
        sb.metadata.length / 2,
    )
    .unwrap();
    let slot = sb.metadata.offset;
    write_metadata_slot(&backend, sb, slot, &checkpoint, None).unwrap();
    let mut header = [0u8; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, slot, &mut header).unwrap();
    let data_start = align_up(
        RAW_HEADER_SIZE as u64 + get_u64(&header, 160).unwrap(),
        METADATA_PAGE_SIZE as u64,
    );
    let counted = CountingRecoveryReads {
        inner: block_backend_for(&images),
        device: sb.disk_id.clone(),
        first_body_byte: slot + data_start,
        last_body_byte: slot + data_start + bytes.len() as u64,
        body_calls: AtomicUsize::new(0),
        overlap: None,
    };
    let result =
        read_tree_metadata_candidate(&counted, sb, slot, sb.metadata.length / 2, &header).unwrap();
    assert_eq!(result, bytes);
    assert_eq!(
        counted.body_calls.load(Ordering::Relaxed),
        3,
        "513 per-page reads must become three contiguous reads",
    );

    // Corruption in the second batch must still be detected at the precise
    // logical page, even when the physical I/O was performed in one request.
    let damaged_page = 300usize;
    let damaged_offset = slot + data_start + damaged_page as u64 * 4096;
    backend
        .write_at(&sb.disk_id, damaged_offset, &[0xa5])
        .unwrap();
    let err = read_tree_metadata_candidate(&counted, sb, slot, sb.metadata.length / 2, &header)
        .unwrap_err();
    assert!(err.to_string().contains("page 300"), "{err}");
}

#[test]
fn recovery_rejects_invalid_page_index_before_body_read() {
    let (_dir, images, _, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let sb = &superblocks[0];
    let bytes = vec![31_u8; 3 * METADATA_PAGE_SIZE];
    let checkpoint = metadata_tree_checkpoint(
        &sb.disk_id,
        30,
        30,
        &bytes,
        &sha256_hex(&bytes),
        sb.metadata.length / 2,
    )
    .unwrap();
    write_metadata_slot(&backend, sb, sb.metadata.offset, &checkpoint, None).unwrap();
    let mut header = checkpoint.header;
    let index_size = get_u64(&header, 160).unwrap() as usize;
    let mut index = vec![0u8; index_size];
    backend
        .read_at(
            &sb.disk_id,
            sb.metadata.offset + RAW_HEADER_SIZE as u64,
            &mut index,
        )
        .unwrap();
    // Change the second page's recorded size, then update the index hash so
    // corruption is detected by extent validation rather than index digest.
    put_u32(&mut index[METADATA_INDEX_ENTRY_SIZE..], 40, 1);
    header[168..168 + HASH_HEX_LEN].copy_from_slice(sha256_hex(&index).as_bytes());
    backend
        .write_at(
            &sb.disk_id,
            sb.metadata.offset + RAW_HEADER_SIZE as u64,
            &index,
        )
        .unwrap();
    assert!(matches!(
        read_tree_metadata_candidate(
            &backend,
            sb,
            sb.metadata.offset,
            sb.metadata.length / 2,
            &header,
        ),
        Err(ArgosError::CorruptedMetadata(_))
    ));
}

#[test]
fn journal_tip_scans_all_records_using_bounded_contiguous_reads() {
    let (_dir, images, metadata, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let sb = &superblocks[0];
    let (record, _) = build_journal_entry(
        &metadata,
        None,
        "tip-batch",
        serde_json::json!({"payload": "x".repeat(4096)}),
        true,
    )
    .unwrap();
    let (large, _) = build_journal_entry(
        &metadata,
        None,
        "large-tip-batch",
        serde_json::json!({"payload": "y".repeat(JOURNAL_TIP_BATCH_BYTES + 17)}),
        true,
    )
    .unwrap();
    let count = JOURNAL_TIP_BATCH_BYTES * 2 / record.len() + 3;
    let mut body = record.repeat(count);
    body.extend_from_slice(&large);
    assert!(body.len() as u64 + RAW_HEADER_SIZE as u64 <= sb.journal.length);
    initialize_journal_region(&backend, &sb.disk_id, sb).unwrap();
    let start = sb.journal.offset + RAW_HEADER_SIZE as u64;
    backend.write_at(&sb.disk_id, start, &body).unwrap();
    let mut header = [0; RAW_HEADER_SIZE];
    backend
        .read_at(&sb.disk_id, sb.journal.offset, &mut header)
        .unwrap();
    put_u64(&mut header, 24, RAW_HEADER_SIZE as u64 + body.len() as u64);
    backend
        .write_at(&sb.disk_id, sb.journal.offset, &header)
        .unwrap();
    let counted = CountingRecoveryReads {
        inner: block_backend_for(&images),
        device: sb.disk_id.clone(),
        first_body_byte: start,
        last_body_byte: start + body.len() as u64,
        body_calls: AtomicUsize::new(0),
        overlap: None,
    };
    assert_eq!(
        journal_tip_identity(&counted, sb).unwrap(),
        Some((metadata.txid, metadata.integrity.meta_hash.clone()))
    );
    assert_eq!(
        counted.body_calls.load(Ordering::Relaxed),
        body.len().div_ceil(JOURNAL_TIP_BATCH_BYTES)
    );
    assert!(body.len().div_ceil(JOURNAL_TIP_BATCH_BYTES) * 20 < (count + 1) * 2);
    // Corrupt a prefix record, not the final record. A fast tip read must
    // still detect corruption before any new transaction can be appended.
    backend
        .write_at(&sb.disk_id, start + 36, &[body[36] ^ 1])
        .unwrap();
    assert!(matches!(
        journal_tip_identity(&counted, sb),
        Err(ArgosError::Checksum(_))
    ));
}

#[test]
fn checkpoint_recovery_overlaps_members_and_shares_decoded_images() {
    let (_dir, images, base, superblocks) = quorum_fixture();
    let backend = CountingRecoveryReads {
        inner: block_backend_for(&images),
        device: String::new(),
        first_body_byte: 0,
        last_body_byte: 0,
        body_calls: AtomicUsize::new(0),
        overlap: Some((AtomicUsize::new(0), AtomicUsize::new(0))),
    };
    let candidates = read_all_checkpoint_candidates(&backend, &superblocks).unwrap();
    assert_eq!(candidates.len(), superblocks.len() * 2);
    let (_, peak) = backend.overlap.as_ref().unwrap();
    assert!(peak.load(Ordering::SeqCst) >= 2);
    let shared = candidates[0].1.as_ref().unwrap();
    for (id, (disk, candidate, report)) in candidates.iter().enumerate() {
        assert_eq!(disk, &superblocks[id / 2].disk_id);
        assert!(report.valid);
        assert!(Arc::ptr_eq(shared, candidate.as_ref().unwrap()));
        assert_eq!(report.txid, base.txid);
    }
}

#[test]
fn checkpoint_budget_counts_stored_payload_not_reserved_capacity() {
    let (_dir, images, _, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let sb = &superblocks[0];
    let headers = read_checkpoint_headers(&backend, sb);
    let actual = checkpoint_payload_budget(sb, &headers);
    assert!(actual > 0);
    assert!(actual < sb.metadata.length / 2);
    // Enlarging only the reserved capacity cannot change the memory required
    // by these already read headers, as on a large real raw member.
    let mut large = sb.clone();
    large.metadata.length = 488 * 1024 * 1024;
    assert_eq!(checkpoint_payload_budget(&large, &headers), actual);
}
