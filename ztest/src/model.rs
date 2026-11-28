//! Reproducible, fault-free semantic workload with exact external state.
//! Fault-free is deliberate: an arbitrary I/O failure cannot excuse a mismatch.
use crate::invariants::verify_clean;
use anyhow::{ensure, Context, Result};
use argosfs::types::{Compression, DiskStatus, VolumeConfig};
use argosfs::{ArgosError, ArgosFs};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::sync::Barrier;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trace {
    pub version: u32,
    pub seed: u64,
    pub paged: bool,
    pub ops: Vec<Operation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Operation {
    Put {
        slot: u8,
        size: u16,
        pattern: u8,
    },
    Range {
        slot: u8,
        offset: u16,
        size: u16,
        pattern: u8,
    },
    Truncate {
        slot: u8,
        size: u16,
    },
    Rename {
        from: u8,
        to: u8,
    },
    Delete {
        slot: u8,
    },
    Reopen,
    Shared,
    Links,
    Topology,
    Atomic {
        operation: crate::atomic::Mutation,
        cut: u64,
    },
}

impl Operation {
    fn kind(&self) -> &'static str {
        match self {
            Self::Put { .. } => "put",
            Self::Range { .. } => "range",
            Self::Truncate { .. } => "truncate",
            Self::Rename { .. } => "rename",
            Self::Delete { .. } => "delete",
            Self::Reopen => "reopen",
            Self::Shared => "shared",
            Self::Links => "links",
            Self::Topology => "topology",
            Self::Atomic { .. } => "atomic",
        }
    }
}

