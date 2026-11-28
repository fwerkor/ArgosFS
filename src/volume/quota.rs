use super::*;

impl ArgosFs {
    pub(super) fn directory_quota_limit(inode: &Inode) -> Result<Option<u64>> {
        Ok(inode.quota_bytes)
    }

    pub(super) fn subtree_logical_usage_locked(meta: &Metadata, root: InodeId) -> Result<u64> {
        fn visit(meta: &Metadata, ino: InodeId, seen: &mut BTreeSet<InodeId>) -> Result<u64> {
            if !seen.insert(ino) {
                return Ok(0);
            }
            let inode = meta
                .inodes
                .get(&ino)
                .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
            match inode.kind {
                NodeKind::File => Ok(inode.size),
                NodeKind::Directory => {
                    let mut total = 0u64;
                    for child in inode.entries.values() {
                        total = total.saturating_add(visit(meta, *child, seen)?);
                    }
                    Ok(total)
                }
                NodeKind::Symlink | NodeKind::Special => Ok(0),
            }
        }

        let root_inode = meta
            .inodes
            .get(&root)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {root}")))?;
        if root_inode.kind != NodeKind::Directory {
            return Err(ArgosError::NotDirectory(format!("inode {root}")));
        }
        visit(meta, root, &mut BTreeSet::new())
    }

    pub(super) fn rebuild_quota_runtime_locked(&self, meta: &Metadata) -> Result<()> {
        *self.quota_runtime.write() = QuotaRuntime::from_metadata(meta)?;
        Ok(())
    }

    /// Prepare an isolated, structurally shared quota update before mutating
    /// metadata. Publish it only after commit; transaction recovery continues
    /// to reconstruct the runtime from the authoritative recovered image.
    pub(super) fn plan_quota_edges_locked(
        &self,
        meta: &Metadata,
        removed: &[(InodeId, InodeId)],
        added: &[(InodeId, InodeId)],
        deleted_directory: Option<InodeId>,
    ) -> Result<Option<QuotaRuntime>> {
        let runtime = self.quota_runtime.read();
        if runtime.active.is_empty() {
            return Ok(None);
        }
        // Aggregate opposing changes before traversing subtrees. A move
        // within the same quota is a net-zero edge change.
        let mut deltas = BTreeMap::<(InodeId, InodeId), i32>::new();
        for (edges, delta) in [(removed, -1), (added, 1)] {
            for (parent, child) in edges {
                if let Some(scopes) = runtime.memberships.get(parent) {
                    for quota in scopes.keys() {
                        *deltas.entry((*child, *quota)).or_default() += delta;
                    }
                }
            }
        }
        let deleted_quota = deleted_directory.filter(|ino| runtime.active.contains(ino));
        if deltas.values().all(|delta| *delta == 0) && deleted_quota.is_none() {
            return Ok(None);
        }
        let mut candidate = runtime.clone();
        drop(runtime);
        let mut affected = BTreeSet::new();
        for ((child, quota), delta) in deltas {
            if delta != 0 {
                affected.insert(quota);
                for _ in 0..delta.unsigned_abs() {
                    candidate.change_edge(meta, child, quota, delta > 0)?;
                }
            }
        }
        if let Some(quota) = deleted_quota {
            candidate.change_edge(meta, quota, quota, false)?;
            candidate.active.remove(&quota);
            candidate.usage_bytes.remove(&quota);
        }
        for quota in affected {
            if !candidate.active.contains(&quota) {
                continue;
            }
            let inode = meta.inodes.get(&quota).ok_or_else(|| {
                ArgosError::CorruptedMetadata(format!("missing quota inode {quota}"))
            })?;
            if let Some(limit) = inode.quota_bytes {
                let used = candidate
                    .usage_bytes
                    .get(&quota)
                    .copied()
                    .unwrap_or(0)
                    .min(u128::from(u64::MAX)) as u64;
                if used > limit && !added.is_empty() {
                    return Err(ArgosError::QuotaExceeded {
                        inode: quota,
                        used,
                        limit,
                    });
                }
            }
        }
        Ok(Some(candidate))
    }

