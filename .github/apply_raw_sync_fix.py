from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    if old not in text:
        raise SystemExit(f"{label} not found")
    return text.replace(old, new, 1)


mod = Path("src/volume/mod.rs")
text = mod.read_text()

text = replace_once(
    text,
    '''struct DeferredCommitState {
    durable_metadata: Option<Metadata>,
    dirty_transactions: u64,
''',
    '''struct DeferredCommitState {
    durable_metadata: Option<Metadata>,
    raw_uncommitted_metadata_dirty: bool,
    dirty_transactions: u64,
''',
    "deferred state field",
)
text = replace_once(
    text,
    '''            durable_metadata: (meta.backend != BackendKind::Host).then(|| meta.clone()),
            dirty_transactions: 0,
''',
    '''            durable_metadata: (meta.backend != BackendKind::Host).then(|| meta.clone()),
            raw_uncommitted_metadata_dirty: false,
            dirty_transactions: 0,
''',
    "deferred state init",
)

old_sync = '''            let superblocks = self.active_superblocks_locked(&meta)?;
            if self.open_backend_covers_superblocks(&superblocks) {
                raw_store::write_metadata_copies(&*self.backend, &superblocks, &meta)?;
                self.backend.flush_all()?;
            } else {
                let backend = self.active_block_backend_locked(&meta, true)?;
                raw_store::write_metadata_copies(&backend, &superblocks, &meta)?;
                backend.flush_all()?;
            }
            return Ok(());
'''
old_sync_intermediate = '''            let durable_previous = self.deferred_commit.lock().durable_metadata.clone();
            let previous_meta_hash = match durable_previous.as_ref() {
                Some(previous) if !previous.integrity.meta_hash.is_empty() => {
                    previous.integrity.meta_hash.clone()
                }
                Some(previous) => journal::canonical_metadata_hash(previous)?,
                None => String::new(),
            };
            if journal::canonical_metadata_hash(&meta)? != meta.integrity.meta_hash {
                journal::prepare_metadata_integrity_with_previous(
                    &mut meta,
                    previous_meta_hash,
                )?;
            }
            let superblocks = self.active_superblocks_locked(&meta)?;
            if self.open_backend_covers_superblocks(&superblocks) {
                raw_store::write_metadata_copies(&*self.backend, &superblocks, &meta)?;
                self.backend.flush_all()?;
            } else {
                let backend = self.active_block_backend_locked(&meta, true)?;
                raw_store::write_metadata_copies(&backend, &superblocks, &meta)?;
                backend.flush_all()?;
            }
            self.deferred_commit.lock().durable_metadata = Some(meta.clone());
            return Ok(());
'''
new_sync = '''            let raw_uncommitted_metadata_dirty =
                self.deferred_commit.lock().raw_uncommitted_metadata_dirty;
            if raw_uncommitted_metadata_dirty {
                let previous_meta_hash = meta.integrity.meta_hash.clone();
                journal::prepare_metadata_integrity_with_previous(
                    &mut meta,
                    previous_meta_hash,
                )?;
            }
            let superblocks = self.active_superblocks_locked(&meta)?;
            if self.open_backend_covers_superblocks(&superblocks) {
                raw_store::write_metadata_copies(&*self.backend, &superblocks, &meta)?;
                self.backend.flush_all()?;
            } else {
                let backend = self.active_block_backend_locked(&meta, true)?;
                raw_store::write_metadata_copies(&backend, &superblocks, &meta)?;
                backend.flush_all()?;
            }
            let mut state = self.deferred_commit.lock();
            state.durable_metadata = Some(meta.clone());
            state.raw_uncommitted_metadata_dirty = false;
            return Ok(());
'''
if new_sync not in text:
    if old_sync_intermediate in text:
        text = text.replace(old_sync_intermediate, new_sync, 1)
    elif old_sync in text:
        text = text.replace(old_sync, new_sync, 1)
    else:
        raise SystemExit("raw sync block not found")

