use super::*;
use crate::raw_format::MIN_DEVICE_BYTES;
use crate::types::{Disk, MetadataIntegrity, VolumeConfig};
use crate::volume::ArgosFs;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

struct FailingBackend {
    inner: FileBlockBackend,
    failed: BTreeSet<String>,
}

struct JournalPayloadFailBackend {
    inner: FileBlockBackend,
    failed_device: String,
    fail_from: u64,
    fail_to: u64,
}

struct FlushFailBackend {
    inner: FileBlockBackend,
    failed: BTreeSet<String>,
}

impl StorageBackend for FlushFailBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }

    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }

    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(device_id, offset, buf)
    }

    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        self.inner.write_at(device_id, offset, data)
    }

    fn flush_device(&self, device_id: &String) -> Result<()> {
        if self.failed.contains(device_id) {
            return Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
        }
        self.inner.flush_device(device_id)
    }

    fn flush_all(&self) -> Result<()> {
        for device in self.inner.list_devices()? {
            self.flush_device(&device.device_id)?;
        }
        Ok(())
    }

    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.inner.capacity(device_id)
    }

    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        self.inner.device_status(device_id)
    }

    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

struct WriteFailBackend {
    inner: FileBlockBackend,
    failed: BTreeSet<String>,
}

impl StorageBackend for WriteFailBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }

    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }

    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(device_id, offset, buf)
    }

    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        if self.failed.contains(device_id) {
            return Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
        }
        self.inner.write_at(device_id, offset, data)
    }

    fn flush_device(&self, device_id: &String) -> Result<()> {
        if self.failed.contains(device_id) {
            return Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
        }
        self.inner.flush_device(device_id)
    }

    fn flush_all(&self) -> Result<()> {
        for device in self.inner.list_devices()? {
            self.flush_device(&device.device_id)?;
        }
        Ok(())
    }

    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.inner.capacity(device_id)
    }

    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        self.inner.device_status(device_id)
    }

    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

impl StorageBackend for JournalPayloadFailBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }

    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }

    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        if device_id == &self.failed_device && offset >= self.fail_from && offset < self.fail_to {
            return Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
        }
        self.inner.read_at(device_id, offset, buf)
    }

    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        self.inner.write_at(device_id, offset, data)
    }

    fn flush_device(&self, device_id: &String) -> Result<()> {
        self.inner.flush_device(device_id)
    }

    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }

    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.inner.capacity(device_id)
    }

    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        self.inner.device_status(device_id)
    }

    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

impl FailingBackend {
    fn eio() -> ArgosError {
        ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO))
    }

    fn fails(&self, device_id: &str) -> bool {
        self.failed.contains(device_id)
    }
}

impl StorageBackend for FailingBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }

    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }

    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.fails(device_id) {
            return Err(Self::eio());
        }
        self.inner.read_at(device_id, offset, buf)
    }

    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        if self.fails(device_id) {
            return Err(Self::eio());
        }
        self.inner.write_at(device_id, offset, data)
    }

    fn flush_device(&self, device_id: &String) -> Result<()> {
        if self.fails(device_id) {
            return Err(Self::eio());
        }
        self.inner.flush_device(device_id)
    }

    fn flush_all(&self) -> Result<()> {
        for device in self.inner.list_devices()? {
            self.flush_device(&device.device_id)?;
        }
        Ok(())
    }

    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.inner.capacity(device_id)
    }

    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        self.inner.device_status(device_id)
    }

    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

fn metadata() -> Metadata {
    let dir = tempdir().unwrap();
    ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap()
    .metadata_snapshot()
}

fn loop_image(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    fs::File::create(&path)
        .unwrap()
        .set_len(MIN_DEVICE_BYTES)
        .unwrap();
    path
}

fn create_loop_pool(dir: &Path, name: &str) -> (PathBuf, ArgosFs) {
    let image = dir.join(format!("{name}.img"));
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        MIN_DEVICE_BYTES,
        name,
        false,
    )
    .unwrap();
    (image, fs)
}

