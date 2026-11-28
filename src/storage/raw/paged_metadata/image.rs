//! Typed bridge between the authenticated page store and ArgosFS Metadata.
//! The inode table is never serialized as a single JSON image. Header fields
//! and each individual inode are separately compressed records.
use super::{Arena, Changes, CommitIo, CommitReport, PagedMetadata};
use crate::backend::StorageBackend;
use crate::error::{ArgosError, Result};
use crate::types::{Inode, Metadata};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

const HEADER_KEY: u64 = 0;
const MAX_EXPANDED_RECORD: usize = 128 * 1024 * 1024;
const JSON: u8 = 0;
const LZ4: u8 = 1;

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_EXPANDED_RECORD {
        return Err(ArgosError::FileTooLarge(
            "paged metadata image record".into(),
        ));
    }
    let compressed = lz4_flex::compress_prepend_size(&bytes);
    let (tag, payload) = if compressed.len() < bytes.len() {
        (LZ4, compressed)
    } else {
        (JSON, bytes)
    };
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(tag);
    out.extend(payload);
    Ok(out)
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (tag, payload) = bytes
        .split_first()
        .ok_or_else(|| super::wire::corrupt("empty metadata image record"))?;
    match *tag {
        JSON if payload.len() <= MAX_EXPANDED_RECORD => Ok(serde_json::from_slice(payload)?),
        LZ4 => {
            let size = payload
                .get(..4)
                .map(|prefix| u32::from_le_bytes(prefix.try_into().unwrap()) as usize)
                .filter(|&len| len <= MAX_EXPANDED_RECORD)
                .ok_or_else(|| super::wire::corrupt("invalid metadata image expanded size"))?;
            let expanded = lz4_flex::decompress_size_prepended(payload).map_err(|err| {
                super::wire::corrupt(format!("invalid compressed metadata image: {err}"))
            })?;
            if expanded.len() != size {
                return Err(super::wire::corrupt("metadata image size mismatch"));
            }
            Ok(serde_json::from_slice(&expanded)?)
        }
        _ => Err(super::wire::corrupt(
            "unknown or oversized metadata image record",
        )),
    }
}

fn header(metadata: &Metadata) -> Result<Vec<u8>> {
    let mut header = metadata.clone();
    header.inodes = Default::default();
    header.checkpoint_member_catalog = None;
    encode(&header)
}

fn initial_records(metadata: &Metadata) -> Result<Changes> {
    let mut changes = Changes::from([(HEADER_KEY, Some(header(metadata)?))]);
    for (&key, inode) in &metadata.inodes {
        if key == HEADER_KEY || key != inode.id {
            return Err(super::wire::corrupt(
                "invalid metadata image inode identity",
            ));
        }
        changes.insert(key, Some(encode(inode)?));
    }
    Ok(changes)
}

fn read_image(tree: &PagedMetadata, pool: Uuid) -> Result<Metadata> {
    let mut records = tree.records()?;
    let mut metadata: Metadata = decode(
        &records
            .remove(&HEADER_KEY)
            .ok_or_else(|| super::wire::corrupt("missing metadata image header"))?,
    )?;
    if Uuid::parse_str(&metadata.uuid).ok() != Some(pool) || !metadata.inodes.is_empty() {
        return Err(super::wire::corrupt("invalid metadata image header"));
    }
    for (key, bytes) in records {
        let inode: Inode = decode(&bytes)?;
        if key != inode.id {
            return Err(super::wire::corrupt(
                "metadata image inode identity mismatch",
            ));
        }
        metadata.inodes.insert(key, inode);
    }
    metadata.inodes.mark_committed();
    Ok(metadata)
}

/// Typed metadata image on dedicated experimental page arenas. The caller
/// supplies transaction metadata, including its logical txid and integrity
/// fields. This bridge does not yet replace the production journal protocol.
pub struct ImageStore {
    tree: PagedMetadata,
    pool: Uuid,
    base: Metadata,
}

impl ImageStore {
    pub fn format(
        backend: Arc<dyn StorageBackend>,
        arenas: Vec<Arena>,
        metadata: &mut Metadata,
    ) -> Result<Self> {
        let pool = Uuid::parse_str(&metadata.uuid)
            .map_err(|err| ArgosError::Invalid(format!("invalid metadata pool UUID: {err}")))?;
        // Validate and serialize all records before initializing any region.
        let records = initial_records(metadata)?;
        let mut tree = PagedMetadata::format(backend, *pool.as_bytes(), arenas)?;
        tree.commit(&records)?;
        metadata.inodes.mark_committed();
        Ok(Self {
            tree,
            pool,
            base: metadata.clone(),
        })
    }

    pub fn open(backend: Arc<dyn StorageBackend>, pool: Uuid, arenas: Vec<Arena>) -> Result<Self> {
        let tree = PagedMetadata::open(backend, *pool.as_bytes(), arenas)?;
        let base = read_image(&tree, pool)?;
        Ok(Self { tree, pool, base })
    }

    /// This is a cheap COW snapshot of the current inode table. Mutate the
    /// returned snapshot and commit it without independently marking its
    /// inode baseline committed. Unrelated snapshots remain supported via
    /// TrackedInodes' exhaustive candidate fallback.
    pub fn snapshot(&self) -> Metadata {
        self.base.clone()
    }

    /// Commit only candidate changed inode records and the metadata header.
    /// The caller's baseline advances only after a certified root commit.
    pub fn commit(&mut self, next: &mut Metadata) -> Result<CommitReport> {
        if Uuid::parse_str(&next.uuid).ok() != Some(self.pool) || next.txid <= self.base.txid {
            return Err(ArgosError::Invalid(
                "metadata image must advance its pool transaction".into(),
            ));
        }
        let mut changes = Changes::from([(HEADER_KEY, Some(header(next)?))]);
        for key in self.base.inodes.changed_candidates(&next.inodes) {
            match (self.base.inodes.get(&key), next.inodes.get(&key)) {
                (None, None) => {}
                (Some(_), None) if key != HEADER_KEY => {
                    changes.insert(key, None);
                }
                (old, Some(inode)) if key != HEADER_KEY && key == inode.id => {
                    let bytes = encode(inode)?;
                    if old.map(encode).transpose()?.as_ref() != Some(&bytes) {
                        changes.insert(key, Some(bytes));
                    }
                }
                _ => {
                    return Err(super::wire::corrupt(
                        "invalid metadata image inode identity",
                    ))
                }
            }
        }
        let report = self.tree.commit(&changes)?;
        next.inodes.mark_committed();
        self.base = next.clone();
        Ok(report)
    }

    pub fn root_hash(&self) -> [u8; 32] {
        self.tree.root_hash()
    }

    pub fn repair_stale(
        &mut self,
    ) -> Result<BTreeMap<String, std::result::Result<CommitIo, String>>> {
        self.tree.repair_stale()
    }
}
