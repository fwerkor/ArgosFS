//! Experimental persistent metadata engine for an explicitly reserved raw
//! region. This is the opt-in raw paged format, separate from legacy
//! checkpoint storage. Fixed membership is authenticated by the manifest.
//!
//! Fixed membership, majority quorum, one serialized writer. Each device has
//! a private page allocator and two checksummed root slots. Commit writes
//! unreachable COW pages and flushes them before publishing the new root.
//! Reopening verifies both root graphs and reconstructs free space; steady
//! state commits update reference counts only on changed paths.
mod image;
mod member;
pub mod migration;
pub mod physical;
mod reader;
pub mod typed;
mod typed_cache;
pub use reader::CertifiedReader;
pub use typed_cache::{CacheInfo, RecordCache};
mod wire;
pub use image::{ImageStore, PAGED_INTEGRITY_SCHEME, V3_INTEGRITY_SCHEME};
pub use wire::{TreeKey, TypedKey, V3Features};

use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use member::{Member, Prepared};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::Arc;
use wire::{Hash, Root, PAGE_SIZE, RESERVED_PAGES};

/// A region exclusively reserved for this metadata format. The first two
/// 4 KiB pages are root slots; the next two are quorum certificates. Never point this at a live legacy checkpoint,
/// data extent, or allocator region.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Arena {
    pub disk_id: String,
    pub offset: u64,
    pub length: u64,
}

// Domain 14 is owned by the physical snapshot index. Generic record writes
// may not fabricate references into the allocator's live graph.
pub(super) const SNAPSHOT_DOMAIN: u16 = 14;
// Private prototype domain: final design reserves 17 for writable View.
pub(super) const SNAPSHOT_PIN_DOMAIN: u16 = 0xff00;

#[derive(Clone, Debug)]
pub(super) enum SnapshotEdit<K> {
    Create(K),
    Delete(K),
}

/// Sorted, unique changes. None deletes a record; Some(empty) is a value.
pub type TreeChanges<K> = BTreeMap<K, Option<Vec<u8>>>;
pub type Changes = TreeChanges<u64>;
pub type TypedChanges = TreeChanges<TypedKey>;
pub type PagedMetadata = MetadataTree<u64>;
pub type TypedMetadata = MetadataTree<TypedKey>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ScanOrder {
    #[default]
    Ascending,
    Descending,
}

/// Bounded ordered query. Resume using Excluded(last returned key) as the
/// start bound for ascending queries, or the end bound for descending queries.
/// The byte budget includes encoded keys and expanded (not on-disk) values.
#[derive(Clone, Debug)]
pub struct ScanWindow<K> {
    pub start: Bound<K>,
    pub end: Bound<K>,
    pub max_records: usize,
    pub max_bytes: usize,
    pub order: ScanOrder,
}