fn quorum_fixture() -> (
    tempfile::TempDir,
    Vec<PathBuf>,
    Metadata,
    Vec<RawSuperblock>,
) {
    let dir = tempdir().unwrap();
    let images = (0..3)
        .map(|index| dir.path().join(format!("quorum-{index}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        MIN_DEVICE_BYTES,
        "quorum-test",
        false,
    )
    .unwrap();
    fs.write_file("/base", b"durable-base", 0o600).unwrap();
    fs.sync().unwrap();
    let metadata = fs.metadata_snapshot();
    drop(fs);
    let superblocks = images
        .iter()
        .map(|path| {
            inspect_device(BackendKind::LoopBlock, path.clone())
                .unwrap()
                .0
        })
        .collect();
    (dir, images, metadata, superblocks)
}

fn block_backend_for(images: &[PathBuf]) -> FileBlockBackend {
    FileBlockBackend::open_with_ids(
        BackendKind::LoopBlock,
        images
            .iter()
            .enumerate()
            .map(|(index, path)| (format!("disk-{index:04}"), path.clone()))
            .collect(),
        true,
    )
    .unwrap()
}

fn next_metadata(previous: &Metadata, pool_name: &str) -> Metadata {
    let mut next = previous.clone();
    next.raw_pool.pool_name = pool_name.to_string();
    next.txid += 1;
    next.updated_at = crate::util::now_f64();
    journal::prepare_metadata_integrity_with_previous(
        &mut next,
        previous.integrity.meta_hash.clone(),
    )
    .unwrap();
    next
}

#[test]
fn scan_paths_rejects_host_missing_and_unformatted_devices() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing.img");
    let host = scan_paths(BackendKind::Host, std::slice::from_ref(&missing));
    assert_eq!(host.len(), 1);
    assert!(!host[0].valid);
    assert!(host[0].error.as_deref().unwrap().contains("not scan-able"));

    let absent = scan_paths(BackendKind::LoopBlock, std::slice::from_ref(&missing));
    assert!(!absent[0].valid);
    assert!(absent[0]
        .error
        .as_deref()
        .unwrap()
        .contains("failed to open"));

    let blank = loop_image(dir.path(), "blank.img");
    let scanned = scan_paths(BackendKind::LoopBlock, &[blank]);
    assert!(!scanned[0].valid);
    assert_eq!(scanned[0].capacity, MIN_DEVICE_BYTES);
    assert!(scanned[0].pool_uuid.is_none());
}

#[test]
fn known_signature_detection_covers_filesystems_partition_tables_and_swap() {
    let mut head = vec![0u8; 0x10050];
    let tail = vec![0u8; 65536];
    for (offset, signature) in [
        (0usize, b"hsqs".as_slice()),
        (0, b"XFSB"),
        (0, b"LUKS\xba\xbe"),
        (3, b"NTFS    "),
        (3, b"EXFAT   "),
        (512, b"EFI PART"),
        (512, b"LABELONE"),
        (0x438, &[0x53, 0xef]),
        (1024, &0xF2F5_2010u32.to_le_bytes()),
        (4096, &0xA92B_4EFCu32.to_le_bytes()),
        (0x10040, b"_BHRfS_M"),
    ] {
        head.fill(0);
        head[offset..offset + signature.len()].copy_from_slice(signature);
        assert!(has_known_signature(&head, &tail), "offset={offset}");
    }
    head.fill(0);
    head[4096 - 10..4096].copy_from_slice(b"SWAPSPACE2");
    assert!(contains_swap_signature(&head));
    assert!(has_known_signature(&head, &tail));

    let mut tail_signature = tail.clone();
    tail_signature[100..108].copy_from_slice(b"EFI PART");
    assert!(has_known_signature(&vec![0; 0x10050], &tail_signature));
    tail_signature.fill(0);
    tail_signature[200..204].copy_from_slice(&0xA92B_4EFCu32.to_le_bytes());
    assert!(has_known_signature(&vec![0; 0x10050], &tail_signature));
    assert!(!has_known_signature(&vec![0; 0x10050], &tail));
    assert!(!contains_swap_signature(&[0; 100]));
}

#[test]
fn raw_journal_quorum_ignores_unreadable_and_invalid_members() {
    assert!(raw_journal_quorum(&[], 0));
    let member = |readable, invalid, txid, hash: &str| RawJournalMemberReport {
        readable,
        invalid_entries: invalid,
        last_valid_txid: txid,
        last_valid_generation: txid,
        last_valid_record_hash: hash.to_string(),
        ..RawJournalMemberReport::default()
    };
    let members = vec![
        member(true, 0, 2, "same"),
        member(true, 0, 2, "same"),
        member(true, 0, 3, "different"),
    ];
    assert!(raw_journal_quorum(&members, 3));
    assert!(!raw_journal_quorum(&members[..1], 3));
    assert!(!raw_journal_quorum(
        &[member(false, 0, 2, "same"), member(true, 1, 2, "same")],
        2
    ));
}

#[test]
fn quorum_journal_commit_survives_one_member_eio() {
    let (_dir, images, previous, superblocks) = quorum_fixture();
    let next = next_metadata(&previous, "committed-with-one-eio");
    let backend = FailingBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0002".to_string()]),
    };

    let report = append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "quorum-eio-test",
        serde_json::json!({}),
    )
    .unwrap();
    assert!(report.failed_devices.contains_key("disk-0002"));
    drop(backend);

    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    let recovered = reopened.metadata_snapshot();
    assert_eq!(recovered.txid, next.txid);
    assert_eq!(recovered.raw_pool.pool_name, "committed-with-one-eio");
}

