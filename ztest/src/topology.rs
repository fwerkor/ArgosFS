//! Repeated member lifecycle exercises over disposable loop images.
use crate::combinations::bytes;
use crate::invariants::verify_clean;
use anyhow::{ensure, Context, Result};
use argosfs::types::DiskStatus;
use argosfs::ArgosError;
use argosfs::{ArgosFs, AutopilotConfig};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Barrier;

const SENTINEL: &str = "/topology-sentinel";
const IMAGE_SIZE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Missing,
    Returning,
    OnlineDrain,
}
impl Mode {
    pub fn round(index: usize) -> Self {
        [Self::Missing, Self::Returning, Self::OnlineDrain][index % 3]
    }
}

fn check_members(fs: &ArgosFs, images: &[PathBuf]) -> Result<()> {
    let expected: BTreeSet<_> = images.iter().cloned().collect();
    let actual: BTreeSet<_> = fs
        .metadata_snapshot()
        .disks
        .values()
        .filter(|d| d.status != DiskStatus::Removed)
        .map(|d| d.path.clone())
        .collect();
    ensure!(
        actual == expected,
        "topology membership mismatch: {actual:?} != {expected:?}"
    );
    Ok(())
}

fn check_online(fs: &ArgosFs, images: &[PathBuf]) -> Result<()> {
    check_members(fs, images)?;
    ensure!(
        fs.metadata_snapshot()
            .disks
            .values()
            .all(|d| matches!(d.status, DiskStatus::Online | DiskStatus::Removed)),
        "unexpected member status after topology transition"
    );
    Ok(())
}

// Never allow a writable recovery to erase evidence of a rollback. Check the
// read-only view first, and require both mounts to retain acknowledged state.
fn reopen(images: &[PathBuf], durable: u64, stage: &str) -> Result<ArgosFs> {
    let read_only =
        ArgosFs::open_loop(images, false).with_context(|| format!("read-only {stage}"))?;
    let recovered = read_only.metadata_snapshot().txid;
    ensure!(
        recovered >= durable,
        "{stage}: acknowledged transaction rolled back: {durable} -> {recovered}"
    );
    drop(read_only);
    let fs = ArgosFs::open_loop(images, true).with_context(|| format!("writable {stage}"))?;
    let recovered = fs.metadata_snapshot().txid;
    ensure!(
        recovered >= durable,
        "{stage}: acknowledged transaction rolled back: {durable} -> {recovered}"
    );
    Ok(fs)
}

// One foreground writer, one maintenance thread. The writer has sole logical
// ownership of the sentinel; maintenance is required to preserve its bytes.
fn concurrent(
    fs: &ArgosFs,
    seed: u64,
    before: &[u8],
    maintenance: impl FnOnce() -> Result<()> + Send,
) -> Result<Vec<u8>> {
    let barrier = Barrier::new(2);
    let result = std::thread::scope(|scope| -> Result<Vec<u8>> {
        let worker = scope.spawn(|| -> Result<Vec<u8>> {
            barrier.wait();
            let ino = fs.resolve_path(SENTINEL, true)?;
            let mut expected = before.to_vec();
            ensure!(
                fs.read_file(SENTINEL, false)? == expected,
                "sentinel changed before foreground write"
            );
            for i in 0..4 {
                let offset = 8191 + i * 4096;
                let patch = bytes(seed.wrapping_add(i as u64), 4097);
                ensure!(
                    fs.write_inode_range(ino, offset as u64, &patch)? == patch.len(),
                    "short foreground write"
                );
                expected[offset..offset + patch.len()].copy_from_slice(&patch);
                ensure!(
                    fs.read_file(SENTINEL, false)? == expected,
                    "maintenance raced with foreground read/write"
                );
            }
            Ok(expected)
        });
        barrier.wait();
        let maintenance_result = maintenance();
        let expected = worker
            .join()
            .map_err(|_| anyhow::anyhow!("foreground worker panicked"))??;
        maintenance_result?;
        Ok(expected)
    })?;
    ensure!(
        fs.read_file(SENTINEL, false)? == result,
        "maintenance lost foreground write"
    );
    fs.sync()?;
    Ok(result)
}

