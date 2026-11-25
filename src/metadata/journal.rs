mod delta;
use delta::*;
pub use delta::{apply_metadata_delta, metadata_delta};

use crate::error::{ArgosError, Result};
use crate::metadata::store::MetadataHeaderPage;
use crate::types::{
    Metadata, MetadataCandidateReport, MetadataIntegrity, TransactionReport, FORMAT_VERSION,
};
use crate::util::{append_json_line, atomic_write, now_f64, read_to_vec, sha256_hex, FileLock};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const JOURNAL_VERSION: u32 = 1;
pub const DEFAULT_CHECKPOINT_INTERVAL_TXIDS: u64 = 128;
const CHECKPOINT_INTERVAL_ENV: &str = "ARGOSFS_CHECKPOINT_INTERVAL_TXIDS";
const JOURNAL_COMPACTION_ENV: &str = "ARGOSFS_DISABLE_JOURNAL_COMPACTION";
const PRIMARY_META: &str = "meta.primary.json";
const SECONDARY_META: &str = "meta.secondary.json";
const COMPAT_META: &str = "meta.json";

thread_local! {
    static THREAD_CRASH_POINT: RefCell<Option<String>> = const { RefCell::new(None) };
    static THREAD_CHECKPOINT_INTERVAL: Cell<Option<u64>> = const { Cell::new(None) };
    static THREAD_COMPACTION_DISABLED: Cell<Option<bool>> = const { Cell::new(None) };
    static THREAD_ARRAY_SPLICE: Cell<bool> = const { Cell::new(false) };
}

fn array_splice_allowed() -> bool {
    THREAD_ARRAY_SPLICE.with(Cell::get)
}

/// Enable gated array-splice deltas for the current commit thread.
pub struct ArraySpliceGuard {
    previous: bool,
}

pub fn allow_array_splice(allow: bool) -> ArraySpliceGuard {
    let previous = THREAD_ARRAY_SPLICE.with(|value| value.replace(allow));
    ArraySpliceGuard { previous }
}

impl Drop for ArraySpliceGuard {
    fn drop(&mut self) {
        THREAD_ARRAY_SPLICE.with(|value| value.set(self.previous));
    }
}

pub struct ThreadCrashPointGuard {
    previous: Option<String>,
}

impl Drop for ThreadCrashPointGuard {
    fn drop(&mut self) {
        THREAD_CRASH_POINT.with(|value| {
            *value.borrow_mut() = self.previous.take();
        });
    }
}

pub struct ThreadCheckpointIntervalGuard {
    previous: Option<u64>,
}

impl Drop for ThreadCheckpointIntervalGuard {
    fn drop(&mut self) {
        THREAD_CHECKPOINT_INTERVAL.with(|value| value.set(self.previous));
    }
}

pub struct ThreadCompactionGuard {
    previous: Option<bool>,
}

impl Drop for ThreadCompactionGuard {
    fn drop(&mut self) {
        THREAD_COMPACTION_DISABLED.with(|value| value.set(self.previous));
    }
}

#[derive(Clone, Debug)]
pub struct RecoveredMetadata {
    pub metadata: Metadata,
    pub report: TransactionReport,
}

#[derive(Clone, Debug)]
struct MetadataCandidate {
    source: String,
    metadata: Option<Metadata>,
    // Keep verified wire bytes: a decoded f64 can serialize differently.
    serialized: Option<Vec<u8>>,
    report: MetadataCandidateReport,
}

struct BuildRecordOptions {
    record_type: JournalRecordKind,
    action: String,
    details: serde_json::Value,
    previous_record_hash: String,
    time: f64,
    metadata: Option<Metadata>,
    metadata_delta: Option<Vec<MetadataDeltaOp>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum JournalRecordKind {
    #[default]
    Transaction,
    Checkpoint,
    Event,
}

impl JournalRecordKind {
    fn is_transaction(kind: &Self) -> bool {
        matches!(kind, Self::Transaction)
    }

