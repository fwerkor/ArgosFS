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

    pub(super) fn validate_all_directory_quotas_locked(&self, meta: &Metadata) -> Result<()> {
        let active = self.quota_runtime.read().active.clone();
        for ino in active {
            let Some(inode) = meta.inodes.get(&ino) else {
                continue;
            };
            let Some(limit) = Self::directory_quota_limit(inode)? else {
                continue;
            };
            let used = Self::subtree_logical_usage_locked(meta, ino)?;
            if used > limit {
                return Err(ArgosError::QuotaExceeded {
                    inode: ino,
                    used,
                    limit,
                });
            }
        }
        Ok(())
    }

    pub(super) fn has_directory_quotas(&self) -> bool {
        !self.quota_runtime.read().active.is_empty()
    }

    pub(super) fn rebuild_quota_runtime_locked(&self, meta: &Metadata) -> Result<()> {
        *self.quota_runtime.write() = QuotaRuntime::from_metadata(meta)?;
        Ok(())
    }

    pub(super) fn inherit_quota_memberships(&self, parent: InodeId, ino: InodeId) {
        let mut runtime = self.quota_runtime.write();
        let inherited = runtime
            .memberships
            .get(&parent)
            .cloned()
            .unwrap_or_default();
        if inherited.is_empty() {
            runtime.memberships.remove(&ino);
        } else {
            runtime.memberships.insert(ino, inherited);
        }
    }

    pub(super) fn adjust_quota_usage_for_resize(&self, ino: InodeId, old_size: u64, new_size: u64) {
        if old_size == new_size {
            return;
        }
        let mut runtime = self.quota_runtime.write();
        let memberships = runtime.memberships.get(&ino).cloned().unwrap_or_default();
        for quota_ino in memberships {
            let used = runtime.usage_bytes.entry(quota_ino).or_default();
            if new_size > old_size {
                *used = used.saturating_add(new_size - old_size);
            } else {
                *used = used.saturating_sub(old_size - new_size);
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
        for quota_ino in memberships {
            let Some(quota_inode) = meta.inodes.get(&quota_ino) else {
                continue;
            };
            let Some(limit) = Self::directory_quota_limit(quota_inode)? else {
                continue;
            };
            let used = runtime.usage_bytes.get(&quota_ino).copied().unwrap_or(0);
            let projected = used.saturating_add(growth);
            if projected > limit {
                return Err(ArgosError::QuotaExceeded {
                    inode: quota_ino,
                    used: projected,
                    limit,
                });
            }
        }
        Ok(())
    }
}
