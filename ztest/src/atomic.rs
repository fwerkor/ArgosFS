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
    }
    fs.sync()?;
    File::create(config.synced)?.sync_all()?;
    Ok(())
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
    let synced = config.synced.exists();
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
        after || (!synced && before),
        "mixed or rolled-back namespace transaction"
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
