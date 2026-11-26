//! Randomised fault-injecting storage backend for the ArgosFS ztest harness.
//!
//! The backend sits between ArgosFS and the real loop/raw backend and can:
//!
//! * return transient I/O errors on reads or writes;
//! * flip bits silently in returned data, optionally writing the corruption
//!   back to the image so that it behaves like media bit rot;
//! * tear writes at a sector boundary, and silently drop writes;
//! * drop flushes and discard a write-back window, modelling a volatile disk
//!   cache that loses acknowledged but unflushed data when the process dies.
//!
//! All choices are deterministic from `FaultConfig::seed`. Injection is
//! disabled until [`FaultBackend::arm`] is called, so mount-time recovery runs
//! with clean storage and mounted I/O gets the chaos.
//!
//! This crate is deliberately separate from the main ArgosFS workspace and is
//! excluded from it, so the harness can later move to its own repository.

use argosfs::error::{ArgosError, Result};
use argosfs::storage::backend::{BackendCapabilities, BackendDeviceInfo, DeviceId, StorageBackend};
use argosfs::types::{BackendKind, DiskStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Error as IoError, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Probabilities are fractions in `0.0..=1.0`; `0.0` disables an injection.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaultConfig {
    pub seed: u64,
    /// Return EIO instead of servicing a read.
    pub read_error: f64,
    /// Flip bits in data returned from a read without changing the image.
    pub read_bit_flip: f64,
    /// Flip bits and write the corruption back, like silent media bit rot.
    pub durable_bit_rot: f64,
    /// Return EIO without performing a write.
    pub write_error: f64,
    /// Perform the write, then return EIO to model an indeterminate commit.
    pub write_error_after_write: f64,
    /// Flip bits in a write before it reaches the inner backend, like silent
    /// media corruption appearing later.
    pub write_bit_rot: f64,
    /// Persist only a sector-aligned prefix of a write.
    pub torn_write: f64,
    /// Silently discard the whole write.
    pub lost_write: f64,
    /// Acknowledge a flush without making pending data durable.
    pub drop_flush: f64,
    /// When a flush is dropped, also discard pending writes. This models a
    /// volatile cache that loses acknowledged data, not just a missing barrier.
    pub discard_pending_on_drop_flush: bool,
    /// Maximum bits flipped by one bit-rot event.
    pub bits_per_flip: usize,
    /// Sector size used to choose a torn-write prefix.
    pub torn_sector_bytes: u64,
    /// Keep this many bytes of writes in a volatile cache before forcing them
    /// to the inner backend. Zero means write-through.
    pub volatile_window_bytes: u64,
    /// Restrict durable bit rot to offsets at or above `data_floor`.
    pub bit_rot_data_only: bool,
    /// Scale write-side fault probabilities for metadata/journal offsets below
    /// `data_floor`.  A single metadata write error can fence a member and stop
    /// the workload, so this defaults below the data-write rate.
    pub metadata_write_fault_scale: f64,
    /// Offset of the first data extent, used by `bit_rot_data_only` and
    /// `metadata_write_fault_scale`.
    pub data_floor: u64,
    /// Artificial per-operation delay, in microseconds. Zero disables it.
    pub max_latency_us: u64,
    /// errno returned for injected read/write errors.
    pub error_errno: i32,
}

impl Default for FaultConfig {
    fn default() -> Self {
        Self {
            seed: 0xA260_5F5A_1D7E_5EED,
            read_error: 0.001,
            read_bit_flip: 0.002,
            durable_bit_rot: 0.0005,
            write_error: 0.001,
            write_error_after_write: 0.0002,
            write_bit_rot: 0.0002,
            torn_write: 0.001,
            lost_write: 0.0005,
            drop_flush: 0.002,
            discard_pending_on_drop_flush: false,
            bits_per_flip: 2,
            torn_sector_bytes: 512,
            volatile_window_bytes: 256 * 1024,
            bit_rot_data_only: true,
            metadata_write_fault_scale: 0.1,
            data_floor: 16 * 1024 * 1024,
            max_latency_us: 0,
            error_errno: libc::EIO,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct FaultStatsSnapshot {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub read_errors: u64,
    pub write_errors: u64,
    pub write_errors_after_write: u64,
    pub write_bit_rots: u64,
    pub transient_bit_flips: u64,
    pub durable_bit_rots: u64,
    pub torn_writes: u64,
    pub torn_write_bytes: u64,
    pub lost_writes: u64,
    pub dropped_flushes: u64,
    pub dropped_write_bytes: u64,
    pub pending_write_bytes: u64,
}

impl FaultStatsSnapshot {
    pub fn injected(&self) -> u64 {
        self.read_errors
            + self.write_errors
            + self.write_errors_after_write
            + self.write_bit_rots
            + self.transient_bit_flips
            + self.durable_bit_rots
            + self.torn_writes
            + self.lost_writes
            + self.dropped_flushes
    }
}

#[derive(Debug, Default)]
pub struct FaultStats {
    reads: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    read_errors: AtomicU64,
    write_errors: AtomicU64,
    write_errors_after_write: AtomicU64,
    write_bit_rots: AtomicU64,
    transient_bit_flips: AtomicU64,
    durable_bit_rots: AtomicU64,
    torn_writes: AtomicU64,
    torn_write_bytes: AtomicU64,
    lost_writes: AtomicU64,
    dropped_flushes: AtomicU64,
    dropped_write_bytes: AtomicU64,
    pending_write_bytes: AtomicU64,
}

impl FaultStats {
    pub fn snapshot(&self) -> FaultStatsSnapshot {
        FaultStatsSnapshot {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            read_errors: self.read_errors.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
            write_errors_after_write: self.write_errors_after_write.load(Ordering::Relaxed),
            write_bit_rots: self.write_bit_rots.load(Ordering::Relaxed),
            transient_bit_flips: self.transient_bit_flips.load(Ordering::Relaxed),
            durable_bit_rots: self.durable_bit_rots.load(Ordering::Relaxed),
            torn_writes: self.torn_writes.load(Ordering::Relaxed),
            torn_write_bytes: self.torn_write_bytes.load(Ordering::Relaxed),
            lost_writes: self.lost_writes.load(Ordering::Relaxed),
            dropped_flushes: self.dropped_flushes.load(Ordering::Relaxed),
            dropped_write_bytes: self.dropped_write_bytes.load(Ordering::Relaxed),
            pending_write_bytes: self.pending_write_bytes.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        value ^ (value >> 31)
    }

    fn chance(&mut self, probability: f64) -> bool {
        probability > 0.0 && self.next_f64() < probability
    }

    fn next_f64(&mut self) -> f64 {
        // 53 random mantissa bits produce a uniform value in [0, 1).
        (self.next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
    }

    fn below(&mut self, limit: usize) -> usize {
        if limit == 0 {
            0
        } else {
            (self.next_u64() % limit as u64) as usize
        }
    }
}

#[derive(Debug)]
struct PendingWrite {
    offset: u64,
    data: Vec<u8>,
}

#[derive(Debug)]
struct DeviceState {
    rng: SplitMix64,
    pending: Vec<PendingWrite>,
    pending_bytes: u64,
}

/// The wrapper itself. `set_inner` is called once by the ArgosFS open hook.
pub struct FaultBackend {
    inner: OnceLock<Arc<dyn StorageBackend>>,
    config: FaultConfig,
    state: Mutex<BTreeMap<DeviceId, DeviceState>>,
    armed: AtomicBool,
    stats: Arc<FaultStats>,
    event_log: Mutex<Option<File>>,
}

impl FaultBackend {
    pub fn new(config: FaultConfig, stats: Arc<FaultStats>, event_log: Option<File>) -> Self {
        Self {
            inner: OnceLock::new(),
            config,
            state: Mutex::new(BTreeMap::new()),
            armed: AtomicBool::new(false),
            stats,
            event_log: Mutex::new(event_log),
        }
    }

    /// Install the real backend. Must be called before the wrapper is used.
    pub fn set_inner(&self, inner: Arc<dyn StorageBackend>) {
        if self.inner.set(inner).is_err() {
            panic!("fault backend inner storage may only be installed once");
        }
    }

    pub fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    pub fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> Arc<FaultStats> {
        self.stats.clone()
    }

    fn inner(&self) -> &Arc<dyn StorageBackend> {
        self.inner
            .get()
            .expect("fault backend used before set_inner")
    }

    fn state(&self) -> std::sync::MutexGuard<'_, BTreeMap<DeviceId, DeviceState>> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn device<'a>(
        state: &'a mut BTreeMap<DeviceId, DeviceState>,
        config: &FaultConfig,
        device_id: &str,
    ) -> &'a mut DeviceState {
        state.entry(device_id.to_string()).or_insert_with(|| {
            let mut seed = config.seed ^ fnv1a(device_id.as_bytes());
            // Avoid a zero state. SplitMix64 is happy with any seed, but this
            // keeps different device streams visibly distinct.
            if seed == 0 {
                seed = 0x9E37_79B9_7F4A_7C15;
            }
            DeviceState {
                rng: SplitMix64::new(seed),
                pending: Vec::new(),
                pending_bytes: 0,
            }
        })
    }

    fn event(&self, kind: &str, device_id: &str, offset: u64, bytes: usize) {
        let mut log = self
            .event_log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(file) = log.as_mut() else {
            return;
        };
        let record = serde_json::json!({
            "kind": kind,
            "device": device_id,
            "offset": offset,
            "bytes": bytes,
        });
        let _ = writeln!(file, "{record}");
    }

    fn injected_error(&self) -> ArgosError {
        ArgosError::Io(IoError::from_raw_os_error(self.config.error_errno))
    }

    fn maybe_delay(&self, rng: &mut SplitMix64) {
        if self.config.max_latency_us == 0 {
            return;
        }
        let micros = rng.next_u64() % self.config.max_latency_us.saturating_add(1);
        if micros > 0 {
            std::thread::sleep(std::time::Duration::from_micros(micros));
        }
    }

    fn flip_bits(&self, rng: &mut SplitMix64, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let flips = 1 + rng.below(self.config.bits_per_flip.max(1));
        for _ in 0..flips {
            let position = rng.below(buf.len());
            let bit = (rng.next_u64() % 8) as u8;
            buf[position] ^= 1u8 << bit;
        }
        flips
    }

    fn read_impl(
        &self,
        device_id: &DeviceId,
        offset: u64,
        buf: &mut [u8],
        shard_read: bool,
    ) -> Result<()> {
        let inner = self.inner();
        if !self.is_armed() {
            return if shard_read {
                inner.read_shard_at(device_id, offset, buf)
            } else {
                inner.read_at(device_id, offset, buf)
            };
        }

        let (read_error, transient_flip, durable_bit_rot, pending) = {
            let mut state = self.state();
            let config = &self.config;
            let device = Self::device(&mut state, config, device_id);
            let rng = &mut device.rng;
            let read_error = rng.chance(config.read_error);
            let durable_bit_rot = !read_error
                && (!config.bit_rot_data_only || offset >= config.data_floor)
                && rng.chance(config.durable_bit_rot);
            let transient_flip =
                !read_error && !durable_bit_rot && rng.chance(config.read_bit_flip);
            let pending = device
                .pending
                .iter()
                .map(|entry| (entry.offset, entry.data.clone()))
                .collect::<Vec<_>>();
            (read_error, transient_flip, durable_bit_rot, pending)
        };

        if read_error {
            self.stats.read_errors.fetch_add(1, Ordering::Relaxed);
            self.event("read_error", device_id, offset, buf.len());
            return Err(self.injected_error());
        }

        if shard_read {
            inner.read_shard_at(device_id, offset, buf)?;
        } else {
            inner.read_at(device_id, offset, buf)?;
        }
        apply_pending(&pending, offset, buf);

        if durable_bit_rot {
            let flips = {
                let mut state = self.state();
                let device = Self::device(&mut state, &self.config, device_id);
                self.flip_bits(&mut device.rng, buf)
            };
            // Persist the damaged bytes so later reads see media bit rot.  Use
            // the unwrapped inner backend; a second fault decision here would
            // make the corruption non-deterministic.
            inner.write_at(device_id, offset, buf)?;
            self.stats.durable_bit_rots.fetch_add(1, Ordering::Relaxed);
            self.event("durable_bit_rot", device_id, offset, flips);
        } else if transient_flip {
            let flips = {
                let mut state = self.state();
                let device = Self::device(&mut state, &self.config, device_id);
                self.flip_bits(&mut device.rng, buf)
            };
            self.stats
                .transient_bit_flips
                .fetch_add(1, Ordering::Relaxed);
            self.event("read_bit_flip", device_id, offset, flips);
        }

        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn commit_pending(&self, device_id: &DeviceId, state: &mut DeviceState) -> Result<()> {
        let inner = self.inner();
        while !state.pending.is_empty() {
            let pending = state.pending.remove(0);
            state.pending_bytes = state
                .pending_bytes
                .saturating_sub(pending.data.len() as u64);
            self.stats
                .pending_write_bytes
                .fetch_sub(pending.data.len() as u64, Ordering::Relaxed);
            inner.write_at(device_id, pending.offset, &pending.data)?;
        }
        Ok(())
    }

    fn discard_pending(&self, device_id: &DeviceId, state: &mut DeviceState) -> u64 {
        let bytes = state.pending_bytes;
        state.pending.clear();
        state.pending_bytes = 0;
        self.stats
            .pending_write_bytes
            .fetch_sub(bytes, Ordering::Relaxed);
        if bytes > 0 {
            self.stats
                .dropped_write_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            self.event("dropped_pending", device_id, 0, bytes as usize);
        }
        bytes
    }

    fn torn_prefix(&self, rng: &mut SplitMix64, data: &[u8]) -> usize {
        let sector = self.config.torn_sector_bytes.max(1) as usize;
        let sectors = data.len().div_ceil(sector).max(1);
        // 0..sectors-1 keeps the result strictly shorter than the write, so a
        // nominally torn write is always observable as lost sectors.
        rng.below(sectors) * sector
    }
}

impl StorageBackend for FaultBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner().backend_kind()
    }

    fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
        self.inner().list_devices()
    }

    fn read_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.read_impl(device_id, offset, buf, false)
    }

    fn read_shard_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.read_impl(device_id, offset, buf, true)
    }

    fn write_at(&self, device_id: &DeviceId, offset: u64, data: &[u8]) -> Result<()> {
        let inner = self.inner();
        if !self.is_armed() {
            return inner.write_at(device_id, offset, data);
        }
        self.stats.writes.fetch_add(1, Ordering::Relaxed);

        let (write_error, error_after_write, lost_write, write_bit_rot, torn_boundary) = {
            let mut state = self.state();
            let config = &self.config;
            let device = Self::device(&mut state, config, device_id);
            let rng = &mut device.rng;
            let scale = if offset >= config.data_floor {
                1.0
            } else {
                config.metadata_write_fault_scale.clamp(0.0, 1.0)
            };
            let write_error = rng.chance(config.write_error * scale);
            let error_after_write =
                !write_error && rng.chance(config.write_error_after_write * scale);
            let lost_write =
                !write_error && !error_after_write && rng.chance(config.lost_write * scale);
            let write_bit_rot = !write_error
                && !error_after_write
                && !lost_write
                && (!config.bit_rot_data_only || offset >= config.data_floor)
                && rng.chance(config.write_bit_rot * scale);
            let torn_boundary = if !write_error
                && !error_after_write
                && !lost_write
                && !write_bit_rot
                && rng.chance(config.torn_write * scale)
            {
                Some(self.torn_prefix(rng, data))
            } else {
                None
            };
            (
                write_error,
                error_after_write,
                lost_write,
                write_bit_rot,
                torn_boundary,
            )
        };

        {
            let mut state = self.state();
            let device = Self::device(&mut state, &self.config, device_id);
            self.maybe_delay(&mut device.rng);
        }

        if write_error {
            self.stats.write_errors.fetch_add(1, Ordering::Relaxed);
            self.event("write_error", device_id, offset, data.len());
            return Err(self.injected_error());
        }
        if lost_write {
            self.stats.lost_writes.fetch_add(1, Ordering::Relaxed);
            self.event("lost_write", device_id, offset, data.len());
            return Ok(());
        }

        let (payload, torn) = match torn_boundary {
            Some(boundary) => (&data[..boundary.min(data.len())], true),
            None => (data, false),
        };
        if torn {
            self.stats.torn_writes.fetch_add(1, Ordering::Relaxed);
            self.stats
                .torn_write_bytes
                .fetch_add(payload.len() as u64, Ordering::Relaxed);
            self.event("torn_write", device_id, offset, payload.len());
        }

        let mut corrupted_payload = None;
        if write_bit_rot {
            let mut damaged = payload.to_vec();
            let flips = {
                let mut state = self.state();
                let device = Self::device(&mut state, &self.config, device_id);
                self.flip_bits(&mut device.rng, &mut damaged)
            };
            self.stats.write_bit_rots.fetch_add(1, Ordering::Relaxed);
            self.event("write_bit_rot", device_id, offset, flips);
            corrupted_payload = Some(damaged);
        }
        let payload = corrupted_payload.as_deref().unwrap_or(payload);

        if self.config.volatile_window_bytes == 0
            || payload.len() as u64 > self.config.volatile_window_bytes
        {
            inner.write_at(device_id, offset, payload)?;
        } else {
            let mut state = self.state();
            let device = Self::device(&mut state, &self.config, device_id);
            device.pending.push(PendingWrite {
                offset,
                data: payload.to_vec(),
            });
            device.pending_bytes = device.pending_bytes.saturating_add(payload.len() as u64);
            self.stats
                .pending_write_bytes
                .fetch_add(payload.len() as u64, Ordering::Relaxed);
            while device.pending_bytes > self.config.volatile_window_bytes {
                let pending = device.pending.remove(0);
                device.pending_bytes = device
                    .pending_bytes
                    .saturating_sub(pending.data.len() as u64);
                self.stats
                    .pending_write_bytes
                    .fetch_sub(pending.data.len() as u64, Ordering::Relaxed);
                inner.write_at(device_id, pending.offset, &pending.data)?;
            }
        }

        if error_after_write {
            self.stats
                .write_errors_after_write
                .fetch_add(1, Ordering::Relaxed);
            self.event("write_error_after_write", device_id, offset, data.len());
            return Err(self.injected_error());
        }
        Ok(())
    }

    fn flush_device(&self, device_id: &DeviceId) -> Result<()> {
        let inner = self.inner();
        if !self.is_armed() {
            return inner.flush_device(device_id);
        }
        self.stats.flushes.fetch_add(1, Ordering::Relaxed);
        let drop = {
            let mut state = self.state();
            let config = &self.config;
            let device = Self::device(&mut state, config, device_id);
            let drop = device.rng.chance(config.drop_flush);
            if drop && config.discard_pending_on_drop_flush {
                self.discard_pending(device_id, device);
            } else {
                self.commit_pending(device_id, device)?;
            }
            drop
        };
        if drop {
            self.stats.dropped_flushes.fetch_add(1, Ordering::Relaxed);
            self.event("drop_flush", device_id, 0, 0);
            // Report success but skip the real barrier. With the default
            // write-through cache the bytes are still in the OS page cache,
            // so a process kill does not lose them; this models a device that
            // drops a flush rather than one that lies about cache contents.
            return Ok(());
        }
        inner.flush_device(device_id)
    }

    fn flush_all(&self) -> Result<()> {
        let inner = self.inner();
        if !self.is_armed() {
            return inner.flush_all();
        }
        let devices = inner.list_devices()?;
        for device in devices {
            self.flush_device(&device.device_id)?;
        }
        Ok(())
    }

    fn drop_cache(&self, device_id: &DeviceId, offset: u64, length: u64) -> Result<()> {
        self.inner().drop_cache(device_id, offset, length)
    }

    fn capacity(&self, device_id: &DeviceId) -> Result<u64> {
        self.inner().capacity(device_id)
    }

    fn device_status(&self, device_id: &DeviceId) -> Result<DiskStatus> {
        self.inner().device_status(device_id)
    }

    fn capabilities(&self) -> BackendCapabilities {
        self.inner().capabilities()
    }
}

fn apply_pending(pending: &[(u64, Vec<u8>)], offset: u64, buf: &mut [u8]) {
    let read_start = offset;
    let read_end = offset.saturating_add(buf.len() as u64);
    for (pending_offset, data) in pending {
        let write_start = *pending_offset;
        let write_end = write_start.saturating_add(data.len() as u64);
        let start = read_start.max(write_start);
        let end = read_end.min(write_end);
        if start >= end {
            continue;
        }
        let src = (start - write_start) as usize;
        let dst = (start - read_start) as usize;
        let len = (end - start) as usize;
        buf[dst..dst + len].copy_from_slice(&data[src..src + len]);
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct MemoryBackend {
        devices: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl MemoryBackend {
        fn new(capacity: usize) -> Self {
            let mut devices = BTreeMap::new();
            devices.insert("disk-0000".to_string(), vec![0u8; capacity]);
            Self {
                devices: Mutex::new(devices),
            }
        }

        fn bytes(&self, device_id: &str) -> Vec<u8> {
            self.devices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(device_id)
                .cloned()
                .unwrap_or_default()
        }
    }

    impl StorageBackend for MemoryBackend {
        fn backend_kind(&self) -> BackendKind {
            BackendKind::LoopBlock
        }

        fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
            Ok(vec![BackendDeviceInfo {
                device_id: "disk-0000".to_string(),
                path: PathBuf::from("/memory/disk-0000"),
                capacity: 64 * 1024,
                status: DiskStatus::Online,
            }])
        }

        fn read_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
            let devices = self
                .devices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let device = devices
                .get(device_id)
                .ok_or_else(|| ArgosError::MissingDevice(device_id.clone()))?;
            let start = offset as usize;
            let end = start + buf.len();
            if end > device.len() {
                return Err(ArgosError::Invalid("memory read out of range".to_string()));
            }
            buf.copy_from_slice(&device[start..end]);
            Ok(())
        }

        fn write_at(&self, device_id: &DeviceId, offset: u64, data: &[u8]) -> Result<()> {
            let mut devices = self
                .devices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let device = devices
                .get_mut(device_id)
                .ok_or_else(|| ArgosError::MissingDevice(device_id.clone()))?;
            let start = offset as usize;
            let end = start + data.len();
            if end > device.len() {
                return Err(ArgosError::Invalid("memory write out of range".to_string()));
            }
            device[start..end].copy_from_slice(data);
            Ok(())
        }

        fn flush_device(&self, _device_id: &DeviceId) -> Result<()> {
            Ok(())
        }

        fn flush_all(&self) -> Result<()> {
            Ok(())
        }

        fn capacity(&self, _device_id: &DeviceId) -> Result<u64> {
            Ok(64 * 1024)
        }

        fn device_status(&self, _device_id: &DeviceId) -> Result<DiskStatus> {
            Ok(DiskStatus::Online)
        }

        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                direct_io: false,
                fallocate: true,
                discard: false,
            }
        }
    }

    fn backend(config: FaultConfig) -> (Arc<FaultBackend>, Arc<MemoryBackend>, Arc<FaultStats>) {
        let inner = Arc::new(MemoryBackend::new(64 * 1024));
        let stats = Arc::new(FaultStats::default());
        let fault = Arc::new(FaultBackend::new(config, stats.clone(), None));
        fault.set_inner(inner.clone());
        (fault, inner, stats)
    }

    fn quiet_config() -> FaultConfig {
        FaultConfig {
            read_error: 0.0,
            read_bit_flip: 0.0,
            durable_bit_rot: 0.0,
            write_error: 0.0,
            write_error_after_write: 0.0,
            torn_write: 0.0,
            lost_write: 0.0,
            drop_flush: 0.0,
            volatile_window_bytes: 0,
            ..FaultConfig::default()
        }
    }

    #[test]
    fn unarmed_backend_is_transparent() {
        let mut config = quiet_config();
        config.read_error = 1.0;
        config.write_error = 1.0;
        let (fault, inner, stats) = backend(config);
        fault
            .write_at(&"disk-0000".to_string(), 0, b"hello")
            .unwrap();
        let mut buf = [0u8; 5];
        fault
            .read_at(&"disk-0000".to_string(), 0, &mut buf)
            .unwrap();
        assert_eq!(&buf, b"hello");
        assert_eq!(stats.snapshot().injected(), 0);
        assert_eq!(&inner.bytes("disk-0000")[..5], b"hello");
    }

    #[test]
    fn read_error_is_reported_and_counted() {
        let mut config = quiet_config();
        config.read_error = 1.0;
        let (fault, _inner, stats) = backend(config);
        fault.arm();
        let mut buf = [0u8; 4];
        let err = fault
            .read_at(&"disk-0000".to_string(), 0, &mut buf)
            .unwrap_err();
        assert_eq!(err.errno(), libc::EIO);
        assert_eq!(stats.snapshot().read_errors, 1);
    }

    #[test]
    fn durable_bit_rot_persists_in_the_inner_image() {
        let mut config = quiet_config();
        config.durable_bit_rot = 1.0;
        config.bits_per_flip = 1;
        config.bit_rot_data_only = false;
        let (fault, inner, stats) = backend(config);
        inner
            .write_at(&"disk-0000".to_string(), 4096, &[0u8; 64])
            .unwrap();
        fault.arm();
        let mut first = [0u8; 64];
        fault
            .read_at(&"disk-0000".to_string(), 4096, &mut first)
            .unwrap();
        assert!(first.iter().any(|byte| *byte != 0));
        assert_eq!(stats.snapshot().durable_bit_rots, 1);
        let stored = inner.bytes("disk-0000");
        assert_eq!(&stored[4096..4160], &first);
    }

    #[test]
    fn torn_write_persists_only_a_prefix() {
        let mut config = quiet_config();
        config.torn_write = 1.0;
        config.torn_sector_bytes = 512;
        config.data_floor = 0;
        let (fault, inner, stats) = backend(config);
        fault.arm();
        let payload = [0xABu8; 4096];
        fault
            .write_at(&"disk-0000".to_string(), 1024, &payload)
            .unwrap();
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.torn_writes + snapshot.lost_writes, 1);
        let stored = inner.bytes("disk-0000");
        let written = snapshot.torn_write_bytes as usize;
        assert!(written < 4096);
        assert!(stored[1024..1024 + written]
            .iter()
            .all(|byte| *byte == 0xAB));
        assert!(stored[1024 + written..1024 + 4096]
            .iter()
            .all(|byte| *byte == 0));
    }

    #[test]
    fn volatile_window_is_readable_then_lost_by_dropped_flush() {
        let mut config = quiet_config();
        config.volatile_window_bytes = 16 * 1024;
        config.drop_flush = 1.0;
        config.discard_pending_on_drop_flush = true;
        let (fault, inner, stats) = backend(config);
        fault.arm();
        fault
            .write_at(&"disk-0000".to_string(), 0, b"pending-data")
            .unwrap();
        let mut buf = [0u8; 12];
        fault
            .read_at(&"disk-0000".to_string(), 0, &mut buf)
            .unwrap();
        assert_eq!(&buf, b"pending-data");
        // Inner storage has not seen the write yet.
        assert!(inner.bytes("disk-0000")[..12].iter().all(|byte| *byte == 0));
        fault.flush_device(&"disk-0000".to_string()).unwrap();
        assert_eq!(stats.snapshot().dropped_flushes, 1);
        assert!(inner.bytes("disk-0000")[..12].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn successful_flush_persists_the_volatile_window() {
        let mut config = quiet_config();
        config.volatile_window_bytes = 16 * 1024;
        config.drop_flush = 0.0;
        let (fault, inner, _stats) = backend(config);
        fault.arm();
        fault
            .write_at(&"disk-0000".to_string(), 0, b"durable")
            .unwrap();
        fault.flush_device(&"disk-0000".to_string()).unwrap();
        assert_eq!(&inner.bytes("disk-0000")[..7], b"durable");
    }
}
