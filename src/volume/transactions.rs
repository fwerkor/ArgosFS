use super::*;

impl ArgosFs {
    /// Check that this mount can still persist writes. Used by frontend write
    /// coalescing before acknowledging a buffered write.
    pub(crate) fn ensure_backend_writable(&self) -> Result<()> {
        let meta = self.meta.read();
        self.ensure_block_backend_writable_locked(&meta)
    }

    pub(super) fn ensure_block_backend_writable_locked(&self, meta: &Metadata) -> Result<()> {
        if meta.backend != BackendKind::Host && !self.backend_writable {
            return Err(ArgosError::ReadonlyRequired(
                "block pool was opened read-only".to_string(),
            ));
        }
        if meta.backend != BackendKind::Host {
            if let Some(reason) = self.write_fence.lock().clone() {
                return Err(ArgosError::ReadonlyRequired(format!(
                    "block pool is write-fenced after durability quorum loss: {reason}"
                )));
            }
        }
        Ok(())
    }

    pub(super) fn transaction_error_is_committed(err: &ArgosError) -> bool {
        // Indeterminate means the new state may already be recoverable after a
        // reboot. Treat it as committed for rollback/block-reclaim decisions and
        // fence the mount until recovery resolves the durable outcome.
        matches!(
            err,
            ArgosError::CommittedDurabilityLoss(_) | ArgosError::IndeterminateCommit(_)
        ) || matches!(
            err,
            ArgosError::InjectedCrash(point)
                if matches!(
                    point.as_str(),
                    "after-journal"
                        | "after-primary-metadata"
                        | "after-secondary-metadata"
                        | "after-compatible-metadata"
                        | "after-journal-commit-before-metadata-commit"
                        | "after-metadata-commit-before-superblock-update"
                )
        )
    }

    pub(super) fn flush_deferred_data_locked(
        &self,
        meta: &Metadata,
        backend: &dyn StorageBackend,
        superblocks: &[RawSuperblock],
        durability_cache: &mut Option<DurabilityValidationCache>,
    ) -> Result<()> {
        let mut unavailable = self.quarantined_devices.lock().clone();
        // Every member has an independent flush barrier. Issue them together
        // and join all workers before checking durability or committing the
        // metadata journal. The quorum rule and per-device ordering stay the
        // same, while a slow USB member no longer serializes every other
        // member's data flush.
        let flushes = std::thread::scope(|scope| {
            let workers = superblocks
                .iter()
                .map(|sb| {
                    scope.spawn(move || (sb.disk_id.clone(), backend.flush_device(&sb.disk_id)))
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("data flush worker panicked"))
                .collect::<Vec<_>>()
        });
        for (disk_id, result) in flushes {
            if let Err(err) = result {
                if Self::device_error_should_quarantine(&err) {
                    unavailable.insert(disk_id);
                } else {
                    return Err(err);
                }
            }
        }
        for disk_id in &unavailable {
            self.quarantine_device(disk_id);
        }

        Self::validate_deferred_data_durability(meta, &unavailable, durability_cache)
    }

    pub(super) fn validate_deferred_data_durability(
        meta: &Metadata,
        unavailable: &BTreeSet<String>,
        cache: &mut Option<DurabilityValidationCache>,
    ) -> Result<()> {
        let disk_statuses = meta
            .disks
            .iter()
            .map(|(disk_id, disk)| (disk_id.clone(), disk.status))
            .collect::<BTreeMap<_, _>>();
        let layouts = serde_json::to_vec(&meta.layouts)?;
        let can_reuse = cache.as_ref().is_some_and(|previous| {
            previous.unavailable == *unavailable
                && previous.disk_statuses == disk_statuses
                && previous.layouts == layouts
        });
        let checked = if can_reuse {
            let changed = meta
                .inodes
                .changed_candidates(&cache.as_ref().expect("validated cache exists").inodes);
            Self::validate_data_durability_blocks(
                meta,
                unavailable,
                changed
                    .iter()
                    .filter_map(|ino| meta.inodes.get(ino))
                    .flat_map(|inode| inode.blocks.iter()),
            )?
        } else {
            Self::validate_data_durability_blocks(
                meta,
                unavailable,
                meta.inodes.values().flat_map(|inode| inode.blocks.iter()),
            )?
        };
        *cache = Some(DurabilityValidationCache {
            inodes: meta.inodes.clone(),
            disk_statuses,
            layouts,
            unavailable: unavailable.clone(),
            #[cfg(test)]
            last_scanned_blocks: checked,
        });
        #[cfg(not(test))]
        let _ = checked;
        Ok(())
    }

    pub(super) fn validate_data_durability_locked(
        &self,
        meta: &Metadata,
        unavailable: &BTreeSet<String>,
    ) -> Result<()> {
        Self::validate_data_durability_snapshot(meta, unavailable)
    }

    pub(super) fn validate_data_durability_snapshot(
        meta: &Metadata,
        unavailable: &BTreeSet<String>,
    ) -> Result<()> {
        Self::validate_data_durability_blocks(
            meta,
            unavailable,
            meta.inodes.values().flat_map(|inode| inode.blocks.iter()),
        )
        .map(|_| ())
    }

    pub(super) fn validate_data_durability_blocks<'a>(
        meta: &Metadata,
        unavailable: &BTreeSet<String>,
        blocks: impl Iterator<Item = &'a FileBlock>,
    ) -> Result<usize> {
        let mut checked = 0;
        for block in blocks {
            checked += 1;
            let layout = layout_by_id(meta, block_layout_id(block))?;
            let missing = block
                .shards
                .iter()
                .filter(|shard| {
                    unavailable.contains(&shard.disk_id)
                        || meta.disks.get(&shard.disk_id).is_none_or(|disk| {
                            matches!(
                                disk.status,
                                DiskStatus::Failed | DiskStatus::Offline | DiskStatus::Removed
                            )
                        })
                })
                .count();
            if missing > layout.m {
                let have = block.shards.len().saturating_sub(missing);
                return Err(ArgosError::QuorumUnavailable {
                    operation: format!("data durability for stripe {}", block.stripe_id),
                    need: layout.k,
                    have,
                });
            }
        }
        Ok(checked)
    }

