//! Mounted workload executed inside a disposable, crash-killed child process.

use crate::fault::{FaultBackend, FaultConfig, FaultStats};
use crate::oracle::{FileRecord, Oracle};
use anyhow::{anyhow, Context, Result};
use argosfs::storage::backend::StorageBackend;
use argosfs::types::NodeKind;
use argosfs::{ArgosError, ArgosFs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub images: Vec<PathBuf>,
    pub oracle: PathBuf,
    pub fault_log: PathBuf,
    pub seed: u64,
    pub threads: usize,
    pub max_file_bytes: usize,
    pub files_per_epoch: usize,
    pub keep_epochs: usize,
    pub fault: FaultConfig,
}

struct LocalModel {
    files: BTreeMap<String, Vec<u8>>,
    sequence: u64,
}

impl LocalModel {
    fn new() -> Self {
        Self {
            files: BTreeMap::new(),
            sequence: 0,
        }
    }

    fn name(&mut self, prefix: &str) -> String {
        let value = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        format!("{prefix}-{value:08}")
    }
}

struct LocalRng {
    state: u64,
}

impl LocalRng {
    fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn below(&mut self, limit: usize) -> usize {
        if limit == 0 {
            0
        } else {
            (self.next_u64() % limit as u64) as usize
        }
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let len = self.below(max.saturating_add(1));
        let mut data = vec![0u8; len];
        for chunk in data.chunks_mut(8) {
            let value = self.next_u64().to_le_bytes();
            let copy = chunk.len().min(value.len());
            chunk[..copy].copy_from_slice(&value[..copy]);
        }
        data
    }
}

/// Open the pool with the fault backend, arm it, and run until SIGKILL.
pub fn run_worker(config: WorkerConfig) -> Result<()> {
    let stats = Arc::new(FaultStats::default());
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.fault_log)
        .with_context(|| format!("open fault log {}", config.fault_log.display()))?;
    let fault = Arc::new(FaultBackend::new(
        config.fault.clone(),
        stats.clone(),
        Some(log),
    ));

    let fs = {
        let fault = fault.clone();
        ArgosFs::open_loop_with_backend(&config.images, true, move |inner| {
            fault.set_inner(inner);
            fault.clone() as Arc<dyn StorageBackend>
        })
        .context("open pool with fault backend")?
    };

    ensure_dir(&fs, "/committed")?;
    ensure_dir(&fs, "/scratch")?;
    let oracle = Oracle::load(&config.oracle)?;
    cleanup_unpublished(&fs, &oracle);
    let _ = fs.sync();
    bootstrap_oracle(&fs, &config)?;
    fault.arm();

    let committer_fs = fs.clone();
    let committer_oracle = config.oracle.clone();
    let committer_config = config.clone();
    std::thread::Builder::new()
        .name("ztest-committer".to_string())
        .spawn(move || {
            if let Err(err) = committed_loop(committer_fs, committer_oracle, committer_config) {
                eprintln!("ztest committer failed: {err:#}");
                std::process::abort();
            }
        })?;

    for thread_index in 0..config.threads.max(1) {
        let fs = fs.clone();
        let config = config.clone();
        std::thread::Builder::new()
            .name(format!("ztest-chaos-{thread_index}"))
            .spawn(move || chaos_loop(fs, config, thread_index))?;
    }

    // The supervisor kills this process with SIGKILL. Never return normally;
    // that would make an early exit look like a successful test.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn bootstrap_oracle(fs: &ArgosFs, config: &WorkerConfig) -> Result<()> {
    let mut oracle = Oracle::load(&config.oracle)?;
    let epoch = oracle.next_epoch();
    let dir = epoch_dir(epoch);
    remove_tree(fs, &dir);
    let mut rng = LocalRng::new(config.seed ^ 0xB007_57A9_1234_5678 ^ epoch);
    let files = write_epoch(fs, &dir, epoch, config, &mut rng)
        .context("write bootstrap epoch before arming faults")?;
    oracle.epochs.insert(epoch, files);
    let retired = oracle.retire_epochs_before(config.keep_epochs);
    oracle
        .save(&config.oracle)
        .with_context(|| format!("save bootstrap oracle {}", config.oracle.display()))?;
    for retired_epoch in retired {
        remove_tree(fs, &epoch_dir(retired_epoch));
    }
    Ok(())
}

