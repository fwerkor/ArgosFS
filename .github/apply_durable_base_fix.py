from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text()
    if text.count(old) != 1:
        raise SystemExit(f"{path}: expected one replacement, found {text.count(old)}")
    file.write_text(text.replace(old, new, 1))


volume = "src/volume/mod.rs"
replace_once(
    volume,
    '''        let previous_meta_hash = if meta.integrity.meta_hash.is_empty() {
            journal::canonical_metadata_hash(meta)?
        } else {
            meta.integrity.meta_hash.clone()
        };
''',
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
)
replace_once(
    volume,
    '''            let replay_previous = match previous_metadata {
                Some(previous) if previous.integrity.meta_hash == previous_meta_hash => {
                    Some(previous)
                }
                _ => None,
            };
''',
    '''            let replay_previous = durable_previous
                .as_ref()
                .filter(|previous| previous.integrity.meta_hash == previous_meta_hash);
''',
)
replace_once(
    volume,
    '''            if let Err(commit_err) = result {
                let should_restore = !Self::transaction_error_is_committed(&commit_err)
                    && (previous_metadata.is_none()
                        || matches!(commit_err, ArgosError::Conflict(_)));
                if should_restore {
                    if let Err(recovery_err) = self.restore_raw_metadata_locked(meta, &superblocks)
                    {
                        return Err(ArgosError::CorruptedMetadata(format!(
                            "raw transaction failed ({commit_err}) and metadata rollback failed ({recovery_err})"
                        )));
                    }
                }
                return Err(commit_err);
            }
            return Ok(());
''',
    '''            if let Err(commit_err) = result {
                if Self::transaction_error_is_committed(&commit_err) {
                    self.deferred_commit.lock().durable_metadata = Some(meta.clone());
                }
                let should_restore = !Self::transaction_error_is_committed(&commit_err)
                    && (previous_metadata.is_none()
                        || matches!(commit_err, ArgosError::Conflict(_)));
                if should_restore {
                    if let Err(recovery_err) = self.restore_raw_metadata_locked(meta, &superblocks)
                    {
                        return Err(ArgosError::CorruptedMetadata(format!(
                            "raw transaction failed ({commit_err}) and metadata rollback failed ({recovery_err})"
                        )));
                    }
                }
                return Err(commit_err);
            }
            self.deferred_commit.lock().durable_metadata = Some(meta.clone());
            return Ok(());
''',
)
replace_once(
    volume,
    '''        *meta = recovered;
        recompute_disk_usage_from_metadata(meta);
        Ok(())
''',
    '''        *meta = recovered;
        recompute_disk_usage_from_metadata(meta);
        self.deferred_commit.lock().durable_metadata = Some(meta.clone());
        Ok(())
''',
)

test = Path("tests/raw_journal_durable_base_regression.rs")
if test.exists():
    raise SystemExit(f"{test}: already exists")
test.write_text(r'''use argosfs::types::{Compression, VolumeConfig};
use argosfs::ArgosFs;
use tempfile::TempDir;

fn config() -> VolumeConfig {
    VolumeConfig {
        k: 1,
        m: 0,
        chunk_size: 1024,
        compression: Compression::None,
        compression_level: 0,
        l2_cache_bytes: 0,
        fsname: "argosfs-durable-base-regression".to_string(),
        ..VolumeConfig::default()
    }
}

#[test]
fn raw_journal_replays_write_after_read_side_metadata_mutation() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("disk.img");
    let images = vec![image];
    let fs = ArgosFs::create_loop(
        &images,
        config(),
        32 * 1024 * 1024,
        "durable-base-regression",
        false,
    )
    .unwrap();

    fs.write_file("/seed", b"seed payload", 0o644).unwrap();
    let seed = fs.resolve_path("/seed", true).unwrap();
    let before = fs.metadata_snapshot().inodes[&seed].access_count;
    assert_eq!(fs.read_file("/seed", false).unwrap(), b"seed payload");
    assert!(fs.metadata_snapshot().inodes[&seed].access_count > before);

    fs.write_file("/after-read", b"must survive replay", 0o644)
        .unwrap();
    let committed_txid = fs.metadata_snapshot().txid;

    // Model power loss: keep the journal newer than the metadata checkpoint and
    // skip ArgosFs::drop's clean-unmount path.
    std::mem::forget(fs);

    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        reopened.read_file("/after-read", false).unwrap(),
        b"must survive replay"
    );
    assert_eq!(reopened.metadata_snapshot().txid, committed_txid);
}
''')
