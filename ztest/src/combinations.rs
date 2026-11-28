//! Feature combinations with an external logical-state oracle. No failures are
//! accepted merely because a maintenance operation happened concurrently.
use anyhow::{ensure, Context, Result};
use argosfs::types::{Compression, PosixAcl, PosixAclEntry, PosixAclTag, VolumeConfig};
use argosfs::{ArgosError, ArgosFs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Barrier;

pub const TEST_KEY: &str = "ztest-disposable-pool-key";
const DIR: &str = "/features";
const A: &str = "/features/a";
const B: &str = "/features/b";
const LIMIT: u64 = 128 * 1024;
const XATTR: &str = "user.ztest";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    Lz4Cache,
    ZstdDeferred,
    Encrypted,
    Uncompressed,
}

impl Profile {
    pub fn encrypted(self) -> bool {
        matches!(self, Self::Encrypted)
    }
    pub fn configure(self, config: &mut VolumeConfig) {
        match self {
            Self::Lz4Cache => {
                config.k = 2;
                config.m = 2;
                config.l2_cache_bytes = 512 * 1024;
            }
            Self::ZstdDeferred => {
                config.compression = Compression::Zstd;
                config.compression_level = 3;
                config.chunk_size = 16 * 1024;
                config.l2_cache_bytes = 512 * 1024;
                config.defer_metadata_commit = true;
                config.defer_data_flush = true;
            }
            Self::Encrypted => {
                config.l2_cache_bytes = 512 * 1024;
                config.k = 2;
                config.m = 2;
            }
            Self::Uncompressed => {
                config.compression = Compression::None;
                config.k = 4;
                config.m = 1;
                config.chunk_size = 64 * 1024;
            }
        }
    }
}

pub(crate) fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn acl() -> PosixAcl {
    use PosixAclTag::*;
    PosixAcl {
        entries: [
            (UserObj, None, 6),
            (User, Some(42421), 0),
            (GroupObj, None, 0),
            (Mask, None, 0),
            (Other, None, 0),
        ]
        .into_iter()
        .map(|(tag, id, perms)| PosixAclEntry { tag, id, perms })
        .collect(),
    }
}

// Two writers compete for one quota reservation. Both files exist before
// the race, so a rejected write must preserve the empty file and its inode.
fn quota_race(fs: &ArgosFs, seed: u64) -> Result<()> {
    let paths = ["/features/q0", "/features/q1"];
    for path in paths {
        fs.write_file(path, &[], 0o600)?;
    }
    let payloads = [bytes(seed, 40 * 1024), bytes(seed ^ 0x8765, 40 * 1024)];
    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| -> Result<Vec<bool>> {
        let mut handles = Vec::new();
        for (path, payload) in paths.into_iter().zip(&payloads) {
            let barrier = &barrier;
            handles.push(scope.spawn(move || {
                barrier.wait();
                fs.write_file(path, payload, 0o600)
            }));
        }
        handles
            .into_iter()
            .map(|handle| {
                match handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("quota writer panic"))?
                {
                    Ok(()) => Ok(true),
                    Err(ArgosError::QuotaExceeded { .. }) => Ok(false),
                    Err(error) => Err(error.into()),
                }
            })
            .collect()
    })?;
    ensure!(
        results.iter().filter(|&&v| v).count() == 1,
        "quota race must admit exactly one write: {results:?}"
    );
    for (i, path) in paths.into_iter().enumerate() {
        let actual = fs.read_file(path, false)?;
        ensure!(
            actual.as_slice() == if results[i] { &payloads[i][..] } else { &[] },
            "quota race changed rejected file or lost accepted data"
        );
        fs.unlink_path(path)?;
    }
    Ok(())
}