    pub(super) fn note_deferred_transaction_locked(&self, meta: &mut Metadata) -> Result<()> {
        let should_commit = {
            let mut state = self.deferred_commit.lock();
            if state.dirty_transactions == 0 {
                state.dirty_since = Some(Instant::now());
            }
            state.dirty_transactions = state.dirty_transactions.saturating_add(1);
            let age_due = state.dirty_since.is_some_and(|since| {
                since.elapsed() >= Duration::from_millis(meta.config.deferred_commit_interval_ms)
            });
            !bulk_import_enabled()
                && (state.dirty_transactions >= meta.config.deferred_commit_max_transactions
                    || age_due)
        };
        if should_commit {
            self.commit_deferred_locked(meta, false)?;
        }
        Ok(())
    }

    pub(super) fn commit_deferred_locked(
        &self,
        meta: &mut Metadata,
        checkpoint: bool,
    ) -> Result<bool> {
        self.ensure_block_backend_writable_locked(meta)?;
        self.drain_read_advisory_locked(meta);
        let mut state = self.deferred_commit.lock();
        if !state.has_pending_sync() {
            return Ok(false);
        }
        let _group_commit_timer = self.runtime.commit_metrics.timer(CommitPhase::GroupCommit);
        // A pool can grow after mount. Never decide the integrity scheme from
        // the superblocks cached at open: dynamically added voting members
        // must also carry the mandatory downgrade-prevention feature.
        let current_members = self.metadata_superblocks_locked(meta)?;
        let active_members = meta
            .disks
            .values()
            .filter(|disk| disk.status != DiskStatus::Removed)
            .count();
        let incremental_integrity_enabled = state.dirty_transactions > 0
            && current_members.len() == active_members
            && active_members > 0
            && current_members.iter().all(|sb| {
                sb.required_feature_flags & raw_format::REQUIRED_FEATURE_DELTA_INTEGRITY != 0
            });
        let checkpoint = checkpoint
            || state.raw_checkpoint_required
            || (state.raw_uncommitted_metadata_dirty && !incremental_integrity_enabled);
        let array_splice_enabled = incremental_integrity_enabled
            && current_members.iter().all(|sb| {
                sb.required_feature_flags & raw_format::REQUIRED_FEATURE_ARRAY_SPLICE != 0
            });
        // The prepared delta is built before append_transaction; keep the
        // feature decision active for both stages.
        let _splice_guard = journal::allow_array_splice(array_splice_enabled);

        // The committed image is shared rather than cloned for every group commit.
        // A full rollback image is only needed if reclaiming old extents changes
        // allocator counters during the commit attempt.
        let previous = state
            .durable_metadata
            .clone()
            .unwrap_or_else(|| Arc::new(meta.clone()));
        let before_commit = (!state.pending_reclaims.is_empty()).then(|| meta.clone());
        let before_integrity = meta.integrity.clone();
        let dirty_transactions = state.dirty_transactions;
        let previous_meta_hash = if previous.integrity.meta_hash.is_empty() {
            journal::canonical_metadata_hash(previous.as_ref())?
        } else {
            previous.integrity.meta_hash.clone()
        };

        let result = (|| -> Result<raw_store::QuorumWriteReport> {
            for block in &state.pending_reclaims {
                self.account_blocks_locked(meta, std::slice::from_ref(block), false);
                self.delete_blocks_locked(meta, std::slice::from_ref(block));
            }
            let mut prepared_delta = None;
            {
                let _integrity_timer = self.runtime.commit_metrics.timer(CommitPhase::Integrity);
                if incremental_integrity_enabled && !checkpoint {
                    prepared_delta = Some(journal::prepare_delta_integrity_with_delta(
                        meta, &previous,
                    )?);
                } else {
                    journal::prepare_metadata_integrity_with_previous(
                        meta,
                        previous_meta_hash.clone(),
                    )?;
                }
            }
            let superblocks = self.metadata_superblocks_locked(meta)?;
            let unavailable = self.quarantined_devices.lock().clone();
            {
                let _validation_timer = self
                    .runtime
                    .commit_metrics
                    .timer(CommitPhase::DurabilityValidation);
                Self::validate_deferred_data_durability(
                    meta,
                    &unavailable,
                    &mut state.durability_cache,
                )?;
            }
            let mut commit =
                |backend: &dyn StorageBackend| -> Result<raw_store::QuorumWriteReport> {
                    if dirty_transactions > 0
                        && (meta.config.defer_data_flush || bulk_import_enabled())
                    {
                        let _flush_timer =
                            self.runtime.commit_metrics.timer(CommitPhase::DataFlush);
                        self.flush_deferred_data_locked(
                            meta,
                            backend,
                            &superblocks,
                            &mut state.durability_cache,
                        )?;
                        journal::inject_crash(
                            FaultPoint::AfterDataFlushBeforeJournalCommit.as_str(),
                        )?;
                    }
                    if checkpoint {
                        let reason = if state.raw_checkpoint_required {
                            CheckpointReason::RecoveryOrFormat
                        } else if bulk_import_enabled() {
                            CheckpointReason::BulkImport
                        } else {
                            CheckpointReason::LegacyFallback
                        };
                        self.runtime.commit_metrics.record_checkpoint_reason(reason);
                        let _checkpoint_timer =
                            self.runtime.commit_metrics.timer(CommitPhase::Checkpoint);
                        raw_store::write_metadata_copies_quorum_measured(
                            backend,
                            &superblocks,
                            meta,
                            &self.runtime.commit_metrics,
                        )
                    } else {
                        let _journal_timer =
                            self.runtime.commit_metrics.timer(CommitPhase::Journal);
                        raw_store::append_transaction_with_trusted_integrity_quorum_measured(
                            backend,
                            &superblocks,
                            meta,
                            raw_store::PreparedJournalBase {
                                previous: &previous,
                                delta: prepared_delta.take(),
                            },
                            &self.runtime.commit_metrics,
                            "group-commit",
                            json!({
                                "transactions": dirty_transactions,
                                "previous_txid": previous.txid,
                                "previous_meta_hash": previous_meta_hash,
                                "txid": meta.txid,
                            }),
                        )
                    }
                };
            if self.open_backend_covers_superblocks(&superblocks) {
                commit(&*self.backend)
            } else {
                let backend = self.metadata_block_backend_locked(meta, &superblocks, true)?;
                commit(&*backend)
            }
        })();

        let durability = match result {
            Ok(report) => self.apply_quorum_write_report(meta, report),
            Err(err) if Self::transaction_error_is_committed(&err) => {
                // The commit may have exposed the new state on some members,
                // but recovery has not yet decided whether it is durable.
                // Every other commit path fences here; a deferred group commit
                // must do the same instead of continuing to acknowledge writes
                // against a state that may disappear on the next mount.
                self.fence_on_quorum_loss(&err);
                Err(err)
            }
            Err(err) => {
                if let Some(snapshot) = before_commit {
                    *meta = snapshot;
                } else {
                    // No pending reclamation touched the inode/allocator graph.
                    // Integrity is the only metadata field changed during the
                    // failed commit attempt.
                    meta.integrity = before_integrity;
                }
                self.rebuild_quota_runtime_locked(meta)?;
                // The failed transaction did not make reclaimed space durable.
                // Retain the previous published snapshot.
                self.fence_on_quorum_loss(&err);
                state.last_error = Some(err.to_string());
                return Err(err);
            }
        };

        // Both a successful quorum and an already-committed error advance the
        // durable baseline. Never roll back or reclaim their acknowledged data.
        meta.inodes.mark_committed();
        if let Some(cache) = &mut state.durability_cache {
            cache.inodes = meta.inodes.clone();
        }
        drop(before_commit);
        // Release the old baseline before retaining the new inode snapshot.
        state.durable_metadata = None;
        drop(previous);
        state.durable_metadata = Some(Arc::new(meta.clone()));
        state.raw_uncommitted_metadata_dirty = false;
        state.raw_advisory_metadata_dirty = false;
        state.raw_checkpoint_required = false;
        state.raw_mount_reanchor_pending = false;
        state.dirty_transactions = 0;
        state.dirty_since = None;
        state.pending_reclaims.clear();
        self.publish_capacity_snapshot_locked(meta);
        state.last_error = durability.as_ref().err().map(ToString::to_string);
        durability.map(|()| true)
    }

