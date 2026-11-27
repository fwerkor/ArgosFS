//! Supervisor: create a pool, repeatedly kill a workload child, and verify
//! crash recovery plus parity repair from a host-side oracle.

use crate::fault::FaultConfig;
use crate::oracle::Oracle;
use crate::workload::{cleanup_unpublished, ensure_dir, remove_tree, verify_oracle, WorkerConfig};
use anyhow::{bail, Context, Result};
use argosfs::types::{BackendKind, Compression, FsckReport, Metadata, ShardLocation, VolumeConfig};
use argosfs::ArgosFs;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SENTINEL_SIGKILL: i32 = libc::SIGKILL;

#[derive(Clone, Debug, Serialize)]
pub struct RunOutcome {
    pub seed: u64,
    pub cycles: usize,
    pub work_dir: PathBuf,
    pub faults: BTreeMap<String, u64>,
    pub total_faults: u64,
    pub recovered: bool,
    /// Committed files retired because the fault model destroyed more shards
    /// than the pool's parity budget could reconstruct.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expected_loss: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parity_drill: Option<ParityDrillReport>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ParityDrillReport {
    pub path: String,
    pub disk_id: String,
    pub offset: u64,
    pub damaged_files: u64,
    pub repaired_files: u64,
    pub reconstructed_shards: u64,
    pub unrecoverable_files: u64,
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub work_dir: Option<PathBuf>,
    pub keep_work_dir: bool,
    pub cycles: usize,
    pub seed: u64,
    pub devices: usize,
    pub k: usize,
    pub m: usize,
    pub image_size: u64,
    pub chunk_size: usize,
    pub threads: usize,
    pub max_file_bytes: usize,
    pub files_per_epoch: usize,
    pub keep_epochs: usize,
    pub kill_after_min: Duration,
    pub kill_after_max: Duration,
    pub min_faults: u64,
    pub parity_drill: bool,
    pub allow_expected_loss: bool,
    pub fault: FaultConfig,
}

struct SupervisorRng {
    state: u64,
}

impl SupervisorRng {
    fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn between(&mut self, low: Duration, high: Duration) -> Duration {
        let low_ms = low.as_millis() as u64;
        let high_ms = high.as_millis() as u64;
        if high_ms <= low_ms {
            return low;
        }
        let span = high_ms - low_ms + 1;
        Duration::from_millis(low_ms + self.next_u64() % span)
    }
}

