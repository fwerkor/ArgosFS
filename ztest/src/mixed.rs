//! Concurrent foreground transactions and storage-management operations.
//! Each round has a finite legal-state oracle, even when the order of
//! conflicting writes is nondeterministic.
use crate::fault::{FaultBackend, FaultConfig, FaultStats};
use crate::invariants::verify_clean;
use anyhow::{ensure, Context, Result};
use argosfs::storage::backend::StorageBackend;
use argosfs::{
    ArgosError, ArgosFs, AutopilotConfig, Compression, DiskStatus, StorageTier, VolumeConfig,
};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn config(k: usize, m: usize) -> VolumeConfig {
    VolumeConfig {
        k,
        m,
        chunk_size: 32768,
        compression: Compression::Lz4,
        l2_cache_bytes: 2 * 1024 * 1024,
        ..VolumeConfig::default()
    }
}

fn create(images: &[PathBuf], cfg: VolumeConfig, paged: bool, size: u64) -> Result<ArgosFs> {
    let create = if paged {
        ArgosFs::create_loop_paged
    } else {
        ArgosFs::create_loop_legacy
    };
    Ok(create(images, cfg, size, "ztest-mixed", false)?)
}

fn absent(fs: &ArgosFs, path: &str) -> Result<()> {
    ensure!(
        matches!(fs.resolve_path(path, false), Err(ArgosError::NotFound(_))),
        "unexpected path {path}"
    );
    Ok(())
}

fn apply(data: &mut Vec<u8>, offset: usize, payload: &[u8]) {
    data.resize(data.len().max(offset + payload.len()), 0);
    data[offset..offset + payload.len()].copy_from_slice(payload);
}

fn legal_states(
    initial: &[u8],
    a: &[u8],
    b: &[u8],
    truncate: usize,
    prefixes: bool,
) -> Vec<Vec<u8>> {
    let mut states = Vec::new();
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut result = initial.to_vec();
        if prefixes {
            states.push(result.clone());
        }
        for op in order {
            match op {
                0 => apply(&mut result, 15000, a),
                1 => apply(&mut result, 31000, b),
                _ => result.resize(truncate, 0),
            }
            if prefixes {
                states.push(result.clone());
            }
        }
        if !prefixes {
            states.push(result);
        }
    }
    states
}

fn autopilot(fs: &ArgosFs) -> Result<()> {
    let result = fs.autopilot_once_with_config(AutopilotConfig {
        probe_interval_sec: u64::MAX,
        smart_interval_sec: u64::MAX,
        scrub_interval_sec: 0,
        rebalance_interval_sec: 0,
        rebalance_min_skew: 0.0,
        scrub_files_per_run: 128,
        rebalance_files_per_run: 128,
        max_drains_per_run: 0,
        ..AutopilotConfig::default()
    })?;
    let actions = result["actions"]
        .as_array()
        .context("missing autopilot actions")?;
    ensure!(
        actions.iter().any(|a| a["action"] == "scrub-incremental"),
        "autopilot did not exercise scrub: {result}"
    );
    for action in actions {
        ensure!(
            action["action"] != "rebalance-skipped",
            "autopilot rebalance failed: {action}"
        );
        if let Some(report) = action.get("report") {
            ensure!(
                report["unrecoverable_files"].as_u64().unwrap_or(0) == 0,
                "autopilot reported lost data: {action}"
            );
        }
    }
    Ok(())
}