#[test]
fn quorum_certificate_preserves_commit_across_changing_member_failures() {
    let (_dir, images, previous, superblocks) = quorum_fixture();
    let next = next_metadata(&previous, "certified-changing-failure");
    let backend = FailingBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0002".to_string()]),
    };
    append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "changing-failure-test",
        serde_json::json!({}),
    )
    .unwrap();
    drop(backend);

    // The write quorum was disk-0000/disk-0001. On the next read quorum,
    // disk-0000 is absent while disk-0002 has recovered and still carries the
    // old checkpoint. The certificate on disk-0001 must preserve the acked tx.
    std::fs::remove_file(&images[0]).unwrap();
    let reopened = ArgosFs::open_loop(&images[1..], false).unwrap();
    let recovered = reopened.metadata_snapshot();
    assert_eq!(recovered.txid, next.txid);
    assert_eq!(recovered.raw_pool.pool_name, "certified-changing-failure");
}

#[test]
fn stale_same_txid_suffix_is_excluded_from_new_quorum_append() {
    let (_dir, images, previous, superblocks) = quorum_fixture();
    let stale = next_metadata(&previous, "stale-uncommitted");
    let (stale_entry, _) = build_journal_entry(
        &stale,
        Some(&previous),
        "stale-test",
        serde_json::json!({}),
        true,
    )
    .unwrap();
    let backend = block_backend_for(&images);
    let (header, write_offset, end, rollover) =
        journal_append_position(&backend, &superblocks[0], stale_entry.len()).unwrap();
    assert!(!rollover);
    append_journal_member(
        &backend,
        &superblocks[0],
        header,
        write_offset,
        end,
        &stale_entry,
        true,
    )
    .unwrap();
    drop(backend);

    let next = next_metadata(&previous, "new-same-txid");
    let backend = FailingBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0002".to_string()]),
    };
    let err = append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "new-same-txid-test",
        serde_json::json!({}),
    )
    .unwrap_err();
    assert!(matches!(err, ArgosError::QuorumUnavailable { .. }));
}

#[test]
fn dirty_superblock_marking_succeeds_with_one_write_rejecting_member() {
    let (_dir, images, metadata, superblocks) = quorum_fixture();
    let backend = WriteFailBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0002".to_string()]),
    };
    let report =
        write_superblock_clean_state_quorum(&backend, &superblocks, &metadata, false).unwrap();
    assert!(report.failed_devices.contains_key("disk-0002"));
}