impl Trace {
    pub fn generate(seed: u64, paged: bool, count: usize, topology: bool) -> Self {
        let mut state = seed;
        let mut next = || {
            state = state.wrapping_add(0x9e3779b97f4a7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
            z ^ (z >> 31)
        };
        let mut ops = vec![Operation::Shared, Operation::Links];
        for i in 0..count {
            let slot = (next() % 8) as u8;
            let size = (next() % 48_000) as u16;
            let pattern = next() as u8;
            ops.push(match next() % 7 {
                0 | 1 => Operation::Put {
                    slot,
                    size,
                    pattern,
                },
                2 => Operation::Range {
                    slot,
                    offset: size,
                    size: size % 8192 + 1,
                    pattern,
                },
                3 => Operation::Truncate { slot, size },
                4 => Operation::Rename {
                    from: slot,
                    to: (slot + 1 + (next() % 7) as u8) % 8,
                },
                5 => Operation::Delete { slot },
                _ => Operation::Reopen,
            });
            if i % 16 == 15 {
                ops.push(Operation::Reopen);
            }
        }
        if topology {
            ops.push(Operation::Topology);
        }
        for (operation, cut) in [
            (crate::atomic::Mutation::Rename, 0),
            (crate::atomic::Mutation::Rename, 4),
            (crate::atomic::Mutation::Delete, 2),
            (crate::atomic::Mutation::Delete, 64),
        ] {
            ops.push(Operation::Atomic { operation, cut });
        }
        ops.push(Operation::Reopen);
        Self {
            version: 1,
            seed,
            paged,
            ops,
        }
    }
}

fn path(slot: u8) -> String {
    format!("/model/file-{slot}")
}

fn data(size: u16, pattern: u8) -> Vec<u8> {
    let mut state = u64::from(pattern) + 1;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn absent<T>(result: argosfs::Result<T>) -> Result<()> {
    ensure!(
        matches!(result, Err(ArgosError::NotFound(_))),
        "expected NotFound"
    );
    Ok(())
}

fn verify(fs: &ArgosFs, expected: &BTreeMap<u8, Vec<u8>>) -> Result<()> {
    let dir = fs.resolve_path("/model", true)?;
    let actual: BTreeSet<_> = fs
        .readdir(dir)?
        .into_iter()
        .filter(|entry| entry.name != "." && entry.name != "..")
        .map(|entry| entry.name)
        .collect();
    let names: BTreeSet<_> = expected.keys().map(|slot| format!("file-{slot}")).collect();
    ensure!(
        actual == names,
        "namespace mismatch: {actual:?} != {names:?}"
    );
    for (slot, bytes) in expected {
        ensure!(
            fs.read_file(&path(*slot), false)? == *bytes,
            "content mismatch at slot {slot}"
        );
    }
    Ok(())
}

fn shared(fs: &ArgosFs) -> Result<()> {
    // All writes target one inode. Disjoint regions must all survive, and
    // overlapping full-region writes must be one complete legal last writer.
    const WIDTH: usize = 8192;
    const THREADS: usize = 4;
    fs.write_file("/shared", &vec![0; WIDTH * (THREADS + 1)], 0o644)?;
    let ino = fs.resolve_path("/shared", true)?;
    let barrier = Barrier::new(THREADS);
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for i in 0..THREADS {
            let barrier = &barrier;
            handles.push(scope.spawn(move || -> Result<()> {
                barrier.wait();
                let bytes = vec![i as u8 + 1; WIDTH];
                ensure!(
                    fs.write_inode_range(ino, (i * WIDTH) as u64, &bytes)? == WIDTH,
                    "short shared write"
                );
                ensure!(
                    fs.write_inode_range(ino, (THREADS * WIDTH) as u64, &bytes)? == WIDTH,
                    "short overlap write"
                );
                Ok(())
            }));
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("shared writer panic"))??;
        }
        Ok(())
    })?;
    fs.sync()?;
    let actual = fs.read_file("/shared", false)?;
    ensure!(actual.len() == WIDTH * (THREADS + 1), "shared file length");
    for i in 0..THREADS {
        ensure!(
            actual[i * WIDTH..(i + 1) * WIDTH]
                .iter()
                .all(|b| *b == i as u8 + 1),
            "lost disjoint write"
        );
    }
    let overlap = &actual[THREADS * WIDTH..];
    ensure!(
        (1..=THREADS as u8).contains(&overlap[0]) && overlap.iter().all(|b| *b == overlap[0]),
        "torn overlapping write"
    );
    fs.unlink_path("/shared")?;
    fs.write_file("/race-source", b"rename winner", 0o644)?;
    let barrier = Barrier::new(THREADS);
    let winner = std::thread::scope(|scope| -> Result<usize> {
        let mut handles = Vec::new();
        for i in 0..THREADS {
            let barrier = &barrier;
            handles.push(scope.spawn(move || {
                barrier.wait();
                (i, fs.rename_path("/race-source", &format!("/race-{i}")))
            }));
        }
        let mut winner = None;
        for handle in handles {
            let (i, result) = handle
                .join()
                .map_err(|_| anyhow::anyhow!("rename racer panic"))?;
            match result {
                Ok(()) => ensure!(
                    winner.replace(i).is_none(),
                    "multiple successful renames of one source"
                ),
                Err(ArgosError::NotFound(_)) => {}
                Err(err) => return Err(err.into()),
            }
        }
        winner.context("no successful rename")
    })?;
    absent(fs.resolve_path("/race-source", false))?;
    for i in 0..THREADS {
        let path = format!("/race-{i}");
        if i == winner {
            ensure!(
                fs.read_file(&path, false)? == b"rename winner",
                "rename race corrupted data"
            );
            fs.unlink_path(&path)?;
        } else {
            absent(fs.resolve_path(&path, false))?;
        }
    }
    Ok(())
}

fn links(fs: &ArgosFs) -> Result<()> {
    fs.write_file("/link-source", b"link identity", 0o644)?;
    let ino = fs.resolve_path("/link-source", true)?;
    let root = fs.resolve_path("/", true)?;
    fs.link_at(ino, root, OsStr::new("link-alias"))?;
    fs.symlink_path("/link-alias", "/link-symbolic")?;
    fs.rename_path("/link-source", "/link-moved")?;
    absent(fs.resolve_path("/link-source", false))?;
    ensure!(
        fs.resolve_path("/link-alias", true)? == ino,
        "hard link identity changed"
    );
    ensure!(
        fs.resolve_path("/link-moved", true)? == ino,
        "rename identity changed"
    );
    fs.unlink_path("/link-moved")?;
    ensure!(
        fs.read_file("/link-alias", false)? == b"link identity",
        "unlink destroyed hard link"
    );
    ensure!(
        fs.read_file("/link-symbolic", true)? == b"link identity",
        "symlink resolution"
    );
    fs.unlink_path("/link-alias")?;
    absent(fs.resolve_path("/link-symbolic", true))?;
    fs.unlink_path("/link-symbolic")?;
    Ok(())
}