    pub(super) fn commit_locked(
        &self,
        meta: &mut Metadata,
        action: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        self.commit_locked_with_previous(meta, None, action, details)
    }

    pub(super) fn commit_locked_with_previous(
        &self,
        meta: &mut Metadata,
        previous_metadata: Option<&Metadata>,
        action: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        self.ensure_block_backend_writable_locked(meta)?;
        self.drain_read_advisory_locked(meta);
        self.deferred_commit.lock().raw_mount_reanchor_pending = false;
        if meta.backend != BackendKind::Host
            && (bulk_import_enabled() || meta.config.defer_metadata_commit)
        {
            meta.txid += 1;
            meta.updated_at = now_f64();
            self.note_deferred_transaction_locked(meta)?;
            return Ok(());
        }
        let raw_uncommitted_metadata_dirty = meta.backend != BackendKind::Host && {
            let state = self.deferred_commit.lock();
            state.raw_uncommitted_metadata_dirty || state.raw_advisory_metadata_dirty
        };
        let previous_meta_hash = if raw_uncommitted_metadata_dirty {
            // Read-side telemetry can change the live metadata without a transaction.
            // The stored integrity hash still identifies the last journal-durable state.
            meta.integrity.meta_hash.clone()
        } else if meta.integrity.meta_hash.is_empty() {
            journal::canonical_metadata_hash(meta)?
        } else {
            meta.integrity.meta_hash.clone()
        };
        let previous_txid = meta.txid;
        meta.txid += 1;
        meta.updated_at = now_f64();
        if meta.backend != BackendKind::Host {
            let superblocks = self.metadata_superblocks_locked(meta)?;
            // The committed baseline excludes advisory read counters and all
            // uncommitted namespace mutations. Use it even for callers that
            // only supplied an inode-local rollback image (or none at all).
            let active_members = meta
                .disks
                .values()
                .filter(|disk| disk.status != DiskStatus::Removed)
                .count();
            let v2_base = {
                let state = self.deferred_commit.lock();
                (!state.raw_checkpoint_required
                    && !state.raw_uncommitted_metadata_dirty
                    && active_members > 0
                    && superblocks.len() == active_members
                    && superblocks.iter().all(|sb| {
                        sb.required_feature_flags & raw_format::REQUIRED_FEATURE_DELTA_INTEGRITY
                            != 0
                    }))
                .then(|| state.durable_metadata.clone())
                .flatten()
                .filter(|base| {
                    base.txid == previous_txid
                        && base.integrity.meta_hash == previous_meta_hash
                        && previous_meta_hash.len() == 64
                })
            };
            // Immediate v2 transactions prepare the authenticated delta before
            // the raw-store append path installs its feature guard. Use the
            // same all-active-member policy here, including the disabled case.
            let _splice_guard = journal::allow_array_splice(
                v2_base.is_some() && raw_store::array_splice_feature_enabled(&superblocks),
            );
            let mut prepared_delta = {
                let _integrity_timer = self.runtime.commit_metrics.timer(CommitPhase::Integrity);
                if let Some(base) = v2_base.as_deref() {
                    Some(journal::prepare_delta_integrity_with_delta(meta, base)?)
                } else {
                    journal::prepare_metadata_integrity_with_previous(
                        meta,
                        previous_meta_hash.clone(),
                    )?;
                    None
                }
            };
            let unavailable = self.quarantined_devices.lock().clone();
            let validation = {
                let _timer = self
                    .runtime
                    .commit_metrics
                    .timer(CommitPhase::DurabilityValidation);
                let mut state = self.deferred_commit.lock();
                Self::validate_deferred_data_durability(
                    meta,
                    &unavailable,
                    &mut state.durability_cache,
                )
            };
            if let Err(precommit_err) = validation {
                self.fence_on_quorum_loss(&precommit_err);
                if let Some(previous) = previous_metadata {
                    *meta = previous.clone();
                    recompute_disk_usage_from_metadata(meta);
                } else if let Err(recovery_err) =
                    self.restore_raw_metadata_locked(meta, &superblocks)
                {
                    return Err(ArgosError::CorruptedMetadata(format!(
                        "raw transaction rejected before commit ({precommit_err}) and metadata rollback failed ({recovery_err})"
                    )));
                }
                return Err(precommit_err);
            }
            let replay_previous = if raw_uncommitted_metadata_dirty {
                None
            } else {
                previous_metadata
                    .filter(|previous| previous.integrity.meta_hash == previous_meta_hash)
            };
            let details = json!({"txid": meta.txid, "previous_meta_hash": previous_meta_hash, "details": details});
            let mut commit = |backend: &dyn StorageBackend| {
                let _timer = self.runtime.commit_metrics.timer(CommitPhase::Journal);
                if let Some(base) = v2_base.as_deref() {
                    return raw_store::append_transaction_with_trusted_integrity_quorum_measured(
                        backend,
                        &superblocks,
                        meta,
                        raw_store::PreparedJournalBase {
                            previous: base,
                            delta: prepared_delta.take(),
                        },
                        &self.runtime.commit_metrics,
                        action,
                        details.clone(),
                    );
                }
                match replay_previous {
                    Some(previous) => raw_store::append_transaction_with_trusted_integrity_quorum(
                        backend,
                        &superblocks,
                        meta,
                        Some(previous),
                        action,
                        details.clone(),
                    ),
                    None => raw_store::append_transaction_quorum(
                        backend,
                        &superblocks,
                        meta,
                        None,
                        action,
                        details.clone(),
                    ),
                }
            };
            let result = if self.open_backend_covers_superblocks(&superblocks) {
                commit(&*self.backend)
            } else {
                let backend = self.metadata_block_backend_locked(meta, &superblocks, true)?;
                commit(&*backend)
            };
            match result {
                Ok(report) => {
                    if let Err(err) = self.apply_quorum_write_report(meta, report) {
                        // The metadata quorum has already committed this transaction.
                        // Keep the live metadata at that durable state, but fail the
                        // mutation and fence further writes because referenced data is
                        // no longer reconstructable from runtime-available members.
                        meta.inodes.mark_committed();
                        let mut state = self.deferred_commit.lock();
                        state.durable_metadata = Some(Arc::new(meta.clone()));
                        state.raw_uncommitted_metadata_dirty = false;
                        state.raw_advisory_metadata_dirty = false;
                        state.last_error = Some(err.to_string());
                        drop(state);
                        self.rebuild_quota_runtime_locked(meta)?;
                        self.publish_capacity_snapshot_locked(meta);
                        return Err(err);
                    }
                }
                Err(commit_err) => {
                    self.fence_on_quorum_loss(&commit_err);
                    if Self::transaction_error_is_committed(&commit_err) {
                        meta.inodes.mark_committed();
                        let mut state = self.deferred_commit.lock();
                        state.durable_metadata = Some(Arc::new(meta.clone()));
                        state.raw_uncommitted_metadata_dirty = false;
                        state.raw_advisory_metadata_dirty = false;
                        state.durability_cache = None;
                        drop(state);
                        self.rebuild_quota_runtime_locked(meta)?;
                        self.publish_capacity_snapshot_locked(meta);
                    }
                    let should_restore = !Self::transaction_error_is_committed(&commit_err)
                        && (previous_metadata.is_none()
                            || matches!(commit_err, ArgosError::Conflict(_)));
                    if should_restore {
                        if let Err(recovery_err) =
                            self.restore_raw_metadata_locked(meta, &superblocks)
                        {
                            return Err(ArgosError::CorruptedMetadata(format!(
                                "raw transaction failed ({commit_err}) and metadata rollback failed ({recovery_err})"
                            )));
                        }
                    }
                    return Err(commit_err);
                }
            }
            meta.inodes.mark_committed();
            let mut state = self.deferred_commit.lock();
            state.durable_metadata = Some(Arc::new(meta.clone()));
            if let Some(cache) = &mut state.durability_cache {
                cache.inodes = meta.inodes.clone();
            }
            state.raw_uncommitted_metadata_dirty = false;
            state.raw_advisory_metadata_dirty = false;
            drop(state);
            self.publish_capacity_snapshot_locked(meta);
            return Ok(());
        }
        let result = journal::append_transaction_checked(
            &self.root,
            meta,
            Some(previous_txid),
            action,
            json!({"txid": meta.txid, "previous_meta_hash": previous_meta_hash, "details": details}),
        );

        if let Err(commit_err) = &result {
            if !Self::transaction_error_is_committed(commit_err) {
                let recovered = journal::load_or_recover(&self.root).map_err(|recovery_err| {
                    ArgosError::CorruptedMetadata(format!(
                        "host transaction failed ({commit_err}) and metadata rollback failed ({recovery_err})"
                    ))
                })?;
                *meta = recovered.metadata;
                recompute_disk_usage_from_metadata(meta);
                self.rebuild_quota_runtime_locked(meta)?;
            } else {
                meta.inodes.mark_committed();
                self.rebuild_quota_runtime_locked(meta)?;
            }
        }

        if result.is_ok() {
            meta.inodes.mark_committed();
        }
        result
    }