pub fn run(config: RunConfig) -> Result<RunOutcome> {
    validate(&config)?;
    let seed = if config.seed == 0 {
        entropy_seed()
    } else {
        config.seed
    };
    let work_dir = match &config.work_dir {
        Some(path) => path.clone(),
        None => std::env::temp_dir().join(format!("argosfs-ztest-{seed:016x}")),
    };
    // Never delete a user-supplied directory, even when it is empty.
    // Atomic creation also rejects symlinks and concurrent runs.
    fs::create_dir(&work_dir).with_context(|| {
        format!(
            "create fresh work dir {} (must not exist)",
            work_dir.display()
        )
    })?;
    let work_dir = fs::canonicalize(&work_dir)?;
    eprintln!("ztest: seed {seed}, work dir {}", work_dir.display());
    let images = (0..config.devices)
        .map(|index| work_dir.join(format!("disk{index}.img")))
        .collect::<Vec<_>>();
    let oracle_path = work_dir.join("oracle.json");
    let worker_config_path = work_dir.join("worker.json");

    let volume_config = VolumeConfig {
        k: config.k,
        m: config.m,
        chunk_size: config.chunk_size,
        compression: Compression::Lz4,
        compression_level: 0,
        l2_cache_bytes: 0,
        fsname: "argosfs-ztest".to_string(),
        ..VolumeConfig::default()
    };
    {
        let fs = ArgosFs::create_loop(
            &images,
            volume_config,
            config.image_size,
            "argosfs-ztest",
            false,
        )
        .context("create ztest loop pool")?;
        ensure_dir(&fs, "/committed")?;
        ensure_dir(&fs, "/scratch")?;
        fs.sync()?;
    }

    let data_floor = images
        .iter()
        .map(|image| {
            argosfs::raw_store::inspect_device(BackendKind::LoopBlock, image.clone())
                .map(|(superblock, _)| superblock.data.offset)
        })
        .collect::<argosfs::Result<Vec<_>>>()?
        .into_iter()
        .min()
        .unwrap_or(16 * 1024 * 1024);

    let mut fault = config.fault.clone();
    fault.seed = seed;
    fault.data_floor = data_floor;
    let mut worker_config = WorkerConfig {
        images: images.clone(),
        oracle: oracle_path.clone(),
        fault_log: work_dir.join("cycle-000.faults.jsonl"),
        seed,
        threads: config.threads,
        max_file_bytes: config.max_file_bytes,
        files_per_epoch: config.files_per_epoch,
        keep_epochs: config.keep_epochs,
        fault,
        ready: work_dir.join("worker.ready"),
    };
    Oracle::default().save(&oracle_path)?;

    let executable = std::env::current_exe().context("locate ztest executable")?;
    let mut rng = SupervisorRng::new(seed ^ 0x5A5A_1234_DEAD_BEEF);
    let mut cycles_done = 0usize;
    let mut expected_loss = Vec::new();
    let mut faults = BTreeMap::<String, u64>::new();
    for cycle in 0..config.cycles {
        worker_config.fault_log = work_dir.join(format!("cycle-{cycle:03}.faults.jsonl"));
        File::create(&worker_config.fault_log)?;
        worker_config.seed = seed.wrapping_add(cycle as u64);
        worker_config.fault.seed = worker_config.seed;
        if worker_config.ready.exists() {
            fs::remove_file(&worker_config.ready)?;
        }
        fs::write(
            &worker_config_path,
            serde_json::to_vec_pretty(&worker_config)?,
        )?;
        let log_path = work_dir.join(format!("cycle-{cycle:03}.worker.log"));
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let stderr = stdout.try_clone()?;
        let mut child = Command::new(&executable)
            .arg("worker")
            .arg("--config")
            .arg(&worker_config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .with_context(|| format!("spawn ztest worker for cycle {cycle}"))?;

        // Do not count a crash during boot as a mounted-workload cycle.
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while !worker_config.ready.exists() {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "cycle {cycle}: worker exited before readiness: {status}; see {}",
                    log_path.display()
                );
            }
            if std::time::Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                bail!(
                    "cycle {cycle}: worker readiness timed out; see {}",
                    log_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let kill_after = rng.between(config.kill_after_min, config.kill_after_max);
        std::thread::sleep(kill_after);
        child.kill().context("kill ztest worker")?;
        let status = child.wait().context("wait for ztest worker")?;
        if status.signal() != Some(SENTINEL_SIGKILL) {
            let details = fs::read_to_string(&log_path).unwrap_or_default();
            bail!("cycle {cycle}: worker exited before SIGKILL with {status:?}\n{details}");
        }

        expected_loss.extend(
            verify_recovery(
                &images,
                &oracle_path,
                &worker_config.fault_log,
                cycle,
                config.allow_expected_loss,
            )
            .with_context(|| format!("cycle {cycle} recovery verification"))?,
        );
        for (kind, count) in summarize_faults(&worker_config.fault_log)? {
            *faults.entry(kind).or_default() += count;
        }
        cycles_done += 1;
        eprintln!(
            "ztest: cycle {cycle} killed after {} ms, recovered and verified",
            kill_after.as_millis()
        );
    }

    let total_faults: u64 = faults.values().sum();
    if total_faults < config.min_faults {
        bail!(
            "only {total_faults} faults injected, below --min-faults {}; raise rates or run longer",
            config.min_faults
        );
    }

    let parity_drill = if config.parity_drill {
        run_parity_drill(&images, &oracle_path)?
    } else {
        None
    };

    let outcome = RunOutcome {
        seed,
        cycles: cycles_done,
        work_dir: work_dir.clone(),
        faults,
        total_faults,
        recovered: true,
        expected_loss,
        parity_drill,
    };
    if !config.keep_work_dir {
        let _ = fs::remove_dir_all(&work_dir);
    }
    Ok(outcome)
}

fn validate(config: &RunConfig) -> Result<()> {
    let shards = config.k.checked_add(config.m).context("k+m overflow")?;
    if config.devices < shards {
        bail!(
            "--devices {} is smaller than k+m {}",
            config.devices,
            shards
        );
    }
    if config.k == 0 {
        bail!("--k must be positive");
    }
    if config.cycles == 0 {
        bail!("--cycles must be at least 1");
    }
    if config.kill_after_min > config.kill_after_max {
        bail!("--kill-after-ms-min must not exceed --kill-after-ms-max");
    }
    if config.files_per_epoch == 0 || config.max_file_bytes == 0 || config.keep_epochs == 0 {
        bail!("file count, maximum file size and retained epochs must be positive");
    }
    if config.parity_drill && config.m == 0 {
        bail!("parity drill requires m > 0; use --no-parity-drill for a parity-free layout");
    }
    if config.threads == 0 {
        bail!("--threads must be at least 1");
    }
    Ok(())
}

/// Open a writable recovery mount and remove disposable state.
///
/// The first writable open after a crash can be fenced by its own cleanup
/// transaction when the injected fault left the journal at a durability
/// boundary: the commit needs a metadata quorum, fails, and fences the mount.
/// That fence is in-memory only. Reopening performs recovery again, clears the
/// transient fence, and lets the cleanup finish; the same thing happens when a
/// service restarts a fenced mount. Only a fence that survives repeated
/// recovery is a hard failure.
fn open_and_clean_recovered_pool(
    images: &[PathBuf],
    oracle: &Oracle,
    cycle: usize,
) -> Result<ArgosFs> {
    with_cleanup_retries(
        3,
        |attempt| {
            ArgosFs::open_loop(images, true)
                .with_context(|| format!("open pool after cycle {cycle} crash (attempt {attempt})"))
        },
        |fs| {
            discard_disposable_state(fs, oracle)
                .with_context(|| format!("cycle {cycle}: discard disposable state"))
        },
    )
}

fn with_cleanup_retries<T>(
    attempts: usize,
    mut open: impl FnMut(usize) -> Result<T>,
    mut clean: impl FnMut(&T) -> Result<()>,
) -> Result<T> {
    let mut last_error = None;
    for attempt in 1..=attempts {
        let value = open(attempt)?;
        match clean(&value) {
            Ok(()) => return Ok(value),
            Err(err) => {
                eprintln!(
                    "ztest: disposable-state cleanup attempt {attempt}/{attempts} failed: {err:#}"
                );
                last_error = Some(err);
            }
        }
    }
    Err(last_error.expect("cleanup retries require at least one attempt"))
}

fn verify_recovery(
    images: &[PathBuf],
    oracle_path: &Path,
    fault_log: &Path,
    cycle: usize,
    allow_expected_loss: bool,
) -> Result<Vec<String>> {
    let mut oracle = Oracle::load(oracle_path)?;
    if oracle.all_files().next().is_none() {
        bail!("cycle {cycle}: empty durability oracle");
    }
    // Scratch and unpublished epochs are disposable by design: a crash may
    // leave a partial epoch, and scratch mutations may legitimately fail under
    // injected faults. Remove them before scrubbing so an unrecoverable
    // scratch file is not mistaken for durable data loss. The worker performs
    // the same cleanup before it starts.
    let fs = open_and_clean_recovered_pool(images, &oracle, cycle)?;
    let mut oracle_inodes = BTreeMap::new();
    for (path, _) in oracle.all_files() {
        let inode = fs
            .resolve_path(path, true)
            .with_context(|| format!("resolve committed path {path}"))?;
        oracle_inodes.insert(inode, path.clone());
    }
    // Remove unreferenced data left by the epoch that was in flight. This is
    // the same offline repair a user would run after a power loss.
    let report = fs
        .scrub()
        .with_context(|| format!("scrub after cycle {cycle} crash"))?;
    // An unrecoverable committed stripe is only a test failure when the
    // injected faults did not exceed the pool's parity budget. If more than
    // `m` shards in one stripe were hit by destructive faults, data loss is
    // the expected outcome of the fault model, not a filesystem bug.
    let meta = fs.metadata_snapshot();
    let classification = classify_scrub_failures(&meta, &oracle_inodes, fault_log, &report)
        .with_context(|| format!("cycle {cycle}: classify scrub failures against the fault log"))?;
    if !classification.unexpected.is_empty() {
        bail!(
            "cycle {cycle}: scrub reported unexpected damage:\n{}",
            classification.unexpected.join("\n")
        );
    }
    // The fault model knowingly destroyed more shards than parity can rebuild.
    // Retire those paths from the pool and from the durability oracle so later
    // cycles do not keep rediscovering a loss that was already explained.
    let expected_loss = classification.expected_loss_paths;
    if !expected_loss.is_empty() {
        if !allow_expected_loss {
            bail!("cycle {cycle}: possible over-budget damage to committed files (strict oracle): {expected_loss:?}");
        }
        for path in &expected_loss {
            fs.unlink_path(path)
                .with_context(|| format!("retire expected-loss file {path}"))?;
            for files in oracle.epochs.values_mut() {
                files.remove(path);
            }
        }
        fs.sync()
            .context("sync after retiring expected-loss files")?;
        oracle
            .save(oracle_path)
            .context("save oracle after retiring expected-loss files")?;
    }
    let failures = verify_oracle(&fs, &oracle);
    if !failures.is_empty() {
        bail!(
            "cycle {cycle}: {} committed path(s) failed verification:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    fs.mark_clean_unmount()?;
    drop(fs);
    let reopened = ArgosFs::open_loop(images, false).context("reopen recovered pool read-only")?;
    let failures = verify_oracle(&reopened, &oracle);
    if !failures.is_empty() {
        bail!("cycle {cycle}: persisted repair/oracle verification failed: {failures:#?}");
    }
    Ok(expected_loss)
}

/// Remove scratch files and committed epochs that the durable oracle does not
/// name. They are allowed to be absent, partial, or damaged after a crash, so
/// leaving them in the pool makes scrub report damage that no caller ever
/// promised to keep.
fn discard_disposable_state(fs: &ArgosFs, oracle: &Oracle) -> Result<()> {
    cleanup_unpublished(fs, oracle);
    remove_tree(fs, "/scratch");
    fs.sync().context("sync after disposable-state cleanup")?;
    if fs.attr_path("/scratch", false).is_ok() {
        bail!("/scratch still exists after disposable-state cleanup");
    }
    let committed = fs
        .resolve_path("/committed", true)
        .context("resolve /committed during disposable-state cleanup")?;
    for entry in fs
        .readdir(committed)
        .context("read /committed during disposable-state cleanup")?
    {
        let Some(epoch) = entry
            .name
            .strip_prefix("epoch-")
            .and_then(|digits| digits.parse::<u64>().ok())
        else {
            continue;
        };
        if !oracle.epochs.contains_key(&epoch) {
            bail!(
                "/committed/{} still exists after disposable-state cleanup",
                entry.name
            );
        }
    }
    Ok(())
}

/// Fault kinds that definitely leave a shard damaged or stale. Transient read
/// errors, read-side bit flips, and indeterminate EIO-after-write events are
/// excluded: they may be retried or may have committed correctly, so counting
/// them could hide a real recovery bug behind the expected-loss exemption.
const DESTRUCTIVE_FAULT_KINDS: &[&str] = &[
    "durable_bit_rot",
    "write_bit_rot",
    "torn_write",
    "lost_write",
];

#[derive(Debug)]
struct DestructiveFault {
    device: String,
    offset: u64,
}

fn read_destructive_faults(path: &Path) -> Result<Vec<DestructiveFault>> {
    let text =
        fs::read_to_string(path).with_context(|| format!("read fault log {}", path.display()))?;
    let mut faults = Vec::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // A SIGKILL can leave a partial trailing line.
            continue;
        };
        let Some(kind) = value.get("kind").and_then(|value| value.as_str()) else {
            continue;
        };
        if !DESTRUCTIVE_FAULT_KINDS.contains(&kind) {
            continue;
        }
        let Some(device) = value.get("device").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some(offset) = value.get("offset").and_then(|value| value.as_u64()) else {
            continue;
        };
        faults.push(DestructiveFault {
            device: device.to_string(),
            offset,
        });
    }
    Ok(faults)
}

/// Parse the scrub detail `inode N: stripe S ...`.
fn parse_unrecoverable_stripe(message: &str) -> Option<(u64, &str)> {
    let rest = message.strip_prefix("inode ")?;
    let (inode, rest) = rest.split_once(": stripe ")?;
    let inode = inode.parse::<u64>().ok()?;
    let stripe = rest
        .split([' ', ':'])
        .next()
        .filter(|stripe| !stripe.is_empty())?;
    Some((inode, stripe))
}

/// Return the shard slots in `stripe_id` that received a destructive injected
/// fault. A fault matches a shard when its device and I/O start offset fall in
/// the shard's raw extent.
fn stripe_damaged_slots(
    meta: &Metadata,
    inode_id: u64,
    stripe_id: &str,
    faults: &[DestructiveFault],
) -> Option<BTreeSet<usize>> {
    let inode = meta.inodes.get(&inode_id)?;
    let mut damaged = BTreeSet::new();
    let mut found = false;
    for block in &inode.blocks {
        if block.stripe_id != stripe_id {
            continue;
        }
        found = true;
        for shard in &block.shards {
            let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
                continue;
            };
            let end = extent
                .offset
                .saturating_add(extent.length.max(shard.size as u64));
            if faults.iter().any(|fault| {
                fault.device == extent.disk_id
                    && fault.offset >= extent.offset
                    && fault.offset < end
            }) {
                damaged.insert(shard.slot);
            }
        }
    }
    found.then_some(damaged)
}