    pub(super) fn publish_quota_edges(&self, candidate: Option<QuotaRuntime>) {
        if let Some(candidate) = candidate {
            *self.quota_runtime.write() = candidate;
        }
    }

    pub(super) fn inherit_quota_memberships(&self, parent: InodeId, ino: InodeId) {
        let mut runtime = self.quota_runtime.write();
        let inherited: imbl::OrdMap<InodeId, u64> = runtime
            .memberships
            .get(&parent)
            .into_iter()
            .flat_map(|scopes| scopes.keys())
            .map(|quota| (*quota, 1u64))
            .collect();
        if !inherited.is_empty() {
            runtime.memberships.insert(ino, inherited);
        }
    }

    pub(super) fn adjust_quota_usage_for_resize(&self, ino: InodeId, old_size: u64, new_size: u64) {
        if old_size == new_size {
            return;
        }
        let mut runtime = self.quota_runtime.write();
        let memberships = runtime.memberships.get(&ino).cloned().unwrap_or_default();
        for quota_ino in memberships.keys() {
            let used = runtime.usage_bytes.entry(*quota_ino).or_default();
            if new_size > old_size {
                *used += u128::from(new_size - old_size);
            } else {
                *used = used.saturating_sub(u128::from(old_size - new_size));
            }
        }
    }

    pub(super) fn ensure_resize_within_quotas_locked(
        &self,
        meta: &Metadata,
        ino: InodeId,
        new_size: u64,
    ) -> Result<()> {
        let inode = meta
            .inodes
            .get(&ino)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
        if new_size <= inode.size {
            return Ok(());
        }
        let growth = new_size - inode.size;
        let runtime = self.quota_runtime.read();
        let memberships = runtime.memberships.get(&ino).cloned().unwrap_or_default();
        for quota_ino in memberships.keys() {
            let Some(quota_inode) = meta.inodes.get(quota_ino) else {
                continue;
            };
            let Some(limit) = Self::directory_quota_limit(quota_inode)? else {
                continue;
            };
            let used = runtime.usage_bytes.get(quota_ino).copied().unwrap_or(0);
            let projected = (used + u128::from(growth)).min(u128::from(u64::MAX)) as u64;
            if projected > limit {
                return Err(ArgosError::QuotaExceeded {
                    inode: *quota_ino,
                    used: projected,
                    limit,
                });
            }
        }
        Ok(())
    }
}

impl QuotaRuntime {
    /// Change one incoming edge (or the quota root's synthetic self-edge).
    /// Walk descendants only when a directory enters or leaves the scope.
    /// Repeated hard links change the reference count, not logical usage.
    pub(super) fn change_edge(
        &mut self,
        meta: &Metadata,
        root: InodeId,
        quota: InodeId,
        add: bool,
    ) -> Result<()> {
        let mut pending = vec![root];
        while let Some(ino) = pending.pop() {
            let inode = meta
                .inodes
                .get(&ino)
                .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
            let mut scopes = self.memberships.get(&ino).cloned().unwrap_or_default();
            let before = scopes.get(&quota).copied().unwrap_or(0);
            let after = if add {
                before.checked_add(1)
            } else {
                before.checked_sub(1)
            }
            .ok_or_else(|| {
                ArgosError::CorruptedMetadata(format!(
                    "invalid quota edge count for inode {ino}, quota {quota}"
                ))
            })?;
            if after == 0 {
                scopes.remove(&quota);
            } else {
                scopes.insert(quota, after);
            }
            if scopes.is_empty() {
                self.memberships.remove(&ino);
            } else {
                self.memberships.insert(ino, scopes);
            }
            if (before == 0) == (after == 0) {
                continue;
            }
            match inode.kind {
                NodeKind::File => {
                    let used = self.usage_bytes.entry(quota).or_default();
                    if add {
                        *used += u128::from(inode.size);
                    } else {
                        *used = used.checked_sub(u128::from(inode.size)).ok_or_else(|| {
                            ArgosError::CorruptedMetadata(format!(
                                "invalid usage for quota {quota}"
                            ))
                        })?;
                    }
                }
                NodeKind::Directory => pending.extend(inode.entries.values().copied()),
                NodeKind::Symlink | NodeKind::Special => {}
            }
        }
        Ok(())
    }
}
