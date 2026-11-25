use super::*;

/// Simulate a power cut after a partial new-slot body write, before header
/// publication. The prior slot must remain usable for quorum recovery.
struct TornCheckpointBackend {
    inner: FileBlockBackend,
    disk_id: String,
    body_start: u64,
    slot_end: u64,
    fail_once: std::sync::atomic::AtomicBool,
}

impl StorageBackend for TornCheckpointBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(id, offset, buf)
    }
    fn write_at(&self, id: &String, offset: u64, bytes: &[u8]) -> Result<()> {
        if id == &self.disk_id
            && offset >= self.body_start
            && offset < self.slot_end
            && self
                .fail_once
                .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.inner
                .write_at(id, offset, &bytes[..bytes.len().div_ceil(2)])?;
            return Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
        }
        self.inner.write_at(id, offset, bytes)
    }
    fn flush_device(&self, id: &String) -> Result<()> {
        self.inner.flush_device(id)
    }
    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }
    fn capacity(&self, id: &String) -> Result<u64> {
        self.inner.capacity(id)
    }
    fn device_status(&self, id: &String) -> Result<DiskStatus> {
        self.inner.device_status(id)
    }
    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

/// A rollover is forced by using a temporarily shortened journal descriptor.
/// Data offsets on disk remain unchanged and the quorum certificate fits.
fn force_small_rollover_journal(backend: &dyn StorageBackend, superblocks: &mut [RawSuperblock]) {
    for sb in superblocks {
        let used = raw_journal_end(backend, sb);
        sb.journal.length = (used + 2048).max(RAW_HEADER_SIZE as u64 + 4096);
    }
}

fn checkpoint_slot_txids(backend: &dyn StorageBackend, sb: &RawSuperblock) -> [u64; 2] {
    std::array::from_fn(|slot| {
        let mut header = [0u8; RAW_HEADER_SIZE];
        backend
            .read_at(
                &sb.disk_id,
                sb.metadata.offset + slot as u64 * (sb.metadata.length / 2),
                &mut header,
            )
            .unwrap();
        assert_eq!(&header[..16], METADATA_MAGIC);
        get_u64(&header, 24).unwrap()
    })
}

#[test]
fn protected_rollover_preserves_prior_checkpoint_and_halves_physical_bytes() {
    let (_dir, images, previous, mut superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let next = next_metadata(&previous, "one-slot-rollover");
    force_small_rollover_journal(&backend, &mut superblocks);
    let slots = protected_rollover_slot_targets(&backend, &superblocks, 2)
        .unwrap()
        .unwrap();
    assert_eq!(slots.len(), 3);
    let checkpoint = prepare_checkpoint_writes(&next, &superblocks).unwrap();
    let bytes_per_slot = RAW_HEADER_SIZE as u64
        + checkpoint
            .body_writes()
            .map(|(_, bytes)| bytes.len() as u64)
            .sum::<u64>();
    let metrics = CommitMetrics::default();
    append_journal_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "protected-rollover-1",
        serde_json::json!({"payload": "a".repeat(64 * 1024)}),
        JournalQuorumOptions {
            mode: JournalQuorumMode::TrustedTransaction,
            metrics: Some(&metrics),
            prepared_delta: None,
        },
    )
    .unwrap();
    let checkpoint_bytes = metrics
        .raw_write_byte_snapshots()
        .find(|(kind, _)| *kind == "checkpoint")
        .unwrap()
        .1;
    assert!(
        checkpoint_bytes <= 3 * bytes_per_slot,
        "sparse writes must never exceed one full slot per healthy member"
    );
    // Historical dual-slot rollover wrote two full copies per member.
    assert!(checkpoint_bytes < 3 * 2 * bytes_per_slot);
    for sb in &superblocks {
        let mut txids = checkpoint_slot_txids(&backend, sb);
        txids.sort_unstable();
        assert_eq!(txids, [previous.txid, next.txid]);
    }
    assert_eq!(
        recover_metadata(&backend, &superblocks).unwrap().txid,
        next.txid
    );

    // Another rollover must overwrite the older slot, not the newly
    // committed checkpoint. This is essential because the prior journal
    // was reset and can no longer replay from the *older* image.
    let third = next_metadata(&next, "one-slot-rollover-again");
    force_small_rollover_journal(&backend, &mut superblocks);
    let metrics2 = CommitMetrics::default();
    append_journal_quorum(
        &backend,
        &superblocks,
        &third,
        Some(&next),
        "protected-rollover-2",
        serde_json::json!({"payload": "a".repeat(64 * 1024)}),
        JournalQuorumOptions {
            mode: JournalQuorumMode::TrustedTransaction,
            metrics: Some(&metrics2),
            prepared_delta: None,
        },
    )
    .unwrap();
    for sb in &superblocks {
        let mut txids = checkpoint_slot_txids(&backend, sb);
        txids.sort_unstable();
        assert_eq!(txids, [next.txid, third.txid]);
    }
    let recovered = recover_metadata(&backend, &superblocks).unwrap();
    assert_eq!(recovered.txid, third.txid);
    assert_eq!(recovered.raw_pool.pool_name, "one-slot-rollover-again");
}