#[test]
fn journal_flush_shortfall_is_indeterminate_after_quorum_exposure() {
    let (_dir, images, previous, superblocks) = quorum_fixture();
    let next = next_metadata(&previous, "indeterminate-flush");
    let backend = FlushFailBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0001".to_string(), "disk-0002".to_string()]),
    };

    let err = append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "indeterminate-flush-test",
        serde_json::json!({}),
    )
    .unwrap_err();
    assert!(matches!(err, ArgosError::IndeterminateCommit(_)));
    drop(backend);

    // The failed flushes do not prove absence: the just-written journal copies
    // can still be visible to recovery, which is why callers must not roll back
    // this result as an ordinary uncommitted quorum failure.
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    let recovered = reopened.metadata_snapshot();
    assert_eq!(recovered.txid, next.txid);
    assert_eq!(recovered.raw_pool.pool_name, "indeterminate-flush");
}

#[test]
fn writable_replay_checkpoint_tolerates_one_persistent_member_write_failure() {
    let (_dir, images, previous, superblocks) = quorum_fixture();
    let next = next_metadata(&previous, "replay-checkpoint-quorum");
    let backend = block_backend_for(&images);
    append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "replay-checkpoint-source",
        serde_json::json!({}),
    )
    .unwrap();
    drop(backend);

    let backend = WriteFailBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0002".to_string()]),
    };
    let (recovered, report) = load_or_recover(&backend, &superblocks, true).unwrap();
    assert!(report.replayed);
    assert_eq!(recovered.txid, next.txid);
    assert_eq!(recovered.raw_pool.pool_name, "replay-checkpoint-quorum");
    drop(backend);

    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(reopened.metadata_snapshot().txid, next.txid);
}

#[test]
fn journal_recovery_ignores_one_member_payload_eio_when_quorum_is_readable() {
    let (_dir, images, previous, superblocks) = quorum_fixture();
    let next = next_metadata(&previous, "recover-with-payload-eio");
    let backend = block_backend_for(&images);
    append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "payload-eio-recovery",
        serde_json::json!({}),
    )
    .unwrap();
    drop(backend);

    let failed = &superblocks[2];
    let backend = JournalPayloadFailBackend {
        inner: block_backend_for(&images),
        failed_device: failed.disk_id.clone(),
        fail_from: failed.journal.offset + RAW_HEADER_SIZE as u64 + 36,
        fail_to: failed.journal.offset + failed.journal.length,
    };
    let mut report = TransactionReport::default();
    let recovered =
        read_latest_journal_metadata(&backend, &superblocks, &mut report, Some(&previous))
            .unwrap()
            .unwrap();

    assert_eq!(recovered.txid, next.txid);
    assert_eq!(recovered.raw_pool.pool_name, "recover-with-payload-eio");
    let failed_member = report
        .raw_journal_members
        .iter()
        .find(|member| member.disk_id == failed.disk_id)
        .unwrap();
    assert!(failed_member
        .error
        .as_deref()
        .unwrap()
        .contains("I/O error"));
    assert!(failed_member.invalid_entries > 0);
    assert_eq!(report.raw_journal_quorum, Some(true));
}

#[test]
fn quorum_report_distinguishes_metadata_failure_from_device_unavailability() {
    let mut report = QuorumWriteReport::default();
    report.record_failure(
        "metadata-full",
        &ArgosError::DiskFull {
            disk_id: "metadata-full".to_string(),
            required: 2,
            available: 1,
        },
    );
    report.record_failure(
        "eio",
        &ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)),
    );

    assert!(report.failed_devices.contains_key("metadata-full"));
    assert!(!report.unavailable_devices.contains("metadata-full"));
    assert!(report.failed_devices.contains_key("eio"));
    assert!(report.unavailable_devices.contains("eio"));

    let retryable = quorum_failure_before_write(&report, "metadata", 3, 2);
    assert!(matches!(retryable, ArgosError::QuorumUnavailable { .. }));

    let mut metadata_only = QuorumWriteReport::default();
    metadata_only.record_failure(
        "metadata-full",
        &ArgosError::DiskFull {
            disk_id: "metadata-full".to_string(),
            required: 2,
            available: 1,
        },
    );
    assert!(matches!(
        quorum_failure_before_write(&metadata_only, "metadata", 3, 2),
        ArgosError::RetryableQuorumUnavailable { .. }
    ));
}

