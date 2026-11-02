#![no_main]

mod common;

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
    let initial_txid = current.txid;
    let mut highest_attempted = current.txid;

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
        highest_attempted = highest_attempted.max(next.txid);

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

        if result.is_ok() {
            current = next;
        } else {
            let recovered = journal::load_or_recover(root)
                .expect("injected journal crash must leave a recoverable state");
            assert!(recovered.metadata.txid >= initial_txid);
            assert!(recovered.metadata.txid <= highest_attempted);
            current = recovered.metadata;
        }
    }

    let system = root.join(".argosfs");
    match common::byte(data, 1024) % 6 {
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
    assert!(recovered.metadata.txid >= initial_txid);
    assert!(recovered.metadata.txid <= highest_attempted);
    journal::scan(root).expect("recovered journal must remain scannable");
});
