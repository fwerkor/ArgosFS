use super::*;
use crate::raw_format::MIN_DEVICE_BYTES;
use crate::types::VolumeConfig;
use crate::util::content_hash_hex;

fn fixture() -> (tempfile::TempDir, ArgosFs, String, u64, u64, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("verifier.img");
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        MIN_DEVICE_BYTES,
        "verify-test",
        false,
    )
    .unwrap();
    let data = vec![7u8; 4096];
    fs.write_file("/data", &data, 0o600).unwrap();
    fs.sync().unwrap();
    let meta = fs.meta.read();
    let shard = meta
        .inodes
        .values()
        .flat_map(|inode| &inode.blocks)
        .flat_map(|block| &block.shards)
        .next()
        .unwrap();
    let stripe = meta
        .inodes
        .values()
        .flat_map(|inode| &inode.blocks)
        .next()
        .unwrap()
        .stripe_id
        .clone();
    let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
        panic!("raw extent expected");
    };
    let (offset, generation) = (extent.offset, extent.generation);
    let mut stored = vec![0u8; shard.size];
    fs.backend
        .read_at(&"disk-0000".to_string(), offset, &mut stored)
        .unwrap();
    assert!(content_hash_matches(&stored, &shard.sha256));
    drop(meta);
    (dir, fs, stripe, offset, generation, stored)
}

fn start(fs: &ArgosFs, delay_ms: u64, retries: u32) -> Arc<VerifyAfterWrite> {
    VerifyAfterWrite::start(
        fs.backend.clone(),
        Duration::from_millis(delay_ms),
        retries,
        fs.meta.clone(),
        fs.quarantined_devices.clone(),
        fs.write_fence.clone(),
    )
}

fn wait_for(verifier: &VerifyAfterWrite, wanted: u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if verifier.checks() >= wanted
            && verifier
                .queue
                .lock()
                .expect("verify queue")
                .active
                .is_empty()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("verifier did not finish {wanted} checks before deadline");
}

#[test]
fn verifies_written_shard_and_fences_on_corruption() {
    let (_dir, fs, stripe, offset, generation, data) = fixture();
    let verifier = start(&fs, 30, 0);
    verifier.enqueue(
        "disk-0000",
        offset,
        generation,
        data.len(),
        &content_hash_hex(&data),
        &stripe,
    );
    wait_for(&verifier, 1);
    assert_eq!(verifier.failures(), 0);
    fs.backend
        .write_at(&"disk-0000".to_string(), offset, &vec![9; data.len()])
        .unwrap();
    fs.backend.flush_device(&"disk-0000".to_string()).unwrap();
    verifier.enqueue(
        "disk-0000",
        offset,
        generation,
        data.len(),
        &content_hash_hex(&data),
        &stripe,
    );
    wait_for(&verifier, 2);
    assert_eq!(verifier.failures(), 1);
    assert!(fs.quarantined_devices.lock().contains("disk-0000"));
    assert!(fs.write_fence.lock().is_some());
    verifier.stop();
}

#[test]
fn retries_corrupt_shard_before_quarantining() {
    let (_dir, fs, stripe, offset, generation, data) = fixture();
    let verifier = start(&fs, 30, 1);
    fs.backend
        .write_at(&"disk-0000".to_string(), offset, &vec![4; data.len()])
        .unwrap();
    fs.backend.flush_device(&"disk-0000".to_string()).unwrap();
    verifier.enqueue(
        "disk-0000",
        offset,
        generation,
        data.len(),
        &content_hash_hex(&data),
        &stripe,
    );
    wait_for(&verifier, 2);
    assert_eq!(verifier.retries(), 1);
    assert_eq!(verifier.failures(), 1);
    verifier.stop();
}

#[test]
fn deleted_extent_and_reused_offset_are_not_misclassified() {
    let (_dir, mut fs, _stripe, original_offset, _generation, _data) = fixture();
    let verifier = start(&fs, 250, 0);
    fs.verify_written = Some(verifier.clone());
    fs.write_file("/data", &vec![8; 4096], 0o600).unwrap();
    fs.write_file("/data", &vec![9; 4096], 0o600).unwrap();
    fs.sync().unwrap();
    let current_offset = fs
        .meta
        .read()
        .inodes
        .values()
        .flat_map(|inode| &inode.blocks)
        .flat_map(|block| &block.shards)
        .find_map(|shard| match &shard.location {
            Some(ShardLocation::RawExtent(extent)) => Some(extent.offset),
            _ => None,
        })
        .unwrap();
    assert_eq!(current_offset, original_offset);
    wait_for(&verifier, 1);
    assert_eq!(verifier.failures(), 0);
    assert!(fs.quarantined_devices.lock().is_empty());
    assert!(verifier.skipped() > 0);
    verifier.stop();
}

#[test]
fn closed_queue_reports_dropped_work() {
    let (_dir, fs, stripe, offset, generation, data) = fixture();
    let verifier = start(&fs, 3_600_000, 0);
    for idx in 0..(MAX_QUEUE + 4) {
        verifier.enqueue(
            "disk-0000",
            offset + (idx as u64) * 4096,
            generation,
            data.len(),
            &content_hash_hex(&data),
            &stripe,
        );
    }
    assert!(verifier.skipped() >= 4);
    verifier.stop();
    assert_eq!(verifier.skipped(), (MAX_QUEUE + 4) as u64);
}