#[test]
fn quorum_journal_commit_rejects_loss_of_majority() {
    let (_dir, images, previous, superblocks) = quorum_fixture();
    let next = next_metadata(&previous, "must-not-commit");
    let backend = FailingBackend {
        inner: block_backend_for(&images),
        failed: BTreeSet::from(["disk-0001".to_string(), "disk-0002".to_string()]),
    };

    let err = append_transaction_with_trusted_integrity_quorum(
        &backend,
        &superblocks,
        &next,
        Some(&previous),
        "quorum-loss-test",
        serde_json::json!({}),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ArgosError::QuorumUnavailable {
            need: 2,
            have: 1,
            ..
        }
    ));
    drop(backend);

    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    let recovered = reopened.metadata_snapshot();
    assert_eq!(recovered.txid, previous.txid);
    assert_eq!(recovered.raw_pool.pool_name, previous.raw_pool.pool_name);
}

#[test]
fn record_hash_and_previous_metadata_hash_are_stable_and_optional() {
    let meta = metadata();
    let mut record = RawJournalRecord {
        version: RAW_STORE_VERSION,
        time: 1.0,
        volume_uuid: meta.uuid.clone(),
        txid: meta.txid,
        generation: meta.integrity.generation,
        action: "write".to_string(),
        details: serde_json::json!({"previous_meta_hash": "previous", "extra": 1}),
        meta_hash: journal::canonical_metadata_hash(&meta).unwrap(),
        metadata: Some(meta.clone()),
        metadata_delta: None,
        record_hash: String::new(),
    };
    let first = raw_record_hash(&record).unwrap();
    record.time = 99.0;
    record.details["extra"] = serde_json::json!(2);
    record.metadata = None;
    assert_eq!(raw_record_hash(&record).unwrap(), first);
    assert_eq!(raw_record_previous_meta_hash(&record), Some("previous"));
    record.details = serde_json::json!({"previous_meta_hash": ""});
    assert_eq!(raw_record_previous_meta_hash(&record), None);
    record.details = serde_json::json!({});
    assert_eq!(raw_record_previous_meta_hash(&record), None);

    let mut legacy = meta.clone();
    legacy.integrity = MetadataIntegrity::default();
    assert_eq!(
        metadata_hash_for_replay(&legacy).unwrap(),
        journal::canonical_metadata_hash(&legacy).unwrap()
    );
    assert_eq!(
        metadata_hash_for_replay(&meta).unwrap(),
        meta.integrity.meta_hash
    );
}

#[test]
fn metadata_tree_checkpoint_covers_pages_headers_and_capacity_errors() {
    let bytes = vec![0x5a; METADATA_PAGE_SIZE * 2 + 17];
    let hash = sha256_hex(&bytes);
    let checkpoint = metadata_tree_checkpoint("disk", 7, 8, &bytes, &hash, 64 * 1024).unwrap();
    assert_eq!(&checkpoint.header[..16], METADATA_MAGIC);
    assert_eq!(
        get_u32(&checkpoint.header, 20).unwrap(),
        METADATA_FORMAT_TREE
    );
    assert_eq!(get_u64(&checkpoint.header, 24).unwrap(), 7);
    assert_eq!(get_u64(&checkpoint.header, 32).unwrap(), bytes.len() as u64);
    assert_eq!(get_u64(&checkpoint.header, 136).unwrap(), 8);
    assert_eq!(get_u64(&checkpoint.header, 152).unwrap(), 3);
    assert_eq!(checkpoint.body_writes.len(), 4);
    assert!(checkpoint
        .body_writes
        .iter()
        .any(|(offset, data)| *offset == RAW_HEADER_SIZE as u64
            && data.len() == 3 * METADATA_INDEX_ENTRY_SIZE));
    assert!(matches!(
        metadata_tree_checkpoint("disk", 0, 0, &[], &sha256_hex(&[]), 64 * 1024),
        Err(ArgosError::DiskFull { .. })
    ));
    assert!(matches!(
        metadata_tree_checkpoint("disk", 0, 0, &bytes, &hash, 4096),
        Err(ArgosError::DiskFull { .. })
    ));
}

