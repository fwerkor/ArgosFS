use super::*;
use crate::types::VolumeConfig;
use crate::volume::ArgosFs;
use std::sync::{Mutex, OnceLock};
use tempfile::tempdir;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
}

fn volume() -> (tempfile::TempDir, ArgosFs) {
    let dir = tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    (dir, fs)
}

#[test]
fn thread_guards_restore_crash_checkpoint_and_compaction_settings() {
    let _guard = env_lock();
    std::env::remove_var("ARGOSFS_CRASH_POINT");
    std::env::remove_var(CHECKPOINT_INTERVAL_ENV);
    std::env::remove_var(JOURNAL_COMPACTION_ENV);
    set_thread_crash_point(None);
    assert!(inject_crash("point").is_ok());
    {
        let _outer = thread_crash_point("other, point");
        assert!(matches!(
            inject_crash("point"),
            Err(ArgosError::InjectedCrash(_))
        ));
        {
            let _inner = thread_crash_point("all");
            assert!(inject_crash("anything").is_err());
        }
        assert!(inject_crash("anything").is_ok());
    }
    assert!(inject_crash("point").is_ok());

    assert_eq!(
        checkpoint_interval_txids(),
        DEFAULT_CHECKPOINT_INTERVAL_TXIDS
    );
    {
        let _outer = thread_checkpoint_interval(0);
        assert_eq!(checkpoint_interval_txids(), 1);
        {
            let _inner = thread_checkpoint_interval(7);
            assert_eq!(checkpoint_interval_txids(), 7);
        }
        assert_eq!(checkpoint_interval_txids(), 1);
    }
    std::env::set_var(CHECKPOINT_INTERVAL_ENV, "9");
    assert_eq!(checkpoint_interval_txids(), 9);
    std::env::set_var(CHECKPOINT_INTERVAL_ENV, "0");
    assert_eq!(
        checkpoint_interval_txids(),
        DEFAULT_CHECKPOINT_INTERVAL_TXIDS
    );
    std::env::set_var(CHECKPOINT_INTERVAL_ENV, "invalid");
    assert_eq!(
        checkpoint_interval_txids(),
        DEFAULT_CHECKPOINT_INTERVAL_TXIDS
    );

    assert!(!journal_compaction_disabled());
    {
        let _outer = thread_journal_compaction_disabled(true);
        assert!(journal_compaction_disabled());
        {
            let _inner = thread_journal_compaction_disabled(false);
            assert!(!journal_compaction_disabled());
        }
        assert!(journal_compaction_disabled());
    }
    for value in ["1", "true", "yes", "on"] {
        std::env::set_var(JOURNAL_COMPACTION_ENV, value);
        assert!(journal_compaction_disabled());
    }
    std::env::set_var(JOURNAL_COMPACTION_ENV, "false");
    assert!(!journal_compaction_disabled());
    std::env::remove_var("ARGOSFS_CRASH_POINT");
    std::env::remove_var(CHECKPOINT_INTERVAL_ENV);
    std::env::remove_var(JOURNAL_COMPACTION_ENV);
}

#[test]
fn json_pointer_helpers_cover_object_array_root_and_invalid_paths() {
    let mut value = json!({"object": {"a/b": 1, "til~de": 2}, "array": [10, 20], "scalar": 1});
    assert_eq!(push_json_pointer("", "a/b~c"), "/a~1b~0c");
    assert_eq!(json_pointer_tokens("/a~1b~0c").unwrap(), ["a/b~c"]);
    assert_eq!(json_pointer_from_tokens(&["a/b~c".to_string()]), "/a~1b~0c");
    assert!(json_pointer_tokens("not-a-pointer").is_err());
    assert!(get_json_pointer_mut(&mut value, "").is_some());
    assert!(get_json_pointer_mut(&mut value, "/missing").is_none());

    set_json_pointer(&mut value, "/object/new", json!(3)).unwrap();
    set_json_pointer(&mut value, "/array/2", json!(30)).unwrap();
    set_json_pointer(&mut value, "/array/1", json!(21)).unwrap();
    assert_eq!(value["object"]["new"], 3);
    assert_eq!(value["array"], json!([10, 21, 30]));
    assert!(set_json_pointer(&mut value, "/array/bad", json!(0)).is_err());
    assert!(set_json_pointer(&mut value, "/array/99", json!(0)).is_err());
    assert!(set_json_pointer(&mut value, "/missing/child", json!(0)).is_err());
    assert!(set_json_pointer(&mut value, "/scalar/child", json!(0)).is_err());

    remove_json_pointer(&mut value, "/object/new").unwrap();
    remove_json_pointer(&mut value, "/array/0").unwrap();
    assert!(value["object"].get("new").is_none());
    assert_eq!(value["array"], json!([21, 30]));
    assert!(remove_json_pointer(&mut value, "/object/missing").is_err());
    assert!(remove_json_pointer(&mut value, "/array/bad").is_err());
    assert!(remove_json_pointer(&mut value, "/array/99").is_err());
    assert!(remove_json_pointer(&mut value, "/missing/child").is_err());
    assert!(remove_json_pointer(&mut value, "/scalar/child").is_err());

    let mut root = json!({"a": 1});
    set_json_pointer(&mut root, "", json!([1, 2])).unwrap();
    assert_eq!(root, json!([1, 2]));
    remove_json_pointer(&mut root, "").unwrap();
    assert!(root.is_null());
}

