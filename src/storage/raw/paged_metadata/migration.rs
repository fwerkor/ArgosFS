//! Resumable conversion into a *separate, exclusively owned* V3 metadata
//! arena. This module never publishes raw superblocks or changes the source.
//! A verified staging result is not a production mount or format switch.
use super::typed::{decode_record, encode_record, import_image_with_events};
use super::{ScanOrder, ScanWindow, TypedChanges, TypedKey, TypedMetadata};
use crate::error::{ArgosError, Result};
use crate::types::{Metadata, PagedAuditEvent};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ops::Bound;

fn marker() -> TypedKey {
    TypedKey::new(0, b"migration/v1").expect("fixed migration key")
}

fn conflict(message: &str) -> ArgosError {
    ArgosError::Conflict(message.into())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StagingProgress {
    pub source_digest: [u8; 32],
    pub source_txid: u64,
    pub target_txid: u64,
    pub total_records: u64,
    pub copied_records: u64,
    pub cursor: Option<Vec<u8>>,
    pub verified: bool,
}

/// An immutable authenticated source image. Encoding still uses the existing
/// in-memory source representation. Target commits and comparison scans are
/// bounded; no full destination image is loaded.
pub struct MigrationSource {
    pool: [u8; 16],
    txid: u64,
    records: TypedChanges,
    digest: [u8; 32],
}

impl MigrationSource {
    /// The caller must supply a fully verified, offline source image and
    /// audit events. No attempt is made to recover a live source here.
    pub fn from_image(meta: &Metadata, events: &[PagedAuditEvent]) -> Result<Self> {
        let pool = uuid::Uuid::parse_str(&meta.uuid)
            .map_err(|_| ArgosError::Invalid("invalid source pool UUID".into()))?;
        let records = import_image_with_events(meta, events)?;
        let mut hash = Sha256::new();
        hash.update(b"ArgosFS/V3/staging-source/v1\0");
        hash.update(pool.as_bytes());
        hash.update(meta.txid.to_le_bytes());
        for (key, value) in &records {
            let value = value
                .as_ref()
                .ok_or_else(|| conflict("deletion in source image"))?;
            hash.update((key.as_bytes().len() as u64).to_le_bytes());
            hash.update(key.as_bytes());
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value);
        }
        Ok(Self {
            pool: *pool.as_bytes(),
            txid: meta.txid,
            records,
            digest: hash.finalize().into(),
        })
    }

    fn check_pool(&self, target: &TypedMetadata) -> Result<()> {
        if target.pool != self.pool {
            return Err(conflict("V3 staging target belongs to a different pool"));
        }
        Ok(())
    }

    fn load(&self, target: &TypedMetadata) -> Result<Option<StagingProgress>> {
        self.check_pool(target)?;
        let Some(bytes) = target.get(marker())? else {
            return Ok(None);
        };
        let progress: StagingProgress = decode_record(&bytes)?;
        if progress.source_digest != self.digest
            || progress.source_txid != self.txid
            || progress.total_records != self.records.len() as u64
            || progress.target_txid != target.txid()
            || progress.copied_records > progress.total_records
            || (progress.copied_records == 0) != progress.cursor.is_none()
            || (progress.verified && progress.copied_records != progress.total_records)
        {
            return Err(conflict("V3 staging source or destination changed"));
        }
        if let Some(cursor) = &progress.cursor {
            let key = TypedKey::from_bytes(cursor.clone())?;
            if !self.records.contains_key(&key) {
                return Err(conflict("V3 staging cursor not in source"));
            }
        }
        Ok(Some(progress))
    }

    /// Start or resume. Refuse a nonempty unmarked arena rather than
    /// interpreting an existing destination as staging scratch space.
    pub fn begin(&self, target: &mut TypedMetadata) -> Result<StagingProgress> {
        if let Some(progress) = self.load(target)? {
            return Ok(progress);
        }
        if target.txid() != 0
            || !target
                .scan(&ScanWindow {
                    start: Bound::Unbounded,
                    end: Bound::Unbounded,
                    max_records: 1,
                    max_bytes: 1024 * 1024,
                    order: ScanOrder::Ascending,
                })?
                .records
                .is_empty()
        {
            return Err(conflict("V3 staging requires an unused destination"));
        }
        let progress = StagingProgress {
            source_digest: self.digest,
            source_txid: self.txid,
            target_txid: 1,
            total_records: self.records.len() as u64,
            copied_records: 0,
            cursor: None,
            verified: false,
        };
        target.commit(&TypedChanges::from([(
            marker(),
            Some(encode_record(&progress)?),
        )]))?;
        Ok(progress)
    }

    /// Copy a bounded window with its resume cursor in the SAME certified
    /// transaction. An interrupted acknowledgement is resolved by reopening
    /// the destination and reading the authoritative progress marker.
    /// max_bytes covers source key/value bytes, excluding the small marker.
    pub fn copy_step(
        &self,
        target: &mut TypedMetadata,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<StagingProgress> {
        if max_records == 0 || max_bytes == 0 {
            return Err(ArgosError::Invalid(
                "V3 staging budget must be nonzero".into(),
            ));
        }
        let mut progress = self
            .load(target)?
            .ok_or_else(|| conflict("V3 staging has not begun"))?;
        if progress.copied_records == progress.total_records {
            return Ok(progress);
        }
        let start = progress
            .cursor
            .clone()
            .map(TypedKey::from_bytes)
            .transpose()?
            .map_or(Bound::Unbounded, Bound::Excluded);
        let mut changes = TypedChanges::new();
        let mut bytes = 0usize;
        for (key, value) in self
            .records
            .range((start, Bound::Unbounded))
            .take(max_records)
        {
            let cost = key
                .as_bytes()
                .len()
                .checked_add(value.as_ref().map_or(0, Vec::len))
                .ok_or_else(|| conflict("V3 staging record size overflow"))?;
            if cost > max_bytes.saturating_sub(bytes) {
                if changes.is_empty() {
                    return Err(ArgosError::Invalid(
                        "V3 staging budget smaller than next record".into(),
                    ));
                }
                break;
            }
            bytes += cost;
            changes.insert(key.clone(), value.clone());
            progress.cursor = Some(key.as_bytes().to_vec());
        }
        if changes.is_empty() {
            return Err(conflict("V3 staging made no progress"));
        }
        progress.copied_records = progress
            .copied_records
            .checked_add(changes.len() as u64)
            .filter(|count| *count <= progress.total_records)
            .ok_or_else(|| conflict("V3 staging record count mismatch"))?;
        progress.target_txid = target
            .txid()
            .checked_add(1)
            .ok_or_else(|| conflict("V3 staging txid overflow"))?;
        changes.insert(marker(), Some(encode_record(&progress)?));
        target.commit(&changes)?;
        Ok(progress)
    }

    /// Exhaustively compare all ordered logical records and every currently
    /// configured readable member's physical allocator before certifying
    /// the staging result. Any unavailable/stale member must be repaired
    /// by the caller before this step. This still does NOT switch formats.
    pub fn verify(
        &self,
        target: &mut TypedMetadata,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<StagingProgress> {
        let mut progress = self
            .load(target)?
            .ok_or_else(|| conflict("V3 staging has not begun"))?;
        if progress.copied_records != progress.total_records {
            return Err(conflict("V3 staging copy incomplete"));
        }
        let mut expected = self.records.iter();
        let mut window = ScanWindow {
            start: Bound::Excluded(marker()),
            end: Bound::Unbounded,
            max_records,
            max_bytes,
            order: ScanOrder::Ascending,
        };
        // The marker is the only legal key in domain zero. Check the
        // prefix before it too, so an unexpected key cannot evade audit.
        if !target
            .scan(&ScanWindow {
                start: Bound::Unbounded,
                end: Bound::Excluded(marker()),
                max_records: 1,
                max_bytes,
                order: ScanOrder::Ascending,
            })?
            .records
            .is_empty()
        {
            return Err(conflict("unexpected record before V3 staging marker"));
        }
        loop {
            let page = target.scan(&window)?;
            for (key, value) in &page.records {
                match expected.next() {
                    Some((expected_key, Some(expected_value)))
                        if expected_key == key && expected_value == value => {}
                    _ => return Err(conflict("V3 staging source/target record mismatch")),
                }
            }
            if !page.more {
                break;
            }
            let (key, _) = page
                .records
                .last()
                .ok_or_else(|| conflict("V3 verification made no progress"))?;
            window.start = Bound::Excluded(key.clone());
        }
        if expected.next().is_some() {
            return Err(conflict("V3 staging destination missing source records"));
        }
        let audit = target.audit_allocators();
        if audit.len() != target.configured_members || audit.values().any(Result::is_err) {
            return Err(conflict(
                "V3 staging requires all configured members with valid allocators",
            ));
        }
        if progress.verified {
            return Ok(progress);
        }
        progress.verified = true;
        progress.target_txid = target
            .txid()
            .checked_add(1)
            .ok_or_else(|| conflict("V3 staging txid overflow"))?;
        let report = target.commit(&TypedChanges::from([(
            marker(),
            Some(encode_record(&progress)?),
        )]))?;
        if report.members.len() != target.configured_members || !report.failures.is_empty() {
            return Err(conflict(
                "V3 staging verification committed with unavailable members; repair and reverify",
            ));
        }
        Ok(progress)
    }
}
