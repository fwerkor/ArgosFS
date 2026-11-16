//! Per-volume, lock-free timing counters for raw metadata commit paths.
//! No samples are persisted and collecting them does not change write ordering.
use parking_lot::RwLock;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) enum CommitPhase {
    GroupCommit,
    Integrity,
    DurabilityValidation,
    DataFlush,
    Journal,
    JournalDelta,
    JournalMemberIo,
    JournalFlush,
    JournalQuorum,
    JournalTip,
    Checkpoint,
    CheckpointEncode,
    CheckpointMemberIo,
    JournalRollover,
    SyncLockWait,
    SyncLockHold,
}

impl CommitPhase {
    const ALL: [Self; 16] = [
        Self::GroupCommit,
        Self::Integrity,
        Self::DurabilityValidation,
        Self::DataFlush,
        Self::Journal,
        Self::JournalDelta,
        Self::JournalMemberIo,
        Self::JournalFlush,
        Self::JournalQuorum,
        Self::JournalTip,
        Self::Checkpoint,
        Self::CheckpointEncode,
        Self::CheckpointMemberIo,
        Self::JournalRollover,
        Self::SyncLockWait,
        Self::SyncLockHold,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::GroupCommit => "group_commit",
            Self::Integrity => "integrity",
            Self::DurabilityValidation => "durability_validation",
            Self::DataFlush => "data_flush",
            Self::Journal => "journal",
            Self::JournalDelta => "journal_delta",
            Self::JournalMemberIo => "journal_member_io",
            Self::JournalFlush => "journal_flush",
            Self::JournalQuorum => "journal_quorum",
            Self::JournalTip => "journal_tip",
            Self::Checkpoint => "checkpoint",
            Self::CheckpointEncode => "checkpoint_encode",
            Self::CheckpointMemberIo => "checkpoint_member_io",
            Self::JournalRollover => "journal_rollover",
            Self::SyncLockWait => "sync_lock_wait",
            Self::SyncLockHold => "sync_lock_hold",
        }
    }
}

/// In-memory payload byte counters for diagnosing FUSE write amplification.
/// These are successful-operation totals, reset on each volume open. They
/// intentionally exclude metadata writes, journal/checkpoint bytes and host
/// filesystem allocation overhead. Compare their deltas over a quiet run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataPlaneByteKind {
    RangeAccepted,
    RangeAlignedAppend,
    RangeRewriteInput,
    ShardPayloadWritten,
}

impl DataPlaneByteKind {
    const ALL: [Self; 4] = [
        Self::RangeAccepted,
        Self::RangeAlignedAppend,
        Self::RangeRewriteInput,
        Self::ShardPayloadWritten,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::RangeAccepted => "range_accepted",
            Self::RangeAlignedAppend => "range_aligned_append",
            Self::RangeRewriteInput => "range_rewrite_input",
            Self::ShardPayloadWritten => "shard_payload_written",
        }
    }
}

#[derive(Default)]
struct PhaseCounters {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
}

