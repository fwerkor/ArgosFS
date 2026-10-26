#![no_main]

mod common;

use argosfs::journal::{JournalRecord, JournalRecordKind, MetadataDeltaOp};
use libfuzzer_sys::fuzz_target;
use serde_json::json;

fuzz_target!(|data: &[u8]| {
    if let Ok(record) = serde_json::from_slice::<JournalRecord>(data) {
        let encoded = serde_json::to_vec(&record).expect("parsed journal record must serialize");
        let round: JournalRecord =
            serde_json::from_slice(&encoded).expect("serialized journal record must parse");
        assert_eq!(
            serde_json::to_value(record).unwrap(),
            serde_json::to_value(round).unwrap()
        );
    }

    let kind = match common::byte(data, 0) % 3 {
        0 => JournalRecordKind::Transaction,
        1 => JournalRecordKind::Checkpoint,
        _ => JournalRecordKind::Event,
    };
    let mut metadata = common::metadata_from(data, 1);
    common::add_small_inode_tree(&mut metadata, data, 128);
    common::prepare(&mut metadata);

    let delta = vec![
        MetadataDeltaOp::Replace {
            path: "/raw_pool/pool_name".to_string(),
            value: json!(common::text_at(data, 512, 40, "next-pool")),
        },
        MetadataDeltaOp::Add {
            path: "/layouts/fuzz".to_string(),
            value: json!({
                "id": "fuzz",
                "k": 1,
                "m": 0,
                "chunk_size": 4096,
                "created_txid": metadata.txid,
                "sealed": false
            }),
        },
    ];

    let record = JournalRecord {
        version: 1,
        record_type: kind,
        time: (common::u64_at(data, 560) % 2_000_000_000) as f64,
        volume_uuid: metadata.uuid.clone(),
        txid: metadata.txid,
        generation: metadata.integrity.generation,
        action: common::text_at(data, 568, 32, "fuzz"),
        details: json!({
            "seed": common::u64_at(data, 604),
            "tag": common::text_at(data, 612, 24, "journal")
        }),
        previous_record_hash: common::text_at(data, 640, 48, "prev"),
        previous_meta_hash: metadata.integrity.previous_meta_hash.clone(),
        meta_hash: metadata.integrity.meta_hash.clone(),
        metadata: (common::byte(data, 700) & 1 != 0).then_some(metadata),
        metadata_delta: (common::byte(data, 701) & 1 != 0).then_some(delta),
        record_hash: common::text_at(data, 704, 48, "record"),
    };

    let encoded = serde_json::to_vec(&record).expect("generated journal record must serialize");
    let round: JournalRecord =
        serde_json::from_slice(&encoded).expect("generated journal record must parse");
    assert_eq!(
        serde_json::to_value(record).unwrap(),
        serde_json::to_value(round).unwrap()
    );
});
