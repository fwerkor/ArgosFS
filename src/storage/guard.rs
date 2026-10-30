//! Deadline and failure isolation for raw/loop block I/O.
//!
//! One worker is reused per device. A timed-out operation may remain blocked
//! inside the kernel; it cannot be cancelled safely. Its device stays fenced
//! for this backend's lifetime, and queued operations are skipped rather than
//! touching that device after fencing. Recovery still requires a durable quorum
//! from the remaining members; a late write is never treated as confirmation.

use crate::backend::{BackendCapabilities, BackendDeviceInfo, StorageBackend};
use crate::error::{ArgosError, Result};
use crate::types::{BackendKind, DiskStatus};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

pub const DEFAULT_IO_TIMEOUT_MS: u64 = 10_000;
const MIN_IO_TIMEOUT_MS: u64 = 100;
const MAX_IO_TIMEOUT_MS: u64 = 120_000;
const MAX_QUEUED_OPERATIONS: usize = 64;

type Job = Box<dyn FnOnce() + Send + 'static>;

enum JobState {
    Pending,
    Running,
    Cancelled,
}

pub fn configured_io_timeout() -> Duration {
    let millis = std::env::var("ARGOSFS_IO_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_IO_TIMEOUT_MS)
        .clamp(MIN_IO_TIMEOUT_MS, MAX_IO_TIMEOUT_MS);
    Duration::from_millis(millis)
}

pub struct GuardedBackend {
    inner: Arc<dyn StorageBackend>,
    timeout: Duration,
    failed: Arc<Mutex<BTreeMap<String, String>>>,
    workers: Mutex<BTreeMap<String, mpsc::SyncSender<Job>>>,
    worker_seq: AtomicU64,
}

impl GuardedBackend {
    pub fn new(inner: Arc<dyn StorageBackend>) -> Self {
        Self::with_timeout(inner, configured_io_timeout())
    }

    pub fn with_timeout(inner: Arc<dyn StorageBackend>, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            failed: Arc::new(Mutex::new(BTreeMap::new())),
            workers: Mutex::new(BTreeMap::new()),
            worker_seq: AtomicU64::new(0),
        }
    }

    pub fn failure(&self, device_id: &str) -> Option<String> {
        self.failed.lock().get(device_id).cloned()
    }

    fn fence(&self, device_id: &str, reason: String) {
        self.failed
            .lock()
            .entry(device_id.to_string())
            .or_insert(reason);
    }

    fn worker(&self, device_id: &str) -> Result<mpsc::SyncSender<Job>> {
        let mut workers = self.workers.lock();
        if let Some(sender) = workers.get(device_id) {
            return Ok(sender.clone());
        }
        let (sender, receiver) = mpsc::sync_channel::<Job>(MAX_QUEUED_OPERATIONS);
        let thread_name = format!(
            "argosfs-io-{}-{}",
            device_id,
            self.worker_seq.fetch_add(1, Ordering::Relaxed)
        );
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    job();
                }
            })?;
        workers.insert(device_id.to_string(), sender.clone());
        Ok(sender)
    }

    fn await_result<T>(
        &self,
        device_id: &str,
        operation: &str,
        receiver: mpsc::Receiver<Result<T>>,
    ) -> Result<T> {
        match receiver.recv_timeout(self.timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => {
                // MissingDevice is needed by the dynamic-member fallback.
                if matches!(err, ArgosError::MissingDevice(_)) || !err.is_fatal_device_error() {
                    return Err(err);
                }
                let reason = err.to_string();
                self.fence(device_id, reason.clone());
                Err(ArgosError::DeviceFailed {
                    disk_id: device_id.to_string(),
                    reason,
                })
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);
                self.fence(
                    device_id,
                    format!("{operation} did not complete within {timeout_ms} ms"),
                );
                Err(ArgosError::DeviceTimeout {
                    disk_id: device_id.to_string(),
                    timeout_ms,
                })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let reason = format!("{operation} worker disconnected");
                self.fence(device_id, reason.clone());
                Err(ArgosError::DeviceFailed {
                    disk_id: device_id.to_string(),
                    reason,
                })
            }
        }
    }

    fn run<T, F>(&self, device_id: &str, operation: &str, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        if let Some(reason) = self.failure(device_id) {
            return Err(ArgosError::DeviceFailed {
                disk_id: device_id.to_string(),
                reason,
            });
        }
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(JobState::Pending));
        let job_state = state.clone();
        let failed = self.failed.clone();
        let id = device_id.to_string();
        let job: Job = Box::new(move || {
            {
                let mut state = job_state.lock();
                if matches!(*state, JobState::Cancelled) {
                    return;
                }
                *state = JobState::Running;
            }
            let _ = started_tx.send(());
            let previously_failed = { failed.lock().get(&id).cloned() };
            let result = if let Some(reason) = previously_failed {
                Err(ArgosError::DeviceFailed {
                    disk_id: id,
                    reason,
                })
            } else {
                op()
            };
            let _ = result_tx.send(result);
        });
        match self.worker(device_id)?.try_send(job) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                return Err(ArgosError::DeviceBusy {
                    disk_id: device_id.to_string(),
                    reason: "queue is full".to_string(),
                });
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                let reason = "I/O worker disconnected".to_string();
                self.fence(device_id, reason.clone());
                return Err(ArgosError::DeviceFailed {
                    disk_id: device_id.to_string(),
                    reason,
                });
            }
        }
        match started_rx.recv_timeout(self.timeout) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut state = state.lock();
                if matches!(*state, JobState::Pending) {
                    *state = JobState::Cancelled;
                    if let Some(reason) = self.failure(device_id) {
                        return Err(ArgosError::DeviceFailed {
                            disk_id: device_id.to_string(),
                            reason,
                        });
                    }
                    return Err(ArgosError::DeviceBusy {
                        disk_id: device_id.to_string(),
                        reason: "queue wait exceeded deadline".to_string(),
                    });
                }
                // The worker began just as the queue wait expired; it owns
                // the operation deadline, so wait for its result below.
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let reason = "I/O worker stopped before starting operation".to_string();
                self.fence(device_id, reason.clone());
                return Err(ArgosError::DeviceFailed {
                    disk_id: device_id.to_string(),
                    reason,
                });
            }
        }
        self.await_result(device_id, operation, result_rx)
    }
}