// DeviceFailed for the injected member is a precommit failure, not an
// acknowledgement. Retry only that error after the member is marked failed;
// the full serial-state oracle still checks the eventual successful writes.
fn during_outage<T>(
    victim: Option<&str>,
    done: &AtomicBool,
    operation: impl Fn() -> argosfs::Result<T>,
) -> Result<T> {
    match operation() {
        Err(ArgosError::DeviceFailed { disk_id, .. }) if victim == Some(disk_id.as_str()) => {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !done.load(Ordering::Acquire) {
                ensure!(Instant::now() < deadline, "member outage did not finish");
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(operation()?)
        }
        other => Ok(other?),
    }
}

// All six foreground threads are released together with the maintenance
// thread. The shared inode stays alive through an anchor hard link; unlink
// is therefore independent of the three competing content mutations.
fn race(
    fs: &ArgosFs,
    seed: u64,
    phase: usize,
    sentinel: &[u8],
    victim: Option<&str>,
    maintenance: impl FnOnce() -> Result<()>,
) -> Result<Vec<u8>> {
    let initial = bytes(seed ^ phase as u64, 49152);
    let a = bytes(seed ^ 0xdead ^ phase as u64, 23000);
    let b = bytes(seed ^ 0xbeef ^ phase as u64, 13000);
    let truncate = 21000 + (seed as usize % 2000);
    fs.write_file("/shared", &initial, 0o644)?;
    let ino = fs.resolve_path("/shared", false)?;
    fs.link_at(
        ino,
        fs.resolve_path("/", true)?,
        OsStr::new("shared-anchor"),
    )?;
    let allowed = legal_states(&initial, &a, &b, truncate, false);
    let intermediate = legal_states(&initial, &a, &b, truncate, true);
    let barrier = Barrier::new(7);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<()> {
        let write_a = scope.spawn(|| -> Result<()> {
            barrier.wait();
            ensure!(
                during_outage(victim, &done, || fs.write_inode_range(ino, 15000, &a))? == a.len(),
                "short shared write A"
            );
            Ok(())
        });
        let write_b = scope.spawn(|| -> Result<()> {
            barrier.wait();
            ensure!(
                during_outage(victim, &done, || fs.write_inode_range(ino, 31000, &b))? == b.len(),
                "short shared write B"
            );
            Ok(())
        });
        let truncator = scope.spawn(|| -> Result<()> {
            barrier.wait();
            during_outage(victim, &done, || fs.truncate_inode(ino, truncate as u64))?;
            Ok(())
        });
        let unlinker = scope.spawn(|| -> Result<()> {
            barrier.wait();
            during_outage(victim, &done, || fs.unlink_path("/shared"))?;
            Ok(())
        });
        let reader = scope.spawn(|| -> Result<()> {
            barrier.wait();
            for _ in 0..8 {
                let actual =
                    during_outage(victim, &done, || fs.read_inode(ino, 0, usize::MAX, false))?;
                ensure!(
                    intermediate.contains(&actual),
                    "concurrent read saw a torn shared inode"
                );
            }
            Ok(())
        });
        let foreground = scope.spawn(|| -> Result<()> {
            barrier.wait();
            for i in 0..12 {
                let path = format!("/foreground-{i}");
                let expected = bytes(seed ^ (phase as u64 * 97 + i), 16384 + i as usize * 1024);
                during_outage(victim, &done, || fs.write_file(&path, &expected, 0o644))?;
                ensure!(
                    during_outage(victim, &done, || fs.read_file(&path, false))? == expected,
                    "foreground content mismatch"
                );
                ensure!(
                    during_outage(victim, &done, || fs.read_file("/sentinel", false))? == sentinel,
                    "maintenance corrupted stable file"
                );
                during_outage(victim, &done, || fs.unlink_path(&path))?;
            }
            Ok(())
        });
        barrier.wait();
        let maintenance_result = maintenance();
        done.store(true, Ordering::Release);
        for h in [write_a, write_b, truncator, unlinker, foreground, reader] {
            h.join()
                .map_err(|_| anyhow::anyhow!("foreground thread panicked"))??;
        }
        maintenance_result
    })?;
    absent(fs, "/shared")?;
    ensure!(
        fs.resolve_path("/shared-anchor", false)? == ino,
        "shared inode identity changed"
    );
    let actual = fs.read_file("/shared-anchor", false)?;
    ensure!(
        allowed.contains(&actual),
        "shared write/truncate result is not any legal serial order"
    );
    ensure!(
        fs.read_file("/sentinel", false)? == sentinel,
        "sentinel changed"
    );
    fs.sync()?;
    Ok(actual)
}

