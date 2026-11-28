use super::*;
use crate::backend::{BackendCapabilities, BackendDeviceInfo, DeviceId, FileBlockBackend};
use crate::types::{BackendKind, DiskStatus};
use parking_lot::Mutex;
use std::fs::File;

fn fixture(count: usize, pages: u64) -> (tempfile::TempDir, Arc<dyn StorageBackend>, Vec<Arena>) {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<_> = (0..count)
        .map(|i| {
            let path = dir.path().join(format!("{i}.img"));
            File::create(&path)
                .unwrap()
                .set_len(pages * PAGE_SIZE as u64)
                .unwrap();
            path
        })
        .collect();
    let arenas = (0..count)
        .map(|i| Arena {
            disk_id: format!("disk-{i:04}"),
            offset: 0,
            length: pages * PAGE_SIZE as u64,
        })
        .collect();
    let backend = Arc::new(FileBlockBackend::open_loop(&paths, true).unwrap());
    (dir, backend, arenas)
}

const POOL: [u8; 16] = [17; 16];

#[test]
fn packed_tree_large_namespace_changes_only_one_path() {
    let (_dir, backend, arenas) = fixture(1, 16_384);
    let mut tree = PagedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    let mut expected = BTreeMap::new();
    let mut changes = Changes::new();
    for key in 0..50_000 {
        let bytes = vec![(key % 251) as u8; 128];
        changes.insert(key, Some(bytes.clone()));
        expected.insert(key, bytes);
    }
    let initial = tree.commit(&changes).unwrap();
    let initial_pages = initial.members["disk-0000"].pages_written;
    assert!(initial_pages > 1_000);
    assert_eq!(tree.get(49_999).unwrap(), expected.get(&49_999).cloned());
    let updated = vec![99; 128];
    let report = tree
        .commit(&BTreeMap::from([(25_000, Some(updated.clone()))]))
        .unwrap();
    let io = report.members["disk-0000"];
    eprintln!(
        "50k records: initial {initial_pages} pages, edit {} reads, {} writes",
        io.pages_read, io.pages_written
    );
    assert!(io.pages_read <= 18 && io.pages_written <= 8);
    expected.insert(25_000, updated);
    drop(tree);
    let mut tree = PagedMetadata::open(backend, POOL, arenas).unwrap();
    assert_eq!(tree.txid(), 2);
    assert_eq!(tree.root_hash(), report.root_hash);
    assert_eq!(tree.records().unwrap(), expected);
    let changes = BTreeMap::from([(0, None), (49_999, None), (u64::MAX, Some(vec![7; 20_000]))]);
    tree.commit(&changes).unwrap();
    assert_eq!(tree.get(0).unwrap(), None);
    assert_eq!(tree.get(49_999).unwrap(), None);
    assert_eq!(tree.get(u64::MAX).unwrap(), Some(vec![7; 20_000]));
}

#[test]
fn page_reclamation_survives_repeated_updates_deletes_and_reopen() {
    let (_dir, backend, arenas) = fixture(1, 96);
    let mut tree = PagedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    let mut expected = BTreeMap::new();
    let mut rng = 0x123456789abcdefu64;
    for round in 0..160 {
        let mut changes = Changes::new();
        for _ in 0..5 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = (rng >> 32) % 80;
            let value = if rng.is_multiple_of(4) {
                None
            } else {
                Some(vec![
                    round as u8;
                    if rng.is_multiple_of(11) { 4400 } else { 96 }
                ])
            };
            changes.insert(key, value);
        }
        for (key, value) in &changes {
            match value {
                Some(value) => {
                    expected.insert(*key, value.clone());
                }
                None => {
                    expected.remove(key);
                }
            }
        }
        tree.commit(&changes).unwrap();
        if round % 13 == 0 {
            drop(tree);
            tree = PagedMetadata::open(backend.clone(), POOL, arenas.clone()).unwrap();
            assert_eq!(tree.records().unwrap(), expected);
        }
    }
    assert_eq!(tree.records().unwrap(), expected);
    tree.commit(&expected.keys().map(|key| (*key, None)).collect())
        .unwrap();
    assert!(tree.records().unwrap().is_empty());
    // Reclaim the previous root as well, then allocate a new large value.
    tree.commit(&Changes::new()).unwrap();
    tree.commit(&BTreeMap::from([(7, Some(vec![1; 128 * 1024]))]))
        .unwrap();
    assert_eq!(tree.get(7).unwrap(), Some(vec![1; 128 * 1024]));
}

