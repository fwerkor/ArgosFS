//! Conservative inode admission control for the in-memory JSON metadata backend.
//! This guards *creation*, not arbitrary memory allocations; measurements from
//! real workloads are necessary before treating the estimate as an RSS bound.
use super::*;
use std::sync::OnceLock;

pub(crate) const ESTIMATED_RSS_BYTES_PER_INODE: u64 = 32 * 1024;
const DEFAULT_METADATA_MEMORY_PERCENT: u64 = 65;
const MIN_INODE_LIMIT: u64 = 1024;

fn read_limit(path: &str) -> Option<u64> {
    fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0 && *value < (1u64 << 60))
}

fn available_memory_budget_bytes() -> Option<u64> {
    static MEMORY_BYTES: OnceLock<Option<u64>> = OnceLock::new();
    *MEMORY_BYTES.get_or_init(|| {
        let total_kib = fs::read_to_string("/proc/meminfo")
            .ok()?
            .lines()
            .find_map(|line| {
                line.strip_prefix("MemTotal:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })?;
        let mut bytes = total_kib.saturating_mul(1024);
        for path in [
            "/sys/fs/cgroup/memory.max",
            "/sys/fs/cgroup/memory/memory.limit_in_bytes",
        ] {
            if let Some(limit) = read_limit(path) {
                bytes = bytes.min(limit);
            }
        }
        Some(bytes)
    })
}

pub(crate) fn estimated_rss_bytes(inodes: usize) -> u64 {
    (inodes as u64).saturating_mul(ESTIMATED_RSS_BYTES_PER_INODE)
}

fn default_inode_limit(kind: BackendKind, memory_bytes: Option<u64>) -> Option<usize> {
    if kind == BackendKind::Host {
        return None;
    }
    let memory_bytes = memory_bytes?;
    let reserved = (memory_bytes / 100).saturating_mul(DEFAULT_METADATA_MEMORY_PERCENT);
    Some(
        (reserved / ESTIMATED_RSS_BYTES_PER_INODE)
            .max(MIN_INODE_LIMIT)
            .min(usize::MAX as u64) as usize,
    )
}

pub(crate) fn inode_limit(kind: BackendKind) -> Result<Option<usize>> {
    match std::env::var("ARGOSFS_METADATA_MAX_INODES") {
        Ok(value) => {
            let cap = value.parse::<usize>().map_err(|_| {
                ArgosError::Invalid(
                    "ARGOSFS_METADATA_MAX_INODES must be a non-negative integer".into(),
                )
            })?;
            Ok((cap > 0).then_some(cap))
        }
        Err(std::env::VarError::NotPresent) => {
            Ok(default_inode_limit(kind, available_memory_budget_bytes()))
        }
        Err(err) => Err(ArgosError::Invalid(format!(
            "invalid ARGOSFS_METADATA_MAX_INODES: {err}"
        ))),
    }
}

fn reject_at_limit(used: usize, limit: usize) -> Result<()> {
    if used >= limit {
        return Err(ArgosError::MetadataInodeLimit { used, limit });
    }
    Ok(())
}

pub(crate) fn check_new_inode(meta: &Metadata) -> Result<()> {
    if let Some(limit) = inode_limit(meta.backend)? {
        let used = meta.inodes.len();
        reject_at_limit(used, limit)?;
        if used >= limit.saturating_mul(8) / 10 && used.is_multiple_of(1024) {
            eprintln!(
                "argosfs: metadata inode budget warning: {used}/{limit} (estimate: {} MiB RSS)",
                estimated_rss_bytes(used) / (1024 * 1024)
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_budget_limits_block_backends_but_not_host() {
        assert_eq!(
            default_inode_limit(BackendKind::Host, Some(16 * 1024 * 1024 * 1024)),
            None
        );
        let cap =
            default_inode_limit(BackendKind::LoopBlock, Some(16 * 1024 * 1024 * 1024)).unwrap();
        assert!((300_000..=400_000).contains(&cap));
        // The original 274,103-entry backup must not be rejected by the
        // *default* admission guard on the reported 15 GiB machine.
        assert!(
            default_inode_limit(BackendKind::RawBlock, Some(15 * 1024 * 1024 * 1024)).unwrap()
                > 274_103
        );
        assert_eq!(default_inode_limit(BackendKind::RawBlock, None), None);
    }

    #[test]
    fn estimate_saturates_and_error_is_enospc() {
        assert!(reject_at_limit(9, 10).is_ok());
        assert!(matches!(
            reject_at_limit(10, 10),
            Err(ArgosError::MetadataInodeLimit {
                used: 10,
                limit: 10
            })
        ));
        assert_eq!(estimated_rss_bytes(10), 10 * 32 * 1024);
        assert_eq!(
            ArgosError::MetadataInodeLimit {
                used: 10,
                limit: 10
            }
            .errno(),
            libc::ENOSPC
        );
    }
}
