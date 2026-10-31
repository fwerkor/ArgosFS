//! Optional delayed verification of raw shard writes.
//!
//! Entries are tied to physical extent generations. Reclaimed or replaced
//! extents are invalidated before they can be reused. The verifier forces
//! the data to stable storage before evicting clean page-cache pages.

use super::{ArgosFs, Metadata};
use crate::backend::{FileBlockBackend, StorageBackend};
use crate::error::{ArgosError, Result};
use crate::storage::guard::GuardedBackend;
use crate::types::{DiskStatus, ShardLocation};
use crate::util::content_hash_matches;
use crate::util::usb_mode;
use parking_lot::{Mutex as DeviceMutex, RwLock};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MAX_QUEUE: usize = 8192;
const MIN_DELAY_MS: u64 = 10;
const MAX_DELAY_MS: u64 = 60 * 60 * 1000;

#[derive(Clone, Debug)]
struct VerifyOp {
    due: Instant,
    retries: u32,
    serial: u64,
    disk_id: String,
    offset: u64,
    generation: u64,
    length: usize,
    hash: String,
    stripe_id: String,
}

type DeviceBackend = (PathBuf, Arc<dyn StorageBackend>);

#[derive(Default)]
struct QueueState {
    pending: VecDeque<VerifyOp>,
    active: BTreeMap<(String, u64), u64>,
}

pub struct VerifyAfterWrite {
    queue: Mutex<QueueState>,
    work: Condvar,
    stop: AtomicBool,
    next_serial: AtomicU64,
    delay: Duration,
    max_retries: u32,
    metadata: Arc<RwLock<Metadata>>,
    quarantined: Arc<DeviceMutex<BTreeSet<String>>>,
    write_fence: Arc<DeviceMutex<Option<String>>>,
    extra_devices: DeviceMutex<BTreeMap<String, DeviceBackend>>,
    checks: AtomicU64,
    retries: AtomicU64,
    failures: AtomicU64,
    skipped: AtomicU64,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl VerifyAfterWrite {
    pub fn configured_delay() -> Option<Duration> {
        let millis = std::env::var("ARGOSFS_VERIFY_WRITE_AFTER_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or_else(|| if usb_mode() { 60_000 } else { 0 });
        (millis != 0).then(|| Duration::from_millis(millis.clamp(MIN_DELAY_MS, MAX_DELAY_MS)))
    }

    pub fn configured_retries() -> u32 {
        for name in ["ARGOS_VERIFY_WRITE_RETRIES", "ARGOSFS_VERIFY_WRITE_RETRIES"] {
            if let Ok(value) = std::env::var(name) {
                if let Ok(retries) = value.trim().parse::<u32>() {
                    return retries.min(10);
                }
            }
        }
        3
    }

    pub fn start(
        backend: Arc<dyn StorageBackend>,
        delay: Duration,
        max_retries: u32,
        metadata: Arc<RwLock<Metadata>>,
        quarantined: Arc<DeviceMutex<BTreeSet<String>>>,
        write_fence: Arc<DeviceMutex<Option<String>>>,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            queue: Mutex::new(QueueState::default()),
            work: Condvar::new(),
            stop: AtomicBool::new(false),
            next_serial: AtomicU64::new(0),
            delay,
            max_retries: max_retries.min(10),
            metadata,
            quarantined,
            write_fence,
            extra_devices: DeviceMutex::new(BTreeMap::new()),
            checks: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            handle: Mutex::new(None),
        });
        let worker = state.clone();
        let handle = thread::Builder::new()
            .name("argosfs-verify-written".to_string())
            .spawn(move || worker.run(backend))
            .expect("spawn read-after-write verifier");
        *state.handle.lock().expect("verify handle") = Some(handle);
        state
    }

    fn remove_active(queue: &mut QueueState, op: &VerifyOp) {
        let key = (op.disk_id.clone(), op.offset);
        if queue.active.get(&key) == Some(&op.serial) {
            queue.active.remove(&key);
        }
    }