#[derive(Debug, Default)]
struct ScrubClassification {
    /// Damage that is not explained by faults exceeding the parity budget.
    unexpected: Vec<String>,
    /// Committed files that must be retired because the fault model destroyed
    /// more shards than the pool's redundancy can reconstruct.
    expected_loss_paths: Vec<String>,
}

/// Drop scrub failures that are an expected consequence of the injected fault
/// model. An unrecoverable committed stripe is expected when more than `m`
/// distinct shards in it received destructive faults; with at most `m` damaged
/// shards, erasure coding should still have reconstructed it.
fn classify_scrub_failures(
    meta: &Metadata,
    oracle_inodes: &BTreeMap<u64, String>,
    fault_log: &Path,
    report: &FsckReport,
) -> Result<ScrubClassification> {
    if report.unrecoverable_files as usize > report.errors.len() {
        bail!(
            "scrub reported {} unrecoverable file(s) but only {} error message(s)",
            report.unrecoverable_files,
            report.errors.len()
        );
    }
    if report.errors.is_empty() {
        return Ok(ScrubClassification::default());
    }
    let faults = read_destructive_faults(fault_log)?;
    let mut classification = ScrubClassification::default();
    let mut expected_loss = BTreeSet::new();
    for message in &report.errors {
        let Some((inode_id, stripe_id)) = parse_unrecoverable_stripe(message) else {
            classification.unexpected.push(message.clone());
            continue;
        };
        let Some(path) = oracle_inodes.get(&inode_id) else {
            // Cleanup has already removed known disposable paths. Any
            // remaining error is unexplained and must fail the run.
            classification.unexpected.push(message.clone());
            continue;
        };
        let Some(damaged_slots) = stripe_damaged_slots(meta, inode_id, stripe_id, &faults) else {
            classification
                .unexpected
                .push(format!("{message} (stripe layout not found)"));
            continue;
        };
        if damaged_slots.len() > meta.config.m {
            eprintln!(
                "ztest: expected loss: {path} inode {inode_id} stripe {stripe_id} had {} destructive injected fault shard(s), m={} (slots {:?})",
                damaged_slots.len(),
                meta.config.m,
                damaged_slots
            );
            expected_loss.insert(path.clone());
        } else {
            classification.unexpected.push(format!(
                "{message} (only {} shard(s) had destructive injected faults, m={})",
                damaged_slots.len(),
                meta.config.m
            ));
        }
    }
    classification.expected_loss_paths = expected_loss.into_iter().collect();
    Ok(classification)
}