    pub(super) fn restore_raw_metadata_locked(
        &self,
        meta: &mut Metadata,
        superblocks: &[RawSuperblock],
    ) -> Result<()> {
        let recovered = if self.open_backend_covers_superblocks(superblocks) {
            raw_store::recover_metadata(&*self.backend, superblocks)?
        } else {
            let backend = self.metadata_block_backend_locked(meta, superblocks, false)?;
            raw_store::recover_metadata(&*backend, superblocks)?
        };
        *meta = recovered;
        recompute_disk_usage_from_metadata(meta);
        self.rebuild_quota_runtime_locked(meta)?;
        let mut state = self.deferred_commit.lock();
        state.durable_metadata = Some(Arc::new(meta.clone()));
        // Recovery may normalize/recompute fields after validating the persisted hash.
        // Re-establish a full durable checkpoint before trusting a delta base again.
        state.raw_uncommitted_metadata_dirty = true;
        state.raw_advisory_metadata_dirty = false;
        state.raw_checkpoint_required = true;
        state.raw_mount_reanchor_pending = false;
        drop(state);
        self.publish_capacity_snapshot_locked(meta);
        Ok(())
    }

    pub(super) fn journal_locked(
        &self,
        meta: &Metadata,
        action: &str,
        details: serde_json::Value,
    ) -> Result<()> {
        self.ensure_block_backend_writable_locked(meta)?;
        if meta.backend != BackendKind::Host {
            // Read-side telemetry is allowed to mutate live metadata without a
            // transaction, so its integrity fields can still describe the last
            // durable state. Audit/event records must never persist that stale
            // integrity tuple as an embedded recovery snapshot.
            let canonical_hash = journal::canonical_metadata_hash(meta)?;
            let journal_meta = if meta.integrity.meta_hash == canonical_hash {
                meta.clone()
            } else {
                let mut snapshot = meta.clone();
                journal::prepare_metadata_integrity_with_previous(
                    &mut snapshot,
                    meta.integrity.meta_hash.clone(),
                )?;
                snapshot
            };
            let superblocks = self.metadata_superblocks_locked(&journal_meta)?;
            let result = if self.open_backend_covers_superblocks(&superblocks) {
                raw_store::append_event_quorum(
                    &*self.backend,
                    &superblocks,
                    &journal_meta,
                    action,
                    details,
                )
            } else {
                let backend =
                    self.metadata_block_backend_locked(&journal_meta, &superblocks, true)?;
                raw_store::append_event_quorum(
                    &*backend,
                    &superblocks,
                    &journal_meta,
                    action,
                    details,
                )
            };
            return match result {
                Ok(report) => {
                    self.apply_quorum_write_report(&journal_meta, report)?;
                    // An event may roll over to a canonical checkpoint using
                    // a different digest from the cached durable baseline.
                    // This also applies to an upgraded pool still carrying
                    // the legacy scheme with post-read telemetry changes.
                    // Re-anchor before the next delta even if the event did
                    // not actually roll over: its tip is not a safe ancestor.
                    if journal_meta.integrity.meta_hash != meta.integrity.meta_hash
                        || meta.integrity.hash_scheme == Some(journal::DELTA_INTEGRITY_SCHEME)
                    {
                        let mut state = self.deferred_commit.lock();
                        state.raw_uncommitted_metadata_dirty = true;
                        state.raw_checkpoint_required = true;
                        state.raw_mount_reanchor_pending = false;
                    }
                    Ok(())
                }
                Err(err) => {
                    self.fence_on_quorum_loss(&err);
                    Err(err)
                }
            };
        }
        journal::append_event(&self.root, meta, action, details)
    }