fn quota(fs: &ArgosFs, seed: u64) -> Result<()> {
    fs.mkdir("/quota", 0o755)?;
    fs.set_directory_quota("/quota", 65536)?;
    let first = bytes(seed, 32768);
    fs.write_file("/quota/a", &first, 0o644)?;
    let ino = fs.resolve_path("/quota/a", true)?;
    fs.link_at(ino, fs.resolve_path("/quota", true)?, OsStr::new("alias"))?;
    ensure!(
        fs.directory_quota("/quota")? == (Some(65536), 32768),
        "hard link charged twice"
    );
    ensure!(
        matches!(
            fs.truncate_path("/quota/a", 65537),
            Err(ArgosError::QuotaExceeded { .. })
        ),
        "quota bypass"
    );
    ensure!(
        fs.read_file("/quota/alias", false)? == first,
        "failed quota write changed data"
    );
    fs.unlink_path("/quota/alias")?;
    fs.truncate_path("/quota/a", 65536)?;
    ensure!(
        fs.directory_quota("/quota")? == (Some(65536), 65536),
        "quota usage mismatch"
    );
    fs.sync()?;
    Ok(())
}

fn reopen_check(
    fs: ArgosFs,
    images: &[PathBuf],
    sentinel: &[u8],
    shared: &[u8],
) -> Result<ArgosFs> {
    fs.sync()?;
    fs.mark_clean_unmount()?;
    let before = fs.metadata_snapshot();
    let txid = before.txid;
    let dir = images[0].parent().context("image directory missing")?;
    let prefix = dir.join(format!("reopen-{txid}"));
    fs::write(
        prefix.with_extension("before.json"),
        serde_json::to_vec(&before)?,
    )?;
    drop(fs);
    // Check recovery read-only before opening writable: a writable mount
    // can reset journals and thereby erase the most useful failure evidence.
    let recovered = ArgosFs::open_loop(images, false)?;
    let after = recovered.metadata_snapshot();
    fs::write(
        prefix.with_extension("after.json"),
        serde_json::to_vec(&after)?,
    )?;
    ensure!(
        after.txid >= txid,
        "durable metadata regressed across reopen: {txid} -> {}",
        after.txid
    );
    let root = recovered.resolve_path("/", true)?;
    let expected = std::collections::BTreeSet::from(["quota", "sentinel", "shared-anchor"]);
    let entries = recovered.readdir(root)?;
    let actual: std::collections::BTreeSet<_> = entries
        .iter()
        .filter(|e| e.name != "." && e.name != "..")
        .map(|e| e.name.as_str())
        .collect();
    ensure!(
        actual == expected,
        "unexpected recovered namespace: {actual:?}"
    );
    ensure!(
        recovered.read_file("/sentinel", false)? == sentinel,
        "sentinel lost on reopen"
    );
    ensure!(
        recovered.read_file("/shared-anchor", false)? == shared,
        "shared state lost on reopen"
    );
    absent(&recovered, "/shared")?;
    drop(recovered);
    let fs = ArgosFs::open_loop(images, true)?;
    fs.unlink_path("/shared-anchor")?;
    Ok(fs)
}

