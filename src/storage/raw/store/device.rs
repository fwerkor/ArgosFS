use super::*;

pub fn scan_paths(kind: BackendKind, paths: &[PathBuf]) -> Vec<ScannedDevice> {
    if kind == BackendKind::Host {
        return paths
            .iter()
            .map(|path| ScannedDevice {
                path: path.clone(),
                valid: false,
                error: Some("host backend is not scan-able as raw block".to_string()),
                pool_uuid: None,
                device_uuid: None,
                disk_id: None,
                disk_index: None,
                generation: None,
                clean: None,
                label: None,
                capacity: 0,
                superblock_source: None,
            })
            .collect();
    }
    paths.iter().map(|path| scan_one_path(kind, path)).collect()
}

pub(super) fn preflight_empty(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    force: bool,
) -> Result<()> {
    if force {
        return Ok(());
    }
    for sb in superblocks {
        let mut buf = vec![0u8; SUPERBLOCK_SIZE];
        if backend
            .read_at(&sb.disk_id, PRIMARY_SUPERBLOCK_OFFSET, &mut buf)
            .is_ok()
            && RawSuperblock::decode(&buf).is_ok()
        {
            return Err(ArgosError::AlreadyExists(format!(
                "{} already contains an ArgosFS raw superblock; pass --force to overwrite",
                sb.disk_id
            )));
        }
        let probe_len = sb.data.offset.clamp(128 * 1024, 4 * 1024 * 1024) as usize;
        let mut head = vec![0u8; probe_len];
        backend.read_at(&sb.disk_id, 0, &mut head)?;
        let capacity = backend.capacity(&sb.disk_id)?;
        let tail_len = probe_len.min(capacity as usize);
        let mut tail = vec![0u8; tail_len];
        backend.read_at(
            &sb.disk_id,
            capacity.saturating_sub(tail_len as u64),
            &mut tail,
        )?;
        if has_known_signature(&head, &tail) || head.iter().chain(&tail).any(|byte| *byte != 0) {
            return Err(ArgosError::UnsafeMount(format!(
                "{} contains an existing signature or non-zero data in protected probe regions; pass --force to overwrite",
                sb.disk_id
            )));
        }
    }
    Ok(())
}

pub(super) fn has_known_signature(head: &[u8], tail: &[u8]) -> bool {
    head.starts_with(b"hsqs")
        || head.starts_with(b"XFSB")
        || head.starts_with(b"LUKS\xba\xbe")
        || head.get(3..11) == Some(b"NTFS    ")
        || head.get(3..11) == Some(b"EXFAT   ")
        || head.get(512..520) == Some(b"EFI PART")
        || head.get(512..520) == Some(b"LABELONE")
        || head.get(0x438..0x43a) == Some(&[0x53, 0xef])
        || head.get(1024..1028) == Some(&0xF2F5_2010u32.to_le_bytes())
        || head.get(4096..4100) == Some(&0xA92B_4EFCu32.to_le_bytes())
        || head
            .get(0x10040..0x10048)
            .is_some_and(|sig| sig == b"_BHRfS_M")
        || contains_swap_signature(head)
        || contains_swap_signature(tail)
        || tail.windows(8).any(|window| window == b"EFI PART")
        || tail
            .windows(4)
            .any(|window| window == 0xA92B_4EFCu32.to_le_bytes())
}

pub(super) fn contains_swap_signature(bytes: &[u8]) -> bool {
    [4096usize, 8192, 16384, 32768, 65536]
        .into_iter()
        .filter_map(|page_size| page_size.checked_sub(10))
        .any(|offset| bytes.get(offset..offset + 10) == Some(b"SWAPSPACE2"))
}

#[allow(clippy::too_many_arguments)]
pub fn superblock_for_device(
    pool_uuid: Uuid,
    disk_index: usize,
    disk_id: &str,
    k: usize,
    m: usize,
    chunk_size: usize,
    capacity: u64,
    label: &str,
) -> Result<RawSuperblock> {
    let disk_index = u32::try_from(disk_index)
        .map_err(|_| ArgosError::Invalid("raw disk index exceeds u32".to_string()))?;
    let k = u32::try_from(k)
        .map_err(|_| ArgosError::Invalid("raw data shard count exceeds u32".to_string()))?;
    let m = u32::try_from(m)
        .map_err(|_| ArgosError::Invalid("raw parity shard count exceeds u32".to_string()))?;
    let chunk_size = u64::try_from(chunk_size)
        .map_err(|_| ArgosError::Invalid("raw chunk size exceeds u64".to_string()))?;
    RawSuperblock::new(
        pool_uuid,
        Uuid::new_v4(),
        disk_id.to_string(),
        disk_index,
        k,
        m,
        chunk_size,
        capacity,
        label.to_string(),
    )
}

