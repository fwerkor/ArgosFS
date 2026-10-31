use crate::error::{ArgosError, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn content_hash_hex(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

pub fn content_hash_matches(data: &[u8], expected: &str) -> bool {
    content_hash_hex(data) == expected || sha256_hex(data) == expected
}

/// Maximum number of extra shard reads after a checksum/size mismatch.
pub const MAX_READ_RETRIES: usize = 10;

fn env_flag(value: Option<&str>) -> bool {
    match value.map(str::trim) {
        Some(value) if !value.is_empty() => !matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        _ => false,
    }
}

/// True when `ARGOSFS_USB` asks for USB-friendly defaults.
///
/// Set it to `1`, `true`, `yes` or `on`.  Explicit settings for the
/// individual options always win.
pub fn usb_mode() -> bool {
    env_flag(std::env::var("ARGOSFS_USB").ok().as_deref())
}

fn env_usize(value: Option<&str>, max: usize) -> Option<usize> {
    value
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| value.min(max))
}

/// Extra reads after a shard fails its checksum.
///
/// `ARGOSFS_READ_RETRIES` sets it (0-10, default 0).  USB mode defaults it
/// to 1 so a transient USB/controller error does not make scrub rewrite a
/// perfectly good shard.
pub fn configured_read_retries() -> usize {
    env_usize(
        std::env::var("ARGOSFS_READ_RETRIES").ok().as_deref(),
        MAX_READ_RETRIES,
    )
    .unwrap_or_else(|| usize::from(usb_mode()))
}

/// Read a value up to `attempts` times.  An error is returned immediately;
/// a successful value that does not validate is retried, and `None` means
/// every attempt produced an invalid value.  The second value is the number
/// of reads actually made, so the caller can report how many retries were
/// needed.
pub fn retry_verified<T, E>(
    attempts: usize,
    mut read: impl FnMut(usize) -> std::result::Result<T, E>,
    mut valid: impl FnMut(&T) -> bool,
) -> std::result::Result<(Option<T>, usize), E> {
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        let value = read(attempt)?;
        if valid(&value) {
            return Ok((Some(value), attempt + 1));
        }
    }
    Ok((None, attempts))
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub struct FileLock {
    file: File,
}

impl FileLock {
    pub fn exclusive(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(ArgosError::Io(std::io::Error::last_os_error()));
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        now_f64().to_bits()
    ));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        let dir = File::open(parent)?;
        dir.sync_all()?;
    }
    Ok(())
}

pub fn append_json_line<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    serde_json::to_writer(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

pub fn read_to_vec(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

pub fn clean_path(path: &str) -> String {
    let mut stack: Vec<String> = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                stack.clear();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                stack.pop();
            }
            Component::Normal(part) => {
                stack.push(part.to_string_lossy().to_string());
            }
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

pub fn split_path(path: &str) -> Vec<String> {
    clean_path(path)
        .split('/')
        .filter(|part| !part.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn parent_name(path: &str) -> Result<(String, String)> {
    let cleaned = clean_path(path);
    if cleaned == "/" {
        return Err(ArgosError::Invalid("root has no parent".to_string()));
    }
    let parts = split_path(&cleaned);
    let name = parts
        .last()
        .cloned()
        .ok_or_else(|| ArgosError::Invalid("empty path".to_string()))?;
    let parent = if parts.len() == 1 {
        "/".to_string()
    } else {
        format!("/{}", parts[..parts.len() - 1].join("/"))
    };
    Ok((parent, name))
}

pub fn stable_u01(parts: &[&str]) -> f64 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let value = u64::from_be_bytes(bytes);
    (value as f64 + 1.0) / (u64::MAX as f64 + 2.0)
}

pub fn directory_size(path: &Path) -> u64 {
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

pub fn relative_or_absolute(base: &Path, stored: &Path) -> PathBuf {
    if stored.is_absolute() {
        stored.to_path_buf()
    } else {
        base.join(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_flag_parsing_accepts_the_usual_spellings() {
        for value in ["1", "true", "YES", "on", "anything"] {
            assert!(env_flag(Some(value)), "{value}");
        }
        for value in ["0", "false", "no", "off", "  ", ""] {
            assert!(!env_flag(Some(value)), "{value}");
        }
        assert!(!env_flag(None));
    }

    #[test]
    fn read_retries_are_parsed_and_clamped() {
        assert_eq!(env_usize(Some("2"), MAX_READ_RETRIES), Some(2));
        assert_eq!(
            env_usize(Some("999"), MAX_READ_RETRIES),
            Some(MAX_READ_RETRIES)
        );
        assert_eq!(env_usize(Some("not-a-number"), MAX_READ_RETRIES), None);
        assert_eq!(env_usize(None, MAX_READ_RETRIES), None);
    }

    #[test]
    fn retry_verified_retries_only_bad_values() {
        let mut reads = 0;
        let result = retry_verified(
            2,
            |_| {
                reads += 1;
                Ok::<_, ()>(if reads == 1 {
                    b"bad".to_vec()
                } else {
                    b"good".to_vec()
                })
            },
            |value| value == b"good",
        );
        assert_eq!(result, Ok((Some(b"good".to_vec()), 2)));
        assert_eq!(reads, 2);

        let mut reads = 0;
        let result = retry_verified(
            1,
            |_| {
                reads += 1;
                Ok::<_, ()>(b"bad".to_vec())
            },
            |value| value == b"good",
        );
        assert_eq!(result, Ok((None, 1)));
        assert_eq!(reads, 1);

        let mut reads = 0;
        let result = retry_verified(
            3,
            |_| -> std::result::Result<&[u8], &'static str> {
                reads += 1;
                Err("device failed")
            },
            |_| true,
        );
        assert_eq!(result, Err("device failed"));
        assert_eq!(reads, 1);
    }
}
