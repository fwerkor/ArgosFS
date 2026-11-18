//! Read-side telemetry is best-effort, not part of an acknowledged transaction.
//! Keep it out of the namespace write lock and aggregate by inode/device so
//! repeated reads consume bounded memory instead of retaining every sample.

use super::*;

const SHARDS: usize = 64;

pub(super) struct ReadAdvisory {
    shards: [Mutex<ReadAdvisoryShard>; SHARDS],
}

impl Default for ReadAdvisory {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(ReadAdvisoryShard::default())),
        }
    }
}

#[derive(Default)]
struct ReadAdvisoryShard {
    inodes: BTreeMap<InodeId, InodeReads>,
    disks: BTreeMap<String, DiskReads>,
}

struct InodeReads {
    count: u64,
    bytes: u64,
    at: f64,
    score_scale: f64,
    score_add: f64,
}

impl Default for InodeReads {
    fn default() -> Self {
        Self {
            count: 0,
            bytes: 0,
            at: 0.0,
            score_scale: 1.0,
            score_add: 0.0,
        }
    }
}

impl InodeReads {
    fn observe(&mut self, bytes: usize, access: bool, at: f64) {
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.at = self.at.max(at);
        if access {
            self.count = self.count.saturating_add(1);
            self.score_scale *= 0.98;
            self.score_add = self.score_add * 0.98 + 1.0;
        }
    }

    fn apply(&self, inode: &mut Inode) {
        inode.read_bytes = inode.read_bytes.saturating_add(self.bytes);
        inode.access_count = inode.access_count.saturating_add(self.count);
        inode.last_accessed_at = inode.last_accessed_at.max(self.at);
        inode.workload_score = inode.workload_score * self.score_scale + self.score_add;
    }
}

/// A constant-space composition of the exact EWMA steps. For an unset
/// baseline, the first measurement initializes the EWMA without decay.
#[derive(Default)]
struct EwmaSamples {
    first: Option<f64>,
    remaining_scale: f64,
    remaining_add: f64,
}

impl EwmaSamples {
    fn observe(&mut self, sample: f64) {
        if self.first.is_none() {
            self.first = Some(sample);
            self.remaining_scale = 1.0;
        } else {
            self.remaining_scale *= 0.80;
            self.remaining_add = self.remaining_add * 0.80 + sample * 0.20;
        }
    }

    fn apply(&self, current: &mut f64) {
        if let Some(first) = self.first {
            let baseline = if *current <= 0.0 {
                first
            } else {
                *current * 0.80 + first * 0.20
            };
            *current = baseline * self.remaining_scale + self.remaining_add;
        }
    }
}

#[derive(Default)]
struct DiskReads {
    count: u64,
    latency_ms: EwmaSamples,
    throughput_mib_s: EwmaSamples,
}

impl DiskReads {
    fn observe(&mut self, bytes: u64, seconds: f64) {
        self.count = self.count.saturating_add(1);
        self.latency_ms.observe(seconds.max(0.000_001) * 1000.0);
        if bytes > 0 {
            self.throughput_mib_s
                .observe(bytes as f64 / (1024.0 * 1024.0) / seconds.max(0.000_001));
        }
    }

    fn apply(&self, disk: &mut Disk) {
        self.latency_ms.apply(&mut disk.read_latency_ewma_ms);
        self.throughput_mib_s.apply(&mut disk.observed_read_mib_s);
        disk.io_samples = disk.io_samples.saturating_add(self.count);
        disk.health.latency_ms = disk
            .health
            .latency_ms
            .max((disk.read_latency_ewma_ms + disk.write_latency_ewma_ms) / 2.0);
    }
}

impl ReadAdvisory {
    #[cfg(test)]
    pub(super) fn pending_entries(&self) -> (usize, usize) {
        self.shards.iter().fold((0, 0), |(inodes, disks), shard| {
            let guard = shard.lock();
            (inodes + guard.inodes.len(), disks + guard.disks.len())
        })
    }

    pub(super) fn inode(&self, ino: InodeId, bytes: usize, access: bool) {
        let mut shard = self.shards[ino as usize % SHARDS].lock();
        shard
            .inodes
            .entry(ino)
            .or_default()
            .observe(bytes, access, now_f64());
    }

    pub(super) fn disk(&self, disk_id: &str, bytes: u64, seconds: f64) {
        // Simple stable byte hash: the member set is small, and independent
        // stripes avoid a mount-global telemetry mutex on parallel reads.
        let stripe = disk_id.bytes().fold(0usize, |hash, byte| {
            hash.wrapping_mul(31).wrapping_add(byte as usize)
        }) % SHARDS;
        self.shards[stripe]
            .lock()
            .disks
            .entry(disk_id.to_string())
            .or_default()
            .observe(bytes, seconds);
    }

    fn apply(&self, meta: &mut Metadata, drain: bool) -> bool {
        let mut any = false;
        for shard in &self.shards {
            let mut guard = shard.lock();
            if !guard.inodes.is_empty() || !guard.disks.is_empty() {
                any = true;
            }
            if drain {
                let inodes = std::mem::take(&mut guard.inodes);
                let disks = std::mem::take(&mut guard.disks);
                drop(guard);
                for (ino, delta) in inodes {
                    if let Some(inode) = meta.inodes.get_mut(&ino) {
                        delta.apply(inode);
                    }
                }
                for (id, delta) in disks {
                    if let Some(disk) = meta.disks.get_mut(&id) {
                        delta.apply(disk);
                    }
                }
            } else {
                for (ino, delta) in &guard.inodes {
                    if let Some(inode) = meta.inodes.get_mut(ino) {
                        delta.apply(inode);
                    }
                }
                for (id, delta) in &guard.disks {
                    if let Some(disk) = meta.disks.get_mut(id) {
                        delta.apply(disk);
                    }
                }
            }
        }
        any
    }

    pub(super) fn project(&self, meta: &mut Metadata) {
        self.apply(meta, false);
    }

    /// Caller holds the exclusive namespace lock. No reader can append a
    /// sample after the drain and before the transaction captures metadata.
    pub(super) fn drain(&self, meta: &mut Metadata) -> bool {
        self.apply(meta, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewma_compaction_matches_each_original_update() {
        let observations = [(1024, 0.1), (0, 0.2), (400, 0.4), (300, 0.3)];
        for baseline in [0.0, 6.0] {
            let mut expected_ms = baseline;
            let mut expected_rate = baseline;
            let mut aggregated = DiskReads::default();
            for (bytes, seconds) in observations {
                update_latency_ewma(&mut expected_ms, &mut expected_rate, seconds, bytes);
                aggregated.observe(bytes, seconds);
            }
            let mut disk_ms = baseline;
            let mut disk_rate = baseline;
            aggregated.latency_ms.apply(&mut disk_ms);
            aggregated.throughput_mib_s.apply(&mut disk_rate);
            assert!((expected_ms - disk_ms).abs() < 0.00001);
            assert!((expected_rate - disk_rate).abs() < 0.00001);
            assert_eq!(aggregated.count, 4);
        }
    }
}
