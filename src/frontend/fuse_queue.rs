//! Blocking FUSE operations run here, never on the kernel request readers.
//!
//! Jobs for one inode execute in submission order. Independent inodes can
//! progress in parallel; a slow checkpoint does not consume request readers.
use parking_lot::{Condvar, Mutex};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

type Job = Box<dyn FnOnce() + Send + 'static>;

#[derive(Default)]
struct State {
    jobs: BTreeMap<u64, VecDeque<Job>>,
    ready: VecDeque<u64>,
    active: BTreeSet<u64>,
    stopping: bool,
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
        let mut state = self.shared.state.lock();
        if state.stopping {
            return false;
        }
        let pending = state.jobs.entry(inode).or_default();
        let was_empty = pending.is_empty();
        pending.push_back(Box::new(job));
        if was_empty && !state.active.contains(&inode) {
            state.ready.push_back(inode);
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
                    let jobs = state.jobs.get_mut(&inode).expect("ready inode has jobs");
                    let job = jobs.pop_front().expect("ready inode has nonempty queue");
                    state.active.insert(inode);
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
        if catch_unwind(AssertUnwindSafe(job)).is_err() {
            eprintln!("argosfs: FUSE I/O task panicked");
        }

        let mut state = shared.state.lock();
        state.active.remove(&inode);
        if state.jobs.get(&inode).is_some_and(|jobs| !jobs.is_empty()) {
            state.ready.push_back(inode);
            shared.ready.notify_one();
        } else {
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