#[test]
fn protected_rollover_verifies_old_bytes_and_falls_back_on_corrupt_member() {
    let (_dir, images, previous, mut superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let bad = &superblocks[0];
    // Both headers still match the other members, but the actual checkpoint
    // bytes are corrupted. The preflight must not count this member as a
    // valid recovery vote, and this member must receive both new slots.
    for slot in 0..2u64 {
        let offset = bad.metadata.offset + slot * bad.metadata.length / 2;
        let mut header = [0u8; RAW_HEADER_SIZE];
        backend.read_at(&bad.disk_id, offset, &mut header).unwrap();
        let first_data = align_up(
            RAW_HEADER_SIZE as u64 + get_u64(&header, 160).unwrap(),
            METADATA_PAGE_SIZE as u64,
        );
        backend
            .write_at(&bad.disk_id, offset + first_data, &[0x7f])
            .unwrap();
    }
    let targets = protected_rollover_slot_targets(&backend, &superblocks, 2)
        .unwrap()
        .unwrap();
    assert_eq!(targets.len(), 2);
    assert!(!targets.contains_key(&bad.disk_id));
    let next = next_metadata(&previous, "corrupt-member-fallback");
    force_small_rollover_journal(&backend, &mut superblocks);
    append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "protected-rollover-corrupt",
        serde_json::json!({"payload": "a".repeat(64 * 1024)}),
    )
    .unwrap();
    let old_quorum = superblocks[1..]
        .iter()
        .all(|sb| checkpoint_slot_txids(&backend, sb).contains(&previous.txid));
    assert!(old_quorum, "old checkpoint must survive on a quorum");
    assert_eq!(
        checkpoint_slot_txids(&backend, &superblocks[0]),
        [next.txid, next.txid]
    );
    assert_eq!(
        recover_metadata(&backend, &superblocks).unwrap().txid,
        next.txid
    );
}