#[test]
fn delta_ops_apply_add_replace_remove_and_report_missing_targets() {
    let mut value = json!({"object": {"old": 1}, "array": [1, 2]});
    apply_delta_op(
        &mut value,
        &MetadataDeltaOp::Add {
            path: "/object/new".to_string(),
            value: json!(2),
        },
    )
    .unwrap();
    apply_delta_op(
        &mut value,
        &MetadataDeltaOp::Replace {
            path: "/object/old".to_string(),
            value: json!(3),
        },
    )
    .unwrap();
    apply_delta_op(
        &mut value,
        &MetadataDeltaOp::Remove {
            path: "/array/0".to_string(),
        },
    )
    .unwrap();
    assert_eq!(value, json!({"object": {"old": 3, "new": 2}, "array": [2]}));
    assert!(apply_delta_op(
        &mut value,
        &MetadataDeltaOp::Replace {
            path: "/missing".to_string(),
            value: json!(0),
        }
    )
    .is_err());
    apply_delta_op(
        &mut value,
        &MetadataDeltaOp::Replace {
            path: String::new(),
            value: json!("root"),
        },
    )
    .unwrap();
    assert_eq!(value, json!("root"));
}

#[test]
fn metadata_delta_round_trip_handles_add_remove_replace_and_escaped_keys() {
    let (_dir, fs) = volume();
    let previous = fs.metadata_snapshot();
    let mut next = previous.clone();
    next.txid += 1;
    next.config.fsname = "changed".to_string();
    let mut escaped_disk = next.disks.values().next().unwrap().clone();
    escaped_disk.id = "a/b~c".to_string();
    next.disks.insert("a/b~c".to_string(), escaped_disk);
    prepare_metadata_integrity_with_previous(&mut next, previous.integrity.meta_hash.clone())
        .unwrap();
    let delta = metadata_delta(&previous, &next).unwrap();
    assert!(!delta.is_empty());
    assert!(delta.iter().any(|op| match op {
        MetadataDeltaOp::Add { path, .. }
        | MetadataDeltaOp::Remove { path }
        | MetadataDeltaOp::Replace { path, .. } => path.contains("a~1b~0c"),
    }));
    let applied = apply_metadata_delta(&previous, &delta).unwrap();
    assert_eq!(applied.txid, next.txid);
    assert_eq!(applied.config.fsname, "changed");
    assert_eq!(applied.disks["a/b~c"].id, "a/b~c");
    assert_eq!(
        canonical_metadata_hash(&applied).unwrap(),
        canonical_metadata_hash(&next).unwrap()
    );

    assert!(apply_metadata_delta(
        &previous,
        &[MetadataDeltaOp::Replace {
            path: "/missing".to_string(),
            value: json!(1),
        }]
    )
    .is_err());
}

#[test]
fn cow_replay_matches_legacy_json_pointer_semantics() {
    let (_dir, fs) = volume();
    fs.write_file("/sample", b"one block", 0o644).unwrap();
    let mut previous = fs.metadata_snapshot();
    // With no stored hash, both readers must derive integrity identically.
    previous.integrity.meta_hash.clear();
    let ino = previous.inodes[&1].entries["sample"];
    let mut next = previous.clone();
    next.txid += 1;
    next.config.fsname = "replay-diff".to_owned();
    next.inodes.get_mut(&ino).unwrap().mode = 0o600;
    next.inodes.get_mut(&ino).unwrap().blocks[0].shards[0].sha256 = "nested-changed".into();
    next.inodes
        .get_mut(&1)
        .unwrap()
        .entries
        .insert("a/b~c".into(), ino);
    let mut added = next.inodes[&ino].clone();
    added.id = 50_000;
    next.inodes.insert(50_000, added);
    next.next_inode = 50_001;
    let disk_key = next.disks.keys().next().unwrap().clone();
    next.disks.get_mut(&disk_key).unwrap().used_bytes += 1;
    let ops = metadata_delta(&previous, &next).unwrap();
    assert!(ops.iter().any(|op| delta_path(op).starts_with("/inodes/")));
    let cow = apply_metadata_delta(&previous, &ops).unwrap();
    let legacy = apply_metadata_delta_legacy(&previous, &ops).unwrap();
    assert_eq!(
        serde_json::to_value(&cow).unwrap(),
        serde_json::to_value(&legacy).unwrap()
    );
    assert_eq!(cow.inodes[&ino].mode, 0o600);
    assert_eq!(cow.inodes[&1].entries["a/b~c"], ino);
    assert_eq!(cow.inodes[&50_000].id, 50_000);

    // Old writers can replace the entire inode index; use the fallback.
    let whole = vec![MetadataDeltaOp::Replace {
        path: "/inodes".to_owned(),
        value: serde_json::to_value(&next.inodes).unwrap(),
    }];
    let cow = apply_metadata_delta(&previous, &whole).unwrap();
    let legacy = apply_metadata_delta_legacy(&previous, &whole).unwrap();
    assert_eq!(
        serde_json::to_value(cow).unwrap(),
        serde_json::to_value(legacy).unwrap()
    );
}