    fn bounded_push(&self, queue: &mut QueueState, op: VerifyOp) {
        if queue.pending.len() >= MAX_QUEUE {
            if let Some(evicted) = queue.pending.pop_front() {
                Self::remove_active(queue, &evicted);
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
        queue.pending.push_back(op);
        self.work.notify_one();
    }

    pub fn enqueue(
        &self,
        disk_id: &str,
        offset: u64,
        generation: u64,
        length: usize,
        hash: &str,
        stripe_id: &str,
    ) {
        if length == 0 || self.stop.load(Ordering::Relaxed) {
            return;
        }
        let serial = self.next_serial.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.queue.lock().expect("verify queue");
        if self.stop.load(Ordering::Relaxed) {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        queue.active.insert((disk_id.to_string(), offset), serial);
        self.bounded_push(
            &mut queue,
            VerifyOp {
                due: Instant::now() + self.delay,
                retries: 0,
                serial,
                disk_id: disk_id.to_string(),
                offset,
                generation,
                length,
                hash: hash.to_string(),
                stripe_id: stripe_id.to_string(),
            },
        );
    }

    pub fn invalidate(&self, disk_id: &str, offset: u64) {
        self.queue
            .lock()
            .expect("verify queue")
            .active
            .remove(&(disk_id.to_string(), offset));
    }

    fn is_current(&self, op: &VerifyOp) -> bool {
        self.queue
            .lock()
            .expect("verify queue")
            .active
            .get(&(op.disk_id.clone(), op.offset))
            == Some(&op.serial)
    }

    fn finish(&self, op: &VerifyOp) {
        Self::remove_active(&mut self.queue.lock().expect("verify queue"), op);
    }

    #[cfg(test)]
    pub fn checks(&self) -> u64 {
        self.checks.load(Ordering::Relaxed)
    }

    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }

    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    fn run(self: Arc<Self>, backend: Arc<dyn StorageBackend>) {
        loop {
            let op = {
                let mut queue = self.queue.lock().expect("verify queue");
                loop {
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let now = Instant::now();
                    if let Some(position) = queue.pending.iter().position(|op| op.due <= now) {
                        break queue.pending.remove(position).expect("queue position");
                    }
                    let wait = queue
                        .pending
                        .iter()
                        .map(|op| op.due)
                        .min()
                        .map(|due| due.saturating_duration_since(now))
                        .unwrap_or(Duration::from_secs(60))
                        .min(Duration::from_secs(60));
                    let (guard, _) = self.work.wait_timeout(queue, wait).expect("verify condvar");
                    queue = guard;
                }
            };
            self.verify(&*backend, op);
        }
    }

    fn backend_for(&self, op: &VerifyOp) -> Result<Arc<dyn StorageBackend>> {
        // Newly added members are not in the backend opened at mount time.
        // Keep one guarded descriptor per such member instead of creating a
        // fresh worker for every verification read.
        let (kind, path) = {
            let meta = self.metadata.read();
            let disk = meta
                .disks
                .get(&op.disk_id)
                .ok_or_else(|| ArgosError::MissingDevice(op.disk_id.clone()))?;
            if disk.status != DiskStatus::Online {
                return Err(ArgosError::MissingDevice(op.disk_id.clone()));
            }
            (meta.backend, disk.path.clone())
        };
        let mut extras = self.extra_devices.lock();
        if let Some((cached_path, backend)) = extras.get(&op.disk_id) {
            if *cached_path == path {
                return Ok(backend.clone());
            }
        }
        let inner: Arc<dyn StorageBackend> = Arc::new(FileBlockBackend::open_with_ids(
            kind,
            vec![(op.disk_id.clone(), path.clone())],
            false,
        )?);
        let guarded: Arc<dyn StorageBackend> = Arc::new(GuardedBackend::new(inner));
        extras.insert(op.disk_id.clone(), (path, guarded.clone()));
        Ok(guarded)
    }

    fn live_in_metadata(meta: &Metadata, op: &VerifyOp) -> bool {
        meta.inodes
            .values()
            .flat_map(|inode| &inode.blocks)
            .filter(|block| block.stripe_id == op.stripe_id)
            .flat_map(|block| &block.shards)
            .any(|shard| {
                shard.disk_id == op.disk_id
                    && shard.size == op.length
                    && shard.sha256 == op.hash
                    && matches!(&shard.location, Some(ShardLocation::RawExtent(extent))
                        if extent.offset == op.offset && extent.generation == op.generation)
            })
    }

    fn verify(&self, primary: &dyn StorageBackend, mut op: VerifyOp) {
        if !self.is_current(&op) || self.stop.load(Ordering::Relaxed) {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            self.finish(&op);
            return;
        }
        let extra = if matches!(
            primary.device_status(&op.disk_id),
            Err(ArgosError::MissingDevice(_))
        ) {
            match self.backend_for(&op) {
                Ok(backend) => Some(backend),
                Err(err) => {
                    eprintln!("argosfs: verifier cannot open {}: {err}", op.disk_id);
                    self.skipped.fetch_add(1, Ordering::Relaxed);
                    self.finish(&op);
                    return;
                }
            }
        } else {
            None
        };
        let backend = extra.as_deref().unwrap_or(primary);
        let mut data = vec![0u8; op.length];
        self.checks.fetch_add(1, Ordering::Relaxed);
        // A forced flush is needed for deferred or bulk-import writes. Clean
        // pages can then be advised away; unsupported advice is inconclusive.
        let problem = match backend
            .flush_device(&op.disk_id)
            .and_then(|()| backend.drop_cache(&op.disk_id, op.offset, op.length as u64))
        {
            Ok(()) => {
                if self.stop.load(Ordering::Relaxed) || !self.is_current(&op) {
                    self.skipped.fetch_add(1, Ordering::Relaxed);
                    self.finish(&op);
                    return;
                }
                match backend.read_at(&op.disk_id, op.offset, &mut data) {
                    Ok(()) if content_hash_matches(&data, &op.hash) => None,
                    Ok(()) => Some("content hash mismatch".to_string()),
                    Err(err) => Some(format!("read failed: {err}")),
                }
            }
            Err(ArgosError::Unsupported(reason)) => {
                eprintln!(
                    "argosfs: verifier skipped {}: cache eviction unsupported: {reason}",
                    op.disk_id
                );
                self.skipped.fetch_add(1, Ordering::Relaxed);
                self.finish(&op);
                return;
            }
            Err(err) => Some(format!("flush or cache eviction failed: {err}")),
        };
        if self.stop.load(Ordering::Relaxed) || !self.is_current(&op) {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            self.finish(&op);
            return;
        }
        let Some(problem) = problem else {
            self.finish(&op);
            return;
        };
        if op.retries < self.max_retries {
            op.retries += 1;
            op.due = Instant::now() + self.delay;
            self.retries.fetch_add(1, Ordering::Relaxed);
            let mut queue = self.queue.lock().expect("verify queue");
            if queue.active.get(&(op.disk_id.clone(), op.offset)) == Some(&op.serial)
                && !self.stop.load(Ordering::Relaxed)
            {
                self.bounded_push(&mut queue, op);
            } else {
                self.skipped.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        // Serialize the final decision against all namespace mutations that
        // can retire an extent. No I/O is performed while holding this lock.
        let meta = self.metadata.read();
        if !self.is_current(&op) || !Self::live_in_metadata(&meta, &op) {
            self.skipped.fetch_add(1, Ordering::Relaxed);
            self.finish(&op);
            return;
        }
        let mut unavailable = self.quarantined.lock();
        unavailable.insert(op.disk_id.clone());
        self.failures.fetch_add(1, Ordering::Relaxed);
        if let Err(err) = ArgosFs::validate_data_durability_snapshot(&meta, &unavailable) {
            let mut fence = self.write_fence.lock();
            if fence.is_none() {
                *fence = Some(format!(
                    "read-after-write verification lost data durability: {err}"
                ));
            }
        }
        eprintln!(
            "argosfs: read-after-write check failed for {} stripe {} at offset {} after {} attempt(s): {}; member quarantined",
            op.disk_id, op.stripe_id, op.offset, op.retries + 1, problem
        );
        drop(unavailable);
        self.finish(&op);
    }

    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut queue = self.queue.lock().expect("verify queue");
        let remaining = queue.pending.len();
        queue.pending.clear();
        queue.active.clear();
        self.skipped.fetch_add(remaining as u64, Ordering::Relaxed);
        drop(queue);
        self.work.notify_all();
        if let Some(handle) = self.handle.lock().expect("verify handle").take() {
            let _ = handle.join();
        }
        let skipped = self.skipped();
        if skipped != 0 {
            eprintln!(
                "argosfs: read-after-write verifier stopped with {skipped} unverified shards"
            );
        }
    }
}

#[cfg(test)]
#[path = "verify_after_write_tests.rs"]
mod tests;