#[test]
fn dynamic_member_is_verified_through_guarded_fallback() {
    let (_dir, fs, stripe, offset, generation, data) = fixture();
    let primary: Arc<dyn StorageBackend> = Arc::new(
        FileBlockBackend::open_with_ids(crate::types::BackendKind::LoopBlock, Vec::new(), false)
            .unwrap(),
    );
    let verifier = VerifyAfterWrite::start(
        primary,
        Duration::from_millis(30),
        0,
        fs.meta.clone(),
        fs.quarantined_devices.clone(),
        fs.write_fence.clone(),
    );
    let op = VerifyOp {
        due: Instant::now(),
        retries: 0,
        serial: 1,
        disk_id: "disk-0000".into(),
        offset,
        generation,
        length: data.len(),
        hash: content_hash_hex(&data),
        stripe_id: stripe.clone(),
    };
    let fallback = verifier.backend_for(&op).unwrap();
    let mut read = vec![0; data.len()];
    fallback.read_at(&op.disk_id, offset, &mut read).unwrap();
    assert_eq!(read, data);
    assert!(Arc::ptr_eq(&fallback, &verifier.backend_for(&op).unwrap()));
    verifier.enqueue(
        &op.disk_id,
        offset,
        generation,
        data.len(),
        &op.hash,
        &stripe,
    );
    wait_for(&verifier, 1);
    assert_eq!(verifier.failures(), 0);
    assert_eq!(verifier.skipped(), 0);
    verifier.stop();
}

#[test]
fn redundant_pool_can_quarantine_one_corrupted_member_without_write_fence() {
    let dir = tempfile::tempdir().unwrap();
    let images = [dir.path().join("a.img"), dir.path().join("b.img")];
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 1,
            m: 1,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        MIN_DEVICE_BYTES,
        "verify-redundant",
        false,
    )
    .unwrap();
    fs.write_file("/data", &vec![7u8; 4096], 0o600).unwrap();
    fs.sync().unwrap();
    let (stripe, disk_id, offset, generation, hash, length) = {
        let meta = fs.meta.read();
        let block = meta
            .inodes
            .values()
            .flat_map(|inode| &inode.blocks)
            .next()
            .unwrap();
        let shard = &block.shards[0];
        let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
            panic!("raw extent expected");
        };
        (
            block.stripe_id.clone(),
            shard.disk_id.clone(),
            extent.offset,
            extent.generation,
            shard.sha256.clone(),
            shard.size,
        )
    };
    fs.backend
        .write_at(&disk_id, offset, &vec![0x55; length])
        .unwrap();
    fs.backend.flush_device(&disk_id).unwrap();
    let verifier = start(&fs, 30, 0);
    verifier.enqueue(&disk_id, offset, generation, length, &hash, &stripe);
    wait_for(&verifier, 1);
    assert_eq!(verifier.failures(), 1);
    assert!(fs.quarantined_devices.lock().contains(&disk_id));
    assert!(fs.write_fence.lock().is_none());
    verifier.stop();
}

#[test]
fn shutting_down_verifier_does_not_skip_clean_unmount() {
    let (dir, mut fs, _stripe, _offset, _generation, _data) = fixture();
    let verifier = start(&fs, 10_000, 0);
    fs.verify_written = Some(verifier.clone());
    drop(verifier);
    drop(fs);
    let image = dir.path().join("verifier.img");
    let (sb, _) =
        crate::raw_store::inspect_device(crate::types::BackendKind::LoopBlock, image).unwrap();
    assert!(sb.clean);
}

#[test]
fn dropping_clone_does_not_disable_verification() {
    let (_dir, mut fs, _stripe, _offset, _generation, _data) = fixture();
    let verifier = start(&fs, 30, 0);
    fs.verify_written = Some(verifier.clone());
    let clone = fs.clone();
    drop(clone);
    assert!(!verifier.stop.load(Ordering::SeqCst));

    fs.write_file("/data", &vec![8u8; 4096], 0o600).unwrap();
    wait_for(&verifier, 1);
    assert_eq!(verifier.failures(), 0);
    assert!(!verifier.stop.load(Ordering::SeqCst));
    drop(fs);
    assert!(verifier.stop.load(Ordering::SeqCst));
}

#[test]
fn final_clone_stops_verifier_and_marks_clean() {
    let (dir, mut fs, _stripe, _offset, _generation, _data) = fixture();
    let verifier = start(&fs, 10_000, 0);
    fs.verify_written = Some(verifier.clone());
    let clone = fs.clone();
    drop(fs);
    assert!(!verifier.stop.load(Ordering::SeqCst));
    clone.write_file("/data", &vec![8u8; 4096], 0o600).unwrap();
    drop(clone);
    assert!(verifier.stop.load(Ordering::SeqCst));
    let image = dir.path().join("verifier.img");
    let (sb, _) =
        crate::raw_store::inspect_device(crate::types::BackendKind::LoopBlock, image).unwrap();
    assert!(sb.clean);
}
