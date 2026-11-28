#![no_main]

mod common;

use argosfs::error::ArgosError;
use argosfs::journal;
use libfuzzer_sys::fuzz_target;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use tempfile::tempdir;

fuzz_target!(|data: &[u8]| {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    fs::create_dir_all(root.join(".argosfs")).expect("create metadata directory");

    let mut current = common::metadata_from(data, 0);
    current.txid = 1;
    current.integrity = Default::default();
    journal::initialize_volume(root, &mut current, 1.0).expect("initialize fuzz journal");
    // The oracle tracks committed states independently of recovered output:
    // a silently rolled-back transaction must not become the next baseline.
    let mut committed_states = vec![current.clone()];
    let mut last_metadata_copy = current.clone();

    let crash_points = [
        "",
        "before-journal",
        "after-journal",
        "after-primary-metadata",
        "after-secondary-metadata",
        "after-compatible-metadata",
    ];
    let steps = 1 + (common::byte(data, 512) as usize % 5);
    for step in 0..steps {
        let mut next = current.clone();
        next.txid = current.txid.saturating_add(1);
        next.updated_at += 1.0 + step as f64;
        next.next_stripe = common::u64_at(data, 520 + step * 8);
        next.raw_pool.pool_name = common::text_at(data, 600 + step * 40, 32, "recovery-pool");
        let point = crash_points[common::byte(data, 900 + step) as usize % crash_points.len()];
        let result = if point.is_empty() {
            journal::append_transaction(
                root,
                &mut next,
                "fuzz-update",
                json!({"step": step, "crash": null}),
            )
        } else {
            let _guard = journal::thread_crash_point(point);
            journal::append_transaction(
                root,
                &mut next,
                "fuzz-update",
                json!({"step": step, "crash": point}),
            )
        };

        if point.is_empty() {
            result.expect("uninterrupted journal append must commit");
        } else {
            match result {
                Err(ArgosError::InjectedCrash(actual)) => assert_eq!(actual, point),
                other => panic!("expected injected crash at {point}, got {other:?}"),
            }
        }

        // BeforeJournal is precommit; all later fault points happen after
        // the journal append is durable. The expected state is derived only
        // from the transaction and fault point, never from recovery output.
        let expected = if point == "before-journal" {
            current.clone()
        } else {
            next
        };
        if point != "before-journal" {
            committed_states.push(expected.clone());
        }
        if point.is_empty()
            || matches!(
                point,
                "after-primary-metadata" | "after-secondary-metadata" | "after-compatible-metadata"
            )
        {
            last_metadata_copy = expected.clone();
        }
        let recovered = journal::load_or_recover(root)
            .expect("every injected journal crash must retain the durable state");
        assert_eq!(
            recovered.metadata.txid, expected.txid,
            "lost committed transaction at {point}"
        );
        assert_eq!(
            serde_json::to_value(&recovered.metadata).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "recovered metadata content differs at {point}"
        );
        current = expected;
    }

    let system = root.join(".argosfs");
    let corruption = common::byte(data, 1024) % 6;
    match corruption {
        0 => {
            let mut file = OpenOptions::new()
                .append(true)
                .open(system.join("journal.jsonl"))
                .unwrap();
            file.write_all(b"{not-json-tail\n").unwrap();
        }
        1 => {
            fs::write(system.join("meta.primary.json"), b"{broken").unwrap();
        }
        2 => {
            fs::write(system.join("meta.secondary.json"), b"[]").unwrap();
        }
        3 => {
            let path = system.join("journal.jsonl");
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let len = file.metadata().unwrap().len();
            if len > 8 {
                file.set_len(len - (1 + common::byte(data, 1025) as u64 % 8))
                    .unwrap();
                file.seek(SeekFrom::End(0)).unwrap();
            }
        }
        4 => {
            let _ = fs::remove_file(system.join("meta.json"));
        }
        _ => {}
    }

    let recovered = journal::load_or_recover(root)
        .expect("single-copy/tail corruption must remain recoverable");
    if corruption != 3 {
        // A malformed tail or one missing/corrupt metadata copy leaves the
        // valid journal and therefore the newest committed state intact.
        assert_eq!(recovered.metadata.txid, current.txid);
        assert_eq!(
            serde_json::to_value(&recovered.metadata).unwrap(),
            serde_json::to_value(&current).unwrap(),
            "single-copy damage must not roll back a committed transaction"
        );
    } else {
        // Deliberately truncating the journal may destroy the sole durable
        // copy of the last append (after-journal crash). An older authenticated
        // committed state is permitted, but not arbitrary partial metadata,
        // and it cannot be older than the last persisted metadata copy.
        assert!(recovered.metadata.txid >= last_metadata_copy.txid);
        assert!(recovered.metadata.txid <= current.txid);
        assert!(
            committed_states.iter().any(|state| {
                state.txid == recovered.metadata.txid
                    && serde_json::to_value(state).unwrap()
                        == serde_json::to_value(&recovered.metadata).unwrap()
            }),
            "truncation recovered a state that was never committed"
        );
    }
    journal::scan(root).expect("recovered journal must remain scannable");
});