#[test]
fn batched_directory_replay_matches_legacy_for_many_interleaved_changes() {
    let (_dir, fs) = volume();
    fs.write_file("/data", b"data", 0o644).unwrap();
    let mut previous = fs.metadata_snapshot();
    let file_id = previous.inodes[&1].entries["data"];
    previous.integrity.meta_hash.clear();
    let root = previous.inodes.get_mut(&1).unwrap();
    for i in 0..8_000 {
        root.entries.insert(format!("existing-{i:05}"), file_id);
    }
    previous.inodes.mark_committed();

    let mut ops = Vec::new();
    for i in 0..900 {
        ops.push(MetadataDeltaOp::Add {
            path: format!("/inodes/1/entries/new-{i:05}"),
            value: json!(file_id),
        });
    }
    for i in 0..400 {
        ops.push(MetadataDeltaOp::Remove {
            path: format!("/inodes/1/entries/existing-{i:05}"),
        });
    }
    // Interleaving a different inode, a disk, and header data flushes the
    // pending directory without reordering any previously applied changes.
    ops.push(MetadataDeltaOp::Replace {
        path: format!("/inodes/{file_id}/mode"),
        value: json!(0o600),
    });
    let disk_key = previous.disks.keys().next().unwrap();
    let disk_bytes = previous.disks[disk_key].used_bytes;
    for increment in [10, 20] {
        ops.push(MetadataDeltaOp::Replace {
            path: push_json_pointer("/disks", disk_key) + "/used_bytes",
            value: json!(disk_bytes + increment),
        });
    }
    ops.push(MetadataDeltaOp::Replace {
        path: "/txid".to_owned(),
        value: json!(previous.txid + 1),
    });
    for i in 900..1_200 {
        ops.push(MetadataDeltaOp::Add {
            path: format!("/inodes/1/entries/new-{i:05}"),
            value: json!(file_id),
        });
    }
    let batched = apply_metadata_delta(&previous, &ops).unwrap();
    let legacy = apply_metadata_delta_legacy(&previous, &ops).unwrap();
    assert_eq!(
        serde_json::to_value(&batched).unwrap(),
        serde_json::to_value(&legacy).unwrap()
    );
    assert_eq!(batched.inodes[&1].entries.len(), 8_001 + 1_200 - 400);
    assert_eq!(batched.inodes[&file_id].mode, 0o600);
    assert_eq!(batched.disks[disk_key].used_bytes, disk_bytes + 20);
    assert!(batched.inodes[&1].entries.contains_key("new-01199"));
    assert!(!batched.inodes[&1].entries.contains_key("existing-00000"));
}

#[test]
fn batched_replay_preserves_whole_inode_operation_order_and_validation() {
    let (_dir, fs) = volume();
    let mut previous = fs.metadata_snapshot();
    previous.integrity.meta_hash.clear();
    let base_root = previous.inodes[&1].clone();
    let mut replacement = base_root.clone();
    replacement.entries.insert("replacement".into(), 1);
    let ops = vec![
        MetadataDeltaOp::Add {
            path: "/inodes/1/entries/temporary".into(),
            value: json!(1),
        },
        MetadataDeltaOp::Replace {
            path: "/inodes/1".into(),
            value: serde_json::to_value(&replacement).unwrap(),
        },
        MetadataDeltaOp::Add {
            path: "/inodes/1/entries/final".into(),
            value: json!(1),
        },
    ];
    let batched = apply_metadata_delta(&previous, &ops).unwrap();
    let legacy = apply_metadata_delta_legacy(&previous, &ops).unwrap();
    assert_eq!(
        serde_json::to_value(&batched).unwrap(),
        serde_json::to_value(&legacy).unwrap()
    );
    assert!(!batched.inodes[&1].entries.contains_key("temporary"));
    assert!(batched.inodes[&1].entries.contains_key("replacement"));
    assert!(batched.inodes[&1].entries.contains_key("final"));

    let invalid = vec![
        MetadataDeltaOp::Add {
            path: "/inodes/1/entries/valid".into(),
            value: json!(1),
        },
        MetadataDeltaOp::Remove {
            path: "/inodes/1/entries/missing".into(),
        },
    ];
    assert!(apply_metadata_delta(&previous, &invalid).is_err());
    assert_eq!(previous.inodes[&1].entries, base_root.entries);
}