impl<K: Ord> ScanWindow<K> {
    fn validate(&self) -> Result<()> {
        if self.max_records == 0 || self.max_bytes == 0 {
            return Err(ArgosError::Invalid(
                "metadata scan budget must be nonzero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanPage<K> {
    pub records: Vec<(K, Vec<u8>)>,
    pub more: bool,
}

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
pub struct MetadataTree<K: TreeKey> {
    backend: Arc<dyn StorageBackend>,
    namespace: Hash,
    pool: [u8; 16],
    configured_members: usize,
    quorum: usize,
    members: Vec<Member<K>>,
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

type Selection<K> = (Hash, usize, Vec<Member<K>>, (u64, Hash));

fn select_members<K: TreeKey>(
    backend: &dyn StorageBackend,
    pool: [u8; 16],
    arenas: Vec<Arena>,
) -> Result<Selection<K>> {
    select_members_inner::<K>(backend, pool, arenas, !K::PERSISTENT_ALLOCATOR)
}

fn select_members_inner<K: TreeKey>(
    backend: &dyn StorageBackend,
    pool: [u8; 16],
    arenas: Vec<Arena>,
    full: bool,
) -> Result<Selection<K>> {
    let quorum = validate_members(&arenas)?;
    let namespace = namespace_identity(pool, &arenas);
    let opened = std::thread::scope(|scope| {
        let jobs = arenas
            .iter()
            .map(|arena| {
                scope.spawn(move || Member::<K>::open(backend, arena.clone(), namespace, full))
            })
            .collect::<Vec<_>>();
        jobs.into_iter()
            .map(|job| job.join().expect("metadata member recovery panicked"))
            .collect::<Vec<_>>()
    });
    let mut members = Vec::new();
    for result in opened {
        match result {
            Ok(member) => members.push(member),
            Err(err @ ArgosError::IncompatibleFormat(_)) => return Err(err),
            Err(_) => {}
        }
    }
    let valid_members = members
        .iter()
        .filter(|member| member.headers_complete && member.roots.iter().any(Option::is_some))
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
    let mut eligible = certified.clone();
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
    for member in &members {
        for root in member
            .roots
            .iter()
            .chain(member.certificates.iter())
            .flatten()
        {
            if root.identity() == current {
                root.validate_features(true)?;
            }
        }
    }
    if !full && !certified.contains(&current) {
        return select_members_inner::<K>(backend, pool, arenas, true);
    }
    if !votes.contains_key(&current) {
        return Err(ArgosError::CorruptedMetadata(
            "latest certified metadata root has no intact copy; refusing rollback".into(),
        ));
    }
    Ok((namespace, quorum, members, current))
}

pub(super) fn recover_records<K: TreeKey>(
    backend: &dyn StorageBackend,
    pool: [u8; 16],
    arenas: Vec<Arena>,
) -> Result<(BTreeMap<K, Vec<u8>>, Hash)> {
    let (_, quorum, members, current) = select_members::<K>(backend, pool, arenas)?;
    let mut last = None;
    for member in &members {
        if let Some(slot) = member.root_slot(current) {
            match member.records(backend, slot) {
                Ok(records) => return Ok((records, current.1)),
                Err(err) => last = Some(err),
            }
        }
    }
    Err(last.unwrap_or_else(|| unavailable(quorum, 0)))
}

impl<K: TreeKey> MetadataTree<K> {
    /// Initialize previously unused, zero-rooted regions. All members must
    /// be accessible. This is intentionally not an in-place legacy migration.
    pub fn format(
        backend: Arc<dyn StorageBackend>,
        pool: [u8; 16],
        arenas: Vec<Arena>,
    ) -> Result<Self> {
        Self::format_selected(backend, pool, arenas, None)
    }

    pub(super) fn format_selected(
        backend: Arc<dyn StorageBackend>,
        pool: [u8; 16],
        arenas: Vec<Arena>,
        selected: Option<&BTreeSet<String>>,
    ) -> Result<Self> {
        let quorum = validate_members(&arenas)?;
        let chosen = arenas
            .iter()
            .filter(|arena| selected.is_none_or(|ids| ids.contains(&arena.disk_id)))
            .collect::<Vec<_>>();
        if chosen.len() < quorum {
            return Err(unavailable(quorum, chosen.len()));
        }

        let namespace = namespace_identity(pool, &arenas);
        for arena in &chosen {
            arena.views::<K>(&*backend)?;
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
            physical: K::PERSISTENT_ALLOCATOR.then(wire::PhysicalState::default),
        };
        for arena in &chosen {
            for slot in 0..2 {
                arena.write(
                    &*backend,
                    slot,
                    &empty.encode::<K>(
                        member_identity(namespace, &arena.disk_id),
                        arena.pages(),
                        false,
                    ),
                )?;
            }
            backend.flush_device(&arena.disk_id)?;
        }
        for arena in &chosen {
            for slot in 2..4 {
                arena.write(
                    &*backend,
                    slot,
                    &empty.encode::<K>(
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
        let configured_members = arenas.len();
        let (namespace, quorum, members, current) = select_members::<K>(&*backend, pool, arenas)?;
        Ok(Self {
            backend,
            namespace,
            pool,
            configured_members,
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
    pub fn commit(&mut self, changes: &TreeChanges<K>) -> Result<CommitReport> {
        self.commit_allowed(changes, None)
    }

    pub(super) fn commit_allowed(
        &mut self,
        changes: &TreeChanges<K>,
        allowed: Option<&BTreeSet<String>>,
    ) -> Result<CommitReport> {
        self.commit_with_snapshot(changes, allowed, None)
    }

    pub(super) fn commit_with_snapshot(
        &mut self,
        changes: &TreeChanges<K>,
        allowed: Option<&BTreeSet<String>>,
        snapshot: Option<&SnapshotEdit<K>>,
    ) -> Result<CommitReport> {
        if changes
            .keys()
            .any(|key| key.domain_id() == Some(SNAPSHOT_DOMAIN))
        {
            return Err(ArgosError::Invalid(
                "snapshot domain requires the snapshot transaction API".into(),
            ));
        }
        if self.fenced {
            return Err(ArgosError::IndeterminateCommit(
                "paged metadata writer must reopen".into(),
            ));
        }
        self.ensure_certified_base(allowed)?;
        let txid = self
            .current
            .0
            .checked_add(1)
            .ok_or_else(|| ArgosError::Invalid("metadata transaction ID exhausted".into()))?;
        let mut failures = BTreeMap::new();
        let plans: Vec<(usize, Prepared)> = std::thread::scope(|scope| {
            let jobs = self
                .members
                .iter()
                .enumerate()
                .filter_map(|(index, member)| {
                    if allowed.is_some_and(|ids| !ids.contains(&member.arena.disk_id)) {
                        return None;
                    }
                    let slot = member.root_slot(self.current)?;
                    let backend = &self.backend;
                    Some(scope.spawn(move || {
                        (
                            index,
                            member.prepare(&**backend, slot, txid, changes, snapshot),
                        )
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
        self.publish_plans(plans, failures, None)
    }

    fn publish_plans(
        &mut self,
        plans: Vec<(usize, Prepared)>,
        mut failures: BTreeMap<String, String>,
        expected: Option<(u64, Hash)>,
    ) -> Result<CommitReport> {
        if plans.len() < self.quorum {
            return Err(unavailable(self.quorum, plans.len()));
        }
        let identity = plans[0].1.root.identity();
        if expected.is_some_and(|expected| expected != identity)
            || plans
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
                    let io = committed.get_mut(&disk).unwrap();
                    io.pages_written += 1;
                    io.pages_read += 1;
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
            txid: identity.0,
            root_hash: identity.1,
            members: committed,
            failures,
        })
    }

    fn ensure_certified_base(&mut self, allowed: Option<&BTreeSet<String>>) -> Result<()> {
        let holders = self
            .members
            .iter()
            .filter(|m| {
                allowed.is_none_or(|ids| ids.contains(&m.arena.disk_id))
                    && m.root_slot(self.current).is_some()
            })
            .count();
        if holders < self.quorum {
            return Err(unavailable(self.quorum, holders));
        }
        let mut certified = self
            .members
            .iter()
            .filter(|m| {
                if allowed.is_some_and(|ids| !ids.contains(&m.arena.disk_id)) {
                    return false;
                }
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
            if allowed.is_some_and(|ids| !ids.contains(&member.arena.disk_id)) {
                continue;
            }
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

    pub(super) fn confirm_all(&mut self, count: usize) -> Result<()> {
        if self.members.len() != count
            || self
                .members
                .iter()
                .any(|m| m.root_slot(self.current).is_none())
        {
            return Err(ArgosError::Conflict(
                "not every configured member has the staged metadata root".into(),
            ));
        }
        for member in &mut self.members {
            member.certify(&*self.backend, self.namespace, self.current)?;
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
                            io.pages_read += 1;
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

    pub fn get(&self, key: K) -> Result<Option<Vec<u8>>> {
        if K::PERSISTENT_ALLOCATOR {
            let window = ScanWindow {
                start: Bound::Included(key.clone()),
                end: Bound::Included(key),
                max_records: 1,
                max_bytes: wire::MAX_VALUE + 4096,
                order: ScanOrder::Ascending,
            };
            return Ok(self.scan(&window)?.records.pop().map(|(_, value)| value));
        }
        self.read_current(|member, slot| member.get(&*self.backend, slot, &key))
    }

    pub fn scan(&self, window: &ScanWindow<K>) -> Result<ScanPage<K>> {
        window.validate()?;
        self.read_current(|member, slot| member.scan(&*self.backend, slot, window))
    }

    /// Full traversal for recovery, migration and offline verification only.
    pub fn records(&self) -> Result<BTreeMap<K, Vec<u8>>> {
        self.read_current(|member, slot| member.records(&*self.backend, slot))
    }

    fn read_current<T>(&self, read: impl Fn(&Member<K>, usize) -> Result<T>) -> Result<T> {
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

/// A named, immutable metadata generation. The digest contains no local
/// page addresses and is identical on every voting member.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotInfo {
    pub txid: u64,
    pub root_hash: [u8; 32],
}

impl SnapshotInfo {
    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 40 {
            return Err(wire::corrupt("invalid snapshot descriptor"));
        }
        Ok(Self {
            txid: u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            root_hash: bytes[8..].try_into().unwrap(),
        })
    }
}

fn snapshot_key(name: &str) -> Result<TypedKey> {
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.contains(['/', '\0'])
    {
        return Err(ArgosError::Invalid("invalid snapshot name".into()));
    }
    TypedKey::new(SNAPSHOT_DOMAIN, name.as_bytes())
}

impl MetadataTree<TypedKey> {
    /// Transfer the complete certified logical DAG to a newly formatted
    /// disjoint staging arena. Translate physical pointers but preserve the
    /// logical root identity and every named snapshot. The source remains
    /// authoritative until the caller certifies its configuration switch.
    pub(super) fn transfer_from(&mut self, source: &mut Self) -> Result<CommitReport> {
        if self.fenced
            || source.fenced
            || self.txid() != 0
            || source.txid() == 0
            || self.pool != source.pool
            || !Arc::ptr_eq(&self.backend, &source.backend)
        {
            return Err(ArgosError::Conflict(
                "V3 graph transfer requires empty same-pool disjoint staging".into(),
            ));
        }
        for target in &self.members {
            for old in &source.members {
                let a = &target.arena;
                let b = &old.arena;
                if a.disk_id == b.disk_id
                    && a.offset < b.offset.saturating_add(b.length)
                    && b.offset < a.offset.saturating_add(a.length)
                {
                    return Err(ArgosError::Conflict(
                        "V3 graph transfer arenas overlap".into(),
                    ));
                }
            }
        }
        source.ensure_certified_base(None)?;
        self.ensure_certified_base(None)?;
        let mut failures = BTreeMap::new();
        let mut plans = Vec::new();
        for (index, target) in self.members.iter().enumerate() {
            let mut last = None;
            let mut plan = None;
            for old in &source.members {
                if let Some(slot) = old.root_slot(source.current) {
                    match target.prepare_repair(&*self.backend, old, slot) {
                        Ok(prepared) => {
                            plan = Some(prepared);
                            break;
                        }
                        Err(err) => last = Some(err.to_string()),
                    }
                }
            }
            if let Some(plan) = plan {
                plans.push((index, plan));
            } else {
                failures.insert(
                    target.arena.disk_id.clone(),
                    last.unwrap_or_else(|| "no verified V3 transfer source".into()),
                );
            }
        }
        self.publish_plans(plans, failures, Some(source.current))
    }

    /// Pin the current catalog without copying its domain trees. This is
    /// an engine snapshot only; callers must separately pin data extents
    /// before offering a filesystem-level snapshot.
    pub fn create_snapshot(&mut self, name: &str) -> Result<SnapshotInfo> {
        let key = snapshot_key(name)?;
        if self.get(key.clone())?.is_some() {
            return Err(ArgosError::AlreadyExists(name.into()));
        }
        self.commit_with_snapshot(
            &TypedChanges::new(),
            None,
            Some(&SnapshotEdit::Create(key.clone())),
        )?;
        match self.get(key).and_then(|bytes| {
            bytes
                .as_deref()
                .map(SnapshotInfo::decode)
                .transpose()?
                .ok_or_else(|| wire::corrupt("committed snapshot missing"))
        }) {
            Ok(info) => Ok(info),
            Err(err) => {
                self.fenced = true;
                Err(ArgosError::CommittedDurabilityLoss(format!(
                    "snapshot committed but descriptor readback failed: {err}"
                )))
            }
        }
    }

    pub fn delete_snapshot(&mut self, name: &str) -> Result<CommitReport> {
        let key = snapshot_key(name)?;
        if self.get(key.clone())?.is_none() {
            return Err(ArgosError::NotFound(name.into()));
        }
        self.commit_with_snapshot(&TypedChanges::new(), None, Some(&SnapshotEdit::Delete(key)))
    }

    pub fn snapshot_info(&self, name: &str) -> Result<Option<SnapshotInfo>> {
        self.get(snapshot_key(name)?)?
            .as_deref()
            .map(SnapshotInfo::decode)
            .transpose()
    }

    pub fn scan_snapshot(
        &self,
        name: &str,
        window: &ScanWindow<TypedKey>,
    ) -> Result<ScanPage<TypedKey>> {
        window.validate()?;
        let key = snapshot_key(name)?;
        self.read_current(|member, slot| member.scan_snapshot(&*self.backend, slot, &key, window))
    }

    pub fn snapshot_page(
        &self,
        after: Option<&str>,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<(Vec<(String, SnapshotInfo)>, bool)> {
        let prefix = TypedKey::new(SNAPSHOT_DOMAIN, &[])?;
        let page = self.scan(&ScanWindow {
            start: match after {
                Some(name) => Bound::Excluded(snapshot_key(name)?),
                None => Bound::Included(prefix.clone()),
            },
            end: Bound::Excluded(prefix.prefix_end().expect("snapshot domain has successor")),
            max_records,
            max_bytes,
            order: ScanOrder::Ascending,
        })?;
        let records = page
            .records
            .into_iter()
            .map(|(key, value)| {
                let name = std::str::from_utf8(&key.as_bytes()[2..])
                    .map_err(|_| wire::corrupt("invalid snapshot name"))?
                    .to_owned();
                snapshot_key(&name)?;
                Ok((name, SnapshotInfo::decode(&value)?))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((records, page.more))
    }

    /// Full physical/semantic allocator audit; does not run on fast mount.
    /// Results are per member, including stale or corrupt members.
    pub fn audit_allocators(&self) -> BTreeMap<String, Result<()>> {
        self.members
            .iter()
            .map(|member| {
                (
                    member.arena.disk_id.clone(),
                    member.audit_allocator(&*self.backend, self.current),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
