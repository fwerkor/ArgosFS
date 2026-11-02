#![allow(dead_code)]

use argosfs::metadata::journal;
use argosfs::types::{
    Inode, Metadata, MetadataIntegrity, NodeKind, RawPoolMetadata, StorageTier, FORMAT_VERSION,
};
use argosfs::VolumeConfig;
use std::collections::BTreeMap;

pub fn byte(data: &[u8], index: usize) -> u8 {
    data.get(index).copied().unwrap_or(0)
}

pub fn u64_at(data: &[u8], offset: usize) -> u64 {
    let mut bytes = [0u8; 8];
    for (index, slot) in bytes.iter_mut().enumerate() {
        *slot = byte(data, offset + index);
    }
    u64::from_le_bytes(bytes)
}

pub fn text_at(data: &[u8], offset: usize, max_len: usize, fallback: &str) -> String {
    if max_len == 0 {
        return fallback.to_string();
    }
    let wanted = 1 + (byte(data, offset) as usize % max_len);
    let mut out = String::with_capacity(wanted);
    for index in 0..wanted {
        let raw = byte(data, offset + 1 + index);
        let ch = match raw % 64 {
            0..=25 => (b'a' + raw % 26) as char,
            26..=51 => (b'A' + raw % 26) as char,
            52..=61 => (b'0' + raw % 10) as char,
            62 => '-',
            _ => '_',
        };
        out.push(ch);
    }
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

pub fn metadata_from(data: &[u8], salt: usize) -> Metadata {
    let mut config = VolumeConfig::default();
    config.k = 1 + (byte(data, salt) as usize % 6);
    config.m = byte(data, salt + 1) as usize % 4;
    config.chunk_size = 4096usize << (byte(data, salt + 2) as usize % 6);
    config.compression_level = (byte(data, salt + 3) as i8 % 10) as i32;
    config.l2_cache_bytes = u64_at(data, salt + 4) % (512 * 1024 * 1024);
    config.fsname = text_at(data, salt + 12, 24, "argosfs-fuzz");
    config.direct_io = byte(data, salt + 40) & 1 != 0;
    config.zero_copy = byte(data, salt + 41) & 1 != 0;
    config.numa_aware = byte(data, salt + 42) & 1 != 0;
    config.defer_journal_flush = byte(data, salt + 43) & 1 != 0;
    config.defer_metadata_commit = byte(data, salt + 44) & 1 != 0;
    config.defer_data_flush = byte(data, salt + 45) & 1 != 0;
    config.deferred_commit_interval_ms = 1 + u64_at(data, salt + 46) % 60_000;
    config.deferred_commit_max_transactions = 1 + u64_at(data, salt + 54) % 1024;

    let txid = 1 + u64_at(data, salt + 62) % 10_000;
    let created_at = (u64_at(data, salt + 70) % 2_000_000_000) as f64;
    let updated_at = created_at + (u64_at(data, salt + 78) % 1_000_000) as f64;

    Metadata {
        format: FORMAT_VERSION.to_string(),
        uuid: format!("fuzz-{:016x}", u64_at(data, salt + 86)),
        backend: Default::default(),
        raw_pool: RawPoolMetadata {
            pool_name: text_at(data, salt + 94, 32, "fuzz-pool"),
            ..RawPoolMetadata::default()
        },
        created_at,
        updated_at,
        txid,
        next_inode: 3,
        next_stripe: u64_at(data, salt + 128),
        config,
        layouts: BTreeMap::new(),
        current_write_layout: String::new(),
        reshape: None,
        encryption: Default::default(),
        integrity: MetadataIntegrity::default(),
        disks: BTreeMap::new(),
        inodes: BTreeMap::new(),
    }
}

pub fn add_small_inode_tree(meta: &mut Metadata, data: &[u8], salt: usize) {
    let mut root_entries = BTreeMap::new();
    root_entries.insert(text_at(data, salt, 20, "file"), 2);

    let mut root_xattrs = BTreeMap::new();
    root_xattrs.insert(
        format!("user.{}", text_at(data, salt + 24, 16, "root")),
        text_at(data, salt + 48, 32, "value"),
    );

    let root = Inode {
        id: 1,
        kind: NodeKind::Directory,
        mode: 0o700 | (byte(data, salt + 82) as u32 & 0o77),
        uid: u64_at(data, salt + 83) as u32,
        gid: u64_at(data, salt + 91) as u32,
        nlink: 2,
        size: 0,
        rdev: 0,
        atime: meta.created_at,
        mtime: meta.updated_at,
        ctime: meta.updated_at,
        entries: root_entries,
        target: None,
        inline_data: None,
        inline_sha256: String::new(),
        blocks: vec![],
        quota_bytes: None,
        xattrs: root_xattrs,
        posix_acl_access: None,
        posix_acl_default: None,
        nfs4_acl: None,
        access_count: u64_at(data, salt + 99),
        write_count: u64_at(data, salt + 107),
        read_bytes: u64_at(data, salt + 115),
        write_bytes: u64_at(data, salt + 123),
        storage_class: StorageTier::Warm,
        boot_critical: byte(data, salt + 131) & 1 != 0,
        workload_score: (byte(data, salt + 132) as f64) / 255.0,
        last_accessed_at: meta.updated_at,
        last_written_at: meta.updated_at,
    };

    let payload_len = (byte(data, salt + 133) as usize).min(96);
    let payload = (0..payload_len)
        .map(|index| byte(data, salt + 134 + index))
        .collect::<Vec<_>>();
    let mut file_xattrs = BTreeMap::new();
    file_xattrs.insert(
        format!("user.{}", text_at(data, salt + 240, 16, "file")),
        text_at(data, salt + 264, 32, "value"),
    );
    let file = Inode {
        id: 2,
        kind: NodeKind::File,
        mode: 0o600 | (byte(data, salt + 300) as u32 & 0o77),
        uid: u64_at(data, salt + 301) as u32,
        gid: u64_at(data, salt + 309) as u32,
        nlink: 1,
        size: payload.len() as u64,
        rdev: 0,
        atime: meta.created_at,
        mtime: meta.updated_at,
        ctime: meta.updated_at,
        entries: BTreeMap::new(),
        target: None,
        inline_data: Some(payload),
        inline_sha256: String::new(),
        blocks: vec![],
        quota_bytes: None,
        xattrs: file_xattrs,
        posix_acl_access: None,
        posix_acl_default: None,
        nfs4_acl: None,
        access_count: u64_at(data, salt + 317),
        write_count: u64_at(data, salt + 325),
        read_bytes: u64_at(data, salt + 333),
        write_bytes: u64_at(data, salt + 341),
        storage_class: StorageTier::Warm,
        boot_critical: byte(data, salt + 349) & 1 != 0,
        workload_score: (byte(data, salt + 350) as f64) / 255.0,
        last_accessed_at: meta.updated_at,
        last_written_at: meta.updated_at,
    };

    meta.inodes.insert(1, root);
    meta.inodes.insert(2, file);
    meta.next_inode = 3;
}

pub fn prepare(meta: &mut Metadata) {
    journal::prepare_metadata_integrity_for_external_store(meta)
        .expect("generated fuzz metadata must be hashable");
}