#[test]
fn initialized_volume_scans_appends_events_transactions_and_conflicts() {
    let (dir, fs) = volume();
    let root = dir.path();
    let initial = scan(root).unwrap();
    assert_eq!(initial.valid_entries, 1);
    assert_eq!(initial.latest_snapshot_txid, fs.metadata_snapshot().txid);
    assert!(has_valid_checkpoint(root).unwrap());
    assert!(!should_write_checkpoint(root, 1).unwrap());

    let mut meta = fs.metadata_snapshot();
    append_event(root, &meta, "audit", json!({"ok": true})).unwrap();
    let after_event = scan(root).unwrap();
    assert_eq!(after_event.valid_entries, 2);
    assert_eq!(after_event.last_valid_txid, meta.txid);

    meta.txid += 1;
    prepare_metadata_integrity_with_previous(
        &mut meta,
        fs.metadata_snapshot().integrity.meta_hash.clone(),
    )
    .unwrap();
    let _interval = thread_checkpoint_interval(2);
    let _compact = thread_journal_compaction_disabled(true);
    let txid = meta.txid;
    append_transaction(root, &mut meta, "update", json!({"txid": txid})).unwrap();
    let report = scan(root).unwrap();
    assert_eq!(report.last_valid_txid, meta.txid);
    assert!(report.valid_entries >= 3);
    assert!(should_write_checkpoint(root, 2).unwrap());

    let mut stale = meta.clone();
    stale.txid += 1;
    assert!(matches!(
        append_transaction_checked(root, &mut stale, Some(0), "stale", json!({})),
        Err(ArgosError::Conflict(_))
    ));
}

#[test]
fn scan_counts_invalid_json_records_hashes_chains_snapshots_and_legacy_lines() {
    let (dir, fs) = volume();
    let root = dir.path();
    let mut meta = fs.metadata_snapshot();
    meta.txid += 1;
    prepare_metadata_integrity_with_previous(
        &mut meta,
        fs.metadata_snapshot().integrity.meta_hash.clone(),
    )
    .unwrap();
    let valid = build_record(
        &meta,
        BuildRecordOptions {
            record_type: JournalRecordKind::Checkpoint,
            action: "valid".to_string(),
            details: json!({}),
            previous_record_hash: String::new(),
            time: 1.0,
            metadata: Some(meta.clone()),
            metadata_delta: None,
        },
    )
    .unwrap();
    let mut bad_hash = valid.clone();
    bad_hash.record_hash = "bad".to_string();
    let mut bad_chain = valid.clone();
    bad_chain.previous_record_hash = "wrong".to_string();
    bad_chain.record_hash = record_hash(&bad_chain).unwrap();
    let mut bad_snapshot = valid.clone();
    bad_snapshot.meta_hash = "wrong".to_string();
    bad_snapshot.record_hash = record_hash(&bad_snapshot).unwrap();
    let legacy = JournalRecord {
        txid: 99,
        action: "legacy".to_string(),
        ..JournalRecord::default()
    };
    let journal = journal_path(root);
    let mut bytes = b"not-json\n".to_vec();
    bytes.extend_from_slice(b"{\"txid\":\"bad\"}\n");
    for record in [&legacy, &bad_hash, &bad_chain, &bad_snapshot, &valid] {
        serde_json::to_writer(&mut bytes, record).unwrap();
        bytes.push(b'\n');
    }
    atomic_write(&journal, &bytes).unwrap();
    let report = scan(root).unwrap();
    assert_eq!(report.legacy_entries, 1);
    assert_eq!(report.valid_entries, 2);
    assert!(report.invalid_entries >= 4);
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("invalid JSON")));
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("invalid record")));
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("record hash mismatch")));
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("previous record hash mismatch")));
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("metadata snapshot hash mismatch")));
}

#[test]
fn candidate_recovery_repairs_missing_corrupt_and_divergent_copies() {
    let (dir, fs) = volume();
    let root = dir.path();
    let system = root.join(".argosfs");
    let original = fs.metadata_snapshot();
    fs::write(system.join(PRIMARY_META), b"{").unwrap();
    fs::remove_file(system.join(SECONDARY_META)).unwrap();
    let recovered = load_or_recover(root).unwrap();
    assert_eq!(recovered.metadata.txid, original.txid);
    assert!(recovered
        .report
        .metadata_candidates
        .iter()
        .any(|item| !item.valid));
    assert!(recovered
        .report
        .metadata_candidates
        .iter()
        .any(|item| !item.present));
    assert!(recovered.report.double_write_mismatches > 0);
    for (_, path) in metadata_paths(root) {
        assert!(read_candidate("copy".to_string(), path).report.valid);
    }

    let mut divergent = original.clone();
    divergent.txid += 1;
    prepare_metadata_integrity_with_previous(&mut divergent, original.integrity.meta_hash.clone())
        .unwrap();
    atomic_write(
        &system.join(PRIMARY_META),
        &serde_json::to_vec_pretty(&divergent).unwrap(),
    )
    .unwrap();
    let candidates = read_metadata_candidates(root);
    assert!(double_write_mismatches(&candidates) > 0);
}

