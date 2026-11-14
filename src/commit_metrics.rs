//! Per-volume, lock-free timing counters for raw metadata commit paths.
//! No samples are persisted and collecting them does not change write ordering.
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
    Checkpoint,
    CheckpointEncode,
    CheckpointMemberIo,
    JournalRollover,
    SyncLockWait,
    SyncLockHold,
}

impl CommitPhase {
    const ALL: [Self; 13] = [
        Self::GroupCommit,
        Self::Integrity,
        Self::DurabilityValidation,
        Self::DataFlush,
        Self::Journal,
        Self::JournalDelta,
        Self::JournalMemberIo,
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
            Self::Checkpoint => "checkpoint",
            Self::CheckpointEncode => "checkpoint_encode",
            Self::CheckpointMemberIo => "checkpoint_member_io",
            Self::JournalRollover => "journal_rollover",
            Self::SyncLockWait => "sync_lock_wait",
            Self::SyncLockHold => "sync_lock_hold",
        }
    }
}

#[derive(Default)]
struct PhaseCounters {
    count: AtomicU64,
    total_ns: AtomicU64,
    max_ns: AtomicU64,
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
}

impl CommitMetrics {
    pub(crate) fn timer(&self, phase: CommitPhase) -> PhaseTimer<'_> {
        PhaseTimer {
            metrics: self,
            phase,
            started: Instant::now(),
        }
    }

    pub(crate) fn observe(&self, phase: CommitPhase, duration: Duration) {
        let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let counters = &self.phases[phase as usize];
        counters.total_ns.fetch_add(nanos, Ordering::Relaxed);
        counters.max_ns.fetch_max(nanos, Ordering::Relaxed);
        // Publish count last. Concurrent snapshots are approximate, like any
        // independent Prometheus counters; the hot path never takes a lock.
        counters.count.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshots(&self) -> impl Iterator<Item = (&'static str, PhaseSnapshot)> + '_ {
        CommitPhase::ALL.into_iter().map(|phase| {
            let counters = &self.phases[phase as usize];
            (
                phase.name(),
                PhaseSnapshot {
                    count: counters.count.load(Ordering::Relaxed),
                    total_ns: counters.total_ns.load(Ordering::Relaxed),
                    max_ns: counters.max_ns.load(Ordering::Relaxed),
                },
            )
        })
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
        assert_eq!(samples.len(), 13);
        assert_eq!(samples[0].0, "group_commit");
        assert_eq!(samples[1].1.count, 1);
        assert_eq!(samples[4].1.count, 2);
        assert_eq!(samples[4].1.total_ns, 11_000_000);
        assert_eq!(samples[4].1.max_ns, 9_000_000);
        assert_eq!(samples[8].1, PhaseSnapshot::default());
    }
}