    fn is_checkpoint(&self) -> bool {
        matches!(self, Self::Checkpoint)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum MetadataDeltaOp {
    Add {
        path: String,
        value: serde_json::Value,
    },
    Remove {
        path: String,
    },
    Replace {
        path: String,
        value: serde_json::Value,
    },
    /// Replace `delete` array elements starting at `start` with `insert`.
    /// A middle insertion or deletion remains one operation regardless of the
    /// array length, so a large allocator free list never has to shift its
    /// suffix through per-element pointer operations.
    Splice {
        path: String,
        start: u64,
        delete: u64,
        insert: Vec<serde_json::Value>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct JournalRecord {
    #[serde(default)]
    pub version: u32,
    #[serde(default, skip_serializing_if = "JournalRecordKind::is_transaction")]
    pub record_type: JournalRecordKind,
    #[serde(default)]
    pub time: f64,
    #[serde(default)]
    pub volume_uuid: String,
    #[serde(default)]
    pub txid: u64,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub details: serde_json::Value,
    #[serde(default)]
    pub previous_record_hash: String,
    #[serde(default)]
    pub previous_meta_hash: String,
    #[serde(default)]
    pub meta_hash: String,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Metadata>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_delta: Option<Vec<MetadataDeltaOp>>,
    #[serde(default)]
    pub record_hash: String,
}

pub fn initialize_volume(root: &Path, meta: &mut Metadata, created_at: f64) -> Result<()> {
    prepare_metadata_integrity(meta, String::new())?;
    let record = build_record(
        meta,
        BuildRecordOptions {
            record_type: JournalRecordKind::Checkpoint,
            action: "mkfs".to_string(),
            details: json!({"txid": meta.txid}),
            previous_record_hash: String::new(),
            time: created_at,
            metadata: Some(meta.clone()),
            metadata_delta: None,
        },
    )?;
    append_record(root, &record)?;
    write_metadata_copies(root, meta)?;
    Ok(())
}

pub fn load_or_recover(root: &Path) -> Result<RecoveredMetadata> {
    let mut candidates = read_metadata_candidates(root);
    let mut report = scan(root)?;
    report.metadata_candidates = candidates
        .iter()
        .map(|candidate| candidate.report.clone())
        .collect();
    report.double_write_mismatches = double_write_mismatches(&candidates);

    let mut best = candidates
        .iter_mut()
        .filter_map(|candidate| {
            candidate.metadata.take().map(|metadata| {
                (
                    candidate.source.clone(),
                    metadata,
                    candidate.serialized.take(),
                )
            })
        })
        .max_by(|left, right| metadata_order(&left.1).cmp(&metadata_order(&right.1)));

    if let Some(replayed) = report_latest_recoverable_metadata(root)? {
        let snapshot_order = metadata_order(&replayed);
        if best
            .as_ref()
            .map(|(_, metadata, _)| snapshot_order > metadata_order(metadata))
            .unwrap_or(true)
        {
            report.replayed = true;
            best = Some(("journal-replay".to_string(), replayed, None));
        }
    }

    let Some((source, mut metadata, serialized)) = best else {
        return Err(ArgosError::Invalid(format!(
            "no valid metadata copy or replayable journal snapshot found: {}",
            serde_json::to_string(&report.metadata_candidates)?
        )));
    };
    ensure_integrity(&mut metadata)?;
    if metadata.format != FORMAT_VERSION {
        return Err(ArgosError::Invalid(format!(
            "unsupported format {}",
            metadata.format
        )));
    }
    report.selected_metadata_source = source;

    let needs_repair =
        report.replayed || report.double_write_mismatches > 0 || missing_or_invalid_copy(&report);
    if needs_repair {
        // Reuse the *verified bytes* of a surviving copy. Re-encoding a
        // deserialized floating-point value can change its representation and
        // make every repaired copy fail hash verification on the next mount.
        let bytes = match serialized {
            Some(bytes) => bytes,
            None => {
                let bytes = serde_json::to_vec_pretty(&metadata)?;
                if canonical_metadata_hash_bytes(&bytes)? != metadata.integrity.meta_hash {
                    // A journal-only recovery is still useful. Do not replace
                    // intact copies with bytes that fail their own checksum.
                    report.errors.push(
                        "replayed metadata cannot be safely re-serialized; left copies unchanged"
                            .to_string(),
                    );
                    return Ok(RecoveredMetadata { metadata, report });
                }
                bytes
            }
        };
        write_metadata_copies_bytes(root, &bytes)?;
    }

    Ok(RecoveredMetadata { metadata, report })
}

pub fn scan(root: &Path) -> Result<TransactionReport> {
    let mut report = TransactionReport::default();
    let mut previous_hash = String::new();
    let journal = journal_path(root);
    let Ok(text) = fs::read_to_string(&journal) else {
        return Ok(report);
    };
    for (line_no, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let value = match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(value) => value,
            Err(err) => {
                report.invalid_entries += 1;
                report
                    .errors
                    .push(format!("journal line {}: invalid JSON: {err}", line_no + 1));
                continue;
            }
        };
        let record = match serde_json::from_value::<JournalRecord>(value.clone()) {
            Ok(record) => record,
            Err(err) => {
                report.invalid_entries += 1;
                report.errors.push(format!(
                    "journal line {}: invalid record: {err}",
                    line_no + 1
                ));
                continue;
            }
        };
        if record.record_hash.is_empty() {
            report.valid_entries += 1;
            report.legacy_entries += 1;
            report.last_valid_txid = report.last_valid_txid.max(record.txid);
            continue;
        }
        if !record_hash_is_valid(&value, &record)? {
            report.invalid_entries += 1;
            report.errors.push(format!(
                "journal line {}: record hash mismatch",
                line_no + 1
            ));
            continue;
        }
        if record.previous_record_hash != previous_hash {
            report.invalid_entries += 1;
            report.errors.push(format!(
                "journal line {}: previous record hash mismatch",
                line_no + 1
            ));
            continue;
        }
        if let Some(metadata_value) = value.get("metadata").filter(|value| !value.is_null()) {
            match canonical_metadata_hash_value(metadata_value) {
                Ok(hash) if hash == record.meta_hash => {
                    report.latest_snapshot_txid = record.txid;
                    report.latest_snapshot_generation = record.generation;
                }
                Ok(_) => {
                    report.invalid_entries += 1;
                    report.errors.push(format!(
                        "journal line {}: metadata snapshot hash mismatch",
                        line_no + 1
                    ));
                    continue;
                }
                Err(err) => {
                    report.invalid_entries += 1;
                    report
                        .errors
                        .push(format!("journal line {}: {err}", line_no + 1));
                    continue;
                }
            }
        }
        report.valid_entries += 1;
        report.last_valid_txid = record.txid;
        report.last_valid_generation = record.generation;
        report.last_valid_record_hash = record.record_hash.clone();
        previous_hash = record.record_hash;
    }
    Ok(report)
}

pub fn append_transaction(
    root: &Path,
    meta: &mut Metadata,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_transaction_checked(root, meta, None, action, details)
}

pub fn append_transaction_checked(
    root: &Path,
    meta: &mut Metadata,
    expected_previous_txid: Option<u64>,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    let _lock = FileLock::exclusive(&transaction_lock_path(root))?;
    let report = scan(root)?;
    let metadata_copy = latest_metadata_copy(root)?;
    let previous_metadata = match metadata_copy {
        Some(metadata) if metadata.txid >= report.last_valid_txid => metadata,
        _ => latest_recoverable_metadata(root)?,
    };
    if let Some(expected) = expected_previous_txid {
        if previous_metadata.txid > expected {
            return Err(ArgosError::Conflict(format!(
                "volume advanced from txid {expected} to {}; reopen and retry",
                previous_metadata.txid
            )));
        }
    }
    inject_crash("before-journal")?;
    let previous_meta_hash = if meta.integrity.meta_hash.is_empty() {
        canonical_metadata_hash(meta)?
    } else {
        meta.integrity.meta_hash.clone()
    };
    prepare_metadata_integrity(meta, previous_meta_hash)?;
    let previous_record_hash = report.last_valid_record_hash;
    let should_checkpoint = should_write_checkpoint(root, meta.txid)?;
    let metadata_delta = if should_checkpoint {
        None
    } else {
        Some(metadata_delta(&previous_metadata, meta)?)
    };
    let record = build_record(
        meta,
        BuildRecordOptions {
            record_type: if should_checkpoint {
                JournalRecordKind::Checkpoint
            } else {
                JournalRecordKind::Transaction
            },
            action: action.to_string(),
            details,
            previous_record_hash,
            time: now_f64(),
            metadata: should_checkpoint.then(|| meta.clone()),
            metadata_delta,
        },
    )?;
    append_record(root, &record)?;
    inject_crash("after-journal")?;
    write_metadata_copies_with_injection(root, meta)?;
    if should_checkpoint {
        // Journal compaction is maintenance work after the transaction is already
        // durable in the journal and in the metadata copies.  A compaction
        // failure must not turn a committed filesystem operation into a
        // user-visible failure; the uncompacted journal is still valid.
        let _ = compact_journal_unlocked(root);
    }
    Ok(())
}

pub fn append_event(
    root: &Path,
    meta: &Metadata,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    let _lock = FileLock::exclusive(&transaction_lock_path(root))?;
    let previous_record_hash = scan(root)?.last_valid_record_hash;
    let record = build_record(
        meta,
        BuildRecordOptions {
            record_type: JournalRecordKind::Event,
            action: action.to_string(),
            details,
            previous_record_hash,
            time: now_f64(),
            metadata: None,
            metadata_delta: None,
        },
    )?;
    append_record(root, &record)
}

pub fn compact_journal(root: &Path) -> Result<()> {
    let _lock = FileLock::exclusive(&transaction_lock_path(root))?;
    compact_journal_unlocked(root)
}

pub fn write_metadata_copies(root: &Path, meta: &Metadata) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(meta)?;
    write_metadata_copies_bytes(root, &bytes)
}

fn write_metadata_copies_bytes(root: &Path, bytes: &[u8]) -> Result<()> {
    let system = root.join(".argosfs");
    atomic_write(&system.join(PRIMARY_META), bytes)?;
    atomic_write(&system.join(SECONDARY_META), bytes)?;
    atomic_write(&system.join(COMPAT_META), bytes)?;
    Ok(())
}

/// Stream a canonical metadata image into SHA-256. Serializing into a giant
/// temporary Vec, deserializing it, and serializing it again previously cost
/// multiple whole-namespace passes and gigabytes of transient allocations.
///
/// The serde round-trip is retained for floating-point normalization, and
/// the final digest is calculated over exactly the legacy pretty JSON bytes.
pub fn canonical_metadata_hash(meta: &Metadata) -> Result<String> {
    let mut clone = meta.clone();
    clone.integrity = MetadataIntegrity::default();
    // A serde round-trip normalizes floating-point representations. This is
    // part of the historical integrity hash and cannot be omitted without
    // invalidating checkpoints written by prior ArgosFS versions.
    let compact = serde_json::to_vec(&clone)?;
    let round_trip = serde_json::from_slice::<Metadata>(&compact)?;
    drop(compact);
    let mut sink = Sha256Sink(Sha256::new());
    {
        let mut buffered = BufWriter::with_capacity(256 * 1024, &mut sink);
        serde_json::to_writer_pretty(&mut buffered, &round_trip)?;
        buffered.flush()?;
    }
    Ok(hex::encode(sink.0.finalize()))
}

struct Sha256Sink(Sha256);

impl Write for Sha256Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn prepare_metadata_integrity_for_external_store(meta: &mut Metadata) -> Result<()> {
    prepare_metadata_integrity(meta, String::new())
}

pub fn prepare_metadata_integrity_with_previous(
    meta: &mut Metadata,
    previous_meta_hash: String,
) -> Result<()> {
    prepare_metadata_integrity(meta, previous_meta_hash)
}

/// Raw journal v2 hashes authenticate the *ordered delta* and the preceding
/// verified metadata identity, rather than serializing the whole inode table.
/// The domain separator and length-prefixing rule out cross-protocol replay.
/// Integrity-field operations are excluded to avoid a circular digest; their
/// expected values are independently checked after applying the delta.
pub const DELTA_INTEGRITY_SCHEME: u32 = 2;
const DELTA_INTEGRITY_DOMAIN: &[u8] = b"ArgosFS/raw-delta-integrity/v2\0";

fn delta_integrity_hash(
    base: &Metadata,
    next: &Metadata,
    ops: &[MetadataDeltaOp],
) -> Result<String> {
    if base.integrity.meta_hash.len() != 64 {
        return Err(ArgosError::CorruptedMetadata(
            "delta integrity has no authenticated previous metadata hash".to_string(),
        ));
    }
    if next.integrity.previous_meta_hash != base.integrity.meta_hash
        || next.integrity.generation != next.txid
        || next.integrity.hash_scheme != Some(DELTA_INTEGRITY_SCHEME)
        || next.uuid != base.uuid
        || next.txid <= base.txid
    {
        return Err(ArgosError::CorruptedMetadata(
            "delta integrity transaction does not chain from its base".to_string(),
        ));
    }
    let mut digest = Sha256::new();
    digest.update(DELTA_INTEGRITY_DOMAIN);
    digest.update(hex::decode(&base.integrity.meta_hash).map_err(|_| {
        ArgosError::CorruptedMetadata("invalid previous metadata hash encoding".to_string())
    })?);
    digest.update(next.uuid.as_bytes());
    digest.update(next.txid.to_le_bytes());
    digest.update(next.integrity.generation.to_le_bytes());
    let covered = ops
        .iter()
        .filter(|op| {
            let path = delta_path(op);
            path != "/integrity" && !path.starts_with("/integrity/")
        })
        .collect::<Vec<_>>();
    // The journal parses its JSON before validating its delta. serde_json's
    // legacy float parser can drift on *multiple* successive parse/serialize
    // passes (e.g. 2.7876436713919087e-8). Normalize until the serialized
    // delta is a fixed point. A stable canonical form is independent of how
    // many times it was parsed, including the parse at journal replay. If it
    // does not stabilize, reject the transaction *before* writing anything.
    // Do not enable float_roundtrip globally: that would break the persisted
    // canonical hashes of older pools.
    let mut payload = serde_json::to_vec(&covered)?;
    const MAX_WIRE_NORMALIZATION_PASSES: usize = 64;
    let mut stable = false;
    for _ in 0..MAX_WIRE_NORMALIZATION_PASSES {
        let decoded: Vec<MetadataDeltaOp> = serde_json::from_slice(&payload)?;
        let serialized = serde_json::to_vec(&decoded)?;
        if serialized == payload {
            stable = true;
            break;
        }
        payload = serialized;
    }
    if !stable {
        return Err(ArgosError::IncompatibleFormat(
            "raw delta floating-point JSON could not reach a stable wire representation"
                .to_string(),
        ));
    }
    digest.update((payload.len() as u64).to_le_bytes());
    digest.update(payload);
    Ok(hex::encode(digest.finalize()))
}

/// Build a v2 transaction using only changed inode keys, disk metadata, and
/// a bounded header, leveraging the existing COW mutation tracker.
pub fn prepare_delta_integrity(meta: &mut Metadata, previous: &Metadata) -> Result<()> {
    prepare_delta_integrity_with_delta(meta, previous).map(|_| ())
}

/// Return the exact delta authenticated by this v2 transaction, including the
/// final integrity field. A group commit can reuse it when encoding its raw
/// journal record instead of traversing the changed inode set twice.
pub(crate) fn prepare_delta_integrity_with_delta(
    meta: &mut Metadata,
    previous: &Metadata,
) -> Result<Vec<MetadataDeltaOp>> {
    meta.integrity.generation = meta.txid;
    meta.integrity.previous_meta_hash = previous.integrity.meta_hash.clone();
    meta.integrity.hash_scheme = Some(DELTA_INTEGRITY_SCHEME);
    meta.integrity.meta_hash.clear();
    let mut delta = metadata_delta(previous, meta)?;
    meta.integrity.meta_hash = delta_integrity_hash(previous, meta, &delta)?;
    // Only the integrity hash changes after constructing the authenticated
    // delta. Patch that one JSON operation rather than rebuilding the entire
    // delta. The previous v2 tip necessarily has a nonempty 64-byte hash.
    let mut found = false;
    for op in &mut delta {
        match op {
            MetadataDeltaOp::Add { path, value } | MetadataDeltaOp::Replace { path, value }
                if path == "/integrity/meta_hash" =>
            {
                *value = serde_json::Value::String(meta.integrity.meta_hash.clone());
                found = true;
                break;
            }
            _ => {}
        }
    }
    if !found {
        return Err(ArgosError::CorruptedMetadata(
            "v2 integrity delta omitted the metadata hash update".to_string(),
        ));
    }
    Ok(delta)
}

pub(crate) fn crash_point_requested(point: &str) -> bool {
    let thread_spec = THREAD_CRASH_POINT.with(|value| value.borrow().clone());
    thread_spec
        .or_else(|| std::env::var("ARGOSFS_CRASH_POINT").ok())
        .is_some_and(|spec| {
            spec.split(',')
                .map(str::trim)
                .any(|wanted| wanted == point || wanted == "all")
        })
}

pub fn inject_crash(point: &str) -> Result<()> {
    if crash_point_requested(point) {
        if std::env::var("ARGOSFS_CRASH_ABORT").ok().as_deref() == Some("1") {
            std::process::abort();
        }
        return Err(ArgosError::InjectedCrash(point.to_string()));
    }
    Ok(())
}

pub fn set_thread_crash_point(point: Option<&str>) {
    THREAD_CRASH_POINT.with(|value| {
        *value.borrow_mut() = point.map(ToString::to_string);
    });
}

pub fn thread_crash_point(point: &str) -> ThreadCrashPointGuard {
    let previous = THREAD_CRASH_POINT.with(|value| value.replace(Some(point.to_string())));
    ThreadCrashPointGuard { previous }
}

/// Multi-member data I/O must preserve the caller-thread fault injection
/// semantics, including process abort at an exact partial-write boundary.
pub(crate) fn crash_injection_configured() -> bool {
    THREAD_CRASH_POINT.with(|value| value.borrow().is_some())
        || std::env::var_os("ARGOSFS_CRASH_POINT").is_some()
}

pub fn thread_checkpoint_interval(interval: u64) -> ThreadCheckpointIntervalGuard {
    let previous = THREAD_CHECKPOINT_INTERVAL.with(|value| value.replace(Some(interval.max(1))));
    ThreadCheckpointIntervalGuard { previous }
}

pub fn thread_journal_compaction_disabled(disabled: bool) -> ThreadCompactionGuard {
    let previous = THREAD_COMPACTION_DISABLED.with(|value| value.replace(Some(disabled)));
    ThreadCompactionGuard { previous }
}

fn write_metadata_copies_with_injection(root: &Path, meta: &Metadata) -> Result<()> {
    let system = root.join(".argosfs");
    let bytes = serde_json::to_vec_pretty(meta)?;
    atomic_write(&system.join(PRIMARY_META), &bytes)?;
    inject_crash("after-primary-metadata")?;
    atomic_write(&system.join(SECONDARY_META), &bytes)?;
    inject_crash("after-secondary-metadata")?;
    atomic_write(&system.join(COMPAT_META), &bytes)?;
    inject_crash("after-compatible-metadata")?;
    Ok(())
}

fn prepare_metadata_integrity(meta: &mut Metadata, previous_meta_hash: String) -> Result<()> {
    meta.integrity.generation = meta.txid;
    meta.integrity.previous_meta_hash = previous_meta_hash;
    meta.integrity.hash_scheme = None;
    meta.integrity.meta_hash = canonical_metadata_hash(meta)?;
    Ok(())
}

fn ensure_integrity(meta: &mut Metadata) -> Result<()> {
    if meta.integrity.meta_hash.is_empty() {
        meta.integrity.generation = meta.txid;
        meta.integrity.previous_meta_hash = String::new();
        meta.integrity.meta_hash = canonical_metadata_hash(meta)?;
    }
    Ok(())
}

fn build_record(meta: &Metadata, options: BuildRecordOptions) -> Result<JournalRecord> {
    let mut record = JournalRecord {
        version: JOURNAL_VERSION,
        record_type: options.record_type,
        time: options.time,
        volume_uuid: meta.uuid.clone(),
        txid: meta.txid,
        generation: meta.integrity.generation,
        action: options.action,
        details: options.details,
        previous_record_hash: options.previous_record_hash,
        previous_meta_hash: meta.integrity.previous_meta_hash.clone(),
        meta_hash: canonical_metadata_hash(meta)?,
        metadata: options.metadata,
        metadata_delta: options.metadata_delta,
        record_hash: String::new(),
    };
    record.record_hash = record_hash(&record)?;
    Ok(record)
}

fn record_hash(record: &JournalRecord) -> Result<String> {
    let mut clone = record.clone();
    clone.record_hash.clear();
    let persisted = serde_json::to_vec_pretty(&clone)?;
    let mut round_trip = serde_json::from_slice::<JournalRecord>(&persisted)?;
    round_trip.record_hash.clear();
    Ok(sha256_hex(&serde_json::to_vec_pretty(&round_trip)?))
}

fn record_hash_decoded(record: &JournalRecord) -> Result<String> {
    let mut clone = record.clone();
    clone.record_hash.clear();
    Ok(sha256_hex(&serde_json::to_vec_pretty(&clone)?))
}

fn record_hash_value(value: &serde_json::Value) -> Result<String> {
    let mut record = serde_json::from_value::<JournalRecord>(value.clone())?;
    record.record_hash.clear();
    record_hash(&record)
}

fn record_hash_is_valid(value: &serde_json::Value, record: &JournalRecord) -> Result<bool> {
    Ok(record_hash_decoded(record)? == record.record_hash
        || record_hash_value(value)? == record.record_hash
        || legacy_record_hash_value(value).is_ok_and(|hash| hash == record.record_hash))
}

fn legacy_record_hash_value(value: &serde_json::Value) -> Result<String> {
    #[derive(Clone, Default, Deserialize, Serialize)]
    struct LegacyJournalRecord {
        #[serde(default)]
        version: u32,
        #[serde(default)]
        time: f64,
        #[serde(default)]
        volume_uuid: String,
        #[serde(default)]
        txid: u64,
        #[serde(default)]
        generation: u64,
        #[serde(default)]
        action: String,
        #[serde(default)]
        details: serde_json::Value,
        #[serde(default)]
        previous_record_hash: String,
        #[serde(default)]
        previous_meta_hash: String,
        #[serde(default)]
        meta_hash: String,
        #[serde(default)]
        metadata: Option<Metadata>,
        #[serde(default)]
        record_hash: String,
    }

    let mut record = serde_json::from_value::<LegacyJournalRecord>(value.clone())?;
    record.record_hash.clear();
    Ok(sha256_hex(&serde_json::to_vec_pretty(&record)?))
}

fn canonical_metadata_hash_decoded(meta: &Metadata) -> Result<String> {
    let mut clone = meta.clone();
    clone.integrity = MetadataIntegrity::default();
    Ok(sha256_hex(&serde_json::to_vec_pretty(&clone)?))
}

fn canonical_metadata_hash_bytes(bytes: &[u8]) -> Result<String> {
    let metadata = serde_json::from_slice::<Metadata>(bytes)?;
    canonical_metadata_hash_decoded(&metadata)
}

fn canonical_metadata_hash_value(value: &serde_json::Value) -> Result<String> {
    let metadata = serde_json::from_value::<Metadata>(value.clone())?;
    canonical_metadata_hash_decoded(&metadata)
}

fn append_record(root: &Path, record: &JournalRecord) -> Result<()> {
    append_json_line(&journal_path(root), record)
}

fn compact_journal_unlocked(root: &Path) -> Result<()> {
    if journal_compaction_disabled() {
        return Ok(());
    }
    let journal = journal_path(root);
    let Ok(text) = fs::read_to_string(&journal) else {
        return Ok(());
    };
    let records = valid_journal_chain_records(&text)?;
    let Some(checkpoint_index) = records
        .iter()
        .rposition(|record| record.record_type.is_checkpoint() && record.metadata.is_some())
    else {
        return Ok(());
    };
    let mut compacted = records[checkpoint_index..].to_vec();
    let before = report_latest_recoverable_metadata_from_text(&text)?;
    for record in &compacted {
        if let Some(metadata) = &record.metadata {
            // Rewriting a decoded float must not invalidate the stored snapshot hash.
            if canonical_metadata_hash(metadata)? != record.meta_hash {
                return Ok(());
            }
        }
    }
    let mut previous_hash = String::new();
    for record in &mut compacted {
        record.previous_record_hash = previous_hash;
        record.record_hash.clear();
        record.record_hash = record_hash(record)?;
        previous_hash = record.record_hash.clone();
    }

    let mut bytes = Vec::new();
    for record in &compacted {
        serde_json::to_writer(&mut bytes, record)?;
        bytes.push(b'\n');
    }
    // Compaction rewrites the journal hash chain. Verify the exact bytes we
    // plan to persist can replay to the *same* authenticated state. This also
    // protects deltas containing non-idempotent JSON floats: a decoded delta
    // that drifts on re-encoding must never cause a durable transaction to
    // disappear silently during recovery.
    let candidate = std::str::from_utf8(&bytes)
        .map_err(|err| ArgosError::Invalid(format!("compacted journal is not UTF-8: {err}")))?;
    let after = report_latest_recoverable_metadata_from_text(candidate)?;
    let same_state = match (before, after) {
        (Some(before), Some(after)) => {
            before.txid == after.txid
                && before.integrity.meta_hash == after.integrity.meta_hash
                && serde_json::to_vec(&before)? == serde_json::to_vec(&after)?
        }
        (None, None) => true,
        _ => false,
    };
    if !same_state || valid_journal_chain_records(candidate)?.len() != compacted.len() {
        return Ok(());
    }
    atomic_write(&journal, &bytes)
}

fn valid_journal_chain_records(text: &str) -> Result<Vec<JournalRecord>> {
    let mut records = Vec::new();
    let mut previous_hash = String::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let Ok(record) = serde_json::from_value::<JournalRecord>(value.clone()) else {
            continue;
        };
        if record.record_hash.is_empty() {
            continue;
        }
        let valid_hash = match record_hash_is_valid(&value, &record) {
            Ok(valid) => valid,
            Err(_) => continue,
        };
        if !valid_hash || record.previous_record_hash != previous_hash {
            continue;
        }
        previous_hash = record.record_hash.clone();
        records.push(record);
    }
    Ok(records)
}

fn journal_compaction_disabled() -> bool {
    THREAD_COMPACTION_DISABLED
        .with(|value| value.get())
        .unwrap_or_else(|| {
            std::env::var(JOURNAL_COMPACTION_ENV)
                .ok()
                .is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
        })
}

fn read_metadata_candidates(root: &Path) -> Vec<MetadataCandidate> {
    metadata_paths(root)
        .into_iter()
        .map(|(source, path)| read_candidate(source, path))
        .collect()
}

fn read_candidate(source: String, path: PathBuf) -> MetadataCandidate {
    let mut report = MetadataCandidateReport {
        path: path.display().to_string(),
        ..MetadataCandidateReport::default()
    };
    if !path.exists() {
        return MetadataCandidate {
            source,
            metadata: None,
            serialized: None,
            report,
        };
    }
    report.present = true;
    let mut serialized = None;
    let metadata = match read_to_vec(&path).and_then(|bytes| {
        let hash = canonical_metadata_hash_bytes(&bytes)?;
        let mut metadata = serde_json::from_slice::<Metadata>(&bytes).map_err(ArgosError::Json)?;
        validate_metadata(&mut metadata, &hash)?;
        serialized = Some(bytes);
        Ok(metadata)
    }) {
        Ok(metadata) => {
            report.valid = true;
            report.txid = metadata.txid;
            report.generation = metadata.integrity.generation;
            report.meta_hash = metadata.integrity.meta_hash.clone();
            Some(metadata)
        }
        Err(err) => {
            report.error = Some(err.to_string());
            None
        }
    };
    MetadataCandidate {
        source,
        metadata,
        serialized,
        report,
    }
}

fn validate_metadata(meta: &mut Metadata, computed: &str) -> Result<()> {
    if meta.format != FORMAT_VERSION {
        return Err(ArgosError::Invalid(format!(
            "unsupported format {}",
            meta.format
        )));
    }
    if meta.integrity.meta_hash.is_empty() {
        meta.integrity.generation = meta.txid;
        meta.integrity.meta_hash = computed.to_string();
        return Ok(());
    }
    if meta.integrity.meta_hash != computed {
        return Err(ArgosError::Invalid(format!(
            "metadata hash mismatch: stored={} computed={computed}",
            meta.integrity.meta_hash
        )));
    }
    Ok(())
}

fn report_latest_recoverable_metadata(root: &Path) -> Result<Option<Metadata>> {
    let journal = journal_path(root);
    let Ok(text) = fs::read_to_string(&journal) else {
        return Ok(None);
    };
    report_latest_recoverable_metadata_from_text(&text)
}

fn report_latest_recoverable_metadata_from_text(text: &str) -> Result<Option<Metadata>> {
    let mut previous_hash = String::new();
    let mut latest: Option<Metadata> = None;
    let mut replay_failed = false;
    for raw in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let Ok(record) = serde_json::from_value::<JournalRecord>(value.clone()) else {
            continue;
        };
        if record.record_hash.is_empty() {
            continue;
        }
        let Ok(valid_hash) = record_hash_is_valid(&value, &record) else {
            continue;
        };
        if !valid_hash {
            continue;
        }
        if record.previous_record_hash != previous_hash {
            continue;
        }
        previous_hash = record.record_hash.clone();
        let Some(metadata_value) = value.get("metadata").filter(|value| !value.is_null()) else {
            if replay_failed {
                continue;
            }
            if let (Some(base), Some(delta)) = (latest.as_ref(), record.metadata_delta.as_ref()) {
                if record.previous_meta_hash != base.integrity.meta_hash {
                    replay_failed = true;
                    continue;
                }
                let Ok(mut metadata) = apply_metadata_delta(base, delta) else {
                    replay_failed = true;
                    continue;
                };
                let Ok(meta_hash) = canonical_metadata_hash(&metadata) else {
                    replay_failed = true;
                    continue;
                };
                if meta_hash != record.meta_hash
                    && canonical_metadata_hash_decoded(&metadata)? != record.meta_hash
                {
                    replay_failed = true;
                    continue;
                }
                if metadata.txid != record.txid
                    || metadata.integrity.generation != record.generation
                {
                    replay_failed = true;
                    continue;
                }
                if ensure_integrity(&mut metadata).is_err() {
                    replay_failed = true;
                    continue;
                }
                latest = Some(metadata);
            }
            continue;
        };
        let Ok(meta_hash) = canonical_metadata_hash_value(metadata_value) else {
            continue;
        };
        if meta_hash != record.meta_hash {
            continue;
        }
        let Some(mut metadata) = record.metadata else {
            continue;
        };
        ensure_integrity(&mut metadata)?;
        latest = Some(metadata);
        replay_failed = false;
    }
    Ok(latest)
}

fn latest_recoverable_metadata(root: &Path) -> Result<Metadata> {
    let mut best = latest_metadata_copy(root)?;
    if let Some(replayed) = report_latest_recoverable_metadata(root)? {
        if best
            .as_ref()
            .map(|metadata| metadata_order(&replayed) > metadata_order(metadata))
            .unwrap_or(true)
        {
            best = Some(replayed);
        }
    }
    best.ok_or_else(|| ArgosError::Invalid("no recoverable metadata base found".to_string()))
}

fn latest_metadata_copy(root: &Path) -> Result<Option<Metadata>> {
    Ok(read_metadata_candidates(root)
        .into_iter()
        .filter_map(|candidate| candidate.metadata)
        .max_by(|left, right| metadata_order(left).cmp(&metadata_order(right))))
}

fn should_write_checkpoint(root: &Path, txid: u64) -> Result<bool> {
    Ok(!has_valid_checkpoint(root)? || txid.is_multiple_of(checkpoint_interval_txids()))
}

pub fn checkpoint_interval_txids() -> u64 {
    THREAD_CHECKPOINT_INTERVAL
        .with(|value| value.get())
        .unwrap_or_else(|| {
            std::env::var(CHECKPOINT_INTERVAL_ENV)
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_CHECKPOINT_INTERVAL_TXIDS)
        })
}