#[test]
fn torn_new_slot_does_not_destroy_old_checkpoint_or_quorum_recovery() {
    let (_dir, images, previous, mut superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    force_small_rollover_journal(&backend, &mut superblocks);
    let targets = protected_rollover_slot_targets(&backend, &superblocks, 2)
        .unwrap()
        .unwrap();
    let sb = &superblocks[0];
    let slot = *targets.get(&sb.disk_id).unwrap();
    let slot_start = sb.metadata.offset + slot * (sb.metadata.length / 2);
    let torn = TornCheckpointBackend {
        inner: block_backend_for(&images),
        disk_id: sb.disk_id.clone(),
        body_start: slot_start + RAW_HEADER_SIZE as u64,
        slot_end: slot_start + sb.metadata.length / 2,
        fail_once: std::sync::atomic::AtomicBool::new(true),
    };
    let next = next_metadata(&previous, "partial-new-slot");
    let report = append_transaction_with_trusted_integrity_quorum(
        &torn,
        &superblocks,
        &next,
        Some(&previous),
        "partial-new-slot",
        serde_json::json!({"payload": "z".repeat(64 * 1024)}),
    )
    .unwrap();
    assert!(report.failed_devices.contains_key(&sb.disk_id));
    assert!(!torn.fail_once.load(std::sync::atomic::Ordering::Relaxed));
    let candidates = read_metadata_candidates_cached(&backend, sb, &mut BTreeMap::new()).unwrap();
    assert_eq!(candidates.iter().filter(|(_, r)| r.valid).count(), 1);
    assert!(candidates
        .iter()
        .any(|(_, r)| r.valid && r.txid == previous.txid));
    // Two other members published the new checkpoint, despite the torn
    // write. Recovery still accepts it on an independently verified quorum.
    assert_eq!(
        recover_metadata(&backend, &superblocks).unwrap().txid,
        next.txid
    );
}

#[test]
fn interrupted_protected_rollover_retains_old_quorum_on_successful_member() {
    let (_dir, images, previous, mut superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    force_small_rollover_journal(&backend, &mut superblocks);
    let next = next_metadata(&previous, "rollover-will-fail");
    let failing = WriteFailBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0000".into(), "disk-0001".into()]),
    };
    let result = append_transaction_with_trusted_integrity_quorum(
        &failing,
        &superblocks,
        &next,
        Some(&previous),
        "rollover-failed",
        serde_json::json!({"payload": "a".repeat(64 * 1024)}),
    );
    assert!(result.is_err());
    // The only member that published the new checkpoint did not erase its
    // previous trusted slot, so previous metadata still has three votes.
    let mut txids = checkpoint_slot_txids(&backend, &superblocks[2]);
    txids.sort_unstable();
    assert_eq!(txids, [previous.txid, next.txid]);
    assert_eq!(
        recover_metadata(&backend, &superblocks).unwrap().txid,
        previous.txid
    );
}

#[test]
fn compressed_large_metadata_rollover_keeps_verified_old_slot() {
    let (_dir, images, mut previous, mut superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let template = previous
        .inodes
        .values()
        .find(|inode| inode.kind == crate::types::NodeKind::File)
        .unwrap()
        .clone();
    // High inode count with repeated xattrs mimics the compressible 919 MB
    // metadata reported in the 294k-inode physical run, at CI-safe scale.
    for ino in 100_000..108_000 {
        let mut inode = template.clone();
        inode.id = ino;
        inode.xattrs.insert("user.archive".into(), "x".repeat(1024));
        previous.inodes.insert(ino, inode);
    }
    previous.next_inode = 108_000;
    previous.txid += 1;
    journal::prepare_metadata_integrity_with_previous(&mut previous, String::new()).unwrap();
    assert!(
        prepare_checkpoint_payload(&previous, &superblocks)
            .unwrap()
            .original
            .is_some(),
        "fixture should exercise compressed checkpoint recovery"
    );
    write_metadata_copies_quorum(&backend, &superblocks, &previous).unwrap();
    for sb in &superblocks {
        let mut header = [0u8; RAW_HEADER_SIZE];
        backend
            .read_at(&sb.disk_id, sb.metadata.offset, &mut header)
            .unwrap();
        assert_eq!(get_u32(&header, 20).unwrap(), METADATA_FORMAT_TREE_ZSTD);
    }
    let targets = protected_rollover_slot_targets(&backend, &superblocks, 2)
        .unwrap()
        .unwrap();
    assert_eq!(targets.len(), 3);
    let next = next_metadata(&previous, "compressed-protected-rollover");
    force_small_rollover_journal(&backend, &mut superblocks);
    let metrics = CommitMetrics::default();
    append_journal_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "compressed-rollover",
        serde_json::json!({"payload": "c".repeat(64 * 1024)}),
        JournalQuorumOptions {
            mode: JournalQuorumMode::TrustedTransaction,
            metrics: Some(&metrics),
            prepared_delta: None,
        },
    )
    .unwrap();
    let strategies = metrics
        .rollover_member_strategy_snapshots()
        .collect::<BTreeMap<_, _>>();
    assert_eq!(strategies["verified_single_slot"], 3);
    assert_eq!(strategies["conservative_dual_slot"], 0);
    for sb in &superblocks {
        let mut txids = checkpoint_slot_txids(&backend, sb);
        txids.sort_unstable();
        assert_eq!(txids, [previous.txid, next.txid]);
    }
    let recovered = recover_metadata(&backend, &superblocks).unwrap();
    assert_eq!(recovered.txid, next.txid);
    assert_eq!(recovered.inodes.len(), previous.inodes.len());
}

#[test]
fn rollover_refuses_to_erase_last_certified_replay_base() {
    let (_dir, images, previous, mut superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let full_lengths = superblocks
        .iter()
        .map(|sb| sb.journal.length)
        .collect::<Vec<_>>();
    let checkpointed = next_metadata(&previous, "certified-checkpoint");
    force_small_rollover_journal(&backend, &mut superblocks);
    append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &checkpointed,
        Some(&previous),
        "initial-rollover",
        serde_json::json!({"payload": "x".repeat(64 * 1024)}),
    )
    .unwrap();
    for (sb, full_len) in superblocks.iter_mut().zip(full_lengths) {
        sb.journal.length = full_len;
    }
    let acknowledged = next_metadata(&checkpointed, "acknowledged-delta");
    append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &acknowledged,
        Some(&checkpointed),
        "acknowledged-delta",
        serde_json::json!({"previous_meta_hash": checkpointed.integrity.meta_hash}),
    )
    .unwrap();

    // Two current checkpoint bodies are corrupt. Their older slots still
    // form a physical quorum, but only the third member can validate the
    // newer checkpoint certificate and replay the acknowledged journal delta.
    for sb in &superblocks[..2] {
        let slot_len = sb.metadata.length / 2;
        let new_slot = read_metadata_candidates(&backend, sb)
            .unwrap()
            .iter()
            .position(|(candidate, _)| {
                candidate
                    .as_ref()
                    .is_some_and(|m| m.txid == checkpointed.txid)
            })
            .unwrap();
        let slot_offset = sb.metadata.offset + new_slot as u64 * slot_len;
        let mut header = [0u8; RAW_HEADER_SIZE];
        backend
            .read_at(&sb.disk_id, slot_offset, &mut header)
            .unwrap();
        let data_offset = align_up(
            RAW_HEADER_SIZE as u64 + get_u64(&header, 160).unwrap(),
            METADATA_PAGE_SIZE as u64,
        );
        let mut byte = [0u8; 1];
        backend
            .read_at(&sb.disk_id, slot_offset + data_offset, &mut byte)
            .unwrap();
        byte[0] ^= 1;
        backend
            .write_at(&sb.disk_id, slot_offset + data_offset, &byte)
            .unwrap();
    }
    assert_eq!(
        recover_metadata(&backend, &superblocks).unwrap().txid,
        acknowledged.txid
    );
    let survivor = superblocks[2].clone();
    let before_slots = checkpoint_slot_txids(&backend, &survivor);
    let before_journal = raw_journal_end(&backend, &survivor);

    // A one-member rollover would overwrite the last current checkpoint and
    // reset its journal. Refuse the write before touching *any* member even if
    // the other two would fail all writes (quorum failure).
    force_small_rollover_journal(&backend, &mut superblocks);
    let failing = WriteFailBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from([
            superblocks[0].disk_id.clone(),
            superblocks[1].disk_id.clone(),
        ]),
    };
    let attempted = next_metadata(&acknowledged, "unsafe-next-rollover");
    let result = append_transaction_with_trusted_integrity_quorum(
        &failing,
        &superblocks,
        &attempted,
        Some(&acknowledged),
        "unsafe-next-rollover",
        serde_json::json!({"payload": "x".repeat(64 * 1024)}),
    );
    assert!(
        matches!(&result, Err(ArgosError::Conflict(message)) if message.contains("newer valid checkpoint lacks a quorum")),
        "{result:?}"
    );
    assert_eq!(checkpoint_slot_txids(&backend, &survivor), before_slots);
    assert_eq!(raw_journal_end(&backend, &survivor), before_journal);
    assert_eq!(
        recover_metadata(&backend, &superblocks).unwrap().txid,
        acknowledged.txid
    );
}

