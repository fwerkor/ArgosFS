use super::*;

/// Probe slow independent per-device flushes without changing the checkpoint
/// quorum or touching real device media.
struct ConcurrentFlushProbe {
    inner: Arc<dyn StorageBackend>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    completed: AtomicUsize,
    fail_disk: Option<String>,
    fatal: bool,
}

impl StorageBackend for ConcurrentFlushProbe {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(id, offset, buf)
    }
    fn write_at(&self, id: &String, offset: u64, data: &[u8]) -> Result<()> {
        self.inner.write_at(id, offset, data)
    }
    fn flush_device(&self, id: &String) -> Result<()> {
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(70));
        let result = if self.fail_disk.as_ref() == Some(id) {
            if self.fatal {
                Err(ArgosError::DeviceFailed {
                    disk_id: id.clone(),
                    reason: "injected flush failure".into(),
                })
            } else {
                Err(ArgosError::DiskFull {
                    disk_id: id.clone(),
                    required: 1,
                    available: 0,
                })
            }
        } else {
            self.inner.flush_device(id)
        };
        self.completed.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }
    fn drop_cache(&self, id: &String, offset: u64, length: u64) -> Result<()> {
        self.inner.drop_cache(id, offset, length)
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

#[test]
fn deferred_data_flush_fanout_joins_all_devices_before_validation() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("flush-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            defer_data_flush: true,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "flush-fanout",
        false,
    )
    .unwrap();
    fs.write_file("/data", &[7u8; 4096], 0o644).unwrap();
    let snapshot = fs.metadata_snapshot();
    for (fail_disk, fatal) in [
        (None, false),
        (Some(fs.raw_superblocks[1].disk_id.clone()), false),
        (Some(fs.raw_superblocks[1].disk_id.clone()), true),
    ] {
        let probe = ConcurrentFlushProbe {
            inner: fs.backend.clone(),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            fail_disk: fail_disk.clone(),
            fatal,
        };
        let mut cache = None;
        let result =
            fs.flush_deferred_data_locked(&snapshot, &probe, &fs.raw_superblocks, &mut cache);
        if fatal {
            // One failed shard member is within m=1 tolerance, but it must
            // be quarantined before a metadata commit can be trusted.
            result.unwrap();
            assert!(fs
                .quarantined_devices
                .lock()
                .contains(fail_disk.as_ref().unwrap()));
        } else if fail_disk.is_some() {
            assert!(matches!(result, Err(ArgosError::DiskFull { .. })));
        } else {
            result.unwrap();
        }
        assert_eq!(probe.completed.load(Ordering::SeqCst), 3);
        assert!(probe.max_active.load(Ordering::SeqCst) >= 2);
        assert_eq!(probe.active.load(Ordering::SeqCst), 0);
    }
}
