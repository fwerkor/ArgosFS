use super::*;

struct WriteFanoutProbe {
    inner: Arc<dyn StorageBackend>,
    data_start: u64,
    data_end: u64,
    active: AtomicUsize,
    peak: AtomicUsize,
    writes: AtomicUsize,
    fail_device: Option<String>,
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
        std::thread::sleep(Duration::from_millis(40));
        let result = if self.fail_device.as_ref() == Some(device) {
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