fn committed_loop(fs: ArgosFs, oracle_path: PathBuf, config: WorkerConfig) -> Result<()> {
    let mut oracle = Oracle::load(&oracle_path)?;
    let mut rng = LocalRng::new(config.seed ^ 0xC0DE_1234_5678_9ABC);
    loop {
        let epoch = oracle.next_epoch();
        let dir = epoch_dir(epoch);
        // A previous attempt may have crashed before publishing this epoch.
        remove_tree(&fs, &dir);
        match write_epoch(&fs, &dir, epoch, &config, &mut rng) {
            Ok(files) => {
                oracle.epochs.insert(epoch, files);
                let retired = oracle.retire_epochs_before(config.keep_epochs);
                oracle
                    .save(&oracle_path)
                    .with_context(|| format!("save oracle {}", oracle_path.display()))?;
                // Publish first, then delete. A crash between the two leaves
                // harmless extra files, never a missing committed file.
                for retired_epoch in retired {
                    remove_tree(&fs, &epoch_dir(retired_epoch));
                }
            }
            Err(err) => {
                eprintln!("ztest: epoch {epoch} not published: {err:#}");
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn write_epoch(
    fs: &ArgosFs,
    dir: &str,
    _epoch: u64,
    config: &WorkerConfig,
    rng: &mut LocalRng,
) -> Result<BTreeMap<String, FileRecord>> {
    ensure_dir(fs, dir)?;
    let mut records = BTreeMap::new();
    let file_count = config.files_per_epoch.max(1);
    for index in 0..file_count {
        let subdir = if index % 3 == 0 {
            let path = format!("{dir}/sub-{:02}", index / 3);
            ensure_dir(fs, &path)?;
            path
        } else {
            dir.to_string()
        };
        let path = format!("{subdir}/file-{index:03}.bin");
        let data = rng.bytes(config.max_file_bytes);
        fs.write_file(&path, &data, 0o644)
            .with_context(|| format!("write committed file {path}"))?;
        records.insert(path, FileRecord::for_bytes(&data));
    }
    fs.sync().context("sync committed epoch")?;
    Ok(records)
}

fn chaos_loop(fs: ArgosFs, config: WorkerConfig, thread_index: usize) -> ! {
    let mut rng = LocalRng::new(
        config
            .seed
            .wrapping_add((thread_index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
    );
    let root = format!("/scratch/t{thread_index:02}");
    let _ = fs.mkdir(&root, 0o755);
    let mut model = LocalModel::new();

    loop {
        match rng.below(12) {
            0 | 1 => {
                let name = model.name("file");
                let path = format!("{root}/{name}");
                let data = rng.bytes(config.max_file_bytes);
                if fs.write_file(&path, &data, 0o644).is_ok() {
                    model.files.insert(path, data);
                }
            }
            2 | 3 => {
                if let Some(path) = pick(&model.files, &mut rng) {
                    let data = rng.bytes(config.max_file_bytes);
                    if fs.write_file(&path, &data, 0o644).is_ok() {
                        model.files.insert(path, data);
                    }
                }
            }
            4 | 5 => {
                if let Some(path) = pick(&model.files, &mut rng) {
                    match fs.read_file(&path, true) {
                        Ok(data) => {
                            let mismatch = model
                                .files
                                .get(&path)
                                .is_some_and(|expected| &data != expected);
                            if mismatch {
                                // A torn, lost, or EIO-after-write fault can
                                // legitimately leave old or partial data. The
                                // host oracle is the durable correctness gate;
                                // a scratch path is only useful while its local
                                // model still matches.
                                eprintln!(
                                    "ztest: scratch mismatch reading {path}; dropping local model entry"
                                );
                                model.files.remove(&path);
                            }
                        }
                        Err(_) => {
                            // Injected EIO and damage are expected to surface
                            // as errors; silent wrong data never is.
                        }
                    }
                }
            }
            6 => {
                if let Some(path) = pick(&model.files, &mut rng) {
                    let append = rng.bytes(4096);
                    if let Ok(ino) = fs.resolve_path(&path, true) {
                        let offset = model.files.get(&path).map_or(0, |data| data.len() as u64);
                        if fs.write_inode_range(ino, offset, &append).is_ok() {
                            if let Some(data) = model.files.get_mut(&path) {
                                data.extend_from_slice(&append);
                            }
                        }
                    }
                }
            }
            7 => {
                if let Some(path) = pick(&model.files, &mut rng) {
                    let size = rng.below(config.max_file_bytes.saturating_add(1)) as u64;
                    if fs.truncate_path(&path, size).is_ok() {
                        if let Some(data) = model.files.get_mut(&path) {
                            data.truncate(size as usize);
                        }
                    }
                }
            }
            8 => {
                if let Some(old_path) = pick(&model.files, &mut rng) {
                    let name = model.name("moved");
                    let new_path = format!("{root}/{name}");
                    if fs.rename_path(&old_path, &new_path).is_ok() {
                        if let Some(data) = model.files.remove(&old_path) {
                            model.files.insert(new_path, data);
                        }
                    }
                }
            }
            9 => {
                if let Some(path) = pick(&model.files, &mut rng) {
                    if fs.unlink_path(&path).is_ok() {
                        model.files.remove(&path);
                    }
                }
            }
            10 => {
                let name = model.name("dir");
                let _ = fs.mkdir(&format!("{root}/{name}"), 0o755);
            }
            _ => {
                if let Some(path) = pick(&model.files, &mut rng) {
                    let name = model.name("link");
                    let link = format!("{root}/{name}");
                    let _ = fs.symlink_path(&path, &link);
                    let _ = fs.unlink_path(&link);
                }
            }
        }
    }
}

fn pick(values: &BTreeMap<String, Vec<u8>>, rng: &mut LocalRng) -> Option<String> {
    if values.is_empty() {
        return None;
    }
    let index = rng.below(values.len());
    values.keys().nth(index).cloned()
}

pub fn ensure_dir(fs: &ArgosFs, path: &str) -> Result<()> {
    match fs.mkdir(path, 0o755) {
        Ok(_) | Err(ArgosError::AlreadyExists(_)) => Ok(()),
        Err(err) => Err(anyhow!("mkdir {path}: {err}")),
    }
}

pub fn epoch_dir(epoch: u64) -> String {
    format!("/committed/epoch-{epoch:06}")
}

/// Remove committed epoch directories that the durable oracle does not name.
/// This cleans up partial epochs from a crash and retired epochs from a crash
/// between the oracle rename and the delete loop.
pub fn cleanup_unpublished(fs: &ArgosFs, oracle: &Oracle) {
    let root = "/committed";
    let ino = match fs.resolve_path(root, true) {
        Ok(ino) => ino,
        Err(_) => return,
    };
    let Ok(entries) = fs.readdir(ino) else {
        return;
    };
    for entry in entries {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        let Some(epoch) = entry
            .name
            .strip_prefix("epoch-")
            .and_then(|digits| digits.parse::<u64>().ok())
        else {
            continue;
        };
        if !oracle.epochs.contains_key(&epoch) {
            remove_tree(fs, &format!("{root}/{}", entry.name));
        }
    }
}

pub fn remove_tree(fs: &ArgosFs, path: &str) {
    let Ok(attr) = fs.attr_path(path, false) else {
        return;
    };
    match attr.kind {
        NodeKind::Directory => {
            if let Ok(entries) = fs.readdir(attr.ino) {
                for entry in entries {
                    if entry.name == "." || entry.name == ".." {
                        continue;
                    }
                    remove_tree(fs, &format!("{path}/{}", entry.name));
                }
            }
            let _ = fs.rmdir_path(path);
        }
        NodeKind::File | NodeKind::Symlink | NodeKind::Special => {
            let _ = fs.unlink_path(path);
        }
    }
}

/// Verify the durable oracle after a crash. Returns a list of violated paths.
pub fn verify_oracle(fs: &ArgosFs, oracle: &Oracle) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, record) in oracle.all_files() {
        match fs.read_file(path, true) {
            Ok(data) => {
                if !record.matches(&data) {
                    failures.push(format!(
                        "{path}: content mismatch ({} bytes, expected {})",
                        data.len(),
                        record.size
                    ));
                }
            }
            Err(err) => failures.push(format!("{path}: {err}")),
        }
    }
    failures
}