fn mixed_pool(dir: &Path, paged: bool, seed: u64, rounds: usize) -> Result<()> {
    fs::create_dir(dir)?;
    let mut images: Vec<_> = (0..6).map(|i| dir.join(format!("disk{i}.img"))).collect();
    let mut fs = create(&images, config(3, 2), paged, 64 * 1024 * 1024)?;
    // The CLI owns this process. Never change the environment in a
    // concurrent unit test. This is a synthetic test key, not a credential.
    const KEY: &str = "ztest-only synthetic encryption key";
    let sentinel = bytes(seed ^ 0xcafe, 98304);
    fs.write_file("/sentinel", &sentinel, 0o600)?;
    fs.sync()?;
    quota(&fs, seed)?;
    for round in 0..rounds {
        let meta = fs.metadata_snapshot();
        let ino = fs.resolve_path("/sentinel", true)?;
        let candidates: std::collections::BTreeSet<_> = meta.inodes[&ino]
            .blocks
            .iter()
            .flat_map(|block| &block.shards)
            .map(|shard| shard.disk_id.clone())
            .collect();
        let online: Vec<_> = meta
            .disks
            .values()
            .filter(|d| d.status == DiskStatus::Online && candidates.contains(&d.id))
            .collect();
        ensure!(!online.is_empty(), "sentinel has no online shards");
        let victim =
            online[(seed.wrapping_add(round as u64) % online.len() as u64) as usize].clone();
        drop(meta);
        fs.sync()?;
        drop(fs);
        let backend = Arc::new(FaultBackend::new(
            FaultConfig::default(),
            Arc::new(FaultStats::default()),
            Some(std::fs::File::create(
                dir.join(format!("outage-{round}.jsonl")),
            )?),
        ));
        let wrapper = backend.clone();
        fs = ArgosFs::open_loop_with_backend(&images, true, move |inner| {
            wrapper.set_inner(inner);
            wrapper.clone() as Arc<dyn StorageBackend>
        })?;
        let mut replacement = dir.join(format!("replacement-{round}.img"));
        eprintln!("ztest mixed: round {round}, failure of {}", victim.id);
        let state = race(&fs, seed, round * 4, &sentinel, Some(&victim.id), || {
            backend.fail_member(victim.id.clone());
            // Verify an actual device I/O error, not just a metadata status
            // transition. Scrub reads the durable shards despite L2 hits.
            let report = fs.fsck(false, false).context("outage scrub")?;
            ensure!(
                report.unrecoverable_files == 0,
                "one-member outage lost data: {:?}",
                report.errors
            );
            fs.mark_disk(&victim.id, DiskStatus::Failed)
                .context("mark failed member")?;
            Ok(())
        })
        .context("foreground versus member failure")?;
        ensure!(
            backend.failed_member_ios() > 0,
            "outage did not reach any backend I/O"
        );
        eprintln!(
            "ztest mixed: {} actual member EIOs",
            backend.failed_member_ios()
        );
        // Omit the failed member from the actual reopen, exercising a
        // missing device rather than only a status flag.
        ensure!(
            images.contains(&victim.path),
            "victim path outside active fixture"
        );
        std::fs::rename(&victim.path, dir.join(format!("retired-{round}.img")))?;
        images.retain(|p| *p != victim.path);
        fs = reopen_check(fs, &images, &sentinel, &state)?;
        eprintln!("ztest mixed: round {round}, add replacement");
        let state = race(&fs, seed, round * 4 + 1, &sentinel, None, || {
            fs.add_block_device(replacement.clone(), 80 * 1024 * 1024, false)?;
            Ok(())
        })
        .context("foreground versus member addition")?;
        images.push(std::mem::take(&mut replacement));
        fs = reopen_check(fs, &images, &sentinel, &state)?;
        eprintln!("ztest mixed: round {round}, rebuild and remove");
        let state = race(&fs, seed, round * 4 + 2, &sentinel, None, || {
            let report = fs.repair_member_shards(&victim.id, |_, _, _| {})?;
            ensure!(
                report.unrecoverable_files == 0 && report.errors.is_empty(),
                "rebuild failed: {:?}",
                report.errors
            );
            fs.remove_disk(&victim.id)?;
            Ok(())
        })
        .context("foreground versus member rebuild/removal")?;
        fs = reopen_check(fs, &images, &sentinel, &state)?;
        eprintln!("ztest mixed: round {round}, reshape and autopilot");
        let state = race(&fs, seed, round * 4 + 3, &sentinel, None, || {
            let (k, m) = if round % 2 == 0 { (2, 2) } else { (3, 2) };
            fs.reshape_layout(k, m, Some(1))?;
            fs.rebalance()?;
            autopilot(&fs)?;
            Ok(())
        })
        .context("foreground versus reshape, rebalance and autopilot")?;
        fs = reopen_check(fs, &images, &sentinel, &state)?;
        let (k, m) = if round % 2 == 0 { (2, 2) } else { (3, 2) };
        ensure!(
            fs.reshape_layout(k, m, None)?.complete,
            "layout conversion did not finish"
        );
        fs.sync()?;
        ensure!(
            fs.metadata_snapshot().disks[&victim.id].status == DiskStatus::Removed,
            "failed member not removed"
        );
        ensure!(
            fs.directory_quota("/quota")? == (Some(65536), 65536),
            "quota lost across topology operations"
        );
        let mut expected_quota = bytes(seed, 32768);
        expected_quota.resize(65536, 0);
        ensure!(
            fs.read_file("/quota/a", false)? == expected_quota,
            "quota data changed across topology operations"
        );
        ensure!(
            fs.read_file("/sentinel", false)? == sentinel,
            "sentinel mismatch after reshape"
        );
        let report = fs.scrub()?;
        ensure!(
            report.errors.is_empty() && report.unrecoverable_files == 0,
            "final scrub failed"
        );
        fs.sync()?;
        verify_clean(&fs)?;
        eprintln!("ztest mixed: round {round} verified, paged={paged}");
    }
    // Exercise encryption conversion in the already reshaped, replaced pool
    // with populated L2 cache and a live quota. Doing every foreground I/O
    // under Argon2 is prohibitively expensive in debug CI.
    eprintln!("ztest mixed: encryption and cached-data conversion");
    std::env::remove_var("ARGOSFS_KEY_FILE");
    fs.enable_encryption(KEY)?;
    std::env::set_var("ARGOSFS_KEY", KEY);
    fs.rebalance()?;
    fs.sync()?;
    ensure!(
        fs.read_file("/sentinel", false)? == sentinel,
        "encryption conversion changed content"
    );
    ensure!(
        fs.directory_quota("/quota")? == (Some(65536), 65536),
        "encryption changed quota"
    );
    let sentinel_ino = fs.resolve_path("/sentinel", true)?;
    let meta = fs.metadata_snapshot();
    ensure!(
        meta.inodes[&sentinel_ino]
            .blocks
            .iter()
            .all(|b| b.encrypted),
        "unencrypted sentinel blocks"
    );
    drop(meta);
    drop(fs);
    std::env::remove_var("ARGOSFS_KEY");
    let fs = ArgosFs::open_loop(&images, false)?;
    ensure!(
        fs.read_file("/sentinel", false)
            .is_err_and(|e| e.errno() == libc::EACCES),
        "encrypted data readable without key"
    );
    drop(fs);
    std::env::set_var("ARGOSFS_KEY", KEY);
    let fs = ArgosFs::open_loop(&images, false)?;
    ensure!(
        fs.read_file("/sentinel", false)? == sentinel,
        "encrypted data cannot be reopened"
    );
    drop(fs);
    std::env::remove_var("ARGOSFS_KEY");
    Ok(())
}

