//! Optional asynchronous writes for fragmented, disjoint raw reservations.
//! A per-device deadline worker reuses its ring. No request is acknowledged
//! before every completion is consumed; the caller retains its flush barrier.
use crate::error::{ArgosError, Result};
use io_uring::{opcode, types, IoUring};
use std::cell::RefCell;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;

const DEPTH: usize = 64;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct WorkerRing {
    ring: Option<IoUring>,
    disabled: bool,
}

thread_local! {
    static WORKER: RefCell<WorkerRing> = RefCell::new(WorkerRing::default());
}

pub(super) fn eligible(writes: &[(u64, Vec<u8>)], capacity: u64) -> bool {
    if !(2..=DEPTH).contains(&writes.len()) {
        return false;
    }
    let mut total = 0usize;
    let mut previous_end = None;
    let mut fragmented = false;
    for (offset, data) in writes {
        if data.is_empty() {
            return false;
        }
        total = total.saturating_add(data.len());
        let Some(end) = offset.checked_add(data.len() as u64) else {
            return false;
        };
        if total > MAX_BYTES || end > capacity || end > i64::MAX as u64 {
            return false;
        }
        if let Some(previous) = previous_end {
            if *offset < previous {
                return false;
            }
            fragmented |= *offset != previous;
        }
        previous_end = Some(end);
    }
    fragmented
}

// On an uncertain submission error, kernel-owned pointers must remain live.
// Successful synchronous cancellation makes reclamation safe; otherwise
// retire the ring and retain the payload allocation rather than risk UAF.
struct PayloadGuard<'a> {
    writes: &'a mut Vec<(u64, Vec<u8>)>,
    pending: bool,
}

impl Drop for PayloadGuard<'_> {
    fn drop(&mut self) {
        if self.pending {
            std::mem::forget(std::mem::take(self.writes));
        }
    }
}

pub(super) fn write(file: &File, writes: &mut Vec<(u64, Vec<u8>)>) -> Result<bool> {
    WORKER.with(|worker| {
        let mut worker = worker.borrow_mut();
        if worker.disabled {
            return Ok(false);
        }
        let mut ring = match worker.ring.take() {
            Some(ring) => ring,
            None => match IoUring::new(DEPTH as u32) {
                Ok(ring) => ring,
                Err(_) => {
                    worker.disabled = true;
                    return Ok(false);
                }
            },
        };
        let mut payload = PayloadGuard {
            writes,
            pending: false,
        };
        // The caller prevalidates lengths, disjointness and range bounds.
        // Non-SQPOLL rings cannot consume entries until enter is called.
        let count = payload.writes.len();
        {
            let mut sq = ring.submission();
            for (index, (offset, data)) in payload.writes.iter().enumerate() {
                let entry = opcode::Write::new(
                    types::Fd(file.as_raw_fd()),
                    data.as_ptr(),
                    data.len() as u32,
                )
                .offset(*offset)
                .build()
                .user_data(index as u64);
                // SAFETY: payload remains owned until all CQEs are drained,
                // or until cancellation confirms no in-flight references.
                unsafe {
                    sq.push(&entry)
                        .map_err(|_| ArgosError::Invalid("raw io_uring queue full".into()))?;
                }
            }
        }
        payload.pending = true;
        let mut results = vec![None; count];
        let mut remaining = count;
        while remaining != 0 {
            for cqe in ring.completion() {
                let index = cqe.user_data() as usize;
                if index >= count || results[index].is_some() {
                    worker.disabled = true;
                    return Err(io::Error::from_raw_os_error(libc::EIO).into());
                }
                results[index] = Some(cqe.result());
                remaining -= 1;
            }
            if remaining == 0 {
                break;
            }
            match ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    worker.disabled = true;
                    let cancelled = ring.submitter().register_sync_cancel(
                        Some(types::Timespec::new().sec(1)),
                        types::CancelBuilder::any(),
                    );
                    if cancelled.is_ok()
                        || cancelled.is_err_and(|err| err.raw_os_error() == Some(libc::ENOENT))
                    {
                        payload.pending = false;
                    }
                    // The guarded backend fences this member. Do not replay
                    // an uncertain batch through synchronous writes.
                    return Err(io::Error::from_raw_os_error(libc::EIO).into());
                }
            }
        }
        payload.pending = false;
        worker.ring = Some(ring);
        // Every submitted request has completed, including on an error.
        // Complete any short writes only after checking all result codes.
        for (result, (_, data)) in results.iter().zip(payload.writes.iter()) {
            let result = result.expect("completed request");
            if result < 0 {
                return Err(io::Error::from_raw_os_error(-result).into());
            }
            if result == 0 || result as usize > data.len() {
                return Err(io::Error::from_raw_os_error(libc::EIO).into());
            }
        }
        for (result, (offset, data)) in results.into_iter().zip(payload.writes.iter()) {
            let written = result.expect("completed request") as usize;
            if written < data.len() {
                file.write_all_at(&data[written..], *offset + written as u64)?;
            }
        }
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disjoint_fragmented_batches_only() {
        assert!(eligible(&[(0, vec![1; 4]), (8, vec![2; 4])], 12));
        assert!(!eligible(&[(0, vec![1; 4]), (4, vec![2; 4])], 12));
        assert!(!eligible(&[(0, vec![1; 4]), (2, vec![2; 4])], 12));
        assert!(!eligible(&[(0, vec![1; 4]), (8, vec![2; 4])], 11));
    }
    #[test]
    fn fragmented_batch_reuses_worker_ring_and_preserves_gaps() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(64).unwrap();
        let mut writes = vec![(4, vec![1; 4]), (24, vec![2; 8])];
        if !write(&file, &mut writes).unwrap() {
            return; // restricted kernels use the ordinary write path
        }
        let fd = WORKER.with(|worker| worker.borrow().ring.as_ref().unwrap().as_raw_fd());
        writes[0].1.fill(3);
        assert!(write(&file, &mut writes).unwrap());
        assert_eq!(
            WORKER.with(|worker| worker.borrow().ring.as_ref().unwrap().as_raw_fd()),
            fd
        );
        let mut data = [0; 64];
        file.read_exact_at(&mut data, 0).unwrap();
        assert_eq!(&data[4..8], &[3; 4]);
        assert_eq!(&data[8..24], &[0; 16]);
        assert_eq!(&data[24..32], &[2; 8]);
    }
}
