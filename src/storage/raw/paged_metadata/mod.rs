//! Experimental persistent metadata engine for an explicitly reserved raw
//! region. This is not the legacy checkpoint layout and is not selected by
//! the normal ArgosFS mount path yet.
//!
//! Fixed membership, majority quorum, one serialized writer. Each device has
//! a private page allocator and two checksummed root slots. Commit writes
//! unreachable COW pages and flushes them before publishing the new root.
//! Reopening verifies both root graphs and reconstructs free space; steady
//! state commits update reference counts only on changed paths.
mod image;
mod member;
mod wire;
pub use image::ImageStore;

use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use member::{Member, Prepared};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use wire::{Hash, Root, PAGE_SIZE, RESERVED_PAGES};

/// A region exclusively reserved for this metadata format. The first two
/// 4 KiB pages are root slots; the next two are quorum certificates. Never point this at a live legacy checkpoint,
/// data extent, or allocator region.
#[derive(Clone, Debug)]
pub struct Arena {
    pub disk_id: String,
    pub offset: u64,
    pub length: u64,
}

/// Sorted, unique changes. None deletes a record; Some(empty) is a value.
pub type Changes = BTreeMap<u64, Option<Vec<u8>>>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommitIo {
    pub pages_read: usize,
    pub pages_written: usize,
}

#[derive(Debug)]
pub struct CommitReport {
    pub txid: u64,
    pub root_hash: [u8; 32],
    pub members: BTreeMap<String, CommitIo>,
    pub failures: BTreeMap<String, String>,
}

/// A single-writer handle. The caller must hold exclusive access to every
/// supplied region for its entire lifetime, including recovery.
pub struct PagedMetadata {
    backend: Arc<dyn StorageBackend>,
    namespace: Hash,
    quorum: usize,
    members: Vec<Member>,
    current: (u64, Hash),
    fenced: bool,
}

fn validate_members(arenas: &[Arena]) -> Result<usize> {
    let unique = arenas
        .iter()
        .map(|arena| &arena.disk_id)
        .collect::<BTreeSet<_>>();
    if arenas.is_empty() || unique.len() != arenas.len() {
        return Err(ArgosError::Invalid(
            "paged metadata needs distinct voting members".into(),
        ));
    }
    Ok(arenas.len() / 2 + 1)
}

// Bind the full configured membership and region geometry into every root.
// A caller cannot accidentally lower the quorum by omitting missing devices.
fn namespace_identity(pool: [u8; 16], arenas: &[Arena]) -> Hash {
    let mut members = arenas.iter().collect::<Vec<_>>();
    members.sort_unstable_by(|a, b| a.disk_id.cmp(&b.disk_id));
    let mut hash = Sha256::new();
    hash.update(b"ArgosFS/paged-metadata/membership/v1\0");
    hash.update(pool);
    hash.update((members.len() as u64).to_le_bytes());
    for member in members {
        hash.update((member.disk_id.len() as u64).to_le_bytes());
        hash.update(member.disk_id.as_bytes());
        hash.update(member.offset.to_le_bytes());
        hash.update(member.length.to_le_bytes());
    }
    hash.finalize().into()
}

fn member_identity(namespace: Hash, disk_id: &str) -> Hash {
    let mut hash = Sha256::new();
    hash.update(b"ArgosFS/paged-metadata/member/v1\0");
    hash.update(namespace);
    hash.update(disk_id.as_bytes());
    hash.finalize().into()
}

fn unavailable(need: usize, have: usize) -> ArgosError {
    ArgosError::QuorumUnavailable {
        operation: "paged metadata".into(),
        need,
        have,
    }
}

