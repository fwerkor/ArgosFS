//! Crash consistency of mutable namespace transactions. A rename replacing
//! an existing destination or an unlink has precisely two legal pre-sync
//! states; once sync returns the new state is mandatory.
use crate::fault::{FaultBackend, FaultConfig, FaultStats, RecoveryFault};
use crate::invariants::verify_clean;
use anyhow::{ensure, Context, Result};
use argosfs::storage::backend::StorageBackend;
use argosfs::{ArgosError, ArgosFs};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SOURCE: &str = "/atomic-source";
const TARGET: &str = "/atomic-target";
const ANCHOR: &str = "/atomic-anchor";
const OLD: &[u8] = b"old target";
const NEW: &[u8] = b"source data";
const REPLACEMENT: &[u8] = b"replacement payload";

fn payload() -> Vec<u8> {
    (0..49152)
        .map(|i| ((i * 37 + i / 251) % 256) as u8)
        .collect()
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Mutation {
    Rename,
    Delete,
    Overwrite,
    Range,
    Truncate,
    Link,
    Quota,
    Xattr,
}

impl Mutation {
    pub const ALL: [Self; 8] = [
        Self::Rename,
        Self::Delete,
        Self::Overwrite,
        Self::Range,
        Self::Truncate,
        Self::Link,
        Self::Quota,
        Self::Xattr,
    ];
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MutationConfig {
    images: Vec<PathBuf>,
    operation: Mutation,
    cut: u64,
    events: PathBuf,
    synced: PathBuf,
}

pub fn worker(config: MutationConfig) -> Result<()> {
    let backend = Arc::new(
        FaultBackend::new(
            FaultConfig::default(),
            Arc::new(FaultStats::default()),
            Some(File::create(config.events)?),
        )
        .with_deferred_boundary(RecoveryFault {
            after_io: config.cut,
            mutations_only: true,
            eio: false,
        }),
    );
    let handle = backend.clone();
    let fs = ArgosFs::open_loop_with_backend(&config.images, true, move |inner| {
        handle.set_inner(inner);
        handle.clone() as Arc<dyn StorageBackend>
    })?;
    backend.enable_boundary();
    match config.operation {
        Mutation::Rename => fs.rename_path(SOURCE, TARGET)?,
        Mutation::Delete => fs.unlink_path(SOURCE)?,
        Mutation::Overwrite => fs.write_file(SOURCE, &payload(), 0o644)?,
        Mutation::Range => {
            let ino = fs.resolve_path(SOURCE, true)?;
            ensure!(
                fs.write_inode_range(ino, 32760, REPLACEMENT)? == REPLACEMENT.len(),
                "short range mutation"
            );
        }
        Mutation::Truncate => fs.truncate_path(SOURCE, 17000)?,
        Mutation::Link => {
            fs.link_at(
                fs.resolve_path(SOURCE, true)?,
                fs.resolve_path("/", true)?,
                OsStr::new("atomic-extra"),
            )?;
        }
        Mutation::Quota => fs.set_directory_quota("/atomic-dir", 65536)?,
        Mutation::Xattr => {
            fs.setxattr_inode(fs.resolve_path(SOURCE, true)?, "user.atomic", REPLACEMENT)?
        }
    }
    fs.sync()?;
    let count = backend.finish_boundary();
    use std::io::Write;
    let mut marker = File::create(config.synced)?;
    writeln!(marker, "{count}")?;
    marker.sync_all()?;
    Ok(())
}

fn present(fs: &ArgosFs, path: &str) -> Result<Option<(u64, Vec<u8>)>> {
    match fs.resolve_path(path, false) {
        Ok(ino) => Ok(Some((ino, fs.read_file(path, false)?))),
        Err(ArgosError::NotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

struct Observation {
    fs: ArgosFs,
    interrupted: bool,
    boundaries: Option<u64>,
}

pub fn exercise(
    fs: ArgosFs,
    images: &[PathBuf],
    dir: &Path,
    index: usize,
    operation: Mutation,
    cut: u64,
) -> Result<ArgosFs> {
    Ok(exercise_observed(fs, images, dir, index, operation, cut)?.fs)
}

fn exercise_observed(
    fs: ArgosFs,
    images: &[PathBuf],
    dir: &Path,
    index: usize,
    operation: Mutation,
    cut: u64,
) -> Result<Observation> {
    let initial = if matches!(operation, Mutation::Range | Mutation::Truncate) {
        payload()
    } else {
        NEW.to_vec()
    };
    fs.write_file(SOURCE, &initial, 0o644)?;
    fs.write_file(TARGET, OLD, 0o644)?;
    fs.mkdir("/atomic-dir", 0o755)?;
    fs.setxattr_inode(fs.resolve_path(SOURCE, true)?, "user.atomic", b"old")?;
    let source = fs.resolve_path(SOURCE, false)?;
    let target = fs.resolve_path(TARGET, false)?;
    fs.link_at(
        source,
        fs.resolve_path("/", true)?,
        OsStr::new("atomic-anchor"),
    )?;
    fs.sync()?;
    let before_txid = fs.metadata_snapshot().txid;
    drop(fs);

    let prefix = format!("mutation-{index:05}");
    let config = MutationConfig {
        images: images.to_vec(),
        operation,
        cut,
        events: dir.join(format!("{prefix}.events")),
        synced: dir.join(format!("{prefix}.synced")),
    };
    let config_file = dir.join(format!("{prefix}.json"));
    fs::write(&config_file, serde_json::to_vec_pretty(&config)?)?;
    let log = File::create(dir.join(format!("{prefix}.log")))?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("mutation-worker")
        .arg("--config")
        .arg(config_file)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            anyhow::bail!("mutation worker timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let boundaries = if config.synced.exists() {
        Some(fs::read_to_string(&config.synced)?.trim().parse::<u64>()?)
    } else {
        None
    };
    let synced = boundaries.is_some();
    let events = fs::read_to_string(&config.events)?;
    let cut_observed = events.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| v["kind"] == "recovery_cut")
    });
    ensure!(
        (status.success() && synced && !cut_observed)
            || (status.signal() == Some(libc::SIGKILL) && cut_observed),
        "unexpected mutation exit {status}"
    );

    eprintln!("ztest model: mutation {index} {operation:?} cut={cut}, interrupted={cut_observed}, synced={synced}");
    let fs = ArgosFs::open_loop(images, true)?;
    let old_source = Some((source, initial.clone()));
    let old_target = Some((target, OLD.to_vec()));
    let mut new_source = old_source.clone();
    let mut new_target = old_target.clone();
    let mut new_anchor = old_source.clone();
    let mut new_extra = None;
    let mut new_quota = None;
    let mut new_xattr = b"old".to_vec();
    match operation {
        Mutation::Rename => {
            new_source = None;
            new_target = old_source.clone();
        }
        Mutation::Delete => new_source = None,
        Mutation::Overwrite => {
            new_source = Some((source, payload()));
            new_anchor = new_source.clone();
        }
        Mutation::Range => {
            let mut bytes = initial.clone();
            bytes[32760..32760 + REPLACEMENT.len()].copy_from_slice(REPLACEMENT);
            new_source = Some((source, bytes));
            new_anchor = new_source.clone();
        }
        Mutation::Truncate => {
            new_source = Some((source, initial[..17000].to_vec()));
            new_anchor = new_source.clone();
        }
        Mutation::Link => new_extra = old_source.clone(),
        Mutation::Quota => new_quota = Some(65536),
        Mutation::Xattr => new_xattr = REPLACEMENT.to_vec(),
    }
    let observe = |fs: &ArgosFs| -> Result<_> {
        Ok((
            present(fs, SOURCE)?,
            present(fs, TARGET)?,
            present(fs, ANCHOR)?,
            present(fs, "/atomic-extra")?,
            fs.directory_quota("/atomic-dir")?.0,
            fs.getxattr_inode(source, "user.atomic")?,
        ))
    };
    let found = observe(&fs)?;
    let before = found
        == (
            old_source.clone(),
            old_target,
            old_source,
            None,
            None,
            b"old".to_vec(),
        );
    let after = found
        == (
            new_source, new_target, new_anchor, new_extra, new_quota, new_xattr,
        );
    ensure!(
        after || (!synced && before),
        "mixed or rolled-back transaction: {operation:?} cut {cut}"
    );
    ensure!(
        fs.metadata_snapshot().txid >= before_txid,
        "durable transaction ID regressed"
    );

    // A crash can leave reclaimable allocations, but must not lose any
    // reachable state or leave corrupt structure after repair.
    let scrub = fs.scrub()?;
    ensure!(
        scrub.errors.is_empty() && scrub.unrecoverable_files == 0,
        "mutation recovery scrub: {:?}",
        scrub.errors
    );
    fs.sync()?;
    verify_clean(&fs)?;
    ensure!(observe(&fs)? == found, "scrub changed transaction state");
    drop(fs);

    let fs = ArgosFs::open_loop(images, true).context("reopen after mutation recovery")?;
    ensure!(observe(&fs)? == found, "recovery was not durable");
    if found.0.is_some() {
        fs.unlink_path(SOURCE)?;
    }
    if found.3.is_some() {
        fs.unlink_path("/atomic-extra")?;
    }
    fs.unlink_path(TARGET)?;
    fs.unlink_path(ANCHOR)?;
    fs.rmdir_path("/atomic-dir")?;
    fs.sync()?;
    Ok(Observation {
        fs,
        interrupted: cut_observed,
        boundaries,
    })
}

/// Each trial uses a newly formatted pool, avoiding state left by earlier cuts.
pub fn sweep(dir: &Path, paged: bool, max_boundaries: u64, operations: &[Mutation]) -> Result<()> {
    use argosfs::types::{Compression, VolumeConfig};
    fs::create_dir(dir)?;
    let dir = fs::canonicalize(dir)?;
    let mut total = 0u64;
    for &operation in operations {
        let phase = dir.join(format!("{operation:?}"));
        fs::create_dir(&phase)?;
        let trial = |cut: u64, name: &str| -> Result<(bool, Option<u64>)> {
            let root = phase.join(name);
            fs::create_dir(&root)?;
            let images: Vec<_> = (0..3).map(|i| root.join(format!("disk{i}.img"))).collect();
            let config = VolumeConfig {
                k: 2,
                m: 1,
                chunk_size: 32768,
                compression: Compression::None,
                l2_cache_bytes: 0,
                ..VolumeConfig::default()
            };
            let create = if paged {
                ArgosFs::create_loop_paged
            } else {
                ArgosFs::create_loop_legacy
            };
            let fs = create(&images, config, 64 * 1024 * 1024, "ztest-sweep", false)?;
            let outcome = exercise_observed(fs, &images, &root, 0, operation, cut)
                .with_context(|| format!("sweep {operation:?} cut={cut}"))?;
            verify_clean(&outcome.fs)?;
            drop(outcome.fs);
            for image in images {
                std::fs::remove_file(image)?;
            }
            Ok((outcome.interrupted, outcome.boundaries))
        };
        // Paged checkpoints may take a different number of writes depending
        // on batching and tree layout. Count multiple complete executions,
        // cover their maximum, then probe the next boundary. A short complete
        // execution is not a crash and must not count as covered.
        let mut count = 0;
        for sample in 0..3 {
            let (_, measured) = trial(u64::MAX, &format!("baseline-{sample}"))?;
            count = count.max(measured.context("baseline did not synchronize")?);
        }
        ensure!(count > 0, "transaction had no mutating I/O");
        let mut cut = 0;
        loop {
            ensure!(
                count <= max_boundaries,
                "{operation:?}: {count} boundaries exceeds configured limit {max_boundaries}"
            );
            if cut < count {
                let mut reached = false;
                for attempt in 0..8 {
                    let (interrupted, measured) = trial(cut, &format!("cut-{cut:05}-{attempt}"))?;
                    if interrupted {
                        reached = true;
                        total += 1;
                        break;
                    }
                    ensure!(
                        measured.is_some_and(|n| n <= cut),
                        "missed reachable boundary {cut}"
                    );
                }
                ensure!(reached, "{operation:?}: boundary {cut} observed in a baseline but not reached in eight attempts");
                cut += 1;
                continue;
            }
            let mut extended = false;
            for sample in 0..3 {
                let (interrupted, measured) = trial(cut, &format!("end-{cut:05}-{sample}"))?;
                if interrupted {
                    total += 1;
                    cut += 1;
                    count = cut;
                    extended = true;
                    break;
                }
                ensure!(
                    measured.is_some_and(|n| n <= cut),
                    "missed reachable end boundary"
                );
            }
            if !extended {
                break;
            }
        }
        eprintln!("ztest sweep: {operation:?}: {count} cut points passed");
    }
    println!("ztest sweep: {total} observed cuts, paged={paged}");
    Ok(())
}
