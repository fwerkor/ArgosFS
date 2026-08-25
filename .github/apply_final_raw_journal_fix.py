from pathlib import Path
import subprocess


def replace_once(text: str, old: str, new: str, label: str) -> str:
    if new in text:
        return text
    if old not in text:
        raise SystemExit(f"{label}: expected text not found")
    return text.replace(old, new, 1)


mod_path = Path("src/volume/mod.rs")
text = mod_path.read_text()

text = replace_once(
    text,
    """            durable_metadata: (meta.backend != BackendKind::Host).then(|| meta.clone()),\n            raw_uncommitted_metadata_dirty: false,\n            dirty_transactions: 0,\n""",
    """            durable_metadata: (meta.backend != BackendKind::Host).then(|| meta.clone()),\n            // Opening or creating a raw pool may normalize/recompute in-memory metadata.\n            // Force one checkpoint before trusting declared integrity hashes as delta bases.\n            raw_uncommitted_metadata_dirty: meta.backend != BackendKind::Host,\n            dirty_transactions: 0,\n""",
    "initial raw dirty state",
)

text = replace_once(
    text,
    """    pub fn sync_deferred_if_dirty(&self) -> Result<bool> {\n        if !self.backend_writable || self.deferred_commit.lock().dirty_transactions == 0 {\n            return Ok(false);\n        }\n        self.sync()?;\n        Ok(true)\n    }\n""",
    """    pub fn sync_deferred_if_dirty(&self) -> Result<bool> {\n        if !self.backend_writable {\n            return Ok(false);\n        }\n        let state = self.deferred_commit.lock();\n        if state.dirty_transactions == 0 && !state.raw_uncommitted_metadata_dirty {\n            return Ok(false);\n        }\n        drop(state);\n        self.sync()?;\n        Ok(true)\n    }\n""",
    "deferred dirty sync gate",
)

text = replace_once(
    text,
    """        let mut state = self.deferred_commit.lock();\n        if state.dirty_transactions == 0 && state.pending_reclaims.is_empty() {\n            return Ok(false);\n        }\n\n        let previous = state\n""",
    """        let mut state = self.deferred_commit.lock();\n        if state.dirty_transactions == 0\n            && state.pending_reclaims.is_empty()\n            && !state.raw_uncommitted_metadata_dirty\n        {\n            return Ok(false);\n        }\n        let checkpoint = checkpoint || state.raw_uncommitted_metadata_dirty;\n\n        let previous = state\n""",
    "deferred checkpoint gate",
)

text = replace_once(
    text,
    """        *meta = recovered;\n        recompute_disk_usage_from_metadata(meta);\n        let mut state = self.deferred_commit.lock();\n        state.durable_metadata = Some(meta.clone());\n        state.raw_uncommitted_metadata_dirty = false;\n        Ok(())\n""",
    """        *meta = recovered;\n        recompute_disk_usage_from_metadata(meta);\n        let mut state = self.deferred_commit.lock();\n        state.durable_metadata = Some(meta.clone());\n        // Recovery may normalize/recompute fields after validating the persisted hash.\n        // Re-establish a full durable checkpoint before trusting a delta base again.\n        state.raw_uncommitted_metadata_dirty = true;\n        Ok(())\n""",
    "recovery dirty state",
)

mod_path.write_text(text)

namespace_path = Path("src/volume/namespace.rs")
ns = namespace_path.read_text()
ns = replace_once(
    ns,
    """        } else if let Some(live) = meta.inodes.get_mut(&ino) {\n            live.access_count = live.access_count.saturating_add(1);\n            live.read_bytes = live.read_bytes.saturating_add(data.len() as u64);\n            live.last_accessed_at = now_f64();\n            live.workload_score = live.workload_score * 0.98 + 1.0;\n        }\n""",
    """        } else {\n            if meta.backend != BackendKind::Host {\n                self.deferred_commit.lock().raw_uncommitted_metadata_dirty = true;\n            }\n            if let Some(live) = meta.inodes.get_mut(&ino) {\n                live.access_count = live.access_count.saturating_add(1);\n                live.read_bytes = live.read_bytes.saturating_add(data.len() as u64);\n                live.last_accessed_at = now_f64();\n                live.workload_score = live.workload_score * 0.98 + 1.0;\n            }\n        }\n""",
    "inode read telemetry dirty state",
)
namespace_path.write_text(ns)

data_plane_path = Path("src/volume/data_plane.rs")
data_plane = data_plane_path.read_text()
data_plane = replace_once(
    data_plane,
    """    pub(super) fn update_read_latency_locked(\n        &self,\n        meta: &mut Metadata,\n        disk_id: &str,\n        bytes: u64,\n        seconds: f64,\n    ) {\n        if let Some(disk) = meta.disks.get_mut(disk_id) {\n""",
    """    pub(super) fn update_read_latency_locked(\n        &self,\n        meta: &mut Metadata,\n        disk_id: &str,\n        bytes: u64,\n        seconds: f64,\n    ) {\n        if meta.backend != BackendKind::Host {\n            self.deferred_commit.lock().raw_uncommitted_metadata_dirty = true;\n        }\n        if let Some(disk) = meta.disks.get_mut(disk_id) {\n""",
    "raw read latency dirty state",
)
data_plane_path.write_text(data_plane)

test_path = Path("tests/raw_journal_durable_base_regression.rs")
t = test_path.read_text()

# Make both read-side regression cases exceed INLINE_DATA_MAX so they exercise
# actual raw shard reads and disk telemetry updates, not only inode access stats.
t = t.replace(
    """    fs.write_file(\"/seed\", b\"seed payload\", 0o644).unwrap();\n    let seed = fs.resolve_path(\"/seed\", true).unwrap();\n    let before = fs.metadata_snapshot().inodes[&seed].access_count;\n    assert_eq!(fs.read_file(\"/seed\", false).unwrap(), b\"seed payload\");\n""",
    """    let seed_payload = vec![0x5a; 2048];\n    fs.write_file(\"/seed\", &seed_payload, 0o644).unwrap();\n    let seed = fs.resolve_path(\"/seed\", true).unwrap();\n    let before_snapshot = fs.metadata_snapshot();\n    assert!(!before_snapshot.inodes[&seed].blocks.is_empty());\n    let before = before_snapshot.inodes[&seed].access_count;\n    assert_eq!(fs.read_file(\"/seed\", false).unwrap(), seed_payload);\n""",
    1,
)
t = t.replace(
    """    fs.write_file(\"/seed\", b\"seed payload\", 0o644).unwrap();\n    assert_eq!(fs.read_file(\"/seed\", false).unwrap(), b\"seed payload\");\n    fs.sync().unwrap();\n""",
    """    let seed_payload = vec![0x5a; 2048];\n    fs.write_file(\"/seed\", &seed_payload, 0o644).unwrap();\n    let seed = fs.resolve_path(\"/seed\", true).unwrap();\n    assert!(!fs.metadata_snapshot().inodes[&seed].blocks.is_empty());\n    assert_eq!(fs.read_file(\"/seed\", false).unwrap(), seed_payload);\n    fs.sync().unwrap();\n""",
    1,
)
if t.count("seed payload") != 0:
    raise SystemExit("seed payload replacement incomplete")
test_path.write_text(t)

# Existing queued materializer runs may have an older workflow-level git-add list.
# Stage every intended source/test file here so any such run produces the same patch.
subprocess.run(
    [
        "git",
        "add",
        "src/volume/mod.rs",
        "src/volume/namespace.rs",
        "src/volume/data_plane.rs",
        "tests/raw_journal_durable_base_regression.rs",
    ],
    check=True,
)