#[test]
fn quorum_selection_uses_distinct_devices_and_highest_supported_generation() {
    let first = metadata();
    let mut second = first.clone();
    second.txid += 1;
    second.integrity.generation = second.txid;
    second.integrity.meta_hash = journal::canonical_metadata_hash(&second).unwrap();
    let report = MetadataCandidateReport::default();
    let candidates = vec![
        ("a".to_string(), Some(first.clone()), report.clone()),
        ("b".to_string(), Some(first.clone()), report.clone()),
        ("a".to_string(), Some(second.clone()), report.clone()),
        ("b".to_string(), Some(second.clone()), report.clone()),
        ("c".to_string(), None, report),
    ];
    let selected = select_quorum_metadata_candidate(&candidates)
        .unwrap()
        .unwrap();
    assert_eq!(selected.txid, second.txid);
    assert_eq!(metadata_quorum_requirement(&selected), 1);

    let mut multi = selected.clone();
    let template: Disk = multi.disks.values().next().unwrap().clone();
    for index in 1..4 {
        let mut disk = template.clone();
        disk.id = format!("disk-{index}");
        disk.status = if index == 3 {
            DiskStatus::Removed
        } else {
            DiskStatus::Online
        };
        multi.disks.insert(disk.id.clone(), disk);
    }
    assert_eq!(metadata_quorum_requirement(&multi), 2);
    assert!(select_quorum_metadata_candidate(&[(
        "a".to_string(),
        Some(multi),
        MetadataCandidateReport::default()
    )])
    .unwrap()
    .is_none());
}

#[test]
fn superblock_constructor_and_label_validation_cover_conversions_and_mismatches() {
    let pool = Uuid::new_v4();
    let sb =
        superblock_for_device(pool, 0, "disk-0000", 1, 0, 4096, MIN_DEVICE_BYTES, "pool").unwrap();
    validate_label_matches_superblock(&sb, &sb.device_label()).unwrap();
    assert!(superblock_for_device(
        pool,
        u32::MAX as usize + 1,
        "disk",
        1,
        0,
        4096,
        MIN_DEVICE_BYTES,
        "pool"
    )
    .is_err());
    assert!(superblock_for_device(
        pool,
        0,
        "disk",
        u32::MAX as usize + 1,
        0,
        4096,
        MIN_DEVICE_BYTES,
        "pool"
    )
    .is_err());
    assert!(superblock_for_device(
        pool,
        0,
        "disk",
        1,
        u32::MAX as usize + 1,
        4096,
        MIN_DEVICE_BYTES,
        "pool"
    )
    .is_err());

    let mut label = sb.device_label();
    label.pool_uuid = Uuid::new_v4();
    assert!(validate_label_matches_superblock(&sb, &label).is_err());
    label = sb.device_label();
    label.device_uuid = Uuid::new_v4();
    assert!(validate_label_matches_superblock(&sb, &label).is_err());
    label = sb.device_label();
    label.disk_id = "other".to_string();
    assert!(validate_label_matches_superblock(&sb, &label).is_err());
    label = sb.device_label();
    label.disk_index += 1;
    assert!(validate_label_matches_superblock(&sb, &label).is_err());
}