#[test]
fn allocation_failure_is_atomic_and_membership_is_bound_to_roots() {
    let (_dir, backend, arenas) = fixture(3, 24);
    let mut tree = PagedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&BTreeMap::from([(1, Some(b"old".to_vec()))]))
        .unwrap();
    assert!(tree
        .commit(&BTreeMap::from([(2, Some(vec![1; 256 * 1024]))]))
        .is_err());
    assert_eq!(tree.txid(), 1);
    assert_eq!(tree.get(1).unwrap(), Some(b"old".to_vec()));
    assert_eq!(tree.get(2).unwrap(), None);
    drop(tree);
    assert!(PagedMetadata::open(backend.clone(), POOL, arenas[..1].to_vec()).is_err());
    assert!(PagedMetadata::open(backend.clone(), [18; 16], arenas.clone()).is_err());
    assert!(PagedMetadata::format(backend.clone(), POOL, arenas.clone()).is_err());
    let tree = PagedMetadata::open(backend, POOL, arenas).unwrap();
    assert_eq!(tree.txid(), 1);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Body,
    Root,
    Certificate,
}

struct FaultBackend {
    inner: Arc<dyn StorageBackend>,
    failures: Mutex<BTreeMap<String, Phase>>,
    offline: Mutex<BTreeSet<String>>,
    reads: Mutex<BTreeMap<(String, u64), usize>>,
    read_calls: std::sync::atomic::AtomicUsize,
}

impl FaultBackend {
    fn new(inner: Arc<dyn StorageBackend>) -> Self {
        Self {
            inner,
            failures: Mutex::new(BTreeMap::new()),
            offline: Mutex::new(BTreeSet::new()),
            reads: Mutex::new(BTreeMap::new()),
            read_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn check(&self, disk: &str) -> Result<()> {
        if self.offline.lock().contains(disk) {
            return Err(ArgosError::MissingDevice(disk.into()));
        }
        Ok(())
    }
}

impl StorageBackend for FaultBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.check(id)?;
        self.read_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut reads = self.reads.lock();
        for page in 0..buf.len().div_ceil(PAGE_SIZE) {
            *reads
                .entry((id.clone(), offset + (page * PAGE_SIZE) as u64))
                .or_default() += 1;
        }
        drop(reads);
        self.inner.read_at(id, offset, buf)
    }
    fn write_at(&self, id: &DeviceId, offset: u64, data: &[u8]) -> Result<()> {
        self.check(id)?;
        let phase = match offset / PAGE_SIZE as u64 {
            0..=1 => Phase::Root,
            2..=3 => Phase::Certificate,
            _ => Phase::Body,
        };
        if self.failures.lock().get(id) == Some(&phase) {
            // Simulate a torn 4 KiB publication or body write, persisted
            // before the failure is reported to the writer.
            self.inner.write_at(id, offset, &data[..data.len() / 2])?;
            self.inner.flush_device(id)?;
            return Err(ArgosError::Io(std::io::Error::from_raw_os_error(libc::EIO)));
        }
        self.inner.write_at(id, offset, data)
    }
    fn flush_device(&self, id: &DeviceId) -> Result<()> {
        self.check(id)?;
        self.inner.flush_device(id)
    }
    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }
    fn capacity(&self, id: &DeviceId) -> Result<u64> {
        self.check(id)?;
        self.inner.capacity(id)
    }
    fn device_status(&self, id: &DeviceId) -> Result<DiskStatus> {
        self.inner.device_status(id)
    }
    fn capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }
}

#[test]
fn torn_body_and_minority_root_preserve_previous_quorum() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = PagedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&BTreeMap::from([(1, Some(b"old".to_vec()))]))
        .unwrap();
    let change = BTreeMap::from([(1, Some(b"new".to_vec()))]);
    for phase in [Phase::Body, Phase::Root] {
        *faults.failures.lock() =
            BTreeMap::from([("disk-0001".into(), phase), ("disk-0002".into(), phase)]);
        let err = tree.commit(&change).unwrap_err();
        match phase {
            Phase::Body => assert!(matches!(err, ArgosError::QuorumUnavailable { .. })),
            Phase::Root => {
                assert!(matches!(err, ArgosError::IndeterminateCommit(_)));
                assert!(tree.commit(&change).is_err());
            }
            _ => unreachable!(),
        }
        faults.failures.lock().clear();
        drop(tree);
        tree = PagedMetadata::open(faults.clone(), POOL, arenas.clone()).unwrap();
        assert_eq!(tree.txid(), 1);
        assert_eq!(tree.get(1).unwrap(), Some(b"old".to_vec()));
    }
    let committed = tree.commit(&change).unwrap();
    assert_eq!(committed.txid, 2);
    assert_eq!(tree.get(1).unwrap(), Some(b"new".to_vec()));
}