impl StorageBackend for GuardedBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }

    fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
        let mut devices = self.inner.list_devices()?;
        let failed = self.failed.lock();
        for device in &mut devices {
            if failed.contains_key(&device.device_id) {
                device.status = DiskStatus::Failed;
            }
        }
        Ok(devices)
    }

    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        let inner = self.inner.clone();
        let id = device_id.clone();
        let length = buf.len();
        let data = self.run(device_id, "read", move || {
            let mut data = vec![0u8; length];
            inner.read_at(&id, offset, &mut data)?;
            Ok(data)
        })?;
        if data.len() != buf.len() {
            return Err(ArgosError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("short read on {device_id} at offset {offset}"),
            )));
        }
        buf.copy_from_slice(&data);
        Ok(())
    }

    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        let inner = self.inner.clone();
        let id = device_id.clone();
        let data = data.to_vec();
        self.run(device_id, "write", move || {
            inner.write_at(&id, offset, &data)
        })
    }

    fn flush_device(&self, device_id: &String) -> Result<()> {
        let inner = self.inner.clone();
        let id = device_id.clone();
        self.run(device_id, "flush", move || inner.flush_device(&id))
    }

    fn drop_cache(&self, device_id: &String, offset: u64, length: u64) -> Result<()> {
        let inner = self.inner.clone();
        let id = device_id.clone();
        self.run(device_id, "drop cache", move || {
            inner.drop_cache(&id, offset, length)
        })
    }

    fn flush_all(&self) -> Result<()> {
        let mut first_error = None;
        for device in self.inner.list_devices()? {
            if self.failure(&device.device_id).is_some() {
                continue;
            }
            if let Err(err) = self.flush_device(&device.device_id) {
                if first_error.is_none() {
                    first_error = Some(err);
                }
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.inner.capacity(device_id)
    }

    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        if self.failure(device_id).is_some() {
            Ok(DiskStatus::Failed)
        } else {
            self.inner.device_status(device_id)
        }
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
