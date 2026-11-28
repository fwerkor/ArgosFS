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

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub enum Mutation {
    Rename,
    Delete,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum WorkerAction {
    Namespace(Mutation),
    Reshape { k: usize, m: usize },
    RebalanceMember { id: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MutationConfig {
    images: Vec<PathBuf>,
    operation: WorkerAction,
    cut: u64,
    #[serde(default)]
    eio: bool,
    events: PathBuf,
    synced: PathBuf,
}

pub fn worker(config: MutationConfig) -> Result<()> {
    let backend = Arc::new(
        FaultBackend::new(
            FaultConfig::default(),
            Arc::new(FaultStats::default()),
            Some(File::create(&config.events)?),
        )
        .with_deferred_boundary(RecoveryFault {
            after_io: config.cut,
            mutations_only: true,
            eio: config.eio,
        }),
    );
    let handle = backend.clone();
    let fs = ArgosFs::open_loop_with_backend(&config.images, true, move |inner| {
        handle.set_inner(inner);
        handle.clone() as Arc<dyn StorageBackend>
    })?;
    backend.enable_boundary();
    let result = (|| -> argosfs::Result<()> {
        match config.operation {
            WorkerAction::Namespace(Mutation::Rename) => fs.rename_path(SOURCE, TARGET)?,
            WorkerAction::Namespace(Mutation::Delete) => fs.unlink_path(SOURCE)?,
            WorkerAction::Reshape { k, m } => {
                fs.reshape_layout(k, m, Some(1))?;
            }
            WorkerAction::RebalanceMember { id } => {
                fs.rebalance_member(&id, 8)?;
            }
        }
        fs.sync()?;
        Ok(())
    })();
    if let Err(err) = result {
        // Only a recorded injected EIO plus an explicit device/durability
        // failure can explain an unsuccessful maintenance request. Corruption,
        // checksum failures, invalid arguments and panics remain fatal.
        let device_failure = matches!(&err, ArgosError::Io(io) if io.raw_os_error() == Some(libc::EIO))
            || matches!(
                &err,
                ArgosError::DeviceFailed { .. }
                    | ArgosError::QuorumUnavailable { .. }
                    | ArgosError::RetryableQuorumUnavailable { .. }
                    | ArgosError::IndeterminateCommit(_)
                    | ArgosError::CommittedDurabilityLoss(_)
                    | ArgosError::ReadonlyRequired(_)
            );
        if config.eio && device_failure && event_present(&config.events, "recovery_eio")? {
            eprintln!("maintenance rejected after injected EIO: {err}");
            return Ok(());
        }
        return Err(err.into());
    }
    // Publish the exact acknowledged transaction only after sync. A process
    // kill while constructing the marker must not look like a durable ack.
    let tmp = config.synced.with_extension("sync-tmp");
    let txid = fs.metadata_snapshot().txid;
    use std::io::Write;
    let mut file = File::create(&tmp)?;
    writeln!(file, "{txid}")?;
    file.sync_all()?;
    fs::rename(tmp, &config.synced)?;
    Ok(())
}

fn event_present(path: &Path, kind: &str) -> Result<bool> {
    Ok(fs::read_to_string(path)?.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| v["kind"] == kind)
    }))
}

/// Run a single bounded mutating operation in a disposable subprocess. The
/// boundary marker distinguishes deliberate interruption from an unrelated exit.
pub(crate) fn run_interrupted(
    images: &[PathBuf],
    dir: &Path,
    prefix: &str,
    operation: WorkerAction,
    cut: u64,
    eio: bool,
) -> Result<(bool, Option<u64>)> {
    let config = MutationConfig {
        images: images.to_vec(),
        operation: operation.clone(),
        cut,
        eio,
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
    let synced = if config.synced.exists() {
        Some(fs::read_to_string(&config.synced)?.trim().parse::<u64>()?)
    } else {
        None
    };
    let kind = if eio { "recovery_eio" } else { "recovery_cut" };
    let injected = event_present(&config.events, kind)?;
    let acceptable = if eio {
        status.success() && (injected || synced.is_some())
    } else {
        (status.success() && synced.is_some() && !injected)
            || (status.signal() == Some(libc::SIGKILL) && injected)
    };
    ensure!(acceptable, "unexpected mutation exit {status}");
    eprintln!("ztest model: {prefix} {operation:?} cut={cut}, eio={eio}, injected={injected}, synced_txid={synced:?}");
    Ok((injected, synced))
}

fn present(fs: &ArgosFs, path: &str) -> Result<Option<(u64, Vec<u8>)>> {
    match fs.resolve_path(path, false) {
        Ok(ino) => Ok(Some((ino, fs.read_file(path, false)?))),
        Err(ArgosError::NotFound(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn exercise(
    fs: ArgosFs,
    images: &[PathBuf],
    dir: &Path,
    index: usize,
    operation: Mutation,
    cut: u64,
) -> Result<ArgosFs> {
    fs.write_file(SOURCE, NEW, 0o644)?;
    fs.write_file(TARGET, OLD, 0o644)?;
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

    let (_, synced) = run_interrupted(
        images,
        dir,
        &format!("mutation-{index:05}"),
        WorkerAction::Namespace(operation),
        cut,
        false,
    )?;
    let fs = ArgosFs::open_loop(images, true)?;
    let old_source = Some((source, NEW.to_vec()));
    let old_target = Some((target, OLD.to_vec()));
    let new_target = match operation {
        Mutation::Rename => old_source.clone(),
        Mutation::Delete => old_target.clone(),
    };
    let found_source = present(&fs, SOURCE)?;
    let found_target = present(&fs, TARGET)?;
    ensure!(
        present(&fs, ANCHOR)? == old_source,
        "hard-linked anchor changed across crash"
    );
    let before = found_source == old_source && found_target == old_target;
    let after = found_source.is_none() && found_target == new_target;
    ensure!(
        after || (synced.is_none() && before),
        "mixed or rolled-back namespace transaction"
    );
    ensure!(
        fs.metadata_snapshot().txid >= synced.unwrap_or(before_txid).max(before_txid),
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
    let after_scrub_source = present(&fs, SOURCE)?;
    let after_scrub_target = present(&fs, TARGET)?;
    ensure!(
        found_source == after_scrub_source && found_target == after_scrub_target,
        "scrub changed namespace transaction state"
    );
    drop(fs);

    let fs = ArgosFs::open_loop(images, true).context("reopen after mutation recovery")?;
    ensure!(
        present(&fs, SOURCE)? == found_source
            && present(&fs, TARGET)? == found_target
            && present(&fs, ANCHOR)? == old_source,
        "recovery was not durable"
    );
    if found_source.is_some() {
        fs.unlink_path(SOURCE)?;
    }
    fs.unlink_path(TARGET)?;
    fs.unlink_path(ANCHOR)?;
    fs.sync()?;
    Ok(fs)
}
