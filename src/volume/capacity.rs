//! Advisory space accounting shared by statfs and the metadata commit path.
//!
//! The snapshot is cheap to compute for raw pools (allocator counters plus the
//! inode count) and can therefore be republished whenever a commit makes new
//! free space durable.  statfs never has to wait for the metadata writer lock
//! just to report capacity that changed while a checkpoint was in flight.

use crate::types::{
    BackendKind, CapacitySource, DiskStatus, Metadata, NodeKind, RawAllocatorState,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Advisory capacity observed from one metadata image.
#[derive(Clone, Debug)]
pub(crate) struct CapacitySnapshot {
    pub(crate) usable_bytes: u64,
    pub(crate) free_bytes: u64,
    pub(crate) inode_count: u64,
    pub(crate) block_size: u64,
    pub(crate) unavailable: BTreeSet<String>,
}

impl CapacitySnapshot {
    pub(crate) fn from_metadata(
        root: &Path,
        meta: &Metadata,
        unavailable: BTreeSet<String>,
    ) -> Self {
        let (usable_bytes, free_bytes) = statfs_bytes(meta, root, &unavailable);
        Self {
            usable_bytes,
            free_bytes,
            inode_count: meta.inodes.len() as u64,
            block_size: (meta.config.chunk_size as u64).max(4096),
            unavailable,
        }
    }
}

pub(crate) fn statfs_bytes(
    meta: &Metadata,
    root: &Path,
    unavailable: &BTreeSet<String>,
) -> (u64, u64) {
    if meta.backend != BackendKind::Host && !meta.raw_pool.allocators.is_empty() {
        let width = meta.config.k.saturating_add(meta.config.m);
        if meta.config.k == 0 || width == 0 {
            return (0, 0);
        }
        let mut capacities = Vec::new();
        let mut free = Vec::new();
        for (disk_id, allocator) in &meta.raw_pool.allocators {
            if unavailable.contains(disk_id) {
                continue;
            }
            let Some(disk) = meta.disks.get(disk_id) else {
                continue;
            };
            if !matches!(
                disk.status,
                DiskStatus::Online | DiskStatus::Degraded | DiskStatus::Draining
            ) {
                continue;
            }
            capacities.push(allocator.data_end.saturating_sub(allocator.data_start));
            if disk.status == DiskStatus::Online {
                free.push(raw_allocator_free_bytes(allocator));
            }
        }
        let usable = max_stripe_units(&capacities, width).saturating_mul(meta.config.k as u64);
        let available = max_stripe_units(&free, width).saturating_mul(meta.config.k as u64);
        return (usable, available.min(usable));
    }

    let (explicit_capacity, grouped_capacity) = meta
        .disks
        .values()
        .filter(|disk| disk.status == DiskStatus::Online && !unavailable.contains(&disk.id))
        .fold((0u64, BTreeMap::<String, u64>::new()), |mut acc, disk| {
            if disk.capacity_source == CapacitySource::UserOverride {
                acc.0 = acc.0.saturating_add(disk.capacity_bytes);
            } else if let Some(fs_id) = disk.backing_fs_id.as_ref() {
                let entry = acc.1.entry(fs_id.clone()).or_default();
                *entry = (*entry).max(disk.capacity_bytes);
            } else {
                acc.0 = acc.0.saturating_add(disk.capacity_bytes);
            }
            acc
        });
    let configured_capacity =
        explicit_capacity.saturating_add(grouped_capacity.values().sum::<u64>());
    let width = meta.config.k.saturating_add(meta.config.m).max(1) as u64;
    if configured_capacity > 0 {
        let logical_used = meta
            .inodes
            .values()
            .filter(|inode| inode.kind == NodeKind::File)
            .map(|inode| inode.size)
            .sum::<u64>();
        let usable = configured_capacity.saturating_mul(meta.config.k as u64) / width;
        return (usable, usable.saturating_sub(logical_used));
    }
    let (capacity, free) = fallback_statfs_capacity(
        root,
        meta.disks
            .values()
            .filter(|disk| !unavailable.contains(&disk.id))
            .map(|disk| {
                if disk.path.is_absolute() {
                    disk.path.clone()
                } else {
                    root.join(&disk.path)
                }
            }),
    );
    (
        capacity.saturating_mul(meta.config.k as u64) / width,
        free.saturating_mul(meta.config.k as u64) / width,
    )
}

fn raw_allocator_free_bytes(allocator: &RawAllocatorState) -> u64 {
    let block = allocator.block_size.max(4096);
    let tail = allocator.data_end.saturating_sub(allocator.next_offset) / block * block;
    tail.saturating_add(allocator.free_extents.aligned_free_bytes(block))
}

pub(crate) fn max_stripe_units(per_disk: &[u64], width: usize) -> u64 {
    if width == 0 || per_disk.len() < width {
        return 0;
    }
    let mut low = 0u64;
    let mut high = (per_disk.iter().map(|value| *value as u128).sum::<u128>() / width as u128)
        .min(u64::MAX as u128) as u64;
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        let support = per_disk
            .iter()
            .map(|value| (*value).min(mid) as u128)
            .sum::<u128>();
        if support >= mid as u128 * width as u128 {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

pub(crate) fn fallback_statfs_capacity(
    root: &Path,
    paths: impl IntoIterator<Item = PathBuf>,
) -> (u64, u64) {
    let mut capacity = 0u64;
    let mut free = 0u64;
    let mut seen = BTreeSet::new();
    for path in std::iter::once(root.to_path_buf()).chain(paths) {
        let Some((fs_id, blocks, available)) = statvfs_capacity(&path) else {
            continue;
        };
        if seen.insert(fs_id) {
            capacity = capacity.saturating_add(blocks);
            free = free.saturating_add(available);
        }
    }
    (capacity, free)
}

pub(crate) fn statvfs_capacity(path: &Path) -> Option<(String, u64, u64)> {
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let block_size = stat.f_frsize.max(stat.f_bsize);
    let fs_id = format!("{}:{}", stat.f_fsid, block_size);
    Some((
        fs_id,
        stat.f_blocks.saturating_mul(block_size),
        stat.f_bavail.saturating_mul(block_size),
    ))
}