#[test]
fn prepared_quorum_recovers_after_certificate_failure() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = PagedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    *faults.failures.lock() = BTreeMap::from([
        ("disk-0001".into(), Phase::Certificate),
        ("disk-0002".into(), Phase::Certificate),
    ]);
    assert!(matches!(
        tree.commit(&BTreeMap::from([(1, Some(b"new".to_vec()))])),
        Err(ArgosError::IndeterminateCommit(_))
    ));
    faults.failures.lock().clear();
    drop(tree);
    let tree = PagedMetadata::open(faults, POOL, arenas).unwrap();
    assert_eq!(tree.txid(), 1);
    assert_eq!(tree.get(1).unwrap(), Some(b"new".to_vec()));
}

#[test]
fn certificate_prevents_rollback_after_losing_a_commit_voter() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = PagedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    faults
        .failures
        .lock()
        .insert("disk-0002".into(), Phase::Body);
    tree.commit(&BTreeMap::from([(1, Some(b"acknowledged".to_vec()))]))
        .unwrap();
    drop(tree);
    faults.failures.lock().clear();
    faults.offline.lock().insert("disk-0000".into());
    let mut tree = PagedMetadata::open(faults.clone(), POOL, arenas.clone()).unwrap();
    assert_eq!(tree.txid(), 1);
    assert_eq!(tree.get(1).unwrap(), Some(b"acknowledged".to_vec()));
    // Recovery knows the new root is authoritative, but cannot mutate
    // until the stale available member has been repaired.
    assert!(matches!(
        tree.commit(&BTreeMap::from([(2, Some(vec![1]))])),
        Err(ArgosError::QuorumUnavailable { .. })
    ));
    let repaired = tree.repair_stale().unwrap();
    assert!(repaired["disk-0002"].is_ok());
    tree.commit(&BTreeMap::from([(2, Some(vec![1]))])).unwrap();
    assert_eq!(tree.txid(), 2);
    assert_eq!(tree.get(2).unwrap(), Some(vec![1]));
    drop(tree);
    faults.offline.lock().clear();
    let tree = PagedMetadata::open(faults, POOL, arenas).unwrap();
    assert_eq!(tree.txid(), 2);
    assert_eq!(tree.get(1).unwrap(), Some(b"acknowledged".to_vec()));
}

#[test]
fn corrupt_page_is_not_a_quorum_vote() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let mut tree = PagedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&BTreeMap::from([(1, Some(vec![1; 10_000]))]))
        .unwrap();
    let member = &tree.members[0];
    let slot = member.root_slot(tree.current).unwrap();
    let page = member.roots[slot].unwrap().tree.unwrap().page;
    drop(tree);
    backend
        .write_at(&arenas[0].disk_id, page * PAGE_SIZE as u64 + 20, &[0xff])
        .unwrap();
    backend.flush_device(&arenas[0].disk_id).unwrap();
    let tree = PagedMetadata::open(backend, POOL, arenas).unwrap();
    assert_eq!(tree.txid(), 1);
    assert_eq!(tree.get(1).unwrap(), Some(vec![1; 10_000]));
}