#[test]
fn changed_page_checkpoint_rewrites_only_modified_and_corrupt_target_pages() {
    let (_dir, images, _, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let sb = &superblocks[0];
    let slot = sb.metadata.offset;
    let mut bytes = (0..2 * CHECKPOINT_BATCH_BYTES + 17)
        .map(|index| (index as u64 * 31 % 251) as u8)
        .collect::<Vec<_>>();
    let old = metadata_tree_checkpoint(
        &sb.disk_id,
        100,
        100,
        &bytes,
        &sha256_hex(&bytes),
        sb.metadata.length / 2,
    )
    .unwrap();
    write_metadata_slot(&backend, sb, slot, &old, None).unwrap();
    let data_start = align_up(
        RAW_HEADER_SIZE as u64 + get_u64(&old.header, 160).unwrap(),
        METADATA_PAGE_SIZE as u64,
    );
    // One new page in each of the first two 1 MiB batches. Corrupt a third
    // page which the logical transaction *did not change*: the writer must
    // use verified physical comparison, not trust the old slot's index hash.
    bytes[100 * METADATA_PAGE_SIZE + 8] ^= 1;
    bytes[300 * METADATA_PAGE_SIZE + 7] ^= 1;
    let corrupt_offset = slot + data_start + 400 * METADATA_PAGE_SIZE as u64 + 11;
    backend
        .write_at(&sb.disk_id, corrupt_offset, &[0xab])
        .unwrap();
    let next = metadata_tree_checkpoint(
        &sb.disk_id,
        101,
        101,
        &bytes,
        &sha256_hex(&bytes),
        sb.metadata.length / 2,
    )
    .unwrap();
    let metrics = CommitMetrics::default();
    write_metadata_slot_differential(&backend, sb, slot, &next, Some(&metrics)).unwrap();
    let written = metrics
        .raw_write_byte_snapshots()
        .find(|(kind, _)| *kind == "checkpoint")
        .unwrap()
        .1;
    let full = next.header.len() as u64
        + next
            .body_writes()
            .map(|(_, data)| data.len() as u64)
            .sum::<u64>();
    assert!(
        written < full / 16,
        "expected sparse write << full checkpoint: {written} vs {full}",
    );
    assert!(
        written >= (3 * METADATA_PAGE_SIZE + RAW_HEADER_SIZE) as u64,
        "all changed and corrupted pages must be rewritten",
    );
    let mut header = [0u8; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, slot, &mut header).unwrap();
    assert_eq!(get_u64(&header, 24).unwrap(), 101);
    assert_eq!(
        read_tree_metadata_candidate(&backend, sb, slot, sb.metadata.length / 2, &header).unwrap(),
        bytes,
    );
}

#[test]
fn unchanged_target_checkpoint_publishes_only_new_header() {
    let (_dir, images, _, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let sb = &superblocks[0];
    let slot = sb.metadata.offset;
    let bytes = vec![61u8; 2 * CHECKPOINT_BATCH_BYTES + 17];
    let first = metadata_tree_checkpoint(
        &sb.disk_id,
        1,
        1,
        &bytes,
        &sha256_hex(&bytes),
        sb.metadata.length / 2,
    )
    .unwrap();
    write_metadata_slot(&backend, sb, slot, &first, None).unwrap();
    let newer = metadata_tree_checkpoint(
        &sb.disk_id,
        2,
        2,
        &bytes,
        &sha256_hex(&bytes),
        sb.metadata.length / 2,
    )
    .unwrap();
    let metrics = CommitMetrics::default();
    write_metadata_slot_differential(&backend, sb, slot, &newer, Some(&metrics)).unwrap();
    let written = metrics
        .raw_write_byte_snapshots()
        .find(|(kind, _)| *kind == "checkpoint")
        .unwrap()
        .1;
    assert_eq!(written, RAW_HEADER_SIZE as u64);
    let mut header = [0u8; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, slot, &mut header).unwrap();
    assert_eq!(get_u64(&header, 24).unwrap(), 2);
    assert_eq!(
        read_tree_metadata_candidate(&backend, sb, slot, sb.metadata.length / 2, &header).unwrap(),
        bytes,
    );
}

#[test]
fn unreadable_target_data_falls_back_to_full_verified_write() {
    let (_dir, images, _, superblocks) = quorum_fixture();
    let backend = block_backend_for(&images);
    let sb = &superblocks[0];
    let slot = sb.metadata.offset;
    let bytes = vec![29u8; METADATA_PAGE_SIZE * 3 + 13];
    let checkpoint = metadata_tree_checkpoint(
        &sb.disk_id,
        17,
        17,
        &bytes,
        &sha256_hex(&bytes),
        sb.metadata.length / 2,
    )
    .unwrap();
    let failing = JournalPayloadFailBackend {
        inner: block_backend_for(&images),
        failed_device: sb.disk_id.clone(),
        fail_from: slot + METADATA_PAGE_SIZE as u64,
        fail_to: slot + sb.metadata.length / 2,
    };
    let metrics = CommitMetrics::default();
    write_metadata_slot_differential(&failing, sb, slot, &checkpoint, Some(&metrics)).unwrap();
    let mut header = [0u8; RAW_HEADER_SIZE];
    backend.read_at(&sb.disk_id, slot, &mut header).unwrap();
    assert_eq!(
        read_tree_metadata_candidate(&backend, sb, slot, sb.metadata.length / 2, &header).unwrap(),
        bytes,
    );
}
