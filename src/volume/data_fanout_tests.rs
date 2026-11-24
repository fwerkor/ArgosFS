use super::*;

struct WriteFanoutProbe {
    inner: Arc<dyn StorageBackend>,
    data_start: u64,
    data_end: u64,
    active: AtomicUsize,
    peak: AtomicUsize,
    writes: AtomicUsize,
    fail_device: Option<String>,
    fail_first_offset: Option<u64>,
    failed_first: std::sync::atomic::AtomicBool,
}

impl StorageBackend for WriteFanoutProbe {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, device: &String, offset: u64, data: &mut [u8]) -> Result<()> {
        self.inner.read_at(device, offset, data)
    }
    fn write_at(&self, device: &String, offset: u64, data: &[u8]) -> Result<()> {
        if offset < self.data_start || offset >= self.data_end {
            return self.inner.write_at(device, offset, data);
        }
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        self.writes.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(80));
        let result = if device == "disk-0000"
            && self.fail_first_offset == Some(offset)
            && !self.failed_first.swap(true, Ordering::SeqCst)
        {
            Err(ArgosError::Invalid(
                "injected pre-publication write failure".into(),
            ))
        } else if self.fail_device.as_ref() == Some(device) {
            Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)))
        } else {
            self.inner.write_at(device, offset, data)
        };
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
    fn flush_device(&self, device: &String) -> Result<()> {
        self.inner.flush_device(device)
    }
    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }
    fn drop_cache(&self, device: &String, offset: u64, length: u64) -> Result<()> {
        self.inner.drop_cache(device, offset, length)
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

fn fixture() -> (tempfile::TempDir, Vec<PathBuf>, ArgosFs) {
    let dir = tempfile::tempdir().unwrap();
    let paths = (0..3)
        .map(|i| dir.path().join(format!("fanout-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &paths,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        64 * 1024 * 1024,
        "data-fanout",
        false,
    )
    .unwrap();
    (dir, paths, fs)
}

fn probe(fs: &mut ArgosFs, fail: Option<String>) -> Arc<WriteFanoutProbe> {
    let region = &fs.raw_superblocks[0].data;
    let backend = Arc::new(WriteFanoutProbe {
        inner: fs.backend.clone(),
        data_start: region.offset,
        data_end: region.offset + region.length,
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
        fail_device: fail,
        fail_first_offset: None,
        failed_first: std::sync::atomic::AtomicBool::new(false),
    });
    fs.backend = backend.clone();
    backend
}

#[test]
fn raw_stripe_writes_overlap_and_recover_exact_payload() {
    let (_dir, paths, mut fs) = fixture();
    let backend = probe(&mut fs, None);
    let data = (0..8192).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    let began = Instant::now();
    fs.write_file("/data", &data, 0o600).unwrap();
    assert_eq!(backend.writes.load(Ordering::SeqCst), 3);
    let peak = backend.peak.load(Ordering::SeqCst);
    assert!(peak >= 2, "physical shard writes never overlapped");
    println!(
        "peak parallel physical writes={peak}; write_file wall_ms={}",
        began.elapsed().as_millis()
    );
    fs.sync().unwrap();
    drop(fs);
    let recovered = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(recovered.read_file("/data", false).unwrap(), data);
}

#[test]
fn raw_stripe_write_error_joins_workers_before_restoring_allocator() {
    let (_dir, paths, mut fs) = fixture();
    let old = vec![7; 8192];
    fs.write_file("/data", &old, 0o600).unwrap();
    fs.sync().unwrap();
    let before = fs.metadata_snapshot();
    let backend = probe(&mut fs, Some("disk-0001".into()));
    assert!(fs.write_file("/data", &vec![8; 8192], 0o600).is_err());
    assert_eq!(
        backend.active.load(Ordering::SeqCst),
        0,
        "rollback raced with outstanding I/O"
    );
    assert_eq!(backend.writes.load(Ordering::SeqCst), 3);
    let after = fs.metadata_snapshot();
    assert_eq!(
        serde_json::to_value(&before.raw_pool.allocators).unwrap(),
        serde_json::to_value(&after.raw_pool.allocators).unwrap()
    );
    assert_eq!(after.txid, before.txid);
    assert_eq!(fs.read_file("/data", false).unwrap(), old);
    drop(fs);
    let recovered = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(recovered.read_file("/data", false).unwrap(), old);
}

#[test]
fn raw_stripe_fault_injection_retains_exact_serial_boundary() {
    let (_dir, _paths, mut fs) = fixture();
    fs.write_file("/data", &vec![7; 8192], 0o600).unwrap();
    let backend = probe(&mut fs, None);
    let guard = journal::thread_crash_point(FaultPoint::AfterDataWriteBeforeFlush.as_str());
    let err = fs.write_file("/data", &vec![8; 8192], 0o600).unwrap_err();
    drop(guard);
    assert!(matches!(err, ArgosError::InjectedCrash(_)));
    assert_eq!(backend.writes.load(Ordering::SeqCst), 1);
    assert_eq!(fs.read_file("/data", false).unwrap(), vec![7; 8192]);
}

#[test]
fn independent_inode_raw_appends_overlap_and_publish_only_completed_reservations() {
    let (_dir, paths, mut fs) = fixture();
    let a = fs.create_file_path("/a", 0o600).unwrap();
    let b = fs.create_file_path("/b", 0o600).unwrap();
    let before = fs.metadata_snapshot();
    let backend = probe(&mut fs, None);
    let payload_a = vec![17u8; 8192];
    let payload_b = vec![39u8; 8192];
    std::thread::scope(|scope| {
        let a_job = scope.spawn(|| fs.write_inode_range(a, 0, &payload_a));
        let deadline = Instant::now() + Duration::from_secs(5);
        while backend.active.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        // The first reservation's raw data is in flight, but it must not be
        // visible in the allocator or namespace, nor block metadata readers.
        let live = fs
            .try_metadata_snapshot()
            .expect("physical write held the metadata lock");
        assert_eq!(live.txid, before.txid);
        assert_eq!(live.inodes[&a].size, 0);
        assert_eq!(
            serde_json::to_value(&live.raw_pool.allocators).unwrap(),
            serde_json::to_value(&before.raw_pool.allocators).unwrap()
        );
        let b_job = scope.spawn(|| fs.write_inode_range(b, 0, &payload_b));
        assert_eq!(a_job.join().unwrap().unwrap(), 8192);
        assert_eq!(b_job.join().unwrap().unwrap(), 8192);
    });
    let peak = backend.peak.load(Ordering::SeqCst);
    println!("peak physical writes across two inode reservations={peak}");
    assert!(peak > 3, "different inode data writes did not overlap");
    fs.sync().unwrap();
    drop(fs);
    let reopened = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(reopened.read_file("/a", false).unwrap(), payload_a);
    assert_eq!(reopened.read_file("/b", false).unwrap(), payload_b);
    let meta = reopened.metadata_snapshot();
    for (disk, state) in &meta.raw_pool.allocators {
        let extents = meta
            .inodes
            .values()
            .flat_map(|inode| &inode.blocks)
            .flat_map(|block| &block.shards)
            .filter_map(|shard| match &shard.location {
                Some(ShardLocation::RawExtent(extent)) if &extent.disk_id == disk => {
                    Some(extent.clone())
                }
                _ => None,
            })
            .collect();
        allocator::validate_allocations(state, extents).unwrap();
    }
}

#[test]
fn failed_earlier_reservation_cannot_publish_or_release_later_writer_extent() {
    let (_dir, paths, mut fs) = fixture();
    let a = fs.create_file_path("/a", 0o600).unwrap();
    let b = fs.create_file_path("/b", 0o600).unwrap();
    let before = fs.metadata_snapshot();
    let region = &fs.raw_superblocks[0].data;
    let backend = Arc::new(WriteFanoutProbe {
        inner: fs.backend.clone(),
        data_start: region.offset,
        data_end: region.offset + region.length,
        active: AtomicUsize::new(0),
        peak: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
        fail_device: None,
        fail_first_offset: Some(before.raw_pool.allocators["disk-0000"].next_offset),
        failed_first: std::sync::atomic::AtomicBool::new(false),
    });
    fs.backend = backend.clone();
    let data = vec![42u8; 8192];
    std::thread::scope(|scope| {
        let a_job = scope.spawn(|| fs.write_inode_range(a, 0, &data));
        let deadline = Instant::now() + Duration::from_secs(5);
        while backend.active.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        let b_job = scope.spawn(|| fs.write_inode_range(b, 0, &data));
        assert!(a_job.join().unwrap().is_err());
        assert_eq!(b_job.join().unwrap().unwrap(), 8192);
    });
    assert!(backend.failed_first.load(Ordering::SeqCst));
    assert!(backend.peak.load(Ordering::SeqCst) > 3);
    assert_eq!(backend.active.load(Ordering::SeqCst), 0);
    fs.sync().unwrap();
    let meta = fs.metadata_snapshot();
    assert_eq!(meta.inodes[&a].size, 0);
    assert_eq!(meta.txid, before.txid + 1);
    for (disk, state) in &meta.raw_pool.allocators {
        let extents = meta
            .inodes
            .values()
            .flat_map(|inode| &inode.blocks)
            .flat_map(|block| &block.shards)
            .filter_map(|shard| match &shard.location {
                Some(ShardLocation::RawExtent(extent)) if &extent.disk_id == disk => {
                    Some(extent.clone())
                }
                _ => None,
            })
            .collect();
        allocator::validate_allocations(state, extents).unwrap();
    }
    drop(fs);
    let reopened = ArgosFs::open_loop(&paths, false).unwrap();
    assert!(reopened.read_file("/a", false).unwrap().is_empty());
    assert_eq!(reopened.read_file("/b", false).unwrap(), data);
}