#[test]
fn validation_hash_helpers_and_metadata_order_cover_legacy_and_mismatch_paths() {
    let (_dir, fs) = volume();
    let mut meta = fs.metadata_snapshot();
    let hash = canonical_metadata_hash(&meta).unwrap();
    assert_eq!(
        canonical_metadata_hash_bytes(&serde_json::to_vec(&meta).unwrap()).unwrap(),
        hash
    );
    assert_eq!(
        canonical_metadata_hash_value(&serde_json::to_value(&meta).unwrap()).unwrap(),
        hash
    );
    assert_eq!(
        metadata_order(&meta),
        (meta.txid, meta.integrity.generation)
    );

    let mut legacy = meta.clone();
    legacy.integrity = MetadataIntegrity::default();
    let legacy_hash = canonical_metadata_hash(&legacy).unwrap();
    validate_metadata(&mut legacy, &legacy_hash).unwrap();
    assert_eq!(legacy.integrity.meta_hash, legacy_hash);
    assert_eq!(legacy.integrity.generation, legacy.txid);

    meta.format = "unsupported-format".to_string();
    assert!(validate_metadata(&mut meta, &hash).is_err());
    meta.format = FORMAT_VERSION.to_string();
    meta.integrity.meta_hash = "wrong".to_string();
    assert!(validate_metadata(&mut meta, &hash).is_err());

    let mut empty = fs.metadata_snapshot();
    empty.integrity = MetadataIntegrity::default();
    ensure_integrity(&mut empty).unwrap();
    assert!(!empty.integrity.meta_hash.is_empty());
}

#[test]
fn default_trend_health_fields_do_not_change_legacy_metadata_hashes() {
    let (_dir, fs) = volume();
    let mut meta = fs.metadata_snapshot();
    for disk in meta.disks.values_mut() {
        disk.health.smart_status_failed = false;
        disk.health.smart_evidence_score = 0.0;
        disk.health.smart_evidence_updated_at = 0.0;
        disk.health.recent_reallocated_delta = 0;
        disk.health.recent_reallocated_delta_at = 0.0;
        disk.health.recent_crc_delta = 0;
        disk.health.recent_crc_delta_at = 0.0;
        disk.health.recent_io_error_delta = 0;
        disk.health.recent_io_error_delta_at = 0.0;
    }

    let value = serde_json::to_value(&meta).unwrap();
    for disk in value["disks"].as_object().unwrap().values() {
        let health = disk["health"].as_object().unwrap();
        for field in [
            "smart_status_failed",
            "smart_evidence_score",
            "smart_evidence_updated_at",
            "recent_reallocated_delta",
            "recent_reallocated_delta_at",
            "recent_crc_delta",
            "recent_crc_delta_at",
            "recent_io_error_delta",
            "recent_io_error_delta_at",
        ] {
            assert!(
                !health.contains_key(field),
                "legacy hash field leaked: {field}"
            );
        }
    }

    let hash = canonical_metadata_hash(&meta).unwrap();
    meta.integrity.meta_hash = hash.clone();
    let bytes = serde_json::to_vec_pretty(&meta).unwrap();
    let computed = canonical_metadata_hash_bytes(&bytes).unwrap();
    assert_eq!(computed, hash);
    validate_metadata(&mut meta, &computed).unwrap();
}

#[test]
fn compaction_handles_missing_disabled_no_checkpoint_and_valid_checkpoint_chains() {
    let empty = tempdir().unwrap();
    compact_journal(empty.path()).unwrap();

    let (dir, fs) = volume();
    let root = dir.path();
    let original = fs::read_to_string(journal_path(root)).unwrap();
    {
        let _disabled = thread_journal_compaction_disabled(true);
        compact_journal(root).unwrap();
    }
    assert_eq!(fs::read_to_string(journal_path(root)).unwrap(), original);

    let mut event_only = fs.metadata_snapshot();
    let event = build_record(
        &event_only,
        BuildRecordOptions {
            record_type: JournalRecordKind::Event,
            action: "event".to_string(),
            details: json!({}),
            previous_record_hash: String::new(),
            time: 1.0,
            metadata: None,
            metadata_delta: None,
        },
    )
    .unwrap();
    let no_checkpoint = tempdir().unwrap();
    fs::create_dir_all(no_checkpoint.path().join(".argosfs")).unwrap();
    append_record(no_checkpoint.path(), &event).unwrap();
    compact_journal(no_checkpoint.path()).unwrap();
    assert_eq!(
        valid_journal_chain_records(
            &fs::read_to_string(journal_path(no_checkpoint.path())).unwrap()
        )
        .unwrap()
        .len(),
        1
    );

    event_only.txid += 1;
    prepare_metadata_integrity_with_previous(
        &mut event_only,
        fs.metadata_snapshot().integrity.meta_hash.clone(),
    )
    .unwrap();
    append_event(root, &event_only, "tail", json!({})).unwrap();
    compact_journal(root).unwrap();
    let records =
        valid_journal_chain_records(&fs::read_to_string(journal_path(root)).unwrap()).unwrap();
    assert!(!records.is_empty());
    assert!(records[0].record_type.is_checkpoint());
    assert!(records[0].previous_record_hash.is_empty());
}