/// Deterministic end-to-end parity check after the random cycles.
///
/// Random bit rot can miss the data extents on a short run. This drill picks a
/// committed file's data shard, damages it in the backing image, and requires
/// scrub to detect, reconstruct from parity, and keep the oracle intact.
fn run_parity_drill(images: &[PathBuf], oracle_path: &Path) -> Result<Option<ParityDrillReport>> {
    let (path, disk_id, disk_path, offset, length) = {
        let fs = ArgosFs::open_loop(images, true).context("open pool for parity drill")?;
        let oracle = Oracle::load(oracle_path)?;
        if oracle.all_files().next().is_none() {
            return Ok(None);
        }
        let meta = fs.metadata_snapshot();
        if meta.config.m == 0 {
            return Ok(None);
        }
        let mut candidate = None;
        'paths: for (path, _) in oracle.all_files() {
            let Ok(ino) = fs.resolve_path(path, true) else {
                continue;
            };
            let Some(inode) = meta.inodes.get(&ino) else {
                continue;
            };
            for block in &inode.blocks {
                for shard in &block.shards {
                    if shard.slot >= meta.config.k {
                        continue;
                    }
                    let Some(argosfs::types::ShardLocation::RawExtent(extent)) = &shard.location
                    else {
                        continue;
                    };
                    if extent.length == 0 || shard.size == 0 {
                        continue;
                    }
                    let Some(disk) = meta.disks.get(&extent.disk_id) else {
                        continue;
                    };
                    // Only corrupt a backing file created by this run, never
                    // an arbitrary path read from recovered metadata.
                    let disk_path = fs::canonicalize(&disk.path)?;
                    if !images.contains(&disk_path) {
                        bail!(
                            "parity drill selected a path outside the test pool: {}",
                            disk_path.display()
                        );
                    }
                    candidate = Some((
                        path.clone(),
                        extent.disk_id.clone(),
                        disk_path,
                        extent.offset,
                        extent.length.min(shard.size as u64),
                    ));
                    break 'paths;
                }
            }
        }
        let Some(candidate) = candidate else {
            return Err(anyhow::anyhow!(
                "parity drill could not find a committed data shard"
            ));
        };
        candidate
    };

    let payload_len = length.min(4096) as usize;
    let mut payload = vec![0u8; payload_len];
    let damage_offset = offset + (length.saturating_sub(payload.len() as u64)) / 2;
    {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&disk_path)
            .with_context(|| format!("open backing image {}", disk_path.display()))?;
        file.read_exact_at(&mut payload, damage_offset)?;
        payload[0] ^= 1;
        file.write_all_at(&payload, damage_offset)
            .with_context(|| {
                format!(
                    "damage raw shard at {damage_offset} on {}",
                    disk_path.display()
                )
            })?;
        file.sync_all()?;
    }

    let fs = ArgosFs::open_loop(images, true).context("reopen pool for parity drill")?;
    let report = fs.scrub().context("scrub after parity drill damage")?;
    if report.damaged_files == 0 || report.repaired_files == 0 || report.reconstructed_shards == 0 {
        bail!(
            "parity drill damage was not detected and repaired: damaged={} repaired={} errors={:#?}",
            report.damaged_files,
            report.repaired_files,
            report.errors
        );
    }
    if report.unrecoverable_files != 0 || !report.errors.is_empty() {
        bail!(
            "parity drill made {} file(s) unrecoverable: {:#?}",
            report.unrecoverable_files,
            report.errors
        );
    }
    let oracle = Oracle::load(oracle_path)?;
    let failures = verify_oracle(&fs, &oracle);
    if !failures.is_empty() {
        bail!(
            "parity drill left {} oracle path(s) damaged:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
    fs.mark_clean_unmount()?;
    drop(fs);
    let reopened = ArgosFs::open_loop(images, false).context("reopen parity-repaired pool")?;
    let persisted = reopened.fsck(false, false)?;
    if persisted.damaged_files != 0
        || persisted.unrecoverable_files != 0
        || !persisted.errors.is_empty()
    {
        bail!("parity repair did not persist: {persisted:#?}");
    }
    let failures = verify_oracle(&reopened, &oracle);
    if !failures.is_empty() {
        bail!("persisted parity repair violated oracle: {failures:#?}");
    }
    Ok(Some(ParityDrillReport {
        path,
        disk_id,
        offset: damage_offset,
        damaged_files: report.damaged_files,
        repaired_files: report.repaired_files,
        reconstructed_shards: report.reconstructed_shards,
        unrecoverable_files: report.unrecoverable_files,
    }))
}

fn summarize_faults(path: &Path) -> Result<BTreeMap<String, u64>> {
    let text = fs::read_to_string(path)?;
    let mut counts = BTreeMap::new();
    for line in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            // A SIGKILL can leave a partial trailing line.
            continue;
        };
        if let Some(kind) = value.get("kind").and_then(|kind| kind.as_str()) {
            if kind == "dropped_pending" {
                continue;
            }
            *counts.entry(kind.to_string()).or_insert(0) += 1;
        }
    }
    Ok(counts)
}

