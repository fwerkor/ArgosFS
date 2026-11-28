//! Blocking FUSE operations run here, never on the kernel request readers.
//!
//! Consecutive reads or disjoint direct writes for one inode may overlap;
//! overlapping writes and exclusive requests preserve submission-order barriers. Independent inodes can
//! progress in parallel; a slow checkpoint does not consume request readers.
use parking_lot::{Condvar, Mutex};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RequestKind {
    Exclusive,
    Read,
    WriteRange { start: u64, end: u64 },
}

struct QueuedJob {
    kind: RequestKind,
    run: Job,
}

#[derive(Default)]
struct Active {
    readers: usize,
    exclusive: bool,
    writes: Vec<(u64, u64)>,
}

impl Active {
    fn accepts(&self, kind: RequestKind) -> bool {
        if self.exclusive {
            return false;
        }
        match kind {
            RequestKind::Exclusive => self.readers == 0 && self.writes.is_empty(),
            RequestKind::Read => self.writes.is_empty(),
            RequestKind::WriteRange { start, end } => {
                self.readers == 0 && self.writes.iter().all(|&(a, b)| end <= a || b <= start)
            }
        }
    }

    fn begin(&mut self, kind: RequestKind) {
        match kind {
            RequestKind::Exclusive => self.exclusive = true,
            RequestKind::Read => self.readers += 1,
            RequestKind::WriteRange { start, end } => self.writes.push((start, end)),
        }
    }

    fn finish(&mut self, kind: RequestKind) {
        match kind {
            RequestKind::Exclusive => self.exclusive = false,
            RequestKind::Read => self.readers -= 1,
            RequestKind::WriteRange { start, end } => {
                let position = self
                    .writes
                    .iter()
                    .position(|&range| range == (start, end))
                    .expect("running range");
                self.writes.swap_remove(position);
            }
        }
    }

    fn empty(&self) -> bool {
        !self.exclusive && self.readers == 0 && self.writes.is_empty()
    }
}

#[derive(Default)]
struct State {
    jobs: BTreeMap<u64, VecDeque<QueuedJob>>,
    ready: VecDeque<u64>,
    ready_set: BTreeSet<u64>,
    active: BTreeMap<u64, Active>,
    stopping: bool,
}