/// Small, independent pool with real allocator exhaustion, not a quota error.
/// Keep already committed files in the oracle through failure and remount.
pub fn capacity(dir: &Path, paged: bool, seed: u64) -> Result<()> {
    std::fs::create_dir(dir)?;
    let images: Vec<_> = (0..3).map(|i| dir.join(format!("disk-{i}.img"))).collect();
    let config = VolumeConfig {
        k: 2,
        m: 1,
        compression: Compression::None,
        chunk_size: 128 * 1024,
        l2_cache_bytes: 256 * 1024,
        ..Default::default()
    };
    let create = if paged {
        ArgosFs::create_loop_paged
    } else {
        ArgosFs::create_loop_legacy
    };
    let fs = create(&images, config, 24 * 1024 * 1024, "ztest-capacity", false)?;
    let payload = bytes(seed, 512 * 1024);
    let mut names = Vec::new();
    let mut full = false;
    for i in 0..64 {
        let path = format!("/fill-{i:03}");
        fs.write_file(&path, &[], 0o600)?;
        match fs.write_file(&path, &payload, 0o600) {
            Ok(()) => {
                names.push(path);
                fs.sync()?;
            }
            Err(ArgosError::DiskFull { .. }) => {
                ensure!(
                    fs.read_file(&path, false)?.is_empty(),
                    "failed allocation changed empty file"
                );
                fs.unlink_path(&path)?;
                full = true;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        full && !names.is_empty(),
        "capacity drill did not reach data ENOSPC"
    );
    fs.sync()?;
    drop(fs);
    let fs = ArgosFs::open_loop(&images, true)?;
    for path in &names {
        ensure!(
            fs.read_file(path, false)? == payload,
            "data changed at ENOSPC: {path}"
        );
    }
    let first = names.remove(0);
    fs.unlink_path(&first)?;
    fs.sync()?;
    fs.write_file("/reused", &payload, 0o600)?;
    fs.sync()?;
    drop(fs);
    let fs = ArgosFs::open_loop(&images, false)?;
    for path in &names {
        ensure!(
            fs.read_file(path, false)? == payload,
            "data changed after space reuse: {path}"
        );
    }
    ensure!(
        fs.read_file("/reused", false)? == payload,
        "reclaimed extent content mismatch"
    );
    ensure!(
        matches!(fs.resolve_path(&first, false), Err(ArgosError::NotFound(_))),
        "deleted name reappeared"
    );
    crate::invariants::verify_clean(&fs)?;
    eprintln!(
        "ztest capacity: {} committed files before ENOSPC, space reuse verified",
        names.len() + 1
    );
    Ok(())
}

pub struct Features {
    a: Vec<u8>,
    b: Vec<u8>,
    attribute: Vec<u8>,
    encrypted: bool,
    tree: Vec<(String, u8)>,
}

impl Features {
    pub fn create(
        fs: &ArgosFs,
        seed: u64,
        encrypted: bool,
        directory_entries: usize,
    ) -> Result<Self> {
        fs.mkdir(DIR, 0o750)?;
        fs.set_directory_quota(DIR, LIMIT)?;
        let mut a = vec![(seed & 255) as u8; 40 * 1024];
        let b = bytes(seed, 36 * 1024);
        fs.write_file(A, &a, 0o600)?;
        fs.write_file(B, &b, 0o640)?;
        // Exercise the selected codec with highly compressible data as well
        // as random bytes; assert the physical encoding was actually used.
        let meta = fs.metadata_snapshot();
        let ino = fs.resolve_path(A, false)?;
        let blocks = &meta
            .inodes
            .get(&ino)
            .context("missing feature inode")?
            .blocks;
        ensure!(
            !blocks.is_empty(),
            "compressible file did not allocate data blocks"
        );
        ensure!(
            blocks.iter().all(|b| b.codec == meta.config.compression),
            "configured codec was not used for compressible blocks"
        );
        if meta.config.compression != Compression::None {
            ensure!(
                blocks.iter().all(|b| b.compressed_size < b.raw_size),
                "compressible blocks did not shrink"
            );
        }

        let ino = fs.resolve_path(A, false)?;
        let attr = bytes(seed.wrapping_add(42), 31);
        fs.setxattr_inode(ino, XATTR, &attr)?;
        fs.set_posix_acl_path(A, false, acl())?;
        // Populate the cache, then overwrite across a chunk boundary through a hard link.
        ensure!(fs.read_file(A, false)? == a, "initial cached read differs");
        fs.link_at(ino, fs.resolve_path(DIR, true)?, OsStr::new("alias"))?;
        fs.rename_path("/features/alias", "/features/moved")?;
        ensure!(
            fs.resolve_path("/features/moved", false)? == ino,
            "alias identity changed"
        );
        let patch = bytes(seed ^ 0x1234, 8193);
        let offset = 15 * 1024;
        ensure!(
            fs.write_inode_range(
                fs.resolve_path("/features/moved", true)?,
                offset as u64,
                &patch
            )? == patch.len(),
            "short alias write"
        );
        a[offset..offset + patch.len()].copy_from_slice(&patch);
        fs.unlink_path("/features/moved")?;
        fs.truncate_path(A, 48 * 1024)?;
        a.resize(48 * 1024, 0);
        // A rejected overwrite of an existing file must leave its bytes and quota intact.
        let rejected = fs.truncate_path(A, LIMIT + 1);
        ensure!(
            matches!(rejected, Err(ArgosError::QuotaExceeded { .. })),
            "quota did not reject growth: {rejected:?}"
        );
        // Exercise quota reclaim and a second allocation.
        fs.truncate_path(A, 32 * 1024)?;
        a.truncate(32 * 1024);
        fs.write_file("/features/reclaimed", &vec![7; 48 * 1024], 0o600)?;
        fs.unlink_path("/features/reclaimed")?;
        quota_race(fs, seed)?;
        let denied = fs.write_inode_range_as(ino, 0, b"denied", 42421, 42421);
        ensure!(
            denied.is_err_and(|e| e.errno() == libc::EACCES),
            "ACL did not reject write"
        );
        fs.symlink_path("a", "/features/symbolic")?;
        let mut tree = Vec::new();
        if directory_entries > 0 {
            fs.mkdir("/features/tree", 0o750)?;
            for i in 0..directory_entries {
                let name = format!("entry-{i:05}");
                let value = seed.wrapping_add(i as u64) as u8;
                fs.write_file(&format!("/features/tree/{name}"), &[value], 0o640)?;
                let name = if i % 5 == 0 {
                    let renamed = format!("moved-{i:05}");
                    fs.rename_path(
                        &format!("/features/tree/{name}"),
                        &format!("/features/tree/{renamed}"),
                    )?;
                    renamed
                } else {
                    name
                };
                tree.push((name, value));
            }
        }
        let model = Self {
            a,
            b,
            attribute: attr,
            encrypted,
            tree,
        };
        model.verify(fs)?;
        fs.sync()?;
        Ok(model)
    }

    pub fn verify(&self, fs: &ArgosFs) -> Result<()> {
        let dir = fs.resolve_path(DIR, true)?;
        let actual: BTreeSet<_> = fs
            .readdir(dir)?
            .into_iter()
            .filter(|e| e.name != "." && e.name != "..")
            .map(|e| e.name)
            .collect();
        let mut expected: BTreeSet<_> = ["a", "b", "symbolic"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        if !self.tree.is_empty() {
            expected.insert("tree".into());
            let names: BTreeSet<_> = fs
                .readdir(fs.resolve_path("/features/tree", true)?)?
                .into_iter()
                .filter(|e| e.name != "." && e.name != "..")
                .map(|e| e.name)
                .collect();
            ensure!(
                names == self.tree.iter().map(|(name, _)| name.clone()).collect(),
                "nested directory entries differ"
            );
            for (name, value) in &self.tree {
                ensure!(
                    fs.read_file(&format!("/features/tree/{name}"), false)? == [*value],
                    "nested file mismatch: {name}"
                );
            }
        }
        ensure!(actual == expected, "feature namespace mismatch: {actual:?}");
        ensure!(
            fs.read_file(A, false)? == self.a,
            "feature a content mismatch"
        );
        ensure!(
            fs.read_file(B, false)? == self.b,
            "feature b content mismatch"
        );
        ensure!(
            fs.read_file("/features/symbolic", false)? == self.a,
            "symlink content mismatch"
        );
        let ino = fs.resolve_path(A, false)?;
        ensure!(fs.attr_inode(ino)?.nlink == 1, "feature hard-link count");
        ensure!(
            fs.attr_inode(ino)?.mode & 0o777 == 0o600,
            "feature ACL mode mismatch"
        );
        ensure!(
            fs.getxattr_inode(ino, XATTR)? == self.attribute,
            "feature xattr mismatch"
        );
        ensure!(
            fs.get_posix_acl_path(A, false)? == Some(acl()),
            "feature ACL mismatch"
        );
        ensure!(
            fs.check_access_inode(ino, 42421, 42421, libc::R_OK)
                .is_err_and(|e| e.errno() == libc::EACCES),
            "feature ACL denial mismatch"
        );
        // Symlink bytes are not regular-file usage; the quota API counts logical file bytes.
        let (limit, used) = fs.directory_quota(DIR)?;
        ensure!(
            limit == Some(LIMIT) && used == (self.a.len() + self.b.len() + self.tree.len()) as u64,
            "feature quota mismatch: {limit:?}, {used}"
        );
        if self.encrypted {
            let meta = fs.metadata_snapshot();
            ensure!(meta.encryption.enabled, "encryption flag lost");
            for path in [A, B] {
                let ino = fs.resolve_path(path, false)?;
                let inode = &meta.inodes.get(&ino).unwrap();
                ensure!(
                    !inode.blocks.is_empty() && inode.blocks.iter().all(|b| b.encrypted),
                    "unencrypted feature blocks"
                );
            }
        }
        Ok(())
    }

    pub fn remove(self, fs: &ArgosFs) -> Result<()> {
        self.verify(fs)?;
        if !self.tree.is_empty() {
            for (name, _) in &self.tree {
                fs.unlink_path(&format!("/features/tree/{name}"))?;
            }
            fs.rmdir_path("/features/tree")?;
        }
        for path in ["/features/symbolic", A, B] {
            fs.unlink_path(path)?;
        }
        ensure!(
            fs.directory_quota(DIR)? == (Some(LIMIT), 0),
            "quota not reclaimed after unlink"
        );
        fs.clear_directory_quota(DIR)?;
        ensure!(fs.directory_quota(DIR)? == (None, 0), "quota not cleared");
        fs.rmdir_path(DIR)?;
        fs.sync()?;
        Ok(())
    }
}
