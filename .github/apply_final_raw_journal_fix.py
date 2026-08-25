from pathlib import Path


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
    """struct DeferredCommitState {\n    durable_metadata: Option<Metadata>,\n    raw_uncommitted_metadata_dirty: bool,\n    dirty_transactions: u64,\n""",
    """struct DeferredCommitState {\n    durable_metadata: Option<Metadata>,\n    durable_metadata_verified: bool,\n    raw_uncommitted_metadata_dirty: bool,\n    dirty_transactions: u64,\n""",
    "durable verified field",
)
text = replace_once(
    text,
    """            durable_metadata: (meta.backend != BackendKind::Host).then(|| meta.clone()),\n            raw_uncommitted_metadata_dirty: false,\n            dirty_transactions: 0,\n""",
    """            durable_metadata: (meta.backend != BackendKind::Host).then(|| meta.clone()),\n            // A recovered raw snapshot is not trusted as a delta base until this\n            // mount has established one uniform durable checkpoint. This prevents\n            // stale declared integrity hashes from hiding cross-member divergence.\n            durable_metadata_verified: false,\n            raw_uncommitted_metadata_dirty: false,\n            dirty_transactions: 0,\n""",
    "durable verified init",
)

text = replace_once(
    text,
    """            let raw_uncommitted_metadata_dirty =\n                self.deferred_commit.lock().raw_uncommitted_metadata_dirty;\n            if raw_uncommitted_metadata_dirty {\n                let previous_meta_hash = meta.integrity.meta_hash.clone();\n                journal::prepare_metadata_integrity_with_previous(\n                    &mut meta,\n                    previous_meta_hash,\n                )?;\n            }\n""",
    """            let (raw_uncommitted_metadata_dirty, durable_metadata_verified) = {\n                let state = self.deferred_commit.lock();\n                (\n                    state.raw_uncommitted_metadata_dirty,\n                    state.durable_metadata_verified,\n                )\n            };\n            if raw_uncommitted_metadata_dirty || !durable_metadata_verified {\n                let previous_meta_hash = meta.integrity.meta_hash.clone();\n                journal::prepare_metadata_integrity_with_previous(\n                    &mut meta,\n                    previous_meta_hash,\n                )?;\n            }\n""",
    "sync reseal gate",
)
text = replace_once(
    text,
    """            let mut state = self.deferred_commit.lock();\n            state.durable_metadata = Some(meta.clone());\n            state.raw_uncommitted_metadata_dirty = false;\n            return Ok(());\n""",
    """            let mut state = self.deferred_commit.lock();\n            state.durable_metadata = Some(meta.clone());\n            state.durable_metadata_verified = true;\n            state.raw_uncommitted_metadata_dirty = false;\n            return Ok(());\n""",
    "sync durable update",
)

text = replace_once(
    text,
    """        let before_commit = meta.clone();\n        let dirty_transactions = state.dirty_transactions;\n        let previous_meta_hash = if previous.integrity.meta_hash.is_empty() {\n""",
    """        let before_commit = meta.clone();\n        let dirty_transactions = state.dirty_transactions;\n        let trusted_delta_base = !checkpoint && state.durable_metadata_verified;\n        let previous_meta_hash = if previous.integrity.meta_hash.is_empty() {\n""",
    "deferred trusted gate",
)
text = replace_once(
    text,
    """                if checkpoint {\n                    raw_store::write_metadata_copies(backend, &superblocks, meta)?;\n                } else {\n                    raw_store::append_transaction_with_trusted_previous(\n""",
    """                if !trusted_delta_base {\n                    raw_store::write_metadata_copies(backend, &superblocks, meta)?;\n                } else {\n                    raw_store::append_transaction_with_trusted_previous(\n""",
    "deferred first checkpoint",
)
text = replace_once(
    text,
    """            Ok(()) => {\n                state.durable_metadata = Some(meta.clone());\n                state.raw_uncommitted_metadata_dirty = false;\n""",
    """            Ok(()) => {\n                state.durable_metadata = Some(meta.clone());\n                state.durable_metadata_verified = true;\n                state.raw_uncommitted_metadata_dirty = false;\n""",
    "deferred success verified",
)
text = replace_once(
    text,
    """            {\n                state.durable_metadata = Some(meta.clone());\n                state.raw_uncommitted_metadata_dirty = false;\n                state.dirty_transactions = 0;\n""",
    """            {\n                state.durable_metadata = Some(meta.clone());\n                state.durable_metadata_verified = true;\n                state.raw_uncommitted_metadata_dirty = false;\n                state.dirty_transactions = 0;\n""",
    "deferred committed failure verified",
)