impl PagedMetadata {
    /// Initialize previously unused, zero-rooted regions. All members must
    /// be accessible. This is intentionally not an in-place legacy migration.
    pub fn format(
        backend: Arc<dyn StorageBackend>,
        pool: [u8; 16],
        arenas: Vec<Arena>,
    ) -> Result<Self> {
        validate_members(&arenas)?;
        let namespace = namespace_identity(pool, &arenas);
        for arena in &arenas {
            arena.validate(&*backend)?;
            for slot in 0..RESERVED_PAGES {
                if arena.read(&*backend, slot)?.iter().any(|byte| *byte != 0) {
                    return Err(ArgosError::Conflict(format!(
                        "metadata arena on {} is not empty",
                        arena.disk_id
                    )));
                }
            }
        }
        let empty = Root {
            txid: 0,
            tree: None,
        };
        for arena in &arenas {
            for slot in 0..2 {
                arena.write(
                    &*backend,
                    slot,
                    &empty.encode(
                        member_identity(namespace, &arena.disk_id),
                        arena.pages(),
                        false,
                    ),
                )?;
            }
            backend.flush_device(&arena.disk_id)?;
        }
        for arena in &arenas {
            for slot in 2..4 {
                arena.write(
                    &*backend,
                    slot,
                    &empty.encode(
                        member_identity(namespace, &arena.disk_id),
                        arena.pages(),
                        true,
                    ),
                )?;
            }
            backend.flush_device(&arena.disk_id)?;
        }
        Self::open(backend, pool, arenas)
    }