impl PhaseCounters {
    fn observe(&self, duration: Duration) {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        self.total_ns.fetch_add(nanos, Ordering::Relaxed);
        self.max_ns.fetch_max(nanos, Ordering::Relaxed);
        // Count is published last; snapshots are intentionally approximate.
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> PhaseSnapshot {
        PhaseSnapshot {
            count: self.count.load(Ordering::Relaxed),
            total_ns: self.total_ns.load(Ordering::Relaxed),
            max_ns: self.max_ns.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PhaseSnapshot {
    pub(crate) count: u64,
    pub(crate) total_ns: u64,
    pub(crate) max_ns: u64,
}

#[derive(Default)]
pub(crate) struct CommitMetrics {
    phases: [PhaseCounters; CommitPhase::ALL.len()],
    data_plane_bytes: [AtomicU64; DataPlaneByteKind::ALL.len()],
    // Member IDs are bounded by the volume's device set. The map lock is
    // independent of the global namespace lock and never spans device I/O.
    per_member: RwLock<BTreeMap<String, [PhaseCounters; 2]>>,
}

impl CommitMetrics {
    pub(crate) fn add_data_plane_bytes(&self, kind: DataPlaneByteKind, bytes: usize) {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        let counter = &self.data_plane_bytes[kind as usize];
        // Use the pre-1.99 atomic API, since CapOS still targets Rust 1.94.
        // The compare-exchange loop saturates without rollover and remains
        // compatible with current Rust where `fetch_update` is deprecated.
        let mut current = counter.load(Ordering::Relaxed);
        loop {
            let next = current.saturating_add(bytes);
            match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn data_plane_byte_snapshots(
        &self,
    ) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        DataPlaneByteKind::ALL.into_iter().map(|kind| {
            (
                kind.name(),
                self.data_plane_bytes[kind as usize].load(Ordering::Relaxed),
            )
        })
    }

    pub(crate) fn timer(&self, phase: CommitPhase) -> PhaseTimer<'_> {
        PhaseTimer {
            metrics: self,
            phase,
            started: Instant::now(),
        }
    }

    pub(crate) fn observe(&self, phase: CommitPhase, duration: Duration) {
        self.phases[phase as usize].observe(duration);
    }

    pub(crate) fn member_timer<'a>(
        &'a self,
        phase: CommitPhase,
        disk_id: &'a str,
    ) -> MemberPhaseTimer<'a> {
        debug_assert!(Self::member_phase_index(phase).is_some());
        MemberPhaseTimer {
            metrics: self,
            phase,
            disk_id,
            started: Instant::now(),
        }
    }

    fn member_phase_index(phase: CommitPhase) -> Option<usize> {
        match phase {
            CommitPhase::JournalMemberIo => Some(0),
            CommitPhase::CheckpointMemberIo => Some(1),
            _ => None,
        }
    }

    fn observe_member(&self, phase: CommitPhase, disk_id: &str, duration: Duration) {
        self.observe(phase, duration);
        let Some(index) = Self::member_phase_index(phase) else {
            return;
        };
        {
            let per_member = self.per_member.read();
            if let Some(counters) = per_member.get(disk_id) {
                counters[index].observe(duration);
                return;
            }
        }
        self.per_member
            .write()
            .entry(disk_id.to_owned())
            .or_default()[index]
            .observe(duration);
    }

    pub(crate) fn member_snapshots(&self) -> Vec<(String, &'static str, PhaseSnapshot)> {
        let per_member = self.per_member.read();
        per_member
            .iter()
            .flat_map(|(disk_id, counters)| {
                [
                    CommitPhase::JournalMemberIo,
                    CommitPhase::CheckpointMemberIo,
                ]
                .into_iter()
                .enumerate()
                .filter_map(move |(index, phase)| {
                    let snapshot = counters[index].snapshot();
                    (snapshot.count != 0).then(|| (disk_id.clone(), phase.name(), snapshot))
                })
            })
            .collect()
    }

    pub(crate) fn snapshots(&self) -> impl Iterator<Item = (&'static str, PhaseSnapshot)> + '_ {
        CommitPhase::ALL
            .into_iter()
            .map(|phase| (phase.name(), self.phases[phase as usize].snapshot()))
    }
}

/// RAII timers count failures and early returns as well as successful paths.
pub(crate) struct PhaseTimer<'a> {
    metrics: &'a CommitMetrics,
    phase: CommitPhase,
    started: Instant,
}

impl Drop for PhaseTimer<'_> {
    fn drop(&mut self) {
        self.metrics.observe(self.phase, self.started.elapsed());
    }
}

/// Member timers feed both the unlabelled phase aggregate and a bounded
/// per-disk series, including failed writes and failed flushes.
pub(crate) struct MemberPhaseTimer<'a> {
    metrics: &'a CommitMetrics,
    phase: CommitPhase,
    disk_id: &'a str,
    started: Instant,
}

impl Drop for MemberPhaseTimer<'_> {
    fn drop(&mut self) {
        self.metrics
            .observe_member(self.phase, self.disk_id, self.started.elapsed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_are_independent_and_timer_records_early_exit() {
        let metrics = CommitMetrics::default();
        let early_exit = || -> Result<(), ()> {
            let _timer = metrics.timer(CommitPhase::Integrity);
            Err(())
        };
        assert_eq!(early_exit(), Err(()));
        metrics.observe(CommitPhase::Journal, Duration::from_millis(9));
        metrics.observe(CommitPhase::Journal, Duration::from_millis(2));
        let samples = metrics.snapshots().collect::<Vec<_>>();
        assert_eq!(samples.len(), 16);
        assert_eq!(samples[0].0, "group_commit");
        assert_eq!(samples[1].1.count, 1);
        assert_eq!(samples[4].1.count, 2);
        assert_eq!(samples[4].1.total_ns, 11_000_000);
        assert_eq!(samples[4].1.max_ns, 9_000_000);
        assert_eq!(samples[11].1, PhaseSnapshot::default());
    }
}

#[cfg(test)]
mod member_tests {
    use super::*;

    #[test]
    fn per_member_samples_accumulate_with_global_counters() {
        let metrics = CommitMetrics::default();
        metrics.observe_member(
            CommitPhase::CheckpointMemberIo,
            "disk-0007",
            Duration::from_millis(10),
        );
        metrics.observe_member(
            CommitPhase::CheckpointMemberIo,
            "disk-0001",
            Duration::from_millis(20),
        );
        metrics.observe_member(
            CommitPhase::CheckpointMemberIo,
            "disk-0007",
            Duration::from_millis(30),
        );
        let samples = metrics.member_snapshots();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].0, "disk-0001");
        assert_eq!(samples[1].0, "disk-0007");
        assert_eq!(samples[1].2.count, 2);
        assert_eq!(samples[1].2.total_ns, 40_000_000);
        assert_eq!(samples[1].2.max_ns, 30_000_000);
        let global = metrics
            .snapshots()
            .find(|(name, _)| *name == "checkpoint_member_io")
            .unwrap()
            .1;
        assert_eq!(global.count, 3);
        assert_eq!(global.total_ns, 60_000_000);
    }
}