fn topology(fs: ArgosFs, images: &mut Vec<std::path::PathBuf>, dir: &Path) -> Result<ArgosFs> {
    fs.write_file("/topology-sentinel", &data(60000, 23), 0o644)?;
    fs.sync()?;
    let retired = fs
        .metadata_snapshot()
        .disks
        .values()
        .find(|disk| disk.status == DiskStatus::Online)
        .context("no online member")?
        .clone();
    let missing = retired.path.clone();
    drop(fs);
    images.retain(|p| *p != missing);
    ensure!(
        images.len() == 4,
        "topology drill requires five active members"
    );
    let degraded = ArgosFs::open_loop(images, true)?;
    ensure!(
        degraded.read_file("/topology-sentinel", false)? == data(60000, 23),
        "degraded read"
    );
    let replacement = dir.join(format!("replacement-{}.img", retired.id));
    degraded.add_block_device(replacement.clone(), 96 * 1024 * 1024, false)?;
    degraded.sync()?;
    drop(degraded);
    images.push(replacement);
    let repaired = ArgosFs::open_loop(images, true)?;
    repaired.remove_disk(&retired.id)?;
    repaired.rebalance()?;
    repaired.sync()?;
    ensure!(
        repaired.metadata_snapshot().disks[&retired.id].status == DiskStatus::Removed,
        "retired member still active"
    );
    ensure!(
        repaired.read_file("/topology-sentinel", false)? == data(60000, 23),
        "replacement changed content"
    );
    repaired.unlink_path("/topology-sentinel")?;
    Ok(repaired)
}

pub fn execute(trace: &Trace, dir: &Path) -> Result<()> {
    ensure!(trace.version == 1, "unsupported trace version");
    fs::create_dir(dir)?;
    let dir = fs::canonicalize(dir)?;
    let mut images: Vec<_> = (0..5).map(|i| dir.join(format!("disk{i}.img"))).collect();
    let config = VolumeConfig {
        k: 3,
        m: 2,
        chunk_size: 32 * 1024,
        compression: Compression::Lz4,
        l2_cache_bytes: 0,
        ..VolumeConfig::default()
    };
    let create = if trace.paged {
        ArgosFs::create_loop_paged
    } else {
        ArgosFs::create_loop
    };
    let mut fs = create(&images, config, 64 * 1024 * 1024, "ztest-model", false)?;
    fs.mkdir("/model", 0o755)?;
    let mut expected = BTreeMap::<u8, Vec<u8>>::new();
    for (index, op) in trace.ops.iter().enumerate() {
        let result = (|| -> Result<()> {
            match *op {
                Operation::Put {
                    slot,
                    size,
                    pattern,
                } => {
                    let bytes = data(size, pattern);
                    fs.write_file(&path(slot), &bytes, 0o644)?;
                    expected.insert(slot, bytes);
                }
                Operation::Range {
                    slot,
                    offset,
                    size,
                    pattern,
                } => {
                    if let Some(bytes) = expected.get_mut(&slot) {
                        let payload = data(size, pattern);
                        let ino = fs.resolve_path(&path(slot), true)?;
                        ensure!(
                            fs.write_inode_range(ino, offset as u64, &payload)? == payload.len(),
                            "short range write"
                        );
                        let start = offset as usize;
                        bytes.resize(bytes.len().max(start + payload.len()), 0);
                        bytes[start..start + payload.len()].copy_from_slice(&payload);
                    } else {
                        absent(fs.resolve_path(&path(slot), true))?;
                    }
                }
                Operation::Truncate { slot, size } => {
                    if let Some(bytes) = expected.get_mut(&slot) {
                        fs.truncate_path(&path(slot), size as u64)?;
                        bytes.resize(size as usize, 0);
                    } else {
                        absent(fs.truncate_path(&path(slot), size as u64))?;
                    }
                }
                Operation::Rename { from, to } => {
                    if let Some(bytes) = expected.remove(&from) {
                        fs.rename_path(&path(from), &path(to))?;
                        expected.insert(to, bytes);
                    } else {
                        absent(fs.rename_path(&path(from), &path(to)))?;
                    }
                }
                Operation::Delete { slot } => {
                    if expected.remove(&slot).is_some() {
                        fs.unlink_path(&path(slot))?;
                    } else {
                        absent(fs.unlink_path(&path(slot)))?;
                    }
                }
                Operation::Reopen | Operation::Topology | Operation::Atomic { .. } => {}
                Operation::Shared => shared(&fs)?,
                Operation::Links => links(&fs)?,
            }
            verify(&fs, &expected)
        })();
        result.with_context(|| format!("{} operation {index}", op.kind()))?;
        if matches!(op, Operation::Reopen) {
            fs.sync()?;
            let txid = fs.metadata_snapshot().txid;
            drop(fs);
            fs = ArgosFs::open_loop(&images, true).context("reopen operation")?;
            ensure!(
                fs.metadata_snapshot().txid >= txid,
                "durable transaction ID regressed"
            );
            verify(&fs, &expected).context("reopen operation")?;
            verify_clean(&fs).context("reopen operation")?;
        } else if matches!(op, Operation::Topology) {
            fs = topology(fs, &mut images, &dir).context("topology operation")?;
            verify(&fs, &expected).context("topology operation")?;
        } else if let Operation::Atomic { operation, cut } = *op {
            fs = crate::atomic::exercise(fs, &images, &dir, index, operation, cut)
                .context("atomic operation")?;
            verify(&fs, &expected).context("atomic operation")?;
        }
    }
    fs.sync()?;
    verify_clean(&fs).context("final invariant")?;
    Ok(())
}