    /// Recover the highest independently verified root on a strict majority
    /// of the supplied *configured* membership, not merely the online set.
    /// A newer minority root is never selected. Missing/unreadable members
    /// are excluded from writes until a separate repair brings them current.
    pub fn open(
        backend: Arc<dyn StorageBackend>,
        pool: [u8; 16],
        arenas: Vec<Arena>,
    ) -> Result<Self> {
        let quorum = validate_members(&arenas)?;
        let namespace = namespace_identity(pool, &arenas);
        let members: Vec<_> = std::thread::scope(|scope| {
            let jobs = arenas
                .into_iter()
                .map(|arena| {
                    let backend = &backend;
                    scope.spawn(move || Member::open(&**backend, arena, namespace))
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .filter_map(|job| job.join().expect("metadata member recovery panicked").ok())
                .collect()
        });
        let valid_members = members
            .iter()
            .filter(|member| member.roots.iter().any(Option::is_some))
            .count();
        if valid_members < quorum {
            return Err(unavailable(quorum, valid_members));
        }
        let mut votes = BTreeMap::<(u64, Hash), usize>::new();
        let mut certified = BTreeSet::new();
        for member in &members {
            let identities = member
                .roots
                .iter()
                .flatten()
                .map(|root| root.identity())
                .collect::<BTreeSet<_>>();
            for identity in identities {
                *votes.entry(identity).or_default() += 1;
            }
            certified.extend(
                member
                    .certificates
                    .iter()
                    .flatten()
                    .map(|root| root.identity()),
            );
        }
        // A certificate is evidence even when its associated root or body
        // is damaged. Never silently fall back behind a known commit.
        let mut eligible = certified;
        eligible.extend(
            votes
                .iter()
                .filter(|(_, count)| **count >= quorum)
                .map(|(identity, _)| *identity),
        );
        let mut eligible = eligible.into_iter().rev();
        let current = eligible
            .next()
            .ok_or_else(|| unavailable(quorum, votes.values().copied().max().unwrap_or(0)))?;
        if eligible.next().is_some_and(|other| other.0 == current.0) {
            return Err(ArgosError::Conflict(
                "divergent metadata roots at the same transaction have quorum evidence".into(),
            ));
        }
        if !votes.contains_key(&current) {
            return Err(ArgosError::CorruptedMetadata(
                "latest certified metadata root has no intact copy; refusing rollback".into(),
            ));
        }
        Ok(Self {
            backend,
            namespace,
            quorum,
            members,
            current,
            fenced: false,
        })
    }

    pub fn txid(&self) -> u64 {
        self.current.0
    }
    pub fn root_hash(&self) -> [u8; 32] {
        self.current.1
    }

    /// Page reads/writes count the engine's operations, excluding recovery
    /// scans and device flushes. No full namespace traversal is performed.
    pub fn commit(&mut self, changes: &Changes) -> Result<CommitReport> {
        if self.fenced {
            return Err(ArgosError::IndeterminateCommit(
                "paged metadata writer must reopen".into(),
            ));
        }
        self.ensure_certified_base()?;
        let txid = self
            .current
            .0
            .checked_add(1)
            .ok_or_else(|| ArgosError::Invalid("metadata transaction ID exhausted".into()))?;
        let mut failures = BTreeMap::new();
        let plans: Vec<(usize, Prepared)> = std::thread::scope(|scope| {
            let jobs =
                self.members
                    .iter()
                    .enumerate()
                    .filter_map(|(index, member)| {
                        let slot = member.root_slot(self.current)?;
                        let backend = &self.backend;
                        Some(scope.spawn(move || {
                            (index, member.prepare(&**backend, slot, txid, changes))
                        }))
                    })
                    .collect::<Vec<_>>();
            jobs.into_iter()
                .filter_map(|job| {
                    let (index, plan) = job.join().expect("metadata plan panicked");
                    match plan {
                        Ok(plan) => Some((index, plan)),
                        Err(err) => {
                            failures
                                .insert(self.members[index].arena.disk_id.clone(), err.to_string());
                            None
                        }
                    }
                })
                .collect()
        });
        if plans.len() < self.quorum {
            return Err(unavailable(self.quorum, plans.len()));
        }
        let identity = plans[0].1.root.identity();
        if plans
            .iter()
            .any(|(_, plan)| plan.root.identity() != identity)
        {
            return Err(ArgosError::Conflict(
                "replicas produced different logical metadata roots".into(),
            ));
        }
        // Complete the body write/flush phase on a quorum before publishing
        // any root. A failed body write cannot change an acknowledged root.
        let durable: Vec<(usize, Prepared)> = std::thread::scope(|scope| {
            let jobs = plans
                .into_iter()
                .map(|(index, plan)| {
                    let backend = &self.backend;
                    let member = &self.members[index];
                    scope.spawn(move || {
                        let result = member.write_body(&**backend, &plan);
                        (index, plan, result)
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .filter_map(|job| {
                    let (index, plan, result) = job.join().expect("metadata body write panicked");
                    match result {
                        Ok(()) => Some((index, plan)),
                        Err(err) => {
                            failures
                                .insert(self.members[index].arena.disk_id.clone(), err.to_string());
                            None
                        }
                    }
                })
                .collect()
        });
        if durable.len() < self.quorum {
            return Err(unavailable(self.quorum, durable.len()));
        }
        // A root write or flush error may nonetheless have reached storage.
        // Drop ambiguous members from this handle and fence the entire
        // writer if a quorum was not confirmed; never retry against stale
        // in-memory allocation state.
        let mut by_index = durable.into_iter().collect::<BTreeMap<_, _>>();
        let namespace = self.namespace;
        let results = std::thread::scope(|scope| {
            let jobs = self
                .members
                .iter_mut()
                .enumerate()
                .filter_map(|(index, member)| {
                    let plan = by_index.remove(&index)?;
                    let backend = &self.backend;
                    Some(scope.spawn(move || {
                        let result = member.publish(&**backend, namespace, plan);
                        (index, member.arena.disk_id.clone(), result)
                    }))
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .map(|job| job.join().expect("metadata root publication panicked"))
                .collect::<Vec<_>>()
        });
        let mut committed = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        for (index, disk, result) in results {
            match result {
                Ok(io) => {
                    committed.insert(disk, io);
                }
                Err(err) => {
                    failures.insert(disk, err.to_string());
                    ambiguous.insert(index);
                }
            }
        }
        let mut index = 0;
        self.members.retain(|_| {
            let keep = !ambiguous.contains(&index);
            index += 1;
            keep
        });
        if committed.len() < self.quorum {
            self.fenced = true;
            return Err(ArgosError::IndeterminateCommit(format!(
                "paged metadata root: confirmed {} of {} required members",
                committed.len(),
                self.quorum
            )));
        }
        // Publish independent certificates only after a root quorum is
        // durable. This second barrier prevents silent rollback if an
        // acknowledged root subsequently loses one of its original voters.
        let certificates = std::thread::scope(|scope| {
            let jobs = self
                .members
                .iter_mut()
                .filter(|member| committed.contains_key(&member.arena.disk_id))
                .map(|member| {
                    let backend = &self.backend;
                    scope.spawn(move || {
                        let result = member.certify(&**backend, namespace, identity);
                        (member.arena.disk_id.clone(), result)
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .map(|job| job.join().expect("metadata certification panicked"))
                .collect::<Vec<_>>()
        });
        let mut certified = 0;
        for (disk, result) in certificates {
            match result {
                Ok(()) => {
                    certified += 1;
                    committed.get_mut(&disk).unwrap().pages_written += 1;
                }
                Err(err) => {
                    failures.insert(disk, err.to_string());
                }
            }
        }
        if certified < self.quorum {
            self.fenced = true;
            return Err(ArgosError::IndeterminateCommit(format!(
                "paged metadata certificate: confirmed {certified} of {} required members",
                self.quorum
            )));
        }
        self.current = identity;
        Ok(CommitReport {
            txid,
            root_hash: identity.1,
            members: committed,
            failures,
        })
    }

    fn ensure_certified_base(&mut self) -> Result<()> {
        let holders = self
            .members
            .iter()
            .filter(|m| m.root_slot(self.current).is_some())
            .count();
        if holders < self.quorum {
            return Err(unavailable(self.quorum, holders));
        }
        let mut certified = self
            .members
            .iter()
            .filter(|m| {
                m.root_slot(self.current)
                    .is_some_and(|slot| m.is_certified(slot))
            })
            .count();
        if certified >= self.quorum {
            return Ok(());
        }
        // The recovered root had physical quorum or a durable certificate.
        // Re-certify it before overwriting any older root slot.
        for member in &mut self.members {
            if member
                .root_slot(self.current)
                .is_some_and(|slot| !member.is_certified(slot))
                && member
                    .certify(&*self.backend, self.namespace, self.current)
                    .is_ok()
            {
                certified += 1;
            }
        }
        if certified < self.quorum {
            return Err(unavailable(self.quorum, certified));
        }
        Ok(())
    }

    /// Resynchronize verified but stale configured members from the selected
    /// root. Full graph copy is a recovery operation, never a steady-state
    /// commit. An ambiguous repair publication excludes that member until
    /// reopen but cannot change the globally selected logical transaction.
    pub fn repair_stale(
        &mut self,
    ) -> Result<BTreeMap<String, std::result::Result<CommitIo, String>>> {
        if self.fenced {
            return Err(ArgosError::IndeterminateCommit(
                "paged metadata writer must reopen".into(),
            ));
        }
        let mut report = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        let source_index = self
            .members
            .iter()
            .position(|member| member.root_slot(self.current).is_some())
            .ok_or_else(|| unavailable(self.quorum, 0))?;
        let source_slot = self.members[source_index].root_slot(self.current).unwrap();
        for index in 0..self.members.len() {
            if self.members[index].root_slot(self.current).is_some() {
                continue;
            }
            let disk = self.members[index].arena.disk_id.clone();
            let prepared = self.members[index].prepare_repair(
                &*self.backend,
                &self.members[source_index],
                source_slot,
            );
            let plan = match prepared {
                Ok(plan) => plan,
                Err(err) => {
                    report.insert(disk, Err(err.to_string()));
                    continue;
                }
            };
            if let Err(err) = self.members[index].write_body(&*self.backend, &plan) {
                report.insert(disk, Err(err.to_string()));
                continue;
            }
            let result = self.members[index].publish(&*self.backend, self.namespace, plan);
            match result {
                Ok(mut io) => {
                    match self.members[index].certify(&*self.backend, self.namespace, self.current)
                    {
                        Ok(()) => {
                            io.pages_written += 1;
                            report.insert(disk, Ok(io));
                        }
                        Err(err) => {
                            report.insert(disk, Err(err.to_string()));
                        }
                    }
                }
                Err(err) => {
                    ambiguous.insert(index);
                    report.insert(disk, Err(err.to_string()));
                }
            }
        }
        let mut index = 0;
        self.members.retain(|_| {
            let keep = !ambiguous.contains(&index);
            index += 1;
            keep
        });
        Ok(report)
    }

    pub fn get(&self, key: u64) -> Result<Option<Vec<u8>>> {
        self.read_current(|member, slot| member.get(&*self.backend, slot, key))
    }

    /// Full traversal for recovery, migration and offline verification only.
    pub fn records(&self) -> Result<BTreeMap<u64, Vec<u8>>> {
        self.read_current(|member, slot| member.records(&*self.backend, slot))
    }

    fn read_current<T>(&self, read: impl Fn(&Member, usize) -> Result<T>) -> Result<T> {
        if self.fenced {
            return Err(ArgosError::IndeterminateCommit(
                "paged metadata reader must reopen".into(),
            ));
        }
        let mut last = None;
        for member in &self.members {
            if let Some(slot) = member.root_slot(self.current) {
                match read(member, slot) {
                    Ok(result) => return Ok(result),
                    Err(err) => last = Some(err),
                }
            }
        }
        Err(last.unwrap_or_else(|| unavailable(self.quorum, 0)))
    }

    /// Size of a persisted page including its physical checksum.
    pub const fn page_size() -> usize {
        PAGE_SIZE
    }
}

#[cfg(test)]
mod tests;