old_previous = """        let raw_uncommitted_metadata_dirty = meta.backend != BackendKind::Host\n            && self.deferred_commit.lock().raw_uncommitted_metadata_dirty;\n        let previous_meta_hash = if raw_uncommitted_metadata_dirty {\n            // Read-side telemetry can change the live metadata without a transaction.\n            // The stored integrity hash still identifies the last journal-durable state.\n            meta.integrity.meta_hash.clone()\n        } else if meta.integrity.meta_hash.is_empty() {\n            journal::canonical_metadata_hash(meta)?\n        } else {\n            meta.integrity.meta_hash.clone()\n        };\n"""
new_previous = """        let (durable_previous, durable_metadata_verified) =\n            if meta.backend != BackendKind::Host && previous_metadata.is_some() {\n                let state = self.deferred_commit.lock();\n                (state.durable_metadata.clone(), state.durable_metadata_verified)\n            } else {\n                (None, false)\n            };\n        let previous_meta_hash = match durable_previous\n            .as_ref()\n            .filter(|_| durable_metadata_verified)\n        {\n            Some(previous) if !previous.integrity.meta_hash.is_empty() => {\n                previous.integrity.meta_hash.clone()\n            }\n            _ if meta.integrity.meta_hash.is_empty() => journal::canonical_metadata_hash(meta)?,\n            _ => meta.integrity.meta_hash.clone(),\n        };\n"""
text = replace_once(text, old_previous, new_previous, "non-deferred durable base")

text = replace_once(
    text,
    """            let replay_previous = if raw_uncommitted_metadata_dirty {\n                None\n            } else {\n                previous_metadata\n                    .filter(|previous| previous.integrity.meta_hash == previous_meta_hash)\n            };\n""",
    """            let replay_previous = durable_previous\n                .as_ref()\n                .filter(|_| durable_metadata_verified)\n                .filter(|previous| previous.integrity.meta_hash == previous_meta_hash);\n""",
    "non-deferred replay base",
)
text = replace_once(
    text,
    """            if let Err(commit_err) = result {\n                if Self::transaction_error_is_committed(&commit_err) {\n                    self.deferred_commit.lock().raw_uncommitted_metadata_dirty = false;\n                }\n""",
    """            if let Err(commit_err) = result {\n                if Self::transaction_error_is_committed(&commit_err) {\n                    let mut state = self.deferred_commit.lock();\n                    state.durable_metadata = Some(meta.clone());\n                    state.durable_metadata_verified = true;\n                    state.raw_uncommitted_metadata_dirty = false;\n                }\n""",
    "known committed durable update",
)
text = replace_once(
    text,
    """            self.deferred_commit.lock().raw_uncommitted_metadata_dirty = false;\n            return Ok(());\n""",
    """            let mut state = self.deferred_commit.lock();\n            state.durable_metadata = Some(meta.clone());\n            state.durable_metadata_verified = true;\n            state.raw_uncommitted_metadata_dirty = false;\n            return Ok(());\n""",
    "successful raw durable update",
)
text = replace_once(
    text,
    """        let mut state = self.deferred_commit.lock();\n        state.durable_metadata = Some(meta.clone());\n        state.raw_uncommitted_metadata_dirty = false;\n        Ok(())\n""",
    """        let mut state = self.deferred_commit.lock();\n        state.durable_metadata = Some(meta.clone());\n        // Recovery may select one of several byte-divergent member checkpoints\n        // that carried the same stale declared integrity hash. Force the next\n        // transaction to publish a full checkpoint before trusting deltas again.\n        state.durable_metadata_verified = false;\n        state.raw_uncommitted_metadata_dirty = false;\n        Ok(())\n""",
    "recovery distrusts base",
)

mod_path.write_text(text)

namespace_path = Path("src/volume/namespace.rs")
ns = namespace_path.read_text()
ns = replace_once(
    ns,
    """        } else if let Some(live) = meta.inodes.get_mut(&ino) {\n            live.access_count = live.access_count.saturating_add(1);\n            live.read_bytes = live.read_bytes.saturating_add(data.len() as u64);\n            live.last_accessed_at = now_f64();\n            live.workload_score = live.workload_score * 0.98 + 1.0;\n        }\n""",
    """        } else {\n            if meta.backend != BackendKind::Host {\n                self.deferred_commit.lock().raw_uncommitted_metadata_dirty = true;\n            }\n            if let Some(live) = meta.inodes.get_mut(&ino) {\n                live.access_count = live.access_count.saturating_add(1);\n                live.read_bytes = live.read_bytes.saturating_add(data.len() as u64);\n                live.last_accessed_at = now_f64();\n                live.workload_score = live.workload_score * 0.98 + 1.0;\n            }\n        }\n""",
    "read-side raw dirty marker",
)
namespace_path.write_text(ns)