#[test]
fn typed_image_roundtrip_and_incremental_dirty_inode_commit() {
    let root = tempfile::tempdir().unwrap();
    let fs = crate::ArgosFs::create(
        root.path(),
        crate::VolumeConfig {
            k: 1,
            m: 0,
            ..Default::default()
        },
        1,
        false,
    )
    .unwrap();
    fs.write_file("/file", b"small", 0o600).unwrap();
    let mut metadata = fs.metadata_snapshot();
    let template = metadata
        .inodes
        .values()
        .find(|inode| inode.kind == crate::types::NodeKind::File)
        .unwrap()
        .clone();
    for key in 10_000..18_192 {
        let mut inode = template.clone();
        inode.id = key;
        metadata.inodes.insert(key, inode);
    }
    metadata.next_inode = 18_192;
    let pool = uuid::Uuid::parse_str(&metadata.uuid).unwrap();
    let (_dir, backend, arenas) = fixture(1, 8_192);
    let mut store = ImageStore::format(backend.clone(), arenas.clone(), &mut metadata).unwrap();
    let mut next = store.snapshot();
    next.txid += 1;
    next.inodes.get_mut(&12_345).unwrap().uid = 123;
    let report = store.commit(&mut next).unwrap();
    assert!(report.members["disk-0000"].pages_written <= 12);
    assert_eq!(next.inodes.pending_changes(), 0);
    assert_eq!(metadata.inodes.get(&12_345).unwrap().uid, template.uid);
    let expected =
        serde_json::from_slice::<crate::types::Metadata>(&serde_json::to_vec(&next).unwrap())
            .unwrap();
    drop(store);
    let mut store = ImageStore::open(backend, pool, arenas).unwrap();
    let recovered = store.snapshot();
    assert_eq!(
        serde_json::to_vec(&recovered).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert_eq!(store.root_hash(), report.root_hash);
    // A snapshot from before the commit has an independent dirty baseline;
    // the fallback must discover every deletion and insertion.
    metadata.txid = next.txid + 1;
    metadata.inodes.remove(&12_345);
    let mut added = template;
    added.id = 20_000;
    metadata.inodes.insert(20_000, added);
    store.commit(&mut metadata).unwrap();
    assert!(!store.snapshot().inodes.contains_key(&12_345));
    assert!(store.snapshot().inodes.contains_key(&20_000));
}

#[test]
#[ignore = "large-scale explicit metadata-page regression"]
fn two_hundred_ninety_four_thousand_records_stay_path_bounded() {
    let (_dir, backend, arenas) = fixture(1, 32_768);
    let mut tree = PagedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    let changes = (1..=294_000)
        .map(|key| (key, Some(vec![(key % 251) as u8; 128])))
        .collect();
    let start = std::time::Instant::now();
    let initial = tree.commit(&changes).unwrap();
    let build = start.elapsed();
    let start = std::time::Instant::now();
    let edit = tree
        .commit(&Changes::from([(147_000, Some(vec![2; 128]))]))
        .unwrap();
    let elapsed = start.elapsed();
    let io = edit.members["disk-0000"];
    eprintln!(
        "294k records: initial {} pages ({build:?}), edit {} reads, {} writes ({elapsed:?})",
        initial.members["disk-0000"].pages_written, io.pages_read, io.pages_written
    );
    assert!(io.pages_read <= 18 && io.pages_written <= 8);
    drop(tree);
    let recovered = PagedMetadata::open(backend, POOL, arenas).unwrap();
    assert_eq!(recovered.txid(), 2);
    assert_eq!(recovered.get(147_000).unwrap(), Some(vec![2; 128]));
    assert_eq!(
        recovered.get(294_000).unwrap(),
        Some(vec![(294_000 % 251) as u8; 128])
    );
}

#[test]
fn known_certified_root_without_intact_pages_must_not_roll_back() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let mut tree = PagedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&Changes::from([(1, Some(b"old".to_vec()))]))
        .unwrap();
    tree.commit(&Changes::from([(1, Some(b"acknowledged".to_vec()))]))
        .unwrap();
    for member in &tree.members {
        let slot = member.root_slot(tree.current).unwrap();
        let page = member.roots[slot].unwrap().tree.unwrap().page;
        backend
            .write_at(&member.arena.disk_id, page * PAGE_SIZE as u64 + 20, &[0xff])
            .unwrap();
        backend.flush_device(&member.arena.disk_id).unwrap();
    }
    drop(tree);
    assert!(matches!(
        PagedMetadata::open(backend, POOL, arenas),
        Err(ArgosError::CorruptedMetadata(message)) if message.contains("refusing rollback")
    ));
}