fn tiering(dir: &Path, seed: u64) -> Result<()> {
    // Explicit heterogeneous tier labels are currently exposed by host pools.
    let fs = ArgosFs::create(dir, config(2, 1), 3, false)?;
    for _ in 0..3 {
        fs.add_disk(None, Some(StorageTier::Hot), Some(1.0), Some(0), false)?;
    }
    fs.add_disk(None, Some(StorageTier::Cold), Some(1.0), Some(0), false)?;
    let data = bytes(seed, 98304);
    fs.write_file("/hot", &data, 0o644)?;
    for _ in 0..12 {
        ensure!(
            fs.read_file("/hot", false)? == data,
            "cache returned stale data"
        );
    }
    fs.rebalance()?;
    fs.sync()?;
    let meta = fs.metadata_snapshot();
    let inode = &meta.inodes[&fs.resolve_path("/hot", true)?];
    ensure!(
        inode.storage_class == StorageTier::Hot,
        "hot file not classified"
    );
    ensure!(
        inode
            .blocks
            .iter()
            .flat_map(|b| &b.shards)
            .any(|s| meta.disks[&s.disk_id].tier == StorageTier::Hot),
        "hot file did not migrate to hot tier"
    );
    drop(meta);
    let newer = bytes(seed ^ 33, 98304);
    fs.write_file("/hot", &newer, 0o644)?;
    ensure!(
        fs.read_file("/hot", false)? == newer,
        "cache served old data after overwrite"
    );
    fs.sync()?;
    drop(fs);
    let fs = ArgosFs::open(dir)?;
    ensure!(
        fs.read_file("/hot", false)? == newer,
        "tiered file changed on reopen"
    );
    ensure!(
        fs.fsck(false, false)?.errors.is_empty(),
        "host tier fsck failed"
    );
    Ok(())
}

