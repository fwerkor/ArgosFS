//! Read-only, root-pinned access without constructing an allocator or image.
//! Uncertified prepared quorums require the ordinary exhaustive recovery path.
//! The owner MUST exclude all writers for the lifetime of this handle.
use super::*;
use std::marker::PhantomData;

/// Only a certified transaction may enter the lazy reader. No writable
/// operations are provided; without a persistent free-space proof this must
/// never be used to initialize a writer.
pub struct CertifiedReader<K: TreeKey> {
    backend: Arc<dyn StorageBackend>,
    roots: Vec<(Arena, Root)>,
    current: (u64, Hash),
    key: PhantomData<K>,
}

impl<K: TreeKey> CertifiedReader<K> {
    pub fn open(
        backend: Arc<dyn StorageBackend>,
        pool: [u8; 16],
        arenas: Vec<Arena>,
    ) -> Result<Self> {
        let quorum = validate_members(&arenas)?;
        let namespace = namespace_identity(pool, &arenas);
        let mut votes = BTreeMap::<(u64, Hash), usize>::new();
        let mut certified = BTreeSet::new();
        let mut certificate_roots = Vec::new();
        let mut roots = Vec::new();
        let mut complete = 0;
        for arena in arenas {
            let Ok((data, _)) = arena.views::<K>(&*backend) else {
                continue;
            };
            let identity = member_identity(namespace, &arena.disk_id);
            let mut headers_read = true;
            let mut local_roots = Vec::new();
            for slot in 0..RESERVED_PAGES {
                match arena.read(&*backend, slot) {
                    Ok(bytes) => {
                        match Root::decode::<K>(&bytes, identity, arena.pages(), slot >= 2) {
                            Ok(root) => {
                                if slot >= 2 {
                                    // Preserve certificate evidence even when another header fails.
                                    certified.insert(root.identity());
                                    certificate_roots.push(root);
                                } else {
                                    local_roots.push(root);
                                }
                            }
                            Err(err @ ArgosError::IncompatibleFormat(_)) => return Err(err),
                            Err(_) => {}
                        }
                    }
                    Err(_) => headers_read = false,
                }
            }
            if headers_read && !local_roots.is_empty() {
                complete += 1;
            }
            let identities: BTreeSet<_> = local_roots.iter().map(|r| r.identity()).collect();
            for id in identities {
                *votes.entry(id).or_default() += 1;
            }
            roots.extend(local_roots.into_iter().map(|r| (data.clone(), r)));
        }
        if complete < quorum {
            return Err(unavailable(quorum, complete));
        }
        let mut eligible = certified.clone();
        eligible.extend(
            votes
                .iter()
                .filter(|(_, count)| **count >= quorum)
                .map(|(id, _)| *id),
        );
        let mut eligible = eligible.into_iter().rev();
        let current = eligible.next().ok_or_else(|| unavailable(quorum, 0))?;
        if eligible.next().is_some_and(|other| other.0 == current.0) {
            return Err(ArgosError::Conflict(
                "divergent metadata roots at the same transaction have quorum evidence".into(),
            ));
        }
        for root in certificate_roots
            .iter()
            .chain(roots.iter().map(|(_, root)| root))
        {
            if root.identity() == current {
                root.validate_features(false)?;
            }
        }
        if !certified.contains(&current) {
            return Err(ArgosError::JournalReplayRequired(
                "uncertified prepared quorum requires exhaustive metadata recovery".into(),
            ));
        }
        // Only authenticate root pages here. Descendants are checked on the
        // queried path. A damaged descendant must never trigger rollback.
        roots.retain(|(arena, root)| {
            root.identity() == current && arena.validate_tree_head::<K>(&*backend, *root).is_ok()
        });
        if roots.is_empty() {
            return Err(ArgosError::CorruptedMetadata(
                "latest certified metadata root has no readable head; refusing rollback".into(),
            ));
        }
        Ok(Self {
            backend,
            roots,
            current,
            key: PhantomData,
        })
    }

    pub fn txid(&self) -> u64 {
        self.current.0
    }
    pub fn root_hash(&self) -> [u8; 32] {
        self.current.1
    }

    pub fn scan(&self, window: &ScanWindow<K>) -> Result<ScanPage<K>> {
        window.validate()?;
        let mut last = None;
        for (arena, root) in &self.roots {
            match arena.scan_window(&*self.backend, *root, window) {
                Ok(page) => return Ok(page),
                Err(err) => last = Some(err),
            }
        }
        Err(last.unwrap_or_else(|| unavailable(1, 0)))
    }

    pub fn get(&self, key: K) -> Result<Option<Vec<u8>>> {
        let window = ScanWindow {
            start: Bound::Included(key.clone()),
            end: Bound::Included(key),
            max_records: 1,
            max_bytes: wire::MAX_VALUE + 4096,
            order: ScanOrder::Ascending,
        };
        Ok(self.scan(&window)?.records.pop().map(|(_, value)| value))
    }
}