fn has_valid_checkpoint(root: &Path) -> Result<bool> {
    let journal = journal_path(root);
    let Ok(text) = fs::read_to_string(&journal) else {
        return Ok(false);
    };
    let mut previous_hash = String::new();
    for raw in text.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        let Ok(record) = serde_json::from_value::<JournalRecord>(value.clone()) else {
            continue;
        };
        if record.record_hash.is_empty() {
            continue;
        }
        let Ok(valid_hash) = record_hash_is_valid(&value, &record) else {
            continue;
        };
        if !valid_hash || record.previous_record_hash != previous_hash {
            continue;
        }
        previous_hash = record.record_hash.clone();
        if record.record_type.is_checkpoint() && record.metadata.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[path = "journal_allocator.rs"]
mod allocator_delta;

fn metadata_paths(root: &Path) -> Vec<(String, PathBuf)> {
    let system = root.join(".argosfs");
    vec![
        ("compat".to_string(), system.join(COMPAT_META)),
        ("primary".to_string(), system.join(PRIMARY_META)),
        ("secondary".to_string(), system.join(SECONDARY_META)),
    ]
}

fn journal_path(root: &Path) -> PathBuf {
    root.join(".argosfs/journal.jsonl")
}

fn transaction_lock_path(root: &Path) -> PathBuf {
    root.join(".argosfs/tx.lock")
}

fn metadata_order(meta: &Metadata) -> (u64, u64) {
    (meta.txid, meta.integrity.generation)
}

fn double_write_mismatches(candidates: &[MetadataCandidate]) -> u64 {
    let mut hashes = BTreeSet::new();
    let mut present = 0u64;
    let mut invalid = 0u64;
    for candidate in candidates {
        if candidate.report.present {
            present += 1;
            if candidate.report.valid {
                hashes.insert(candidate.report.meta_hash.clone());
            } else {
                invalid += 1;
            }
        }
    }
    let divergent = if hashes.len() > 1 {
        hashes.len() as u64 - 1
    } else {
        0
    };
    invalid + divergent + u64::from(present > 0 && hashes.is_empty())
}

fn missing_or_invalid_copy(report: &TransactionReport) -> bool {
    report
        .metadata_candidates
        .iter()
        .any(|candidate| !candidate.present || !candidate.valid)
}

#[cfg(test)]
#[path = "journal_array_delta_tests.rs"]
mod array_delta_tests;
#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