#[test]
fn valid_chain_filter_skips_blank_malformed_legacy_bad_hash_and_bad_links() {
    let (_dir, fs) = volume();
    let meta = fs.metadata_snapshot();
    let first = build_record(
        &meta,
        BuildRecordOptions {
            record_type: JournalRecordKind::Checkpoint,
            action: "first".to_string(),
            details: json!({}),
            previous_record_hash: String::new(),
            time: 1.0,
            metadata: Some(meta.clone()),
            metadata_delta: None,
        },
    )
    .unwrap();
    let second = build_record(
        &meta,
        BuildRecordOptions {
            record_type: JournalRecordKind::Event,
            action: "second".to_string(),
            details: json!({}),
            previous_record_hash: first.record_hash.clone(),
            time: 2.0,
            metadata: None,
            metadata_delta: None,
        },
    )
    .unwrap();
    let mut bad = second.clone();
    bad.record_hash = "bad".to_string();
    let mut wrong_link = second.clone();
    wrong_link.previous_record_hash = "wrong".to_string();
    wrong_link.record_hash = record_hash(&wrong_link).unwrap();
    let legacy = JournalRecord::default();
    let mut text = "\nnot-json\n[]\n".to_string();
    for record in [&legacy, &bad, &wrong_link, &first, &second] {
        text.push_str(&serde_json::to_string(record).unwrap());
        text.push('\n');
    }
    let records = valid_journal_chain_records(&text).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].action, "first");
    assert_eq!(records[1].action, "second");
}

#[test]
fn incremental_delta_matches_full_json_diff_for_mixed_namespace_changes() {
    let (_dir, fs) = volume();
    fs.write_file("/removed", b"x", 0o644).unwrap();
    fs.write_file("/kept", b"xyz", 0o644).unwrap();
    let previous = fs.metadata_snapshot();
    let mut next = previous.clone();

    let removed_id = *next.inodes[&1].entries.get("removed").unwrap();
    let kept_id = *next.inodes[&1].entries.get("kept").unwrap();
    next.inodes.remove(&removed_id);
    next.inodes.get_mut(&1).unwrap().entries.remove("removed");
    next.inodes
        .get_mut(&1)
        .unwrap()
        .entries
        .insert("new/name~".into(), kept_id + 100);
    let mut new_inode = next.inodes[&kept_id].clone();
    new_inode.id = kept_id + 100;
    new_inode.blocks.clear();
    new_inode.size = 0;
    next.inodes.insert(new_inode.id, new_inode);
    next.inodes.get_mut(&kept_id).unwrap().mode = 0o100600;
    next.config.fsname = "updated".into();
    next.txid += 1;
    next.disks.values_mut().next().unwrap().used_bytes += 1;

    let mut expected = Vec::new();
    diff_json(
        "",
        &serde_json::to_value(&previous).unwrap(),
        &serde_json::to_value(&next).unwrap(),
        &mut expected,
    );
    let actual = metadata_delta(&previous, &next).unwrap();
    // Op order can differ across independent subtrees; exact effects cannot.
    let mut expected_value = serde_json::to_value(&previous).unwrap();
    let mut actual_value = expected_value.clone();
    for op in &expected {
        apply_delta_op(&mut expected_value, op).unwrap();
    }
    for op in &actual {
        apply_delta_op(&mut actual_value, op).unwrap();
    }
    assert_eq!(actual_value, expected_value);
    assert_eq!(actual_value, serde_json::to_value(&next).unwrap());
}

#[test]
fn persistent_inode_diff_preserves_wire_identity_and_signed_zero_changes() {
    let (_dir, fs) = volume();
    let mut previous = fs.metadata_snapshot();
    let template = previous.inodes[&1].clone();
    for ino in 2..10_002 {
        let mut file = template.clone();
        file.id = ino;
        file.kind = crate::types::NodeKind::File;
        file.entries.clear();
        previous.inodes.insert(ino, file);
    }
    let mut next = previous.clone();
    // A COW fork must keep the old snapshot isolated even when the mutation
    // runs inside a path-sharing persistent B-tree.
    next.inodes.get_mut(&2).unwrap().workload_score = -0.0;
    next.inodes.get_mut(&3).unwrap().size += 1;
    next.inodes.remove(&4);
    next.txid += 1;
    assert_eq!(previous.inodes[&3].size, 0);
    assert!(previous.inodes.contains_key(&4));

    let delta = metadata_delta(&previous, &next).unwrap();
    let inode_ops = delta
        .iter()
        .filter(|op| match op {
            MetadataDeltaOp::Add { path, .. }
            | MetadataDeltaOp::Remove { path }
            | MetadataDeltaOp::Replace { path, .. } => path.starts_with("/inodes/"),
        })
        .collect::<Vec<_>>();
    assert_eq!(inode_ops.len(), 3);
    assert!(inode_ops
        .iter()
        .any(|op| matches!(op, MetadataDeltaOp::Remove { path } if path == "/inodes/4")));
    assert!(inode_ops.iter().any(|op| matches!(op, MetadataDeltaOp::Replace { path, .. } if path == "/inodes/2/workload_score")));

    // Compare exact canonical JSON bytes with the previous full JSON approach.
    let mut value = serde_json::to_value(&previous).unwrap();
    for op in &delta {
        apply_delta_op(&mut value, op).unwrap();
    }
    assert_eq!(value, serde_json::to_value(&next).unwrap());
    let canonical_btree_json = serde_json::to_string_pretty(
        &previous
            .inodes
            .iter()
            .map(|(key, value)| (*key, value))
            .collect::<BTreeMap<_, _>>(),
    )
    .unwrap();
    assert_eq!(
        canonical_btree_json,
        serde_json::to_string_pretty(&previous.inodes).unwrap()
    );
}

