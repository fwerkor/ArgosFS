use super::*;
use tempfile::tempdir;

fn redundant_volume() -> (tempfile::TempDir, ArgosFs, Vec<u8>) {
    let dir = tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        4,
        false,
    )
    .unwrap();
    let data = (0..8_192)
        .map(|i| ((i * 31 + i / 7) % 251) as u8)
        .collect::<Vec<_>>();
    fs.write_file("/file", &data, 0o600).unwrap();
    (dir, fs, data)
}

fn shard_path(fs: &ArgosFs, slot: usize) -> (String, std::path::PathBuf) {
    let meta = fs.metadata_snapshot();
    let ino = fs.resolve_path("/file", true).unwrap();
    let shard = meta.inodes[&ino].blocks[0]
        .shards
        .iter()
        .find(|s| s.slot == slot)
        .unwrap();
    (
        shard.disk_id.clone(),
        fs.shard_path_locked(&meta, &shard.disk_id, &shard.relpath),
    )
}

#[test]
fn normal_read_skips_parity_while_scrub_verifies_and_repairs_it() {
    let (_dir, fs, bytes) = redundant_volume();
    let (disk_id, parity_path) = shard_path(&fs, 2);
    let mut damaged = std::fs::read(&parity_path).unwrap();
    damaged[0] ^= 0xff;
    std::fs::write(&parity_path, &damaged).unwrap();
    let before = fs.metadata_snapshot().disks[&disk_id].io_samples;
    assert_eq!(fs.read_file("/file", true).unwrap(), bytes);
    // A normal verified read never touches healthy-unneeded parity.
    let after = fs.metadata_snapshot().disks[&disk_id].io_samples;
    assert_eq!(before, after);
    assert_eq!(std::fs::read(&parity_path).unwrap(), damaged);

    // Physical scrub bypasses reconstructed cache, detects the parity error,
    // and repairs it using the verified data shards.
    let (report, _) = fs.scrub_limited(1, None);
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 1, "{report:?}");
    assert_eq!(fs.read_file("/file", false).unwrap(), bytes);
    let (after_report, _) = fs.scrub_limited(1, None);
    assert_eq!(after_report.damaged_files, 0, "{after_report:?}");
}

#[test]
fn data_shard_damage_fetches_parity_and_recovers_even_without_repair() {
    let (_dir, fs, bytes) = redundant_volume();
    let (disk_id, data_path) = shard_path(&fs, 0);
    let (parity_disk, _) = shard_path(&fs, 2);
    std::fs::remove_file(data_path).unwrap();
    let before = fs.metadata_snapshot().disks[&parity_disk].io_samples;
    // Even an ordinary non-repairing read must recover via parity.
    assert_eq!(fs.read_file("/file", false).unwrap(), bytes);
    let after = fs.metadata_snapshot().disks[&parity_disk].io_samples;
    assert!(
        after > before,
        "missing data shard did not trigger parity I/O"
    );
    // The missing shard was not silently classified as healthy.
    let report = fs.fsck(false, false).unwrap();
    assert!(report.missing_shards > 0, "{report:?}");
    assert_ne!(disk_id, parity_disk);
}

#[test]
fn missing_data_and_corrupt_parity_fail_closed() {
    let (_dir, fs, _bytes) = redundant_volume();
    let (_, data_path) = shard_path(&fs, 0);
    let (_, parity_path) = shard_path(&fs, 2);
    std::fs::remove_file(data_path).unwrap();
    let mut parity = std::fs::read(&parity_path).unwrap();
    parity[0] ^= 0x80;
    std::fs::write(parity_path, parity).unwrap();
    assert!(matches!(
        fs.read_file("/file", false),
        Err(ArgosError::UnrecoverableStripe { .. })
    ));
}

struct ReadConcurrencyProbe {
    inner: Arc<dyn StorageBackend>,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl StorageBackend for ReadConcurrencyProbe {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        // Long enough for every shard reader to reach the backend even when
        // thread startup is uneven, short enough to keep the test quick.
        std::thread::sleep(Duration::from_millis(200));
        let result = self.inner.read_at(device_id, offset, buf);
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
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
    fn drop_cache(&self, device_id: &String, offset: u64, length: u64) -> Result<()> {
        self.inner.drop_cache(device_id, offset, length)
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

#[test]
fn healthy_stripe_reads_data_shards_concurrently() {
    let dir = tempdir().unwrap();
    let images = (0..3)
        .map(|index| dir.path().join(format!("member-{index}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop_legacy(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            l2_cache_bytes: 0,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "parallel-shard-read",
        false,
    )
    .unwrap();
    let data = (0..8_192)
        .map(|i| ((i * 31 + i / 7) % 251) as u8)
        .collect::<Vec<u8>>();
    fs.write_file("/file", &data, 0o600).unwrap();
    fs.sync().unwrap();
    let ino = fs.resolve_path("/file", true).unwrap();
    let meta = fs.metadata_snapshot();
    let block = &meta.inodes[&ino].blocks[0];
    fs.cache.remove(&format!(
        "{}:{}:{}",
        meta.uuid, block.stripe_id, block.raw_sha256
    ));

    let backend = Arc::new(ReadConcurrencyProbe {
        inner: fs.backend.clone(),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
    });
    fs.backend = backend.clone();
    assert_eq!(fs.read_inode(ino, 0, data.len(), true).unwrap(), data);
    assert!(
        backend.max_active.load(Ordering::SeqCst) >= 2,
        "data shards of one stripe must be read concurrently"
    );
}

#[test]
fn cold_multistripe_range_overlaps_independent_stripe_reads() {
    let dir = tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("range-{i}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop_legacy(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            l2_cache_bytes: 0,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "parallel-range",
        false,
    )
    .unwrap();
    let data = (0..32_768)
        .map(|i| ((i * 31 + i / 7) % 251) as u8)
        .collect::<Vec<_>>();
    fs.write_file("/range", &data, 0o600).unwrap();
    fs.sync().unwrap();
    let ino = fs.resolve_path("/range", true).unwrap();
    let meta = fs.metadata_snapshot();
    assert_eq!(meta.inodes[&ino].blocks.len(), 4);
    for block in &meta.inodes[&ino].blocks {
        fs.cache.remove(&format!(
            "{}:{}:{}",
            meta.uuid, block.stripe_id, block.raw_sha256
        ));
    }
    let backend = Arc::new(ReadConcurrencyProbe {
        inner: fs.backend.clone(),
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
    });
    fs.backend = backend.clone();
    assert_eq!(
        fs.read_inode(ino, 127, data.len() - 512, true).unwrap(),
        &data[127..data.len() - 385]
    );
    assert!(
        backend.max_active.load(Ordering::SeqCst) > 2,
        "reads spanning multiple stripes must overlap, not just the two shards of one stripe"
    );
    // The repeated subrange is now served by verified caches.
    backend.max_active.store(0, Ordering::SeqCst);
    assert_eq!(
        fs.read_inode(ino, 127, data.len() - 512, true).unwrap(),
        &data[127..data.len() - 385]
    );
    assert_eq!(backend.max_active.load(Ordering::SeqCst), 0);
}