#[test]
fn binary_helpers_and_backup_offset_handle_short_utf8_and_saturation() {
    let mut bytes = [0u8; 32];
    put_u32(&mut bytes, 0, 0x12345678);
    put_u64(&mut bytes, 8, 0x0102030405060708);
    put_fixed_str(&mut bytes, 16, 8, "abcdef");
    assert_eq!(get_u32(&bytes, 0).unwrap(), 0x12345678);
    assert_eq!(get_u64(&bytes, 8).unwrap(), 0x0102030405060708);
    assert_eq!(get_fixed_hex(&bytes, 16, 8).unwrap(), "abcdef");
    assert!(get_u32(&bytes[..3], 0).is_err());
    assert!(get_u64(&bytes[..7], 0).is_err());
    assert!(get_fixed_hex(&bytes[..3], 0, 4).is_err());
    assert!(get_fixed_hex(&[0xff, 0], 0, 2).is_err());
    assert_eq!(checked_usize(7, "value").unwrap(), 7);
    assert_eq!(backup_superblock_offset_for_capacity(0), 0);
    assert_eq!(
        backup_superblock_offset_for_capacity(MIN_DEVICE_BYTES) % 4096,
        0
    );
}

#[test]
fn loop_pool_lifecycle_scans_inspects_audits_and_uses_backup_superblock() {
    let dir = tempdir().unwrap();
    let (image, fs) = create_loop_pool(dir.path(), "lifecycle");
    fs.write_file("/file", b"payload", 0o600).unwrap();
    fs.sync().unwrap();
    let initial = scan_paths(BackendKind::LoopBlock, std::slice::from_ref(&image));
    assert!(initial[0].valid);
    assert_eq!(initial[0].superblock_source.as_deref(), Some("primary"));
    let (sb, label) = inspect_device(BackendKind::LoopBlock, image.clone()).unwrap();
    validate_label_matches_superblock(&sb, &label).unwrap();
    assert!(inspect_device(BackendKind::Host, image.clone()).is_err());
    assert!(superblock_from_path(BackendKind::Host, &image).is_err());
    let opened = open_pool(BackendKind::LoopBlock, std::slice::from_ref(&image), false).unwrap();
    assert_eq!(opened.metadata.backend, BackendKind::LoopBlock);
    assert!(!opened.superblocks.is_empty());
    let report = audit(&*opened.backend, &opened.superblocks).unwrap();
    assert!(report.raw_journal_quorum.is_some());
    preflight_devices_empty(&*opened.backend, &opened.superblocks, true).unwrap();
    assert!(preflight_devices_empty(&*opened.backend, &opened.superblocks, false).is_err());

    let backend = FileBlockBackend::open_loop(std::slice::from_ref(&image), true).unwrap();
    backend
        .write_at(
            &"disk-0000".to_string(),
            PRIMARY_SUPERBLOCK_OFFSET,
            &[0; SUPERBLOCK_SIZE],
        )
        .unwrap();
    backend.flush_all().unwrap();
    let scanned = scan_paths(BackendKind::LoopBlock, std::slice::from_ref(&image));
    assert!(scanned[0].valid);
    assert_eq!(scanned[0].superblock_source.as_deref(), Some("backup"));
    let (_, source) = read_superblock_with_backup(&backend, "disk-0000", MIN_DEVICE_BYTES).unwrap();
    assert_eq!(source, "backup");
    let (backup_sb, backup_label) = inspect_device(BackendKind::LoopBlock, image.clone()).unwrap();
    validate_label_matches_superblock(&backup_sb, &backup_label).unwrap();
    backend
        .write_at(
            &"disk-0000".to_string(),
            sb.backup_superblock_offset,
            &[0; SUPERBLOCK_SIZE],
        )
        .unwrap();
    backend.flush_all().unwrap();
    assert!(inspect_device(BackendKind::LoopBlock, image.clone()).is_err());
}

#[test]
fn open_pool_rejects_empty_duplicate_and_mixed_pool_device_sets() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing.img");
    assert!(matches!(
        open_pool(BackendKind::LoopBlock, &[missing], false),
        Err(ArgosError::MissingDevice(_))
    ));

    let (first, first_fs) = create_loop_pool(dir.path(), "first");
    first_fs.mark_clean_unmount().unwrap();
    drop(first_fs);
    assert!(open_pool(
        BackendKind::LoopBlock,
        &[first.clone(), first.clone()],
        false
    )
    .is_err());

    let (second, second_fs) = create_loop_pool(dir.path(), "second");
    second_fs.mark_clean_unmount().unwrap();
    drop(second_fs);
    assert!(open_pool(BackendKind::LoopBlock, &[first, second], false).is_err());
}