    pub fn deferred_commit_interval(&self) -> Option<Duration> {
        let meta = self.meta.read();
        (self.backend_writable
            && meta.backend != BackendKind::Host
            && meta.config.defer_metadata_commit)
            .then(|| Duration::from_millis(meta.config.deferred_commit_interval_ms))
    }

    pub fn sync_deferred_if_dirty(&self) -> Result<bool> {
        if !self.backend_writable || self.write_fence.lock().is_some() {
            return Ok(false);
        }
        let state = self.deferred_commit.lock();
        if !state.has_pending_sync() {
            return Ok(false);
        }
        drop(state);
        self.sync()?;
        Ok(true)
    }

    pub(super) fn drain_read_advisory_locked(&self, meta: &mut Metadata) {
        if self.runtime.read_advisory.drain(meta) && meta.backend != BackendKind::Host {
            self.deferred_commit.lock().raw_advisory_metadata_dirty = true;
        }
    }

    pub(super) fn commit_previous_with_advisory_locked(
        &self,
        meta: &mut Metadata,
    ) -> Option<Metadata> {
        // Immediate commits restore this snapshot on failure. Merge all
        // successful *preceding* reads before taking it so a rollback cannot
        // silently erase their counters. Deferred raw writes have their own
        // durable commit baseline and must avoid draining on every write.
        if meta.backend == BackendKind::Host || !meta.config.defer_metadata_commit {
            self.drain_read_advisory_locked(meta);
        }
        commit_previous_snapshot(meta)
    }