fn entropy_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    nanos ^ ((std::process::id() as u64) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::FileRecord;
    use argosfs::types::ShardLocation;

    /// A damaged scratch file must not be reported as durable data loss. Only
    /// the host-side oracle names data that recovery promises to preserve.
    #[test]
    fn damaged_scratch_does_not_fail_recovery() {
        let temp = tempfile::TempDir::new().unwrap();
        let images = (0..4)
            .map(|index| temp.path().join(format!("disk{index}.img")))
            .collect::<Vec<_>>();
        let oracle_path = temp.path().join("oracle.json");
        let committed = vec![0x5Au8; 8192];
        let scratch = vec![0xA5u8; 8192];
        let mut extents = Vec::new();
        {
            let fs = ArgosFs::create_loop(
                &images,
                VolumeConfig {
                    k: 2,
                    m: 2,
                    chunk_size: 4096,
                    compression: Compression::None,
                    fsname: "argosfs-ztest-unit".to_string(),
                    ..VolumeConfig::default()
                },
                32 * 1024 * 1024,
                "argosfs-ztest-unit",
                false,
            )
            .unwrap();
            ensure_dir(&fs, "/committed").unwrap();
            ensure_dir(&fs, "/scratch").unwrap();
            fs.write_file("/committed/keep.bin", &committed, 0o644)
                .unwrap();
            fs.write_file("/scratch/bad.bin", &scratch, 0o644).unwrap();
            fs.sync().unwrap();

            let mut oracle = Oracle::default();
            oracle.epochs.insert(
                1,
                BTreeMap::from([(
                    "/committed/keep.bin".to_string(),
                    FileRecord::for_bytes(&committed),
                )]),
            );
            oracle.save(&oracle_path).unwrap();

            let meta = fs.metadata_snapshot();
            let ino = fs.resolve_path("/scratch/bad.bin", true).unwrap();
            let inode = meta.inodes.get(&ino).unwrap();
            for shard in inode.blocks[0].shards.iter().take(3) {
                let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
                    panic!("scratch shard is not a raw extent");
                };
                let disk = meta.disks.get(&extent.disk_id).unwrap();
                extents.push((disk.path.clone(), extent.offset));
            }
        }
        assert_eq!(extents.len(), 3);
        for (path, offset) in extents {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mut byte = [0u8; 1];
            file.read_exact_at(&mut byte, offset).unwrap();
            byte[0] ^= 0xFF;
            file.write_all_at(&byte, offset).unwrap();
            file.sync_all().unwrap();
        }

        verify_recovery(
            &images,
            &oracle_path,
            &temp.path().join("cycle-000.faults.jsonl"),
            0,
            false,
        )
        .expect("damaged scratch file must not fail committed-data recovery");
    }

    /// A committed stripe with more than `m` destructively faulted shards is
    /// expected data loss under the fault model. At `m` or fewer it is a real
    /// failure, because parity should still have reconstructed it.
    #[test]
    fn fault_budget_exceeded_is_expected_loss() {
        let temp = tempfile::TempDir::new().unwrap();
        let images = (0..4)
            .map(|index| temp.path().join(format!("disk{index}.img")))
            .collect::<Vec<_>>();
        let oracle_path = temp.path().join("oracle.json");
        let fault_log = temp.path().join("cycle-000.faults.jsonl");
        let committed = vec![0x5Au8; 8192];
        let fs = ArgosFs::create_loop(
            &images,
            VolumeConfig {
                k: 2,
                m: 2,
                chunk_size: 4096,
                compression: Compression::None,
                fsname: "argosfs-ztest-unit".to_string(),
                ..VolumeConfig::default()
            },
            32 * 1024 * 1024,
            "argosfs-ztest-unit",
            false,
        )
        .unwrap();
        ensure_dir(&fs, "/committed").unwrap();
        fs.write_file("/committed/keep.bin", &committed, 0o644)
            .unwrap();
        fs.sync().unwrap();

        let mut oracle = Oracle::default();
        oracle.epochs.insert(
            1,
            BTreeMap::from([(
                "/committed/keep.bin".to_string(),
                FileRecord::for_bytes(&committed),
            )]),
        );
        oracle.save(&oracle_path).unwrap();

        let meta = fs.metadata_snapshot();
        let ino = fs.resolve_path("/committed/keep.bin", true).unwrap();
        let inode = meta.inodes.get(&ino).unwrap();
        let block = &inode.blocks[0];
        let stripe = block.stripe_id.clone();
        let shards = block
            .shards
            .iter()
            .filter_map(|shard| match &shard.location {
                Some(ShardLocation::RawExtent(extent)) => {
                    Some((extent.disk_id.clone(), extent.offset))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(shards.len() >= 3);
        let report = FsckReport {
            unrecoverable_files: 1,
            errors: vec![format!(
                "inode {ino}: stripe {stripe} has 1/2 verified shards available"
            )],
            ..FsckReport::default()
        };

        let write_log = |count: usize| {
            let lines = shards
                .iter()
                .take(count)
                .map(|(device, offset)| {
                    serde_json::json!({
                        "kind": "durable_bit_rot",
                        "device": device,
                        "offset": offset,
                        "bytes": 2,
                    })
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(&fault_log, format!("{lines}\n")).unwrap();
        };

        let oracle_inodes = BTreeMap::from([(ino, "/committed/keep.bin".to_string())]);

        // m + 1 damaged shards: expected loss, not a filesystem failure.
        write_log(3);
        let classification =
            classify_scrub_failures(&meta, &oracle_inodes, &fault_log, &report).unwrap();
        assert!(
            classification.unexpected.is_empty(),
            "more than m damaged shards should be expected loss: {:?}",
            classification.unexpected
        );
        assert_eq!(
            classification.expected_loss_paths,
            vec!["/committed/keep.bin".to_string()]
        );

        // m damaged shards: parity should have recovered the stripe.
        write_log(2);
        let classification =
            classify_scrub_failures(&meta, &oracle_inodes, &fault_log, &report).unwrap();
        assert!(
            !classification.unexpected.is_empty(),
            "damage within the parity budget must still fail the run"
        );
        assert!(classification.expected_loss_paths.is_empty());
    }

    /// End-to-end: an over-parity committed loss is retired from the pool and
    /// from the oracle instead of failing every subsequent recovery cycle.
    #[test]
    fn expected_loss_retires_committed_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let images = (0..4)
            .map(|index| temp.path().join(format!("disk{index}.img")))
            .collect::<Vec<_>>();
        let oracle_path = temp.path().join("oracle.json");
        let fault_log = temp.path().join("cycle-000.faults.jsonl");
        let committed = vec![0x5Au8; 8192];
        let mut extents = Vec::new();
        {
            let fs = ArgosFs::create_loop(
                &images,
                VolumeConfig {
                    k: 2,
                    m: 2,
                    chunk_size: 4096,
                    compression: Compression::None,
                    fsname: "argosfs-ztest-unit".to_string(),
                    ..VolumeConfig::default()
                },
                32 * 1024 * 1024,
                "argosfs-ztest-unit",
                false,
            )
            .unwrap();
            ensure_dir(&fs, "/committed").unwrap();
            fs.write_file("/committed/keep.bin", &committed, 0o644)
                .unwrap();
            fs.sync().unwrap();

            let mut oracle = Oracle::default();
            oracle.epochs.insert(
                1,
                BTreeMap::from([(
                    "/committed/keep.bin".to_string(),
                    FileRecord::for_bytes(&committed),
                )]),
            );
            oracle.save(&oracle_path).unwrap();

            let meta = fs.metadata_snapshot();
            let ino = fs.resolve_path("/committed/keep.bin", true).unwrap();
            let inode = meta.inodes.get(&ino).unwrap();
            let block = &inode.blocks[0];
            for shard in block.shards.iter().take(3) {
                let Some(ShardLocation::RawExtent(extent)) = &shard.location else {
                    panic!("committed shard is not a raw extent");
                };
                let disk = meta.disks.get(&extent.disk_id).unwrap();
                extents.push((disk.path.clone(), extent.disk_id.clone(), extent.offset));
            }
        }
        assert_eq!(extents.len(), 3);

        let lines = extents
            .iter()
            .map(|(_, device, offset)| {
                serde_json::json!({
                    "kind": "durable_bit_rot",
                    "device": device,
                    "offset": offset,
                    "bytes": 2,
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&fault_log, format!("{lines}\n")).unwrap();
        for (path, _, offset) in extents {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            let mut byte = [0u8; 1];
            file.read_exact_at(&mut byte, offset).unwrap();
            byte[0] ^= 0xFF;
            file.write_all_at(&byte, offset).unwrap();
            file.sync_all().unwrap();
        }

        assert!(verify_recovery(&images, &oracle_path, &fault_log, 0, false).is_err());
        assert_eq!(Oracle::load(&oracle_path).unwrap().all_files().count(), 1);
        let expected =
            verify_recovery(&images, &oracle_path, &fault_log, 0, true).expect("expected loss");
        assert_eq!(expected, vec!["/committed/keep.bin".to_string()]);
        assert_eq!(
            Oracle::load(&oracle_path).unwrap().all_files().count(),
            0,
            "expected-loss path must be removed from the oracle"
        );
        let fs = ArgosFs::open_loop(&images, false).unwrap();
        assert!(
            fs.resolve_path("/committed/keep.bin", true).is_err(),
            "expected-loss file must be removed from the pool"
        );
    }

    #[test]
    fn cleanup_retries_until_a_fresh_open_succeeds() {
        let mut opens = 0;
        let mut cleans = 0;
        let value = with_cleanup_retries(
            3,
            |_| {
                opens += 1;
                Ok(opens)
            },
            |_| {
                cleans += 1;
                if cleans < 3 {
                    bail!("transient write fence");
                }
                Ok(())
            },
        )
        .expect("third attempt should succeed");
        assert_eq!(value, 3);
        assert_eq!(opens, 3);
        assert_eq!(cleans, 3);
    }

    #[test]
    fn cleanup_surfaces_the_last_failure_after_all_attempts() {
        let mut cleans = 0;
        let err = with_cleanup_retries(
            3,
            |_| Ok(()),
            |_| {
                cleans += 1;
                bail!("persistent cleanup failure {cleans}");
            },
        )
        .expect_err("all attempts fail");
        assert_eq!(cleans, 3);
        assert!(format!("{err:#}").contains("persistent cleanup failure 3"));
    }
}