fn guarded_single_device(
    kind: BackendKind,
    path: &PathBuf,
    write: bool,
    host_error: &str,
) -> Result<Arc<dyn StorageBackend>> {
    let inner: Arc<dyn StorageBackend> = match kind {
        BackendKind::LoopBlock => Arc::new(FileBlockBackend::open_loop(
            std::slice::from_ref(path),
            write,
        )?),
        BackendKind::RawBlock => Arc::new(FileBlockBackend::open_raw(
            std::slice::from_ref(path),
            write,
        )?),
        BackendKind::Host => {
            return Err(ArgosError::Unsupported(host_error.to_string()));
        }
    };
    Ok(Arc::new(GuardedBackend::new(inner)))
}

pub fn inspect_device(kind: BackendKind, path: PathBuf) -> Result<(RawSuperblock, RawDeviceLabel)> {
    let backend = guarded_single_device(
        kind,
        &path,
        false,
        "inspect-device is for loop/raw block backends",
    )?;
    let id = "disk-0000".to_string();
    let capacity = backend.capacity(&id)?;
    let mut sb = vec![0u8; SUPERBLOCK_SIZE];
    backend.read_at(&id, PRIMARY_SUPERBLOCK_OFFSET, &mut sb)?;
    let decode = |bytes: &[u8]| -> Result<RawSuperblock> {
        let superblock = RawSuperblock::decode(bytes)?;
        superblock.validate_device_capacity(capacity)?;
        Ok(superblock)
    };
    let superblock = match decode(&sb) {
        Ok(superblock) => superblock,
        Err(primary_err) => {
            let mut backup = vec![0u8; SUPERBLOCK_SIZE];
            let backup_offset = backup_superblock_offset_for_capacity(capacity);
            backend.read_at(&id, backup_offset, &mut backup)?;
            decode(&backup).map_err(|backup_err| {
                ArgosError::IncompatibleFormat(format!(
                    "primary superblock failed ({primary_err}); backup superblock failed ({backup_err})"
                ))
            })?
        }
    };
    let label = read_label_with_backup(&*backend, &id, &superblock)?;
    Ok((superblock, label))
}

/// Read a device label, falling back to the backup copy next to the backup
/// superblock. The primary label is refreshed on every writable open and clean
/// unmount, so a single torn 4 KiB label write must not remove an otherwise
/// healthy member from the pool.
pub(super) fn read_label_with_backup(
    backend: &dyn StorageBackend,
    device_id: &str,
    superblock: &RawSuperblock,
) -> Result<RawDeviceLabel> {
    let mut primary = vec![0u8; DEVICE_LABEL_SIZE];
    let primary_result = backend
        .read_at(&device_id.to_string(), DEVICE_LABEL_OFFSET, &mut primary)
        .and_then(|()| RawDeviceLabel::decode(&primary))
        .and_then(|label| {
            validate_label_matches_superblock(superblock, &label)?;
            Ok(label)
        });
    match primary_result {
        Ok(label) => Ok(label),
        Err(primary_err) => {
            let mut backup = vec![0u8; DEVICE_LABEL_SIZE];
            let backup_offset = superblock.backup_superblock_offset + SUPERBLOCK_SIZE as u64;
            backend
                .read_at(&device_id.to_string(), backup_offset, &mut backup)
                .and_then(|()| RawDeviceLabel::decode(&backup))
                .and_then(|label| {
                    validate_label_matches_superblock(superblock, &label)?;
                    Ok(label)
                })
                .map_err(|backup_err| {
                    ArgosError::CorruptedMetadata(format!(
                        "primary device label failed ({primary_err}); backup label failed ({backup_err})"
                    ))
                })
        }
    }
}

fn scan_one_path(kind: BackendKind, path: &PathBuf) -> ScannedDevice {
    // Each attempt opens a fresh guarded device, since a fatal I/O error
    // fences the previous backend for its entire lifetime.
    scan_one_path_with_retry(|| scan_one_path_once(kind, path))
}

pub(super) fn scan_one_path_with_retry(
    mut scan_once: impl FnMut() -> ScannedDevice,
) -> ScannedDevice {
    let mut last = None;
    for attempt in 0..RAW_SCAN_ATTEMPTS {
        let scanned = scan_once();
        if scanned.valid {
            return scanned;
        }
        last = Some(scanned);
        if attempt + 1 < RAW_SCAN_ATTEMPTS {
            std::thread::sleep(RAW_SCAN_RETRY_DELAY);
        }
    }
    last.expect("scan makes at least one attempt")
}