    pub fn sync(&self) -> Result<()> {
        let waiting = Instant::now();
        let mut meta = self.meta.write();
        self.drain_read_advisory_locked(&mut meta);
        self.runtime
            .commit_metrics
            .observe(CommitPhase::SyncLockWait, waiting.elapsed());
        let _lock_hold = self.runtime.commit_metrics.timer(CommitPhase::SyncLockHold);
        if meta.backend != BackendKind::Host {
            self.ensure_block_backend_writable_locked(&meta)?;
            if bulk_import_enabled() || meta.config.defer_metadata_commit {
                self.commit_deferred_locked(&mut meta, bulk_import_enabled())?;
                return Ok(());
            }
            let checkpoint_required = {
                let state = self.deferred_commit.lock();
                state.has_pending_sync()
            };
            // Immediate raw mutations are already journal-durable. Read-side
            // telemetry changes only the live view, not the trusted durable
            // hash; fsync must not turn it into a full metadata fanout.
            if !checkpoint_required {
                return Ok(());
            }
            let previous_meta_hash = meta.integrity.meta_hash.clone();
            // This checkpoint may publish discovery/normalization changes
            // (for example an offline member recorded during open) that were
            // not a journal transaction. Advance the transaction identity so
            // the preserved previous checkpoint cannot tie with the new one
            // and win or lose recovery based on a hash comparison.
            meta.txid = meta
                .txid
                .checked_add(1)
                .ok_or_else(|| ArgosError::Invalid("metadata txid overflow".to_string()))?;
            meta.updated_at = now_f64();
            journal::prepare_metadata_integrity_with_previous(&mut meta, previous_meta_hash)?;
            let superblocks = self.metadata_superblocks_locked(&meta)?;
            self.runtime
                .commit_metrics
                .record_checkpoint_reason(CheckpointReason::RecoveryOrFormat);
            let _checkpoint_timer = self.runtime.commit_metrics.timer(CommitPhase::Checkpoint);
            let result = if self.open_backend_covers_superblocks(&superblocks) {
                raw_store::write_metadata_copies_quorum_measured(
                    &*self.backend,
                    &superblocks,
                    &meta,
                    &self.runtime.commit_metrics,
                )
            } else {
                let backend = self.metadata_block_backend_locked(&meta, &superblocks, true)?;
                raw_store::write_metadata_copies_quorum_measured(
                    &*backend,
                    &superblocks,
                    &meta,
                    &self.runtime.commit_metrics,
                )
            };
            let durability = match result {
                Ok(report) => self.apply_quorum_write_report(&meta, report),
                Err(err) => {
                    self.fence_on_quorum_loss(&err);
                    return Err(err);
                }
            };
            // A successful metadata checkpoint is durable even when a
            // subsequent device-loss fence reports CommittedDurabilityLoss.
            meta.inodes.mark_committed();
            let mut state = self.deferred_commit.lock();
            state.durable_metadata = Some(Arc::new(meta.clone()));
            state.raw_uncommitted_metadata_dirty = false;
            state.raw_advisory_metadata_dirty = false;
            state.raw_checkpoint_required = false;
            state.raw_mount_reanchor_pending = false;
            drop(state);
            self.publish_capacity_snapshot_locked(&meta);
            durability?;
            return Ok(());
        }

        self.sync_dirty_host_shards()?;

        for path in [
            self.root.join(".argosfs/journal.jsonl"),
            self.root.join(".argosfs/meta.primary.json"),
            self.root.join(".argosfs/meta.secondary.json"),
            self.root.join(".argosfs/meta.json"),
        ] {
            if let Ok(file) = fs::File::open(&path) {
                file.sync_all()?;
            }
        }
        sync_directory(&self.root.join(".argosfs"));
        for disk in meta.disks.values() {
            let disk_root = relative_or_absolute(&self.root, &disk.path);
            sync_directory(&disk_root);
            sync_directory(&disk_root.join("shards"));
        }
        Ok(())
    }

