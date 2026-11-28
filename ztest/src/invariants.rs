//! Independent namespace checks over a quiescent metadata snapshot.
use anyhow::{ensure, Result};
use argosfs::types::{DiskStatus, NodeKind, ShardLocation};
use argosfs::ArgosFs;
use std::collections::{BTreeMap, BTreeSet};

pub fn verify_structure(fs: &ArgosFs) -> Result<()> {
    let root = fs.resolve_path("/", true)?;
    let meta = fs.metadata_snapshot();
    let mut seen = BTreeSet::new();
    let mut references = BTreeMap::<_, u32>::new();
    let mut todo = vec![root];
    ensure!(
        meta.inodes
            .get(&root)
            .is_some_and(|inode| inode.kind == NodeKind::Directory),
        "root is not a directory"
    );
    while let Some(id) = todo.pop() {
        let inode = meta
            .inodes
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("missing inode {id}"))?;
        ensure!(inode.id == id, "inode identity mismatch {id}");
        ensure!(id < meta.next_inode, "inode {id} exceeds allocation cursor");
        if !seen.insert(id) {
            ensure!(
                inode.kind != NodeKind::Directory,
                "directory cycle or multiple parents: {id}"
            );
            continue;
        }
        if inode.kind == NodeKind::Directory {
            for child in inode.entries.values() {
                *references.entry(*child).or_default() += 1;
                todo.push(*child);
            }
        } else {
            ensure!(
                inode.entries.is_empty(),
                "non-directory inode {id} has entries"
            );
        }
    }
    for (id, inode) in meta.inodes.iter() {
        ensure!(seen.contains(id), "unreachable inode {id}");
        if inode.kind != NodeKind::Directory {
            ensure!(
                references.get(id).copied().unwrap_or_default() == inode.nlink,
                "link count mismatch for inode {id}"
            );
        }
    }
    Ok(())
}

/// A second, independent interval sweep: every byte below the allocator
/// high-water mark must be owned by exactly one live extent or the free list.
fn verify_allocations(fs: &ArgosFs) -> Result<()> {
    let meta = fs.metadata_snapshot();
    let mut extents = BTreeMap::<String, Vec<(u64, u64)>>::new();
    for inode in meta.inodes.values() {
        for block in &inode.blocks {
            let mut slots = BTreeSet::new();
            let mut disks = BTreeSet::new();
            for shard in &block.shards {
                ensure!(
                    slots.insert(shard.slot),
                    "duplicate shard slot in {}",
                    block.stripe_id
                );
                ensure!(
                    disks.insert(&shard.disk_id),
                    "duplicate shard device in {}",
                    block.stripe_id
                );
                let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
                    anyhow::bail!("non-raw shard in loop pool");
                };
                ensure!(
                    extent.disk_id == shard.disk_id,
                    "shard/extent device mismatch"
                );
                ensure!(
                    meta.disks
                        .get(&extent.disk_id)
                        .is_some_and(|disk| disk.status != DiskStatus::Removed),
                    "shard references unknown or removed device {}",
                    extent.disk_id
                );
                ensure!(
                    extent.length >= shard.size as u64,
                    "extent shorter than shard"
                );
                extents
                    .entry(extent.disk_id.clone())
                    .or_default()
                    .push((extent.offset, extent.length));
            }
        }
    }
    for (disk, allocator) in &meta.raw_pool.allocators {
        let mut ranges = extents.remove(disk).unwrap_or_default();
        ranges.extend(allocator.free_extents.iter().map(|e| (e.offset, e.length)));
        ranges.sort_unstable();
        let block = allocator.block_size.max(4096);
        ensure!(
            allocator.data_start <= allocator.next_offset
                && allocator.next_offset <= allocator.data_end,
            "invalid allocator high-water mark on {disk}"
        );
        let mut cursor = allocator.data_start;
        for (offset, length) in ranges {
            ensure!(
                offset == cursor,
                "allocation gap or overlap on {disk} at {cursor}, next {offset}"
            );
            ensure!(
                length > 0 && offset % block == 0 && length % block == 0,
                "unaligned extent on {disk}"
            );
            cursor = offset
                .checked_add(length)
                .ok_or_else(|| anyhow::anyhow!("extent overflow on {disk}"))?;
        }
        ensure!(
            cursor == allocator.next_offset,
            "allocation coverage mismatch on {disk}"
        );
    }
    ensure!(
        extents.is_empty(),
        "shard references device without allocator"
    );
    Ok(())
}

/// Verify the *result* of scrub, not just the return code of the repairing pass.
pub fn verify_clean(fs: &ArgosFs) -> Result<()> {
    verify_structure(fs)?;
    verify_allocations(fs)?;
    let report = fs.fsck(false, false)?;
    ensure!(report.errors.is_empty(), "fsck: {:?}", report.errors);
    ensure!(
        report.damaged_files == 0 && report.unrecoverable_files == 0,
        "damaged files after recovery"
    );
    ensure!(
        report.orphan_shards == 0 && report.orphan_extent_bytes == 0,
        "unreferenced storage: {} shards, {} bytes",
        report.orphan_shards,
        report.orphan_extent_bytes
    );
    Ok(())
}
