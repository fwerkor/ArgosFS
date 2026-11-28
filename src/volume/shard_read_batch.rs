//! Bounded parallel verification for maintenance operations.
use super::*;

const MAX_MAINTENANCE_READS: usize = 8;

impl ArgosFs {
    pub(super) fn read_verified_shards_batched(
        &self,
        meta: &Metadata,
        pending: &[&Shard],
        mut accept: impl FnMut(&Shard, Result<Option<Vec<u8>>>),
    ) {
        for batch in pending.chunks(MAX_MAINTENANCE_READS) {
            std::thread::scope(|scope| {
                let (last, workers) = batch.split_last().expect("nonempty batch");
                let mut results = Vec::with_capacity(workers.len());
                for &shard in workers {
                    let task = std::thread::Builder::new()
                        .spawn_scoped(scope, move || self.read_shard_verified_locked(meta, shard));
                    results.push((shard, task.ok()));
                }
                let last_result = self.read_shard_verified_locked(meta, last);
                for (shard, task) in results {
                    let result = match task {
                        Some(task) => task
                            .join()
                            .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                        None => self.read_shard_verified_locked(meta, shard),
                    };
                    accept(shard, result);
                }
                accept(last, last_result);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tempfile::tempdir;

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
    fn maintenance_batches_overlap_and_preserve_slot_order() {
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
            "maintenance-batch",
            false,
        )
        .unwrap();
        let data = vec![42; 8192];
        fs.write_file("/file", &data, 0o600).unwrap();
        fs.sync().unwrap();
        let ino = fs.resolve_path("/file", true).unwrap();
        let meta = fs.metadata_snapshot();
        let block = &meta.inodes[&ino].blocks[0];
        let shards = block.shards.iter().collect::<Vec<_>>();
        let backend = Arc::new(ReadConcurrencyProbe {
            inner: fs.backend.clone(),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        fs.backend = backend.clone();
        let mut slots = Vec::new();
        fs.read_verified_shards_batched(&meta, &shards, |shard, result| {
            assert_eq!(result.unwrap().unwrap().len(), shard.size);
            slots.push(shard.slot);
        });
        assert_eq!(
            slots,
            shards.iter().map(|shard| shard.slot).collect::<Vec<_>>()
        );
        assert!(backend.max_active.load(Ordering::SeqCst) >= 2);
        assert!(backend.max_active.load(Ordering::SeqCst) <= MAX_MAINTENANCE_READS);
    }
}
