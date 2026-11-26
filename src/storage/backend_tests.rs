use super::*;
use std::io::Write as _;
use tempfile::{tempdir, NamedTempFile};

#[test]
fn host_backend_reports_its_intentionally_limited_capabilities() {
    let backend = HostFsBackend::new("/tmp/argosfs-host-backend");
    let id = "unused".to_string();
    assert_eq!(backend.backend_kind(), BackendKind::Host);
    assert!(backend.list_devices().unwrap().is_empty());
    assert!(matches!(
        backend.read_at(&id, 0, &mut [0u8; 1]),
        Err(ArgosError::Unsupported(_))
    ));
    assert!(matches!(
        backend.write_at(&id, 0, b"x"),
        Err(ArgosError::Unsupported(_))
    ));
    backend.flush_device(&id).unwrap();
    backend.flush_all().unwrap();
    assert!(matches!(
        backend.capacity(&id),
        Err(ArgosError::Unsupported(_))
    ));
    assert_eq!(backend.device_status(&id).unwrap(), DiskStatus::Online);
    let capabilities = backend.capabilities();
    assert!(!capabilities.direct_io);
    assert!(capabilities.fallocate);
    assert!(!capabilities.discard);
}

fn image(size: u64) -> NamedTempFile {
    let file = NamedTempFile::new().unwrap();
    file.as_file().set_len(size).unwrap();
    file
}

#[test]
fn loop_backend_lists_reads_writes_flushes_and_reports_capacity() {
    let first = image(8192);
    let second = image(4096);
    let paths = vec![first.path().to_path_buf(), second.path().to_path_buf()];
    let backend = FileBlockBackend::open_loop(&paths, true).unwrap();
    assert_eq!(backend.backend_kind(), BackendKind::LoopBlock);
    assert_eq!(backend.paths(), paths);
    let devices = backend.list_devices().unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].device_id, "disk-0000");
    assert_eq!(devices[0].capacity, 8192);
    assert_eq!(devices[0].status, DiskStatus::Online);

    let id = "disk-0000".to_string();
    backend.write_at(&id, 1024, b"backend payload").unwrap();
    backend.flush_device(&id).unwrap();
    backend.flush_all().unwrap();
    let mut output = vec![0; b"backend payload".len()];
    backend.read_at(&id, 1024, &mut output).unwrap();
    assert_eq!(output, b"backend payload");
    assert_eq!(backend.capacity(&id).unwrap(), 8192);
    assert_eq!(backend.device_status(&id).unwrap(), DiskStatus::Online);
    let capabilities = backend.capabilities();
    assert!(capabilities.fallocate);
    assert!(!capabilities.discard);
}

#[test]
fn explicit_ids_and_raw_capabilities_are_preserved() {
    let file = image(4096);
    let backend = FileBlockBackend::open_with_ids(
        BackendKind::RawBlock,
        vec![("member-a".to_string(), file.path().to_path_buf())],
        true,
    )
    .unwrap();
    assert_eq!(backend.backend_kind(), BackendKind::RawBlock);
    assert_eq!(backend.list_devices().unwrap()[0].device_id, "member-a");
    assert!(backend.capabilities().discard);

    let raw = FileBlockBackend::open_raw(&[file.path().to_path_buf()], false).unwrap();
    assert_eq!(raw.backend_kind(), BackendKind::RawBlock);
    assert!(raw.capabilities().discard);
}

#[test]
fn file_backend_reports_missing_full_overflow_and_short_read_errors() {
    let file = image(16);
    let backend = FileBlockBackend::open_loop(&[file.path().to_path_buf()], true).unwrap();
    let id = "disk-0000".to_string();
    let missing = "missing".to_string();
    assert!(matches!(
        backend.capacity(&missing),
        Err(ArgosError::MissingDevice(_))
    ));
    assert!(matches!(
        backend.write_at(&id, 15, b"xx"),
        Err(ArgosError::DiskFull { .. })
    ));
    assert!(matches!(
        backend.write_at(&id, u64::MAX, b"xx"),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        backend.read_at(&id, 16, &mut [0u8; 1]),
        Err(ArgosError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof
    ));
    assert!(matches!(
        backend.flush_device(&missing),
        Err(ArgosError::MissingDevice(_))
    ));
    assert!(matches!(
        backend.device_status(&missing),
        Err(ArgosError::MissingDevice(_))
    ));
}

#[test]
fn readonly_backend_rejects_writes_at_the_os_boundary() {
    let file = image(4096);
    let backend = FileBlockBackend::open_loop(&[file.path().to_path_buf()], false).unwrap();
    let id = "disk-0000".to_string();
    assert!(matches!(
        backend.write_at(&id, 0, b"x"),
        Err(ArgosError::Io(_))
    ));
}

#[test]
fn capacity_detection_handles_regular_empty_and_nonempty_files() {
    let mut nonempty = NamedTempFile::new().unwrap();
    nonempty.write_all(b"12345678").unwrap();
    nonempty.flush().unwrap();
    assert_eq!(detect_capacity(nonempty.as_file_mut()).unwrap(), 8);

    let mut empty = NamedTempFile::new().unwrap();
    assert_eq!(detect_capacity(empty.as_file_mut()).unwrap(), 0);
    assert_eq!(block_device_capacity(empty.as_file()).unwrap(), None);
}

#[test]
fn opening_missing_devices_returns_io_errors() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("missing.img");
    assert!(matches!(
        FileBlockBackend::open_loop(&[missing], false),
        Err(ArgosError::Io(_))
    ));
}

#[test]
fn raw_members_separate_shard_and_bulk_read_handles() {
    // Shard payload reads use a POSIX_FADV_RANDOM handle so the kernel does
    // not prefetch past a chunk-sized read; large metadata reads keep a
    // second handle with the default readahead policy.
    const LARGE: usize = 1024 * 1024;
    let size = 2 * LARGE as u64;
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(&vec![0x5au8; size as usize]).unwrap();
    file.as_file().write_at(b"tail", size - 4).unwrap();
    let path = file.path().to_path_buf();

    let raw = FileBlockBackend::open_raw(std::slice::from_ref(&path), false).unwrap();
    let device = raw.device(&"disk-0000".to_string()).unwrap();
    assert!(device.shard_file.is_some());

    let mut small = vec![0u8; 4096];
    raw.read_at(&"disk-0000".to_string(), 0, &mut small)
        .unwrap();
    assert_eq!(small, vec![0x5au8; 4096]);

    let offset = LARGE as u64 - 8;
    let mut large = vec![0u8; LARGE];
    raw.read_shard_at(&"disk-0000".to_string(), offset, &mut large)
        .unwrap();
    assert_eq!(large[..8], [0x5au8; 8]);
    assert_eq!(large[LARGE - 8..], [0x5au8; 8]);

    assert!(same_open_device(&device.file, device.shard_file.as_ref().unwrap()).unwrap());
    assert!(!same_open_device(&device.file, NamedTempFile::new().unwrap().as_file()).unwrap());
    let mut tail = vec![0u8; 4];
    raw.read_at(&"disk-0000".to_string(), size - 4, &mut tail)
        .unwrap();
    assert_eq!(tail, b"tail");

    // Loop images keep a single handle: their backing file is host storage,
    // where the default readahead policy is left alone.
    let loop_backend = FileBlockBackend::open_loop(&[path], false).unwrap();
    assert!(loop_backend
        .device(&"disk-0000".to_string())
        .unwrap()
        .shard_file
        .is_none());
}
