//! Reproducible 9-member (4+4, one spare) shard-rebuild I/O benchmark.
//! Run: cargo test --test repair_benchmark -- --ignored --nocapture --test-threads=1

use argosfs::{ArgosFs, Compression, DiskStatus, VolumeConfig};
use std::time::Instant;

#[test]
#[ignore = "explicit benchmark; 151 files and fault injection"]
fn nine_member_4_plus_4_rebuild_151_files() {
    let root = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        root.path(),
        VolumeConfig {
            k: 4,
            m: 4,
            chunk_size: 4096,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        9,
        false,
    )
    .unwrap();
    let files: usize = std::env::var("ARGOSFS_REPAIR_BENCH_FILES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(151);
    const SIZE: usize = 64 * 1024;
    for i in 0..files {
        let data = (0..SIZE)
            .map(|j| ((j.wrapping_mul(17) + i.wrapping_mul(23)) % 251) as u8)
            .collect::<Vec<_>>();
        fs.write_file(&format!("/f-{i:03}"), &data, 0o600).unwrap();
    }
    let before = fs.metadata_snapshot();
    let first = before
        .inodes
        .values()
        .find(|inode| inode.blocks.iter().any(|block| !block.shards.is_empty()))
        .unwrap();
    let failed_disk = first.blocks[0].shards[0].disk_id.clone();
    fs.mark_disk(&failed_disk, DiskStatus::Removed).unwrap();

    let started = Instant::now();
    let report = fs.scrub().unwrap();
    let elapsed = started.elapsed();
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.files_checked, files as u64, "{report:?}");
    assert_eq!(report.repaired_files, report.damaged_files, "{report:?}");
    let after = fs.metadata_snapshot();
    let mut changed_files = 0usize;
    let mut changed_stripes = 0usize;
    let mut changed_shards = 0usize;
    let mut written_payload_bytes = 0usize;
    for (id, old_inode) in before.inodes.iter() {
        if old_inode.blocks.is_empty() {
            continue;
        }
        let new_inode = &after.inodes[id];
        assert_eq!(new_inode.blocks.len(), old_inode.blocks.len());
        let mut file_changed = false;
        for (old, new) in old_inode.blocks.iter().zip(&new_inode.blocks) {
            assert_eq!(old.raw_offset, new.raw_offset);
            assert_eq!(old.raw_sha256, new.raw_sha256);
            let mut stripe_changed = false;
            for shard in &new.shards {
                let original = old
                    .shards
                    .iter()
                    .find(|candidate| candidate.slot == shard.slot)
                    .unwrap();
                if original.disk_id != shard.disk_id
                    || original.location != shard.location
                    || original.relpath != shard.relpath
                {
                    changed_shards += 1;
                    written_payload_bytes += shard.size;
                    stripe_changed = true;
                }
            }
            if stripe_changed {
                file_changed = true;
                changed_stripes += 1;
            }
        }
        if file_changed {
            changed_files += 1;
        }
    }
    assert_eq!(changed_files as u64, report.repaired_files);
    assert!(changed_shards > 0);
    assert!(fs.fsck(false, false).unwrap().errors.is_empty());

    println!(
        "BENCH_RESULT {{\"files\":{files},\"file_size\":{SIZE},\"layout\":\"4+4\",\"members\":9,\"failed_disk\":\"{failed_disk}\",\"elapsed_ms\":{:.3},\"changed_files\":{changed_files},\"changed_stripes\":{changed_stripes},\"changed_shards\":{changed_shards},\"physical_payload_bytes_written\":{written_payload_bytes}}}",
        elapsed.as_secs_f64() * 1000.0,
    );
}
