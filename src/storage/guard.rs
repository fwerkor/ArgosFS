//! Deadline guard around a block backend.
//!
//! A failing USB card can leave a `pwrite`/`fsync` blocked for tens of
//! seconds per command.  The filesystem must not hold its metadata lock while
//! that happens, or every lookup/statfs request queues behind the sick card.
//!
//! `GuardedBackend` runs each device operation on a short-lived worker thread
//! and waits only for the configured deadline.  On timeout the device is
//! fenced for the lifetime of this mount: later operations fail immediately
//! with `DeviceFailed`, the quorum code records the member as unavailable,
//! and the pool continues on its remaining members when redundancy allows.
//! The stuck worker is deliberately detached; it owns no filesystem lock and
//! its eventual result is ignored.

use crate::backend::{BackendCapabilities, BackendDeviceInfo, StorageBackend};
use crate::error::{ArgosError, Result};
use crate::types::{BackendKind, DiskStatus};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Default deadline for one device read, write or flush.
pub const DEFAULT_IO_TIMEOUT_MS: u64 = 10_000;
const MIN_IO_TIMEOUT_MS: u64 = 100;
const MAX_IO_TIMEOUT_MS: u64 = 120_000;

/// Deadline for raw/loop device I/O, taken from `ARGOSFS_IO_TIMEOUT_MS`.
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
    failed: Mutex<BTreeMap<String, String>>,
    thread_seq: AtomicU64,
}

impl GuardedBackend {
    pub fn new(inner: Arc<dyn StorageBackend>) -> Self {
        Self::with_timeout(inner, configured_io_timeout())
    }

    pub fn with_timeout(inner: Arc<dyn StorageBackend>, timeout: Duration) -> Self {
        Self {
            inner,
            timeout,
            failed: Mutex::new(BTreeMap::new()),
            thread_seq: AtomicU64::new(0),
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

        let (sender, receiver) = mpsc::sync_channel(1);
        let thread_name = format!(
            "argosfs-io-{}-{}",
            device_id,
            self.thread_seq.fetch_add(1, Ordering::Relaxed)
        );
        let _worker = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                // A detached worker may outlive the deadline.  Its result is
                // only useful if the caller is still waiting for it.
                let _ = sender.send(op());
            })?;

        match receiver.recv_timeout(self.timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => {
                // MissingDevice means "this device is not in the backend's
                // map"; the volume may still open it individually, so pass it
                // through without fencing.  Every other fatal device error
                // fences the member for the rest of the mount.
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
            Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                let timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX);
                let reason = format!("{operation} did not complete within {timeout_ms} ms");
                self.fence(device_id, reason);
                Err(ArgosError::DeviceTimeout {
                    disk_id: device_id.to_string(),
                    timeout_ms,
                })
            }
        }
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
            return Err(ArgosError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
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
