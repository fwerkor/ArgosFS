//! Optional read-after-write verification for raw shards.
//!
//! A microSD card can acknowledge a write from its internal cache and only
//! later discover that the media cannot hold it.  This worker keeps a queue of
//! shard writes, waits until each one is old enough that a card cache is very
//! unlikely to answer the read, drops the operating system page cache, and
//! reads the shard back to compare its content hash.
//!
//! It is **off by default**.  Set `ARGOSFS_VERIFY_WRITE_AFTER_MS` (for example
//! `60000` for one minute) before mounting to turn it on.  A failure is logged
//! to stderr as `argosfs: read-after-write check failed ...`; a failed check
//! is retried after another delay (USB errors are more common than lost
//! data) and only after `ARGOS_VERIFY_WRITE_RETRIES` failures (default 3) is
//! the device marked failed.  The shard itself is left to the normal read
//! path, which can repair it from parity.

use crate::backend::StorageBackend;
use crate::util::content_hash_hex;
use parking_lot::Mutex as DeviceMutex;
use std::collections::{BTreeSet, VecDeque};
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
    disk_id: String,
    offset: u64,
    length: usize,
    hash: String,
    stripe_id: String,
}

pub struct VerifyAfterWrite {
    queue: Mutex<VecDeque<VerifyOp>>,
    work: Condvar,
    stop: AtomicBool,
    delay: Duration,
    max_retries: u32,
    quarantined: Option<Arc<DeviceMutex<BTreeSet<String>>>>,
    checks: AtomicU64,
    retries: AtomicU64,
    failures: AtomicU64,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl VerifyAfterWrite {
    /// The delay configured by `ARGOSFS_VERIFY_WRITE_AFTER_MS`, or None when
    /// the feature is off (the default) or the value is not a number.
    pub fn configured_delay() -> Option<Duration> {
        let millis = std::env::var("ARGOSFS_VERIFY_WRITE_AFTER_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if millis == 0 {
            None
        } else {
            Some(Duration::from_millis(
                millis.clamp(MIN_DELAY_MS, MAX_DELAY_MS),
            ))
        }
    }

    /// Number of retries after the first failed check.  `ARGOS_VERIFY_WRITE_RETRIES`
    /// (or `ARGOSFS_VERIFY_WRITE_RETRIES`) overrides the default of 3; it is
    /// clamped to 0..10.
    pub fn configured_retries() -> u32 {
        for name in ["ARGOS_VERIFY_WRITE_RETRIES", "ARGOSFS_VERIFY_WRITE_RETRIES"] {
            if let Some(value) = std::env::var(name).ok() {
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
        quarantined: Option<Arc<DeviceMutex<BTreeSet<String>>>>,
    ) -> Arc<Self> {
        let state = Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            work: Condvar::new(),
            stop: AtomicBool::new(false),
            delay,
            max_retries: max_retries.min(10),
            quarantined,
            checks: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            failures: AtomicU64::new(0),
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

    /// Remember a shard write.  The queue is bounded; under extreme write
    /// pressure the oldest unverified entry is dropped rather than growing
    /// without limit.
    pub fn enqueue(&self, disk_id: &str, offset: u64, length: usize, hash: &str, stripe_id: &str) {
        if length == 0 {
            return;
        }
        let mut queue = self.queue.lock().expect("verify queue");
        if queue.len() >= MAX_QUEUE {
            queue.pop_front();
        }
        queue.push_back(VerifyOp {
            due: Instant::now() + self.delay,
            retries: 0,
            disk_id: disk_id.to_string(),
            offset,
            length,
            hash: hash.to_string(),
            stripe_id: stripe_id.to_string(),
        });
        self.work.notify_one();
    }

    #[cfg(test)]
    pub fn checks(&self) -> u64 {
        self.checks.load(Ordering::Relaxed)
    }

    pub fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
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
                    if let Some(position) = queue.iter().position(|op| op.due <= now) {
                        break queue.remove(position).expect("queue position");
                    }
                    let wait = queue
                        .iter()
                        .map(|op| op.due)
                        .min()
                        .map(|due| due.saturating_duration_since(now))
                        .unwrap_or(Duration::from_secs(60))
                        .min(Duration::from_secs(60));
                    let (guard, _) = self
                        .work
                        .wait_timeout(queue, wait)
                        .expect("read-after-write condvar");
                    queue = guard;
                }
            };
            self.verify(&*backend, op);
        }
    }