impl State {
    fn schedule(&mut self, inode: u64) -> bool {
        let Some(next) = self.jobs.get(&inode).and_then(|jobs| jobs.front()) else {
            return false;
        };
        let runnable = self
            .active
            .get(&inode)
            .is_none_or(|active| active.accepts(next.kind));
        if runnable && self.ready_set.insert(inode) {
            self.ready.push_back(inode);
            return true;
        }
        false
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

pub(super) struct FuseRequestExecutor {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl FuseRequestExecutor {
    pub(super) fn start(threads: usize) -> io::Result<Self> {
        assert!(threads > 0);
        let shared = Arc::new(Shared::default());
        let mut executor = Self {
            shared: Arc::clone(&shared),
            threads: Vec::with_capacity(threads),
        };
        for index in 0..threads {
            let state = Arc::clone(&shared);
            let thread = thread::Builder::new()
                .name(format!("argosfs-fuse-io-{index}"))
                .spawn(move || worker(state))?;
            executor.threads.push(thread);
        }
        Ok(executor)
    }

    /// Submission cannot block a FUSE request reader, including when all
    /// execution threads are doing slow I/O or waiting for metadata locks.
    pub(super) fn submit(&self, inode: u64, job: impl FnOnce() + Send + 'static) -> bool {
        self.submit_kind(inode, RequestKind::Exclusive, job)
    }

    pub(super) fn submit_read(&self, inode: u64, job: impl FnOnce() + Send + 'static) -> bool {
        self.submit_kind(inode, RequestKind::Read, job)
    }

    pub(super) fn submit_kind(
        &self,
        inode: u64,
        kind: RequestKind,
        job: impl FnOnce() + Send + 'static,
    ) -> bool {
        let mut state = self.shared.state.lock();
        if state.stopping {
            return false;
        }
        state.jobs.entry(inode).or_default().push_back(QueuedJob {
            kind,
            run: Box::new(job),
        });
        if state.schedule(inode) {
            self.shared.ready.notify_one();
        }
        true
    }

    /// Finish every accepted request before the filesystem is unmounted.
    pub(super) fn shutdown(&mut self) {
        {
            let mut state = self.shared.state.lock();
            state.stopping = true;
            self.shared.ready.notify_all();
        }
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                eprintln!("argosfs: FUSE I/O worker panicked during shutdown");
            }
        }
    }
}

impl Drop for FuseRequestExecutor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker(shared: Arc<Shared>) {
    loop {
        let (inode, job) = {
            let mut state = shared.state.lock();
            loop {
                if let Some(inode) = state.ready.pop_front() {
                    state.ready_set.remove(&inode);
                    let jobs = state.jobs.get_mut(&inode).expect("ready inode has jobs");
                    let job = jobs.pop_front().expect("ready inode has nonempty queue");
                    let active = state.active.entry(inode).or_default();
                    active.begin(job.kind);
                    if state.schedule(inode) {
                        shared.ready.notify_one();
                    }
                    break (inode, job);
                }
                if state.stopping {
                    return;
                }
                shared.ready.wait(&mut state);
            }
        };

        // A panicking handler must not strand subsequent requests for its
        // inode. Dropping an unused fuser reply reports EIO to the caller.
        let kind = job.kind;
        if catch_unwind(AssertUnwindSafe(job.run)).is_err() {
            eprintln!("argosfs: FUSE I/O task panicked");
        }

        let mut state = shared.state.lock();
        let active = state.active.get_mut(&inode).expect("running inode");
        active.finish(kind);
        if active.empty() {
            state.active.remove(&inode);
        }
        if state.schedule(inode) {
            shared.ready.notify_one();
        } else if !state.active.contains_key(&inode)
            && state.jobs.get(&inode).is_some_and(|jobs| jobs.is_empty())
        {
            state.jobs.remove(&inode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn blocked_inode_does_not_block_dispatch_or_other_inodes() {
        let mut executor = FuseRequestExecutor::start(2).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        assert!(executor.submit(1, move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (queued_tx, queued_rx) = mpsc::channel();
        assert!(executor.submit(1, move || queued_tx.send(()).unwrap()));
        assert!(queued_rx.recv_timeout(Duration::from_millis(50)).is_err());
        let (other_tx, other_rx) = mpsc::channel();
        assert!(executor.submit(2, move || other_tx.send(()).unwrap()));
        other_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        release_tx.send(()).unwrap();
        queued_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        executor.shutdown();
    }

    #[test]
    fn same_inode_is_fifo_and_shutdown_drains() {
        let mut executor = FuseRequestExecutor::start(4).unwrap();
        let trace = Arc::new(Mutex::new(Vec::new()));
        for n in 0..100 {
            let trace = Arc::clone(&trace);
            assert!(executor.submit(42, move || trace.lock().push(n)));
        }
        executor.shutdown();
        assert_eq!(*trace.lock(), (0..100).collect::<Vec<_>>());
        assert!(!executor.submit(42, || panic!("must never execute after shutdown")));
    }

    #[test]
    fn same_inode_reads_overlap_but_writes_are_fifo_barriers() {
        let mut executor = FuseRequestExecutor::start(4).unwrap();
        let (started, running) = mpsc::channel();
        let (release_a, wait_a) = mpsc::channel();
        let (release_b, wait_b) = mpsc::channel();
        let a = started.clone();
        assert!(executor.submit_read(1, move || {
            a.send("a").unwrap();
            wait_a.recv().unwrap();
        }));
        let b = started.clone();
        assert!(executor.submit_read(1, move || {
            b.send("b").unwrap();
            wait_b.recv().unwrap();
        }));
        let (release_write, wait_write) = mpsc::channel();
        let w = started.clone();
        assert!(executor.submit(1, move || {
            w.send("write").unwrap();
            wait_write.recv().unwrap();
        }));
        assert!(executor.submit_read(1, move || started.send("after").unwrap()));
        let mut first = [
            running.recv_timeout(Duration::from_secs(2)).unwrap(),
            running.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        first.sort_unstable();
        assert_eq!(first, ["a", "b"]);
        release_a.send(()).unwrap();
        assert!(running.recv_timeout(Duration::from_millis(50)).is_err());
        release_b.send(()).unwrap();
        assert_eq!(
            running.recv_timeout(Duration::from_secs(2)).unwrap(),
            "write"
        );
        assert!(running.recv_timeout(Duration::from_millis(50)).is_err());
        release_write.send(()).unwrap();
        assert_eq!(
            running.recv_timeout(Duration::from_secs(2)).unwrap(),
            "after"
        );
        executor.shutdown();
    }

    #[test]
    fn saturated_execution_pool_keeps_request_submission_live() {
        let mut executor = FuseRequestExecutor::start(2).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release = Arc::new(Mutex::new(release_rx));
        for inode in [1, 2] {
            let started = started_tx.clone();
            let release = Arc::clone(&release);
            assert!(executor.submit(inode, move || {
                started.send(()).unwrap();
                release.lock().recv().unwrap();
            }));
        }
        for _ in 0..2 {
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        // All filesystem execution workers are occupied. Submitting a third
        // FUSE request must not wait for any of them to finish.
        let (submitted_tx, submitted_rx) = mpsc::channel();
        let (completed_tx, completed_rx) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(|| {
                let accepted = executor.submit(3, move || completed_tx.send(()).unwrap());
                submitted_tx.send(accepted).unwrap();
            });
            assert!(submitted_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        });
        assert!(completed_rx
            .recv_timeout(Duration::from_millis(50))
            .is_err());
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();
        completed_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        executor.shutdown();
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn disjoint_writes_overlap_but_overlapping_writes_and_barriers_wait() {
        let mut executor = FuseRequestExecutor::start(4).unwrap();
        let (started, events) = mpsc::channel();
        let (release_a, wait_a) = mpsc::channel();
        let (release_b, wait_b) = mpsc::channel();
        let tx = started.clone();
        assert!(executor.submit_kind(
            1,
            RequestKind::WriteRange { start: 0, end: 10 },
            move || {
                tx.send("a").unwrap();
                wait_a.recv().unwrap();
            }
        ));
        let tx = started.clone();
        assert!(executor.submit_kind(
            1,
            RequestKind::WriteRange { start: 20, end: 30 },
            move || {
                tx.send("b").unwrap();
                wait_b.recv().unwrap();
            }
        ));
        let mut got = vec![
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        got.sort();
        assert_eq!(got, ["a", "b"]);
        let (release_c, wait_c) = mpsc::channel();
        let tx = started.clone();
        assert!(executor.submit_kind(
            1,
            RequestKind::WriteRange { start: 5, end: 15 },
            move || {
                tx.send("c").unwrap();
                wait_c.recv().unwrap();
            }
        ));
        assert!(executor.submit(1, move || started.send("barrier").unwrap()));
        assert!(events.recv_timeout(Duration::from_millis(40)).is_err());
        release_a.send(()).unwrap();
        assert_eq!(events.recv_timeout(Duration::from_secs(2)).unwrap(), "c");
        release_c.send(()).unwrap();
        assert!(events.recv_timeout(Duration::from_millis(40)).is_err());
        release_b.send(()).unwrap();
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            "barrier"
        );
        executor.shutdown();
    }
}