/// Explicit, opt-in memory stress regression for issue #212. This avoids
/// writing hundreds of thousands of disposable files while exercising the
/// actual inode and eight-shard metadata types used by the production mount.
#[test]
#[ignore = "run manually with cargo test --lib metadata_scale_274k -- --ignored --nocapture"]
fn metadata_scale_274k_eight_shard_inodes() {
    fn rss_mib() -> f64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let rss = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        rss as f64 / 1024.0
    }
    let (dir, fs) = volume();
    fs.write_file("/sample-8-shards", b"a real one-block sample", 0o644)
        .unwrap();
    let mut meta = fs.metadata_snapshot();
    let file_id = meta.inodes[&1].entries["sample-8-shards"];
    let mut sample = meta.inodes[&file_id].clone();
    let template_shard = sample.blocks[0].shards[0].clone();
    sample.blocks[0].shards = (0..8)
        .map(|slot| {
            let mut shard = template_shard.clone();
            shard.slot = slot;
            shard
        })
        .collect();
    let count = std::env::var("ARGOSFS_SCALE_INODES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(274_103);
    let initial_rss = rss_mib();
    let varied_shards = std::env::var_os("ARGOSFS_SCALE_UNIQUE_SHARDS").is_some();
    let started = std::time::Instant::now();
    for ino in 3..count + 3 {
        let mut file = sample.clone();
        file.id = ino;
        if varied_shards {
            for block in &mut file.blocks {
                block.stripe_id = format!("s{ino:016x}");
                for shard in &mut block.shards {
                    let slot = shard.slot;
                    let digest = blake3::hash(format!("{ino}:{slot}").as_bytes())
                        .to_hex()
                        .to_string();
                    shard.sha256 = digest.clone();
                    shard.subblock_sha256 = vec![digest];
                    let relpath = std::path::PathBuf::from(format!(
                        "shards/{:02x}/s{ino:016x}.{slot:03}.blk",
                        ino % 256,
                    ));
                    shard.relpath = relpath.clone();
                    if let Some(crate::types::ShardLocation::HostPath {
                        relpath: location_path,
                        ..
                    }) = &mut shard.location
                    {
                        *location_path = relpath;
                    }
                }
            }
        }
        meta.inodes.insert(ino, file);
        if ino % 50_000 == 0 {
            eprintln!(
                "inodes={ino} RSS={:.1} MiB elapsed={:.1}s",
                rss_mib(),
                started.elapsed().as_secs_f64()
            );
        }
    }
    let built_rss = rss_mib();
    struct ByteCounter(u64);
    impl std::io::Write for ByteCounter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    if std::env::var_os("ARGOSFS_SCALE_DELTA_INTEGRITY_BENCH").is_some() {
        // Benchmark O(changed) without also serializing gigabytes of an
        // unrelated checkpoint. This is a synthetic, uncommitted fixture.
    } else if std::env::var_os("ARGOSFS_SCALE_COMPRESS_CHECKPOINT").is_some() {
        // Opt-in: allocate a whole JSON checkpoint and compress it exactly
        // as the raw writer does once its original slot is too small.
        let checkpoint = serde_json::to_vec(&meta).unwrap();
        let compressed = zstd::stream::encode_all(checkpoint.as_slice(), 3).unwrap();
        eprintln!(
            "checkpoint_json_mib={:.1} checkpoint_zstd_mib={:.1} compression_ratio={:.2} unique_shards={}",
            checkpoint.len() as f64 / (1024.0 * 1024.0),
            compressed.len() as f64 / (1024.0 * 1024.0),
            checkpoint.len() as f64 / compressed.len() as f64,
            varied_shards,
        );
        if varied_shards {
            assert!(
                compressed.len() < 512 * 1024 * 1024,
                "representative 274k-file checkpoint must fit a 512 MiB raw metadata slot"
            );
        }
    } else {
        let mut checkpoint = ByteCounter(0);
        serde_json::to_writer(&mut checkpoint, &meta).unwrap();
        eprintln!(
            "checkpoint_json_bytes={} checkpoint_json_mib={:.1}",
            checkpoint.0,
            checkpoint.0 as f64 / (1024.0 * 1024.0)
        );
    }
    meta.inodes.mark_committed();
    let snapshot = meta.clone();
    let cloned_rss = rss_mib();
    // Clone and mutate only a handful of entries, as in a normal journal commit.
    for ino in [3, count / 2, count + 2] {
        meta.inodes.get_mut(&ino).unwrap().size += 1;
    }
    let ops = metadata_delta(&snapshot, &meta).unwrap();
    if std::env::var_os("ARGOSFS_SCALE_DELTA_INTEGRITY_BENCH").is_some() {
        let mut next = meta.clone();
        next.txid += 1;
        next.updated_at += 1.0;
        let timer = std::time::Instant::now();
        prepare_delta_integrity(&mut next, &snapshot).unwrap();
        let hash_ms = timer.elapsed().as_secs_f64() * 1000.0;
        let timer = std::time::Instant::now();
        let replayed =
            apply_metadata_delta(&snapshot, &metadata_delta(&snapshot, &next).unwrap()).unwrap();
        let replay_ms = timer.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(replayed.integrity.meta_hash, next.integrity.meta_hash);
        assert_eq!(replayed.inodes.len(), next.inodes.len());
        eprintln!(
            "DELTA_INTEGRITY_BENCH inodes={} shards_per_inode=8 hash_ms={:.3} replay_ms={:.3} ops={} rss_mib={:.1}",
            count, hash_ms, replay_ms, ops.len(), rss_mib()
        );
    }
    let mutated_rss = rss_mib();
    eprintln!(
        "METADATA_SCALE inodes={} shards_per_inode=8 initial_rss_mib={:.1} built_rss_mib={:.1} cloned_rss_mib={:.1} mutated_rss_mib={:.1} \
         elapsed_sec={:.1} delta_ops={} relative_build_mib={:.1} snapshot_growth_mib={:.1}",
        count, initial_rss, built_rss, cloned_rss, mutated_rss,
        started.elapsed().as_secs_f64(), ops.len(),
        built_rss - initial_rss, cloned_rss - built_rss
    );
    assert_eq!(ops.len(), 3);
    assert_eq!(snapshot.inodes[&3].size + 1, meta.inodes[&3].size);
    if std::env::var_os("ARGOSFS_SCALE_PROFILE_RECOVERY").is_some() {
        // Profile the real deserialization, delta replay and integrity hash at
        // report-size inode counts. Kept opt-in to avoid heavy CI allocations.
        let checkpoint = serde_json::to_vec(&snapshot).unwrap();
        let compressed = zstd::stream::encode_all(checkpoint.as_slice(), 3).unwrap();
        eprintln!(
            "RECOVERY_PROFILE checkpoint={} compressed={} rss_mib={:.1}",
            checkpoint.len(),
            compressed.len(),
            rss_mib()
        );
        let expanded = zstd::stream::decode_all(compressed.as_slice()).unwrap();
        let mut recovered: Metadata = serde_json::from_slice(&expanded).unwrap();
        eprintln!(
            "RECOVERY_PROFILE decoded={} rss_mib={:.1}",
            recovered.inodes.len(),
            rss_mib()
        );
        // Synthetic mutations are not a committed journal record. Clear the
        // stale baseline hash so recovery derives the expected updated hash.
        recovered.integrity.meta_hash.clear();
        let replayed = apply_metadata_delta(&recovered, &ops).unwrap();
        eprintln!(
            "RECOVERY_PROFILE replayed={} rss_mib={:.1}",
            replayed.inodes.len(),
            rss_mib()
        );
        assert_eq!(replayed.inodes[&3].size, meta.inodes[&3].size);
        let hash = canonical_metadata_hash(&replayed).unwrap();
        eprintln!("RECOVERY_PROFILE hash={} rss_mib={:.1}", hash, rss_mib());
    }
    drop((dir, fs));
}