    fn verify(&self, backend: &dyn StorageBackend, mut op: VerifyOp) {
        let mut data = vec![0u8; op.length];
        self.checks.fetch_add(1, Ordering::Relaxed);
        // Drop cached pages first so this compares the card, not RAM.
        let _ = backend.drop_cache(&op.disk_id, op.offset, op.length as u64);
        let problem = match backend.read_at(&op.disk_id, op.offset, &mut data) {
            Ok(()) => {
                let actual = content_hash_hex(&data);
                (actual != op.hash)
                    .then(|| format!("hash {} does not match expected {}", actual, op.hash))
            }
            Err(err) => Some(format!("read failed: {err}")),
        };
        let Some(problem) = problem else {
            return;
        };
        if op.retries < self.max_retries {
            op.retries += 1;
            op.due = Instant::now() + self.delay;
            self.retries.fetch_add(1, Ordering::Relaxed);
            eprintln!(
                "argosfs: read-after-write check for {} stripe {} at offset {} failed \
                 ({}); retry {}/{} after {:?}",
                op.disk_id,
                op.stripe_id,
                op.offset,
                problem,
                op.retries,
                self.max_retries,
                self.delay
            );
            let mut queue = self.queue.lock().expect("verify queue");
            if queue.len() >= MAX_QUEUE {
                queue.pop_front();
            }
            queue.push_back(op);
            self.work.notify_one();
            return;
        }
        self.failures.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "argosfs: read-after-write check failed for {} stripe {} at offset {} after \
             {} attempt(s): {}; marking the device failed",
            op.disk_id,
            op.stripe_id,
            op.offset,
            op.retries + 1,
            problem
        );
        if let Some(quarantined) = &self.quarantined {
            quarantined.lock().insert(op.disk_id.clone());
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.work.notify_all();
        if let Some(handle) = self.handle.lock().expect("verify handle").take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileBlockBackend;
    use std::io::Write;

    fn temp_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "argosfs-verify-{}-{}-{name}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&vec![0u8; 1024 * 1024])
            .unwrap();
        path
    }

    fn wait_for_checks(verifier: &VerifyAfterWrite, wanted: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while verifier.checks() < wanted && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn verifies_a_written_shard_and_reports_corruption() {
        let path = temp_file("shard");
        let backend: Arc<dyn StorageBackend> =
            Arc::new(FileBlockBackend::open_raw(&[path.clone()], true).unwrap());
        let data = vec![7u8; 4096];
        backend
            .write_at(&"disk-0000".to_string(), 0, &data)
            .unwrap();
        backend.flush_device(&"disk-0000".to_string()).unwrap();
        let verifier = VerifyAfterWrite::start(backend.clone(), Duration::from_millis(20), 0, None);
        verifier.enqueue(
            "disk-0000",
            0,
            data.len(),
            &content_hash_hex(&data),
            "s-test",
        );
        wait_for_checks(&verifier, 1);
        assert_eq!(verifier.checks(), 1);
        assert_eq!(verifier.failures(), 0);

        backend
            .write_at(&"disk-0000".to_string(), 0, &vec![9u8; 4096])
            .unwrap();
        backend.flush_device(&"disk-0000".to_string()).unwrap();
        verifier.enqueue(
            "disk-0000",
            0,
            data.len(),
            &content_hash_hex(&data),
            "s-test",
        );
        wait_for_checks(&verifier, 2);
        assert_eq!(verifier.checks(), 2);
        assert_eq!(verifier.failures(), 1);
        verifier.stop();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retries_a_transient_failure_before_marking_the_device_failed() {
        let path = temp_file("retry");
        let backend: Arc<dyn StorageBackend> =
            Arc::new(FileBlockBackend::open_raw(&[path.clone()], true).unwrap());
        let data = vec![3u8; 4096];
        backend
            .write_at(&"disk-0000".to_string(), 0, &data)
            .unwrap();
        backend.flush_device(&"disk-0000".to_string()).unwrap();
        let quarantined = Arc::new(DeviceMutex::new(BTreeSet::new()));
        let verifier = VerifyAfterWrite::start(
            backend.clone(),
            Duration::from_millis(10),
            1,
            Some(quarantined.clone()),
        );
        // Corrupt the shard immediately so every check fails.
        backend
            .write_at(&"disk-0000".to_string(), 0, &vec![4u8; 4096])
            .unwrap();
        backend.flush_device(&"disk-0000".to_string()).unwrap();
        verifier.enqueue(
            "disk-0000",
            0,
            data.len(),
            &content_hash_hex(&data),
            "s-retry",
        );
        wait_for_checks(&verifier, 2);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(verifier.checks(), 2);
        assert_eq!(verifier.retries(), 1);
        assert_eq!(verifier.failures(), 1);
        assert!(quarantined.lock().contains("disk-0000"));
        verifier.stop();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn disabled_without_the_environment_variable() {
        let previous = std::env::var_os("ARGOSFS_VERIFY_WRITE_AFTER_MS");
        std::env::remove_var("ARGOSFS_VERIFY_WRITE_AFTER_MS");
        assert!(VerifyAfterWrite::configured_delay().is_none());
        if let Some(value) = previous {
            std::env::set_var("ARGOSFS_VERIFY_WRITE_AFTER_MS", value);
        }
    }
}