    pub fn mark_clean_unmount(&self) -> Result<()> {
        // Commit any real transactions and required recovery/format checkpoint.
        // Advisory-only read counters are crash-optional: do not promote them
        // to a transaction or write the untrusted live hash to metadata slots.
        // The journal already contains acknowledged changes, so a clean
        // unmount only needs the small superblock clean-state quorum write.
        self.sync()?;
        let meta = self.meta.read();
        if meta.backend == BackendKind::Host {
            return Ok(());
        }
        self.ensure_block_backend_writable_locked(&meta)?;
        let superblocks = self.metadata_superblocks_locked(&meta)?;
        let mark_clean = |backend: &dyn StorageBackend| -> Result<()> {
            let report = raw_store::write_superblock_clean_state_quorum_measured(
                backend,
                &superblocks,
                &meta,
                true,
                &self.runtime.commit_metrics,
            )?;
            self.apply_quorum_write_report(&meta, report)
        };
        if self.open_backend_covers_superblocks(&superblocks) {
            mark_clean(&*self.backend)
        } else {
            let backend = self.metadata_block_backend_locked(&meta, &superblocks, true)?;
            mark_clean(&*backend)
        }
    }

    pub(super) fn sync_dirty_host_shards(&self) -> Result<()> {
        let shard_paths = {
            let mut dirty = self.runtime.dirty_host_shards.lock();
            std::mem::take(&mut *dirty)
        };
        let mut failed = BTreeSet::new();
        let mut first_error = None;
        for path in shard_paths {
            match fs::File::open(&path).and_then(|file| file.sync_all()) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    failed.insert(path);
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }
        if !failed.is_empty() {
            self.runtime.dirty_host_shards.lock().extend(failed);
        }
        if let Some(err) = first_error {
            return Err(ArgosError::Io(err));
        }
        Ok(())
    }

    pub(super) fn mark_host_shard_dirty(&self, path: PathBuf) {
        self.runtime.dirty_host_shards.lock().insert(path);
    }
}