fn signature(error: &anyhow::Error) -> String {
    // Include the operation category but not the changing trace index.
    let context = error.to_string();
    let category = context.split_whitespace().next().unwrap_or_default();
    format!("{category}: {}", error.root_cause())
}

pub fn run(trace: Trace, dir: &Path, minimize: bool) -> Result<()> {
    fs::create_dir(dir)?;
    fs::write(dir.join("trace.json"), serde_json::to_vec_pretty(&trace)?)?;
    let initial = dir.join("initial");
    let error = match execute(&trace, &initial) {
        Ok(()) => {
            fs::remove_dir_all(initial)?;
            eprintln!(
                "ztest model: {} trace operations verified (seed {}, paged={})",
                trace.ops.len(),
                trace.seed,
                trace.paged
            );
            return Ok(());
        }
        Err(error) => error,
    };
    if minimize
        && (error.to_string().contains("operation") || error.to_string() == "final invariant")
    {
        let target = signature(&error);
        let mut reduced = trace;
        let mut granularity = 2;
        let mut attempt = 0;
        while !reduced.ops.is_empty() && attempt < 128 {
            let chunk = reduced.ops.len().div_ceil(granularity);
            let mut improved = false;
            for start in (0..reduced.ops.len()).step_by(chunk) {
                let mut candidate = reduced.clone();
                candidate
                    .ops
                    .drain(start..(start + chunk).min(candidate.ops.len()));
                attempt += 1;
                let trial = dir.join(format!("trial-{attempt}"));
                let result = execute(&candidate, &trial);
                if trial.exists() {
                    fs::remove_dir_all(&trial)?;
                }
                if result.as_ref().is_err_and(|err| signature(err) == target) {
                    let confirm = dir.join(format!("confirm-{attempt}"));
                    let reproduced = execute(&candidate, &confirm);
                    if confirm.exists() {
                        fs::remove_dir_all(&confirm)?;
                    }
                    if !reproduced
                        .as_ref()
                        .is_err_and(|err| signature(err) == target)
                    {
                        continue;
                    }
                    reduced = candidate;
                    granularity = granularity.saturating_sub(1).max(2);
                    improved = true;
                    break;
                }
                if attempt >= 128 {
                    break;
                }
            }
            if !improved {
                if granularity >= reduced.ops.len() {
                    break;
                }
                granularity = (granularity * 2).min(reduced.ops.len());
            }
        }
        fs::write(
            dir.join("minimized.json"),
            serde_json::to_vec_pretty(&reduced)?,
        )?;
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_trace_roundtrips_and_reproduces() {
        let trace = Trace::generate(356, true, 32, true);
        let encoded = serde_json::to_vec(&trace).unwrap();
        let decoded: Trace = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
        assert_eq!(
            serde_json::to_vec(&Trace::generate(356, true, 32, true)).unwrap(),
            encoded
        );
        assert_ne!(
            serde_json::to_vec(&Trace::generate(357, true, 32, true)).unwrap(),
            encoded
        );
    }
}