pub(super) fn scan_one_path_once(kind: BackendKind, path: &PathBuf) -> ScannedDevice {
    let backend = guarded_single_device(kind, path, false, "scan is for loop/raw block backends");
    let Ok(backend) = backend else {
        return ScannedDevice {
            path: path.clone(),
            valid: false,
            error: Some("failed to open block path".to_string()),
            pool_uuid: None,
            device_uuid: None,
            disk_id: None,
            disk_index: None,
            generation: None,
            clean: None,
            label: None,
            capacity: 0,
            superblock_source: None,
        };
    };
    let id = "disk-0000".to_string();
    let capacity = backend.capacity(&id).unwrap_or(0);
    match read_superblock_with_backup(&*backend, &id, capacity) {
        Ok((sb, source)) => {
            let label_result = read_label_with_backup(&*backend, &id, &sb).map(|_label| ());
            if let Err(err) = label_result {
                return ScannedDevice {
                    path: path.clone(),
                    valid: false,
                    error: Some(err.to_string()),
                    pool_uuid: Some(sb.pool_uuid.to_string()),
                    device_uuid: Some(sb.device_uuid.to_string()),
                    disk_id: Some(sb.disk_id),
                    disk_index: Some(sb.disk_index),
                    generation: Some(sb.generation),
                    clean: Some(sb.clean),
                    label: Some(sb.label),
                    capacity,
                    superblock_source: Some(source.to_string()),
                };
            }
            ScannedDevice {
                path: path.clone(),
                valid: true,
                error: None,
                pool_uuid: Some(sb.pool_uuid.to_string()),
                device_uuid: Some(sb.device_uuid.to_string()),
                disk_id: Some(sb.disk_id),
                disk_index: Some(sb.disk_index),
                generation: Some(sb.generation),
                clean: Some(sb.clean),
                label: Some(sb.label),
                capacity,
                superblock_source: Some(source.to_string()),
            }
        }
        Err(err) => ScannedDevice {
            path: path.clone(),
            valid: false,
            error: Some(err.to_string()),
            pool_uuid: None,
            device_uuid: None,
            disk_id: None,
            disk_index: None,
            generation: None,
            clean: None,
            label: None,
            capacity,
            superblock_source: None,
        },
    }
}

pub(super) fn superblock_from_path(
    kind: BackendKind,
    path: &PathBuf,
) -> Result<(RawSuperblock, &'static str)> {
    let backend = guarded_single_device(kind, path, false, "host backend has no raw superblock")?;
    let id = "disk-0000".to_string();
    let capacity = backend.capacity(&id)?;
    read_superblock_with_backup(&*backend, &id, capacity)
}

pub(super) fn read_superblock_with_backup(
    backend: &dyn StorageBackend,
    device_id: &str,
    capacity: u64,
) -> Result<(RawSuperblock, &'static str)> {
    let mut last = None;
    for attempt in 0..RAW_SCAN_ATTEMPTS {
        match read_superblock_with_backup_once(backend, device_id, capacity) {
            Ok(found) => return Ok(found),
            Err(err @ (ArgosError::DeviceFailed { .. } | ArgosError::DeviceTimeout { .. })) => {
                // A fenced device cannot recover inside the same backend.
                // Leave recovery to the outer scan, which reopens it.
                return Err(err);
            }
            Err(err) => last = Some(err),
        }
        if attempt + 1 < RAW_SCAN_ATTEMPTS {
            std::thread::sleep(RAW_SCAN_RETRY_DELAY);
        }
    }
    Err(last.expect("superblock read makes at least one attempt"))
}

fn read_superblock_with_backup_once(
    backend: &dyn StorageBackend,
    device_id: &str,
    capacity: u64,
) -> Result<(RawSuperblock, &'static str)> {
    let mut primary = vec![0u8; SUPERBLOCK_SIZE];
    let primary_result = backend
        .read_at(
            &device_id.to_string(),
            PRIMARY_SUPERBLOCK_OFFSET,
            &mut primary,
        )
        .and_then(|()| RawSuperblock::decode(&primary))
        .and_then(|sb| {
            sb.validate_device_capacity(capacity)?;
            Ok(sb)
        });
    match primary_result {
        Ok(sb) => Ok((sb, "primary")),
        Err(primary_err) => {
            let mut backup = vec![0u8; SUPERBLOCK_SIZE];
            let backup_offset = backup_superblock_offset_for_capacity(capacity);
            backend
                .read_at(&device_id.to_string(), backup_offset, &mut backup)
                .and_then(|()| RawSuperblock::decode(&backup))
                .and_then(|sb| {
                    sb.validate_device_capacity(capacity)?;
                    Ok(sb)
                })
                .map(|sb| (sb, "backup"))
                .map_err(|backup_err| {
                    if matches!(
                        backup_err,
                        ArgosError::DeviceFailed { .. } | ArgosError::DeviceTimeout { .. }
                    ) {
                        return backup_err;
                    }
                    ArgosError::IncompatibleFormat(format!(
                        "primary superblock failed ({primary_err}); backup superblock failed ({backup_err})"
                    ))
                })
        }
    }
}

pub(super) fn backup_superblock_offset_for_capacity(capacity: u64) -> u64 {
    align_down(capacity.saturating_sub(BACKUP_REGION_SIZE), 4096)
}

pub(super) fn validate_label_matches_superblock(
    superblock: &RawSuperblock,
    label: &RawDeviceLabel,
) -> Result<()> {
    if superblock.pool_uuid != label.pool_uuid
        || superblock.device_uuid != label.device_uuid
        || superblock.disk_id != label.disk_id
        || superblock.disk_index != label.disk_index
    {
        return Err(ArgosError::CorruptedMetadata(
            "device label does not match raw superblock".to_string(),
        ));
    }
    Ok(())
}
