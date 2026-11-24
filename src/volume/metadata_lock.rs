//! Separate the ordinary mutation exclusion barrier from the metadata image.
//! Ordinary writers take an exclusive barrier, while bounded raw write plans
//! share it and acquire the image lock only to reserve/publish. Readers still
//! see only the published metadata image, never private reservations.
use super::*;
use parking_lot::{Condvar, RwLockReadGuard, RwLockWriteGuard};
use std::ops::{Deref, DerefMut};

pub(super) struct MetadataLock {
    image: RwLock<Metadata>,
    mutation: RwLock<()>,
    queue: Mutex<ReservationQueue>,
    changed: Condvar,
}

#[derive(Clone)]
pub(super) struct ReservationSpec {
    pub(super) first_stripe: u64,
    pub(super) next_stripe: u64,
    pub(super) extents: Vec<PhysicalExtent>,
}

impl ReservationSpec {
    /// Reproduce the exact allocation decisions in order. Failed earlier
    /// writers can invalidate a later plan; such plans retry on the normal
    /// path, rather than manufacturing holes or changing allocator policy.
    pub(super) fn apply(&self, metadata: &mut Metadata) -> Result<()> {
        if metadata.next_stripe != self.first_stripe {
            return Err(ArgosError::Conflict(
                "raw reservation stripe cursor changed".into(),
            ));
        }
        for expected in &self.extents {
            let state = metadata
                .raw_pool
                .allocators
                .get_mut(&expected.disk_id)
                .ok_or_else(|| ArgosError::MissingDevice(expected.disk_id.clone()))?;
            let actual = allocator::allocate(
                state,
                &expected.disk_id,
                expected.length,
                expected.generation,
            )?;
            if actual.offset != expected.offset || actual.length != expected.length {
                return Err(ArgosError::Conflict(
                    "raw reservation allocation changed".into(),
                ));
            }
            let disk = metadata
                .disks
                .get_mut(&expected.disk_id)
                .ok_or_else(|| ArgosError::MissingDevice(expected.disk_id.clone()))?;
            disk.used_bytes = disk.used_bytes.saturating_add(actual.length);
        }
        metadata.next_stripe = self.next_stripe;
        Ok(())
    }
}

#[derive(Default)]
struct ReservationQueue {
    next: u64,
    pending: BTreeMap<u64, ReservationSpec>,
}

pub(super) struct MetadataWriteGuard<'a> {
    // Drop the image guard before opening the ordinary mutation barrier.
    image: RwLockWriteGuard<'a, Metadata>,
    _mutation: RwLockWriteGuard<'a, ()>,
}
impl Deref for MetadataWriteGuard<'_> {
    type Target = Metadata;
    fn deref(&self) -> &Self::Target {
        &self.image
    }
}
impl DerefMut for MetadataWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.image
    }
}

impl MetadataLock {
    pub(super) fn new(metadata: Metadata) -> Self {
        Self {
            image: RwLock::new(metadata),
            mutation: RwLock::new(()),
            queue: Mutex::new(ReservationQueue::default()),
            changed: Condvar::new(),
        }
    }
    pub(super) fn read(&self) -> RwLockReadGuard<'_, Metadata> {
        self.image.read()
    }
    pub(super) fn try_read(&self) -> Option<RwLockReadGuard<'_, Metadata>> {
        self.image.try_read()
    }
    pub(super) fn write(&self) -> MetadataWriteGuard<'_> {
        let mutation = self.mutation.write();
        let image = self.image.write();
        MetadataWriteGuard {
            image,
            _mutation: mutation,
        }
    }
    pub(super) fn parallel(&self) -> Option<ParallelMutation<'_>> {
        let barrier = self.mutation.try_read()?;
        Some(ParallelMutation {
            owner: self,
            _barrier: barrier,
        })
    }
}

pub(super) struct ParallelMutation<'a> {
    owner: &'a MetadataLock,
    _barrier: RwLockReadGuard<'a, ()>,
}
impl ParallelMutation<'_> {
    pub(super) fn image(&self) -> RwLockWriteGuard<'_, Metadata> {
        self.owner.image.write()
    }
    /// Must be called under the image lock. A private image sees all earlier
    /// reservations but never publishes them to readers or checkpoints.
    pub(super) fn reserve(
        &self,
        image: &Metadata,
        plan: impl FnOnce(&mut Metadata) -> Result<ReservationSpec>,
    ) -> Result<(Reservation<'_>, Metadata)> {
        let mut queue = self.owner.queue.lock();
        let mut private = image.clone();
        for previous in queue.pending.values() {
            previous.apply(&mut private)?;
        }
        let spec = plan(&mut private)?;
        let ticket = queue.next;
        queue.next = queue
            .next
            .checked_add(1)
            .ok_or_else(|| ArgosError::Invalid("raw reservation ticket overflow".into()))?;
        queue.pending.insert(ticket, spec.clone());
        Ok((
            Reservation {
                owner: self.owner,
                ticket,
                spec,
            },
            private,
        ))
    }
}

pub(super) struct Reservation<'a> {
    owner: &'a MetadataLock,
    ticket: u64,
    pub(super) spec: ReservationSpec,
}
impl Reservation<'_> {
    /// Do not hold the image lock while waiting for an earlier writer.
    pub(super) fn wait_turn(&self) {
        let mut queue = self.owner.queue.lock();
        while queue.pending.keys().next() != Some(&self.ticket) {
            self.owner.changed.wait(&mut queue);
        }
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        self.owner.queue.lock().pending.remove(&self.ticket);
        self.owner.changed.notify_all();
    }
}