pub fn exercise(
    mut fs: ArgosFs,
    images: &mut Vec<PathBuf>,
    dir: &Path,
    seed: u64,
    index: usize,
    plan: (Option<Mode>, Option<u64>),
    verify: impl Fn(&ArgosFs) -> Result<()>,
) -> Result<ArgosFs> {
    let (mode, cut) = plan;
    check_online(&fs, images)?;
    verify(&fs)?;
    let mut expected = bytes(seed, 60 * 1024);
    fs.write_file(SENTINEL, &expected, 0o600)?;
    fs.sync()?;
    let retired = {
        let meta = fs.metadata_snapshot();
        let active: Vec<_> = meta
            .disks
            .values()
            .filter(|d| d.status == DiskStatus::Online)
            .collect();
        active[(seed % active.len() as u64) as usize].clone()
    };
    let parked = retired.path.with_extension("absent");
    // Preserve the original baseline scenario: a member disappears *before*
    // replacement capacity is added. Extended rounds also cover add-before-loss.
    if mode.is_none() {
        let durable = fs.metadata_snapshot().txid;
        drop(fs);
        std::fs::rename(&retired.path, &parked)?;
        let available: Vec<_> = images
            .iter()
            .filter(|p| **p != retired.path)
            .cloned()
            .collect();
        fs = reopen(
            &available,
            durable,
            "degraded mount before adding replacement",
        )?;
        verify(&fs)?;
        ensure!(
            fs.read_file(SENTINEL, false)? == expected,
            "pre-add degraded data mismatch"
        );
    }
    let replacement = dir.join(format!("member-{index:05}.img"));
    let replacement_size = if mode.is_none() || seed.is_multiple_of(2) {
        96 * 1024 * 1024
    } else {
        IMAGE_SIZE
    };
    let id = fs.add_block_device(replacement.clone(), replacement_size, false)?;
    images.push(replacement);
    fs.sync()?;
    let durable = fs.metadata_snapshot().txid;
    drop(fs);
    let available: Vec<_> = images
        .iter()
        .filter(|p| mode.is_some() || **p != retired.path)
        .cloned()
        .collect();
    fs = reopen(&available, durable, "mount after adding replacement")?;
    if mode.is_some() {
        check_online(&fs, images)?;
    } else {
        check_members(&fs, images)?;
    }
    ensure!(
        fs.metadata_snapshot().disks[&id].status == DiskStatus::Online,
        "replacement not online"
    );
    verify(&fs)?;
    if mode.is_some() {
        // A bounded layout transition must survive a reopen while old and new
        // layouts coexist. Five remaining members always suffice for either target.
        let current = fs.metadata_snapshot().config;
        let rejected = fs.reshape_layout(images.len(), 1, Some(0));
        ensure!(
            matches!(rejected, Err(ArgosError::NotEnoughDisks { .. })),
            "invalid reshape accepted"
        );
        let after = fs.metadata_snapshot();
        ensure!(
            (after.config.k, after.config.m) == (current.k, current.m) && after.reshape.is_none(),
            "rejected reshape modified layout"
        );
        let target = if (current.k, current.m) == (2, 2) {
            (3, 2)
        } else {
            (2, 2)
        };
        let start = fs.reshape_layout(target.0, target.1, Some(0))?;
        ensure!(
            !start.complete && start.remaining_files > 0,
            "reshape did not leave work to resume"
        );
        fs.sync()?;
        let durable = fs.metadata_snapshot().txid;
        drop(fs);
        fs = reopen(images, durable, "mount during reshape")?;
        ensure!(
            fs.metadata_snapshot().reshape.is_some(),
            "reshape progress lost on reopen"
        );
        verify(&fs)?;
        if let Some(cut) = cut {
            // Reshape may commit a bounded prefix before the interruption.
            // Logical contents, identities and all acknowledged transactions
            // must survive either outcome. Resume the same target afterwards.
            fs.sync()?;
            let durable = fs.metadata_snapshot().txid;
            drop(fs);
            let (interrupted, synced) = crate::atomic::run_interrupted(
                images,
                dir,
                &format!("reshape-{index:05}"),
                crate::atomic::WorkerAction::Reshape {
                    k: target.0,
                    m: target.1,
                },
                cut,
                false,
            )?;
            fs = reopen(
                images,
                synced.unwrap_or(durable).max(durable),
                "mount after interrupted reshape",
            )?;
            ensure!(
                fs.read_file(SENTINEL, false)? == expected,
                "interrupted reshape changed sentinel"
            );
            verify(&fs)?;
            if synced.is_some() {
                ensure!(
                    fs.metadata_snapshot().txid > durable,
                    "synced reshape step did not persist progress"
                );
            }
            if cut == 2 {
                ensure!(interrupted, "reshape interruption boundary was not reached");
            }
            let scrub = fs.scrub()?;
            ensure!(
                scrub.errors.is_empty() && scrub.unrecoverable_files == 0,
                "interrupted reshape scrub: {:?}",
                scrub.errors
            );
            fs.sync()?;
            verify_clean(&fs)?;
            // A separate one-shot write EIO in the same mixed-layout state.
            // Cut zero ensures the fault occurs before a mutating I/O. The
            // external oracle remains authoritative even if the request fails.
            if fs.metadata_snapshot().reshape.is_some() {
                let durable = fs.metadata_snapshot().txid;
                drop(fs);
                let (injected, synced) = crate::atomic::run_interrupted(
                    images,
                    dir,
                    &format!("reshape-eio-{index:05}"),
                    crate::atomic::WorkerAction::Reshape {
                        k: target.0,
                        m: target.1,
                    },
                    0,
                    true,
                )?;
                ensure!(injected, "reshape EIO boundary was not reached");
                fs = reopen(
                    images,
                    synced.unwrap_or(durable).max(durable),
                    "mount after reshape EIO",
                )?;
                ensure!(
                    fs.read_file(SENTINEL, false)? == expected,
                    "reshape EIO changed sentinel"
                );
                verify(&fs)?;
                let scrub = fs.scrub()?;
                ensure!(
                    scrub.errors.is_empty() && scrub.unrecoverable_files == 0,
                    "reshape EIO scrub: {:?}",
                    scrub.errors
                );
                fs.sync()?;
                verify_clean(&fs)?;
            }
        }
        expected = concurrent(&fs, seed ^ 0x41, &expected, || {
            let report = fs.reshape_layout(target.0, target.1, None)?;
            ensure!(
                report.complete && report.remaining_files == 0,
                "reshape incomplete"
            );
            Ok(())
        })
        .context("concurrent reshape")?;
        let meta = fs.metadata_snapshot();
        ensure!(
            meta.reshape.is_none() && (meta.config.k, meta.config.m) == target,
            "reshape final state mismatch"
        );
        ensure!(
            meta.inodes
                .values()
                .flat_map(|i| i.blocks.iter())
                .all(|b| b.layout_id == meta.current_write_layout),
            "stale stripe layout after reshape"
        );
        verify(&fs)?;
        // Explicit offline/online transition while a spare is present.
        fs.mark_disk(&retired.id, DiskStatus::Offline)?;
        ensure!(
            fs.metadata_snapshot().disks[&retired.id].status == DiskStatus::Offline,
            "offline transition lost"
        );
        ensure!(
            fs.read_file(SENTINEL, false)? == expected,
            "offline read mismatch"
        );
        verify(&fs)?;
        fs.mark_disk(&retired.id, DiskStatus::Online)?;
        check_online(&fs, images)?;
        if let Some(cut) = cut {
            fs.sync()?;
            let durable = fs.metadata_snapshot().txid;
            drop(fs);
            // A bounded member move may legitimately have no eligible work.
            // A completed no-op is legal; an interrupted move must preserve
            // both the external oracle and physical consistency.
            let (_, synced) = crate::atomic::run_interrupted(
                images,
                dir,
                &format!("member-move-{index:05}"),
                crate::atomic::WorkerAction::RebalanceMember { id: id.clone() },
                cut,
                false,
            )?;
            fs = reopen(
                images,
                synced.unwrap_or(durable).max(durable),
                "mount after interrupted member move",
            )?;
            ensure!(
                fs.read_file(SENTINEL, false)? == expected,
                "interrupted member move changed sentinel"
            );
            verify(&fs)?;
            let scrub = fs.scrub()?;
            ensure!(
                scrub.errors.is_empty() && scrub.unrecoverable_files == 0,
                "interrupted member move scrub: {:?}",
                scrub.errors
            );
            fs.sync()?;
            verify_clean(&fs)?;
        }
        expected = concurrent(&fs, seed ^ 0x42, &expected, || {
            let report = fs.rebalance_member(&id, 8)?;
            ensure!(report.moved_shards <= 8, "member rebalance exceeded budget");
            Ok(())
        })
        .context("concurrent member rebalance")?;
        verify(&fs)?;
        // Automatic maintenance runs with a foreground writer on the same pool.
        expected = concurrent(&fs, seed ^ 0x43, &expected, || {
            let report = fs.autopilot_once_with_config(AutopilotConfig {
                max_drains_per_run: 0,
                scrub_interval_sec: 0,
                rebalance_interval_sec: 0,
                scrub_files_per_run: 8,
                rebalance_files_per_run: 8,
                ..Default::default()
            })?;
            let actions = report["actions"]
                .as_array()
                .context("autopilot actions absent")?;
            ensure!(
                actions
                    .iter()
                    .any(|a| a["action"] == "verify-actions" && a["result"] == "ok"),
                "autopilot verification failed: {actions:?}"
            );
            ensure!(
                !actions
                    .iter()
                    .any(|a| a["action"] == "rebalance-skipped" || a["action"] == "scrub-skipped"),
                "autopilot maintenance failed: {actions:?}"
            );
            Ok(())
        })
        .context("concurrent autopilot")?;
        verify(&fs)?;
    }
    fs.sync()?;
    if mode.is_some() {
        verify_clean(&fs).context("physical consistency before departure")?;
    }
    std::fs::write(
        dir.join(format!("topology-{index:05}-before-departure.json")),
        serde_json::to_vec(&fs.metadata_snapshot())?,
    )?;
    let degraded: Vec<_> = images
        .iter()
        .filter(|p| **p != retired.path)
        .cloned()
        .collect();
    let removing = if mode.is_none() || matches!(mode, Some(Mode::OnlineDrain)) {
        fs
    } else {
        let durable = fs.metadata_snapshot().txid;
        drop(fs);
        std::fs::rename(&retired.path, &parked)?;
        let missing = reopen(&degraded, durable, "mount without selected member")?;
        std::fs::write(
            dir.join(format!("topology-{index:05}-degraded.json")),
            serde_json::to_vec(&missing.metadata_snapshot())?,
        )?;
        verify(&missing).context("degraded oracle before replacement")?;
        if matches!(mode, Some(Mode::Returning)) {
            expected = concurrent(&missing, seed ^ 0x45, &expected, || Ok(()))?;
            missing.sync()?;
            let durable = missing.metadata_snapshot().txid;
            drop(missing);
            std::fs::rename(&parked, &retired.path)?;
            let returned = reopen(images, durable, "mount with returning member")?;
            ensure!(
                returned.read_file(SENTINEL, false)? == expected,
                "returning member changed data"
            );
            verify(&returned).context("returning member oracle")?;
            returned.mark_disk(&retired.id, DiskStatus::Online)?;
            check_online(&returned, images)?;
            returned.sync()?;
            verify_clean(&returned).context("physical consistency after member return")?;
            std::fs::write(
                dir.join(format!("topology-{index:05}-returned.json")),
                serde_json::to_vec(&returned.metadata_snapshot())?,
            )?;
            let durable = returned.metadata_snapshot().txid;
            drop(returned);
            std::fs::rename(&retired.path, &parked)?;
            let again = reopen(&degraded, durable, "mount after second member departure")?;
            std::fs::write(
                dir.join(format!("topology-{index:05}-second-departure.json")),
                serde_json::to_vec(&again.metadata_snapshot())?,
            )?;
            verify(&again).context("second departure oracle")?;
            again
        } else {
            missing
        }
    };
    ensure!(
        removing.read_file(SENTINEL, false)? == expected,
        "pre-drain read mismatch"
    );
    expected = concurrent(&removing, seed ^ 0x44, &expected, || {
        removing.remove_disk(&retired.id)?;
        Ok(())
    })
    .context("concurrent drain/remove")?;
    ensure!(
        removing.metadata_snapshot().disks[&retired.id].status == DiskStatus::Removed,
        "retired member still active"
    );
    removing.sync()?;
    let durable = removing.metadata_snapshot().txid;
    drop(removing);
    if matches!(mode, Some(Mode::OnlineDrain)) {
        std::fs::rename(&retired.path, &parked)?;
    }
    *images = degraded;
    fs = reopen(images, durable, "mount after removal")?;
    check_online(&fs, images)?;
    ensure!(
        fs.read_file(SENTINEL, false)? == expected,
        "replacement changed foreground contents"
    );
    verify(&fs)?;
    verify_clean(&fs).context("post-removal physical consistency")?;
    fs.unlink_path(SENTINEL)?;
    std::fs::remove_file(parked)?;
    fs.sync()?;
    eprintln!(
        "ztest topology: round {index} mode={mode:?} retired {}, added {id}, active={}",
        retired.id,
        images.len()
    );
    Ok(fs)
}
