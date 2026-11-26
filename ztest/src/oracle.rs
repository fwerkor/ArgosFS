//! Crash-consistent oracle written outside the ArgosFS pool.
//!
//! An epoch is published only after `ArgosFs::sync()` succeeds. A crash before
//! that point leaves the previous oracle intact, so recovery is allowed to
//! contain a partially written newer epoch. A crash after the atomic rename
//! makes the new epoch mandatory. Old epochs may still exist on disk after the
//! oracle stops naming them; extra files are harmless, missing committed files
//! are not.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

pub const ORACLE_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileRecord {
    pub size: u64,
    pub blake3: String,
}

impl FileRecord {
    pub fn for_bytes(data: &[u8]) -> Self {
        Self {
            size: data.len() as u64,
            blake3: blake3::hash(data).to_hex().to_string(),
        }
    }

    pub fn matches(&self, data: &[u8]) -> bool {
        self.size == data.len() as u64 && self.blake3 == blake3::hash(data).to_hex().as_str()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Oracle {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub epochs: BTreeMap<u64, BTreeMap<String, FileRecord>>,
}

impl Default for Oracle {
    fn default() -> Self {
        Self {
            version: ORACLE_VERSION,
            epochs: BTreeMap::new(),
        }
    }
}

fn default_version() -> u32 {
    ORACLE_VERSION
}

impl Oracle {
    pub fn load(path: &Path) -> io::Result<Self> {
        match File::open(path) {
            Ok(mut file) => {
                let mut data = Vec::new();
                file.read_to_end(&mut data)?;
                serde_json::from_slice(&data).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("oracle {} is not valid JSON: {err}", path.display()),
                    )
                })
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self {
                version: ORACLE_VERSION,
                epochs: BTreeMap::new(),
            }),
            Err(err) => Err(err),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let tmp = temp_path(path);
        {
            let mut file = File::create(&tmp)?;
            serde_json::to_writer_pretty(&mut file, self)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
            file.write_all(b"\n")?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        sync_dir(parent);
        Ok(())
    }

    pub fn next_epoch(&self) -> u64 {
        self.epochs
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
    }

    pub fn all_files(&self) -> impl Iterator<Item = (&String, &FileRecord)> {
        self.epochs.values().flat_map(|files| files.iter())
    }

    pub fn retire_epochs_before(&mut self, keep: usize) -> Vec<u64> {
        let mut retired = Vec::new();
        while self.epochs.len() > keep.max(1) {
            let Some(oldest) = self.epochs.keys().next().copied() else {
                break;
            };
            self.epochs.remove(&oldest);
            retired.push(oldest);
        }
        retired
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    tmp.set_extension(format!("{extension}.tmp"));
    tmp
}

fn sync_dir(path: &Path) {
    if let Ok(dir) = File::open(path) {
        let _ = dir.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn oracle_round_trips_and_retires_old_epochs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oracle.json");
        let mut oracle = Oracle::load(&path).unwrap();
        assert_eq!(oracle.next_epoch(), 1);
        oracle.epochs.insert(
            1,
            BTreeMap::from([(
                "/committed/epoch-000001/a".to_string(),
                FileRecord::for_bytes(b"hello"),
            )]),
        );
        oracle.save(&path).unwrap();
        let mut loaded = Oracle::load(&path).unwrap();
        assert!(loaded.all_files().next().unwrap().1.matches(b"hello"));
        loaded.epochs.insert(2, BTreeMap::new());
        loaded.epochs.insert(3, BTreeMap::new());
        assert_eq!(loaded.retire_epochs_before(2), vec![1]);
        assert_eq!(loaded.epochs.len(), 2);
        assert_eq!(
            loaded.epochs.keys().copied().collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
