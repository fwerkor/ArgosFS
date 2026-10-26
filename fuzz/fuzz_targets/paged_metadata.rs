#![no_main]

mod common;

use argosfs::metadata_store::{metadata_page_delta, PagedMetadata, PagedMetadataOptions};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut before = common::metadata_from(data, 0);
    common::add_small_inode_tree(&mut before, data, 128);
    common::prepare(&mut before);

    let options = PagedMetadataOptions {
        directory_entries_per_page: 1 + common::byte(data, 512) as usize % 8,
        xattrs_per_page: 1 + common::byte(data, 513) as usize % 8,
        file_blocks_per_page: 1 + common::byte(data, 514) as usize % 8,
    };
    let paged = PagedMetadata::from_metadata_with_options(&before, options)
        .expect("generated metadata must page");
    for page in paged.pages.values() {
        page.verify_hash().expect("fresh page hash must verify");
    }
    let round = paged
        .to_metadata()
        .expect("fresh paged metadata must decode");
    assert_eq!(
        serde_json::to_value(&before).unwrap(),
        serde_json::to_value(&round).unwrap()
    );

    let mut after = before.clone();
    after.txid = before.txid.saturating_add(1);
    after.updated_at += (common::u64_at(data, 520) % 1000) as f64;
    after.raw_pool.pool_name = common::text_at(data, 528, 32, "paged-next");
    after.config.fsname = common::text_at(data, 564, 32, "paged-fs");
    after.integrity = Default::default();
    argosfs::journal::prepare_metadata_integrity_with_previous(
        &mut after,
        before.integrity.meta_hash.clone(),
    )
    .unwrap();

    let after_paged = PagedMetadata::from_metadata_with_options(&after, options).unwrap();
    let delta = metadata_page_delta(&paged, &after_paged);
    let mut replayed = paged.clone();
    replayed
        .apply_delta(&delta)
        .expect("page delta generated from stores must apply");
    assert_eq!(
        serde_json::to_value(after_paged.to_metadata().unwrap()).unwrap(),
        serde_json::to_value(replayed.to_metadata().unwrap()).unwrap()
    );
});
