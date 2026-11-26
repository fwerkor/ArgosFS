//! Supervisor: create a pool, repeatedly kill a workload child, and verify
//! crash recovery plus parity repair from a host-side oracle.

use crate::fault::FaultConfig;
use crate::oracle::Oracle;
use crate::workload::{ensure_dir, verify_oracle, WorkerConfig};
use anyhow::{bail, Context, Result};
use argosfs::types::{BackendKind, Compression, VolumeConfig};
use argosfs::ArgosFs;
use serde::Serialize;
use std::collections::BTreeMap;
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

        verify_recovery(&images, &oracle_path, cycle)
            .with_context(|| format!("cycle {cycle} recovery verification"))?;
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

fn verify_recovery(images: &[PathBuf], oracle_path: &Path, cycle: usize) -> Result<()> {
    let fs = ArgosFs::open_loop(images, true)
        .with_context(|| format!("open pool after cycle {cycle} crash"))?;
    let oracle = Oracle::load(oracle_path)?;
    if oracle.all_files().next().is_none() {
        bail!("cycle {cycle}: empty durability oracle");
    }
    // Remove unreferenced data left by the epoch that was in flight. This is
    // the same offline repair a user would run after a power loss.
    let report = fs
        .scrub()
        .with_context(|| format!("scrub after cycle {cycle} crash"))?;
    if report.unrecoverable_files != 0 {
        bail!(
            "cycle {cycle}: scrub found {} unrecoverable file(s): {:#?}",
            report.unrecoverable_files,
            report.errors
        );
    }
    if !report.errors.is_empty() {
        bail!("cycle {cycle}: scrub reported errors: {:#?}", report.errors);
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
    Ok(())
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