#[test]
fn copied_member_bytes_do_not_vote_as_a_second_device() {
    let (_dir, backend, arenas) = fixture(3, 64);
    let mut tree = PagedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&Changes::from([(1, Some(b"committed".to_vec()))]))
        .unwrap();
    drop(tree);
    let mut bytes = vec![0; arenas[0].length as usize];
    backend.read_at(&arenas[0].disk_id, 0, &mut bytes).unwrap();
    backend.write_at(&arenas[1].disk_id, 0, &bytes).unwrap();
    backend.flush_device(&arenas[1].disk_id).unwrap();
    let faults = Arc::new(FaultBackend::new(backend));
    faults.offline.lock().insert("disk-0002".into());
    assert!(matches!(
        PagedMetadata::open(faults, POOL, arenas),
        Err(ArgosError::QuorumUnavailable { .. })
    ));
}

#[test]
#[ignore = "large real-inode metadata arena performance regression"]
fn paged_real_inode_population_294k_incremental_commit() {
    let root = tempfile::tempdir().unwrap();
    let fs = crate::ArgosFs::create(
        root.path(),
        crate::VolumeConfig {
            k: 1,
            m: 0,
            ..Default::default()
        },
        1,
        false,
    )
    .unwrap();
    fs.write_file("/file", b"small", 0o600).unwrap();
    let mut meta = fs.metadata_snapshot();
    let template = meta
        .inodes
        .values()
        .find(|inode| inode.kind == crate::types::NodeKind::File)
        .unwrap()
        .clone();
    for ino in 3..=294000 {
        let mut inode = template.clone();
        inode.id = ino;
        meta.inodes.insert(ino, inode);
        meta.inodes
            .get_mut(&1)
            .unwrap()
            .entries
            .insert(format!("file-{ino:06}"), ino);
    }
    meta.next_inode = 294001;
    let pool = uuid::Uuid::parse_str(&meta.uuid).unwrap();
    let (_dir, backend, arenas) = fixture(1, 65536);
    let start = std::time::Instant::now();
    let mut store = ImageStore::format(backend.clone(), arenas.clone(), &mut meta).unwrap();
    let build = start.elapsed();
    let mut next = store.snapshot();
    next.txid += 1;
    next.inodes.get_mut(&147000).unwrap().uid = 123;
    next.inodes
        .get_mut(&1)
        .unwrap()
        .entries
        .remove("file-147000");
    next.inodes
        .get_mut(&1)
        .unwrap()
        .entries
        .insert("renamed-file".into(), 147000);
    let start = std::time::Instant::now();
    let report = store.commit(&mut next).unwrap();
    let elapsed = start.elapsed();
    let io = report.members["disk-0000"];
    eprintln!("294k inode-shaped records and directory entries: build {build:?}, edit {} reads, {} writes ({elapsed:?})",io.pages_read,io.pages_written);
    assert!(io.pages_read < 80 && io.pages_written < 40);
    drop(store);
    let recovered = ImageStore::open(backend, pool, arenas).unwrap().snapshot();
    assert_eq!(recovered.inodes.len(), 294000);
    assert_eq!(recovered.inodes[&147000].uid, 123);
    assert_eq!(recovered.inodes[&1].entries["renamed-file"], 147000);
    assert!(!recovered.inodes[&1].entries.contains_key("file-147000"));
}

#[test]
fn cold_open_reads_shared_cow_pages_only_once_per_member() {
    let (_dir, backend, arenas) = fixture(3, 4096);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = PagedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    let changes = (0..4096)
        .map(|i| {
            (
                i,
                Some(vec![(i % 251) as u8; if i % 113 == 0 { 9000 } else { 128 }]),
            )
        })
        .collect();
    tree.commit(&changes).unwrap();
    tree.commit(&BTreeMap::from([(100, Some(b"changed".to_vec()))]))
        .unwrap();
    drop(tree);
    faults.reads.lock().clear();
    faults
        .read_calls
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let tree = PagedMetadata::open(faults.clone(), POOL, arenas).unwrap();
    {
        let reads = faults.reads.lock();
        assert!(reads.len() > 600, "exercise a substantial shared graph");
        let calls = faults.read_calls.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "cold recovery: {} unique pages, {calls} read requests",
            reads.len()
        );
        assert!(
            calls < reads.len() * 3 / 4,
            "adjacent page reads were not coalesced"
        );
        assert!(
            reads.values().all(|&n| n == 1),
            "cold open reread an authenticated page"
        );
    }
    assert_eq!(tree.get(100).unwrap(), Some(b"changed".to_vec()));
    assert_eq!(tree.get(113).unwrap().unwrap().len(), 9000);
}
