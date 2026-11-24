use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

struct ParallelFlushProbe {
    inner: FileBlockBackend,
    active: AtomicUsize,
    peak: AtomicUsize,
    completed: Mutex<BTreeMap<String, usize>>,
    members: usize,
}

impl StorageBackend for ParallelFlushProbe {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, device: &String, offset: u64, bytes: &mut [u8]) -> Result<()> {
        self.inner.read_at(device, offset, bytes)
    }
    fn write_at(&self, device: &String, offset: u64, bytes: &[u8]) -> Result<()> {
        self.inner.write_at(device, offset, bytes)
    }
    fn flush_device(&self, device: &String) -> Result<()> {
        {
            let completed = self.completed.lock().unwrap();
            if completed.get(device).copied().unwrap_or(0) == 1 {
                assert_eq!(
                    completed.len(),
                    self.members,
                    "certificate started before every data flush completed"
                );
                assert!(completed.values().all(|n| *n >= 1));
            }
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        let result = self.inner.flush_device(device);
        self.active.fetch_sub(1, Ordering::SeqCst);
        *self
            .completed
            .lock()
            .unwrap()
            .entry(device.clone())
            .or_default() += 1;
        result
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
fn journal_fanout_overlaps_member_flushes_but_preserves_quorum_barrier() {
    for deferred in [false, true] {
        let (_dir, images, previous, superblocks) = quorum_fixture();
        let mut next = next_metadata(&previous, "parallel-journal");
        next.config.defer_journal_flush = deferred;
        journal::prepare_metadata_integrity_with_previous(
            &mut next,
            previous.integrity.meta_hash.clone(),
        )
        .unwrap();
        let probe = ParallelFlushProbe {
            inner: block_backend_for(&images),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            completed: Mutex::new(BTreeMap::new()),
            members: superblocks.len(),
        };
        let began = Instant::now();
        append_transaction_with_trusted_integrity_quorum(
            &probe,
            &superblocks,
            &next,
            Some(&previous),
            "parallel-journal",
            serde_json::json!({}),
        )
        .unwrap();
        let peak = probe.peak.load(Ordering::SeqCst);
        assert!(peak >= 2, "no overlap with deferred={deferred}");
        assert_eq!(
            probe
                .completed
                .lock()
                .unwrap()
                .values()
                .copied()
                .collect::<Vec<_>>(),
            vec![2; superblocks.len()]
        );
        println!(
            "deferred={deferred}, peak concurrent member flushes={peak}, wall_ms={}",
            began.elapsed().as_millis()
        );
        let recovered = recover_metadata(&probe, &superblocks).unwrap();
        assert_eq!(recovered.txid, next.txid);
        assert_eq!(recovered.integrity.meta_hash, next.integrity.meta_hash);
    }
}