text = replace_once(
    text,
    '''        let durable_previous = if meta.backend != BackendKind::Host {
            self.deferred_commit.lock().durable_metadata.clone()
        } else {
            None
        };
        let previous_meta_hash = if let Some(previous) = durable_previous.as_ref() {
            if previous.integrity.meta_hash.is_empty() {
                journal::canonical_metadata_hash(previous)?
            } else {
                previous.integrity.meta_hash.clone()
            }
        } else if meta.integrity.meta_hash.is_empty() {
            journal::canonical_metadata_hash(meta)?
        } else {
            meta.integrity.meta_hash.clone()
        };
''',
    '''        let raw_uncommitted_metadata_dirty = meta.backend != BackendKind::Host
            && self.deferred_commit.lock().raw_uncommitted_metadata_dirty;
        let previous_meta_hash = if raw_uncommitted_metadata_dirty {
            // Read-side telemetry can change the live metadata without a transaction.
            // The stored integrity hash still identifies the last journal-durable state.
            meta.integrity.meta_hash.clone()
        } else if meta.integrity.meta_hash.is_empty() {
            journal::canonical_metadata_hash(meta)?
        } else {
            meta.integrity.meta_hash.clone()
        };
''',
    "non-deferred previous hash",
)
text = replace_once(
    text,
    '''            let replay_previous = if previous_metadata.is_some() {
                durable_previous
                    .as_ref()
                    .filter(|previous| previous.integrity.meta_hash == previous_meta_hash)
            } else {
                None
            };
''',
    '''            let replay_previous = if raw_uncommitted_metadata_dirty {
                None
            } else {
                previous_metadata
                    .filter(|previous| previous.integrity.meta_hash == previous_meta_hash)
            };
''',
    "non-deferred replay base",
)
text = replace_once(
    text,
    '''            if let Err(commit_err) = result {
                if Self::transaction_error_is_committed(&commit_err) {
                    self.deferred_commit.lock().durable_metadata = Some(meta.clone());
                }
''',
    '''            if let Err(commit_err) = result {
                if Self::transaction_error_is_committed(&commit_err) {
                    self.deferred_commit.lock().raw_uncommitted_metadata_dirty = false;
                }
''',
    "committed error state",
)
text = replace_once(
    text,
    '''            self.deferred_commit.lock().durable_metadata = Some(meta.clone());
            return Ok(());
        }
        let result = journal::append_transaction_checked(
''',
    '''            self.deferred_commit.lock().raw_uncommitted_metadata_dirty = false;
            return Ok(());
        }
        let result = journal::append_transaction_checked(
''',
    "successful non-deferred state",
)
text = replace_once(
    text,
    '''        *meta = recovered;
        recompute_disk_usage_from_metadata(meta);
        self.deferred_commit.lock().durable_metadata = Some(meta.clone());
        Ok(())
''',
    '''        *meta = recovered;
        recompute_disk_usage_from_metadata(meta);
        let mut state = self.deferred_commit.lock();
        state.durable_metadata = Some(meta.clone());
        state.raw_uncommitted_metadata_dirty = false;
        Ok(())
''',
    "recovery state",
)

# Deferred group commits still need their full durable snapshot, but a successful
# group commit also makes any read-side live metadata mutation durable.
text = replace_once(
    text,
    '''            Ok(()) => {
                state.durable_metadata = Some(meta.clone());
                state.dirty_transactions = 0;
''',
    '''            Ok(()) => {
                state.durable_metadata = Some(meta.clone());
                state.raw_uncommitted_metadata_dirty = false;
                state.dirty_transactions = 0;
''',
    "deferred success state",
)
text = replace_once(
    text,
    '''            {
                state.durable_metadata = Some(meta.clone());
                state.dirty_transactions = 0;
''',
    '''            {
                state.durable_metadata = Some(meta.clone());
                state.raw_uncommitted_metadata_dirty = false;
                state.dirty_transactions = 0;
''',
    "deferred committed-error state",
)

mod.write_text(text)

namespace = Path("src/volume/namespace.rs")
ns = namespace.read_text()
old_read = '''        } else if let Some(live) = meta.inodes.get_mut(&ino) {
            live.access_count = live.access_count.saturating_add(1);
            live.read_bytes = live.read_bytes.saturating_add(data.len() as u64);
            live.last_accessed_at = now_f64();
            live.workload_score = live.workload_score * 0.98 + 1.0;
        }
'''
new_read = '''        } else {
            if meta.backend != BackendKind::Host {
                self.deferred_commit.lock().raw_uncommitted_metadata_dirty = true;
            }
            if let Some(live) = meta.inodes.get_mut(&ino) {
                live.access_count = live.access_count.saturating_add(1);
                live.read_bytes = live.read_bytes.saturating_add(data.len() as u64);
                live.last_accessed_at = now_f64();
                live.workload_score = live.workload_score * 0.98 + 1.0;
            }
        }
'''
ns = replace_once(ns, old_read, new_read, "read-side dirty marker")
namespace.write_text(ns)

test = Path("tests/raw_journal_durable_base_regression.rs")
t = test.read_text()
if "use argosfs::journal;" not in t:
    t = t.replace(
        "use argosfs::types::{Compression, VolumeConfig};\n",
        "use argosfs::journal;\nuse argosfs::types::{Compression, VolumeConfig};\n",
        1,
    )
name = "raw_sync_after_read_refreshes_integrity_and_dirty_state"
if name not in t:
    t += r'''

#[test]
fn raw_sync_after_read_refreshes_integrity_and_dirty_state() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("disk-sync.img");
    let images = vec![image];
    let fs = ArgosFs::create_loop(
        &images,
        config(),
        32 * 1024 * 1024,
        "read-sync-regression",
        false,
    )
    .unwrap();

    fs.write_file("/seed", b"seed payload", 0o644).unwrap();
    assert_eq!(fs.read_file("/seed", false).unwrap(), b"seed payload");
    fs.sync().unwrap();

    let synced = fs.metadata_snapshot();
    assert_eq!(
        synced.integrity.meta_hash,
        journal::canonical_metadata_hash(&synced).unwrap()
    );

    fs.write_file("/after-sync", b"must survive replay", 0o644)
        .unwrap();
    let committed_txid = fs.metadata_snapshot().txid;
    std::mem::forget(fs);

    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        reopened.read_file("/after-sync", false).unwrap(),
        b"must survive replay"
    );
    assert_eq!(reopened.metadata_snapshot().txid, committed_txid);
}
'''

test.write_text(t)