#[test]
fn delta_integrity_v2_binds_changes_and_replays_without_full_hash() {
    let (_dir, fs) = volume();
    let mut base = fs.metadata_snapshot();
    base.inodes.mark_committed();
    let mut next = base.clone();
    next.txid += 1;
    next.updated_at += 1.0;
    next.inodes.get_mut(&1).unwrap().read_bytes += 341;
    prepare_delta_integrity(&mut next, &base).unwrap();
    assert_eq!(next.integrity.hash_scheme, Some(DELTA_INTEGRITY_SCHEME));
    assert_eq!(next.integrity.meta_hash.len(), 64);
    let ops = metadata_delta(&base, &next).unwrap();
    let replayed = apply_metadata_delta(&base, &ops).unwrap();
    assert_eq!(replayed.integrity, next.integrity);
    assert_eq!(replayed.inodes[&1].read_bytes, next.inodes[&1].read_bytes);
    let mut tampered = ops.clone();
    let inode_change = tampered
        .iter_mut()
        .find_map(|op| match op {
            MetadataDeltaOp::Replace { path, value } if path.ends_with("/read_bytes") => {
                Some(value)
            }
            _ => None,
        })
        .unwrap();
    *inode_change = json!(9999999);
    assert!(apply_metadata_delta(&base, &tampered).is_err());
    let mut unrelated = base.clone();
    unrelated.integrity.meta_hash = "f".repeat(64);
    assert!(apply_metadata_delta(&unrelated, &ops).is_err());

    let mut third = next.clone();
    third.inodes.mark_committed();
    third.txid += 1;
    third.updated_at += 2.0;
    third.inodes.get_mut(&1).unwrap().read_bytes += 1;
    prepare_delta_integrity(&mut third, &next).unwrap();
    let third_ops = metadata_delta(&next, &third).unwrap();
    assert_eq!(
        apply_metadata_delta(&next, &third_ops).unwrap().integrity,
        third.integrity
    );
}