fn full(dir: &Path, paged: bool, seed: u64) -> Result<()> {
    fs::create_dir(dir)?;
    let images: Vec<_> = (0..2).map(|i| dir.join(format!("disk{i}.img"))).collect();
    let mut cfg = config(1, 1);
    cfg.compression = Compression::None;
    cfg.chunk_size = 262144;
    let fs = create(&images, cfg, paged, 32 * 1024 * 1024)?;
    let payload = bytes(seed, 1024 * 1024);
    let mut count = 0;
    for i in 0..64 {
        let path = format!("/fill-{i}");
        fs.write_file(&path, b"", 0o644)?;
        match fs.write_file(&path, &payload, 0o644) {
            Ok(()) => count += 1,
            Err(e) if e.errno() == libc::ENOSPC => {
                ensure!(
                    fs.read_file(&path, false)?.is_empty(),
                    "failed full-disk write modified file"
                );
                fs.unlink_path(&path)?;
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    ensure!(count > 0 && count < 64, "space exhaustion was not reached");
    for i in 0..count {
        ensure!(
            fs.read_file(&format!("/fill-{i}"), false)? == payload,
            "ENOSPC corrupted existing data"
        );
    }
    fs.unlink_path("/fill-0")?;
    fs.write_file("/reuse", &payload, 0o644)?;
    fs.sync()?;
    ensure!(
        fs.read_file("/reuse", false)? == payload,
        "freed space not reusable"
    );
    let report = fs.scrub()?;
    ensure!(
        report.errors.is_empty(),
        "ENOSPC scrub errors: {:?}",
        report.errors
    );
    fs.sync()?;
    verify_clean(&fs)?;
    drop(fs);
    let fs = ArgosFs::open_loop(&images, false)?;
    ensure!(
        fs.read_file("/reuse", false)? == payload,
        "reused space not durable"
    );
    absent(&fs, "/fill-0")?;
    for i in 1..count {
        ensure!(
            fs.read_file(&format!("/fill-{i}"), false)? == payload,
            "ENOSPC damaged durable file"
        );
    }
    eprintln!("ztest mixed: ENOSPC after {count} MiB and space reuse verified");
    Ok(())
}

pub fn run(dir: &Path, paged: bool, seed: u64, rounds: usize) -> Result<()> {
    ensure!(rounds > 0 && rounds <= 32, "rounds must be in 1..=32");
    fs::create_dir(dir)?;
    let dir = fs::canonicalize(dir)?;
    mixed_pool(&dir.join("mixed"), paged, seed, rounds)?;
    tiering(&dir.join("tiers"), seed)?;
    full(&dir.join("full"), paged, seed)?;
    println!("ztest mixed: {rounds} rounds, seed={seed}, paged={paged}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_states_exclude_torn_mutations() {
        let states = legal_states(
            &vec![0; 49152],
            &vec![1; 23000],
            &vec![2; 13000],
            21000,
            false,
        );
        assert!(states.iter().any(|data| data.len() == 21000));
        assert!(states.iter().any(|data| data.len() == 44000));
        assert!(!states.contains(&vec![3; 44000]));
    }
}
