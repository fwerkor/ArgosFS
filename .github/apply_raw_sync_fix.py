from pathlib import Path

mod = Path("src/volume/mod.rs")
text = mod.read_text()
old = '''            let superblocks = self.active_superblocks_locked(&meta)?;
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
new = '''            let durable_previous = self.deferred_commit.lock().durable_metadata.clone();
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
if new not in text:
    if old not in text:
        raise SystemExit("raw sync block not found")
    mod.write_text(text.replace(old, new, 1))

test = Path("tests/raw_journal_durable_base_regression.rs")
t = test.read_text()
if "use argosfs::journal;" not in t:
    t = t.replace("use argosfs::types::{Compression, VolumeConfig};\n", "use argosfs::journal;\nuse argosfs::types::{Compression, VolumeConfig};\n", 1)
name = "raw_sync_after_read_refreshes_integrity_and_durable_base"
if name not in t:
    t += r'''

#[test]
fn raw_sync_after_read_refreshes_integrity_and_durable_base() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("disk-sync.img");
    let images = vec![image];
    let fs = ArgosFs::create_loop(
        &images,
        config(),
        32 * 1024 * 1024,
        "durable-base-sync-regression",
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