#[test]
fn clean_state_updates_generation_and_mount_timestamps() {
    let dir = tempdir().unwrap();
    let (image, fs) = create_loop_pool(dir.path(), "clean-state");
    let (before, _) = superblock_from_path(BackendKind::LoopBlock, &image).unwrap();
    let backend = FileBlockBackend::open_loop(std::slice::from_ref(&image), true).unwrap();
    write_superblock_clean_state(&backend, std::slice::from_ref(&before), true).unwrap();
    let (clean, _) = superblock_from_path(BackendKind::LoopBlock, &image).unwrap();
    assert!(clean.clean);
    assert!(clean.generation > before.generation);
    assert!(clean.last_clean_unmount_time > 0);
    write_superblock_clean_state(&backend, std::slice::from_ref(&clean), false).unwrap();
    let (dirty, _) = superblock_from_path(BackendKind::LoopBlock, &image).unwrap();
    assert!(!dirty.clean);
    assert!(dirty.last_mount_time > 0);
    drop(fs);
}

#[test]
fn writable_recovery_checkpoints_and_resets_consumed_journal() {
    let dir = tempdir().unwrap();
    let (image, fs) = create_loop_pool(dir.path(), "recovery-journal");
    fs.write_file("/persistent", b"survives replay compaction", 0o644)
        .unwrap();
    let txid = fs.metadata_snapshot().txid;
    assert!(fs.transaction_report().unwrap().replayed);

    // Model power loss: keep the on-disk journal newer than the metadata
    // checkpoint instead of letting ArgosFs::drop mark the pool clean.
    std::mem::forget(fs);

    let recovered = open_pool(BackendKind::LoopBlock, std::slice::from_ref(&image), true).unwrap();
    assert!(recovered.report.replayed);
    assert_eq!(recovered.metadata.txid, txid);
    let compacted = audit(&*recovered.backend, &recovered.superblocks).unwrap();
    assert_eq!(
        compacted.raw_journal_members[0].journal_end,
        RAW_HEADER_SIZE as u64
    );
    assert!(!compacted.replayed);
    assert_eq!(
        recover_metadata(&*recovered.backend, &recovered.superblocks)
            .unwrap()
            .txid,
        txid
    );
}

#[test]
fn journal_falls_back_to_checkpoint_when_delta_base_hash_is_stale() {
    let dir = tempdir().unwrap();
    let (image, fs) = create_loop_pool(dir.path(), "stale-delta-base");
    fs.mark_clean_unmount().unwrap();
    drop(fs);

    let opened = open_pool(BackendKind::LoopBlock, std::slice::from_ref(&image), true).unwrap();
    let persisted = opened.metadata.clone();
    let persisted_hash = journal::canonical_metadata_hash(&persisted).unwrap();
    assert_eq!(persisted.integrity.meta_hash, persisted_hash);

    let mut stale_base = persisted.clone();
    stale_base.disks.values_mut().next().unwrap().used_bytes += 4096;
    assert_ne!(
        journal::canonical_metadata_hash(&stale_base).unwrap(),
        stale_base.integrity.meta_hash
    );

    let mut next = stale_base.clone();
    next.txid += 1;
    next.updated_at = now_f64();
    journal::prepare_metadata_integrity_with_previous(&mut next, persisted_hash).unwrap();
    append_transaction_with_previous(
        &*opened.backend,
        &opened.superblocks,
        &next,
        Some(&stale_base),
        "stale-base-regression",
        serde_json::json!({"previous_meta_hash": next.integrity.previous_meta_hash}),
    )
    .unwrap();

    let report = audit(&*opened.backend, &opened.superblocks).unwrap();
    assert_eq!(report.invalid_entries, 0);
    assert!(report.replayed);
    let recovered = recover_metadata(&*opened.backend, &opened.superblocks).unwrap();
    assert_eq!(recovered.txid, next.txid);
    assert_eq!(
        recovered.disks.values().next().unwrap().used_bytes,
        next.disks.values().next().unwrap().used_bytes
    );
}
