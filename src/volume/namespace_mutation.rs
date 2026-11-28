use super::*;

impl ArgosFs {
    pub(super) fn mkdir_locked(
        &self,
        meta: &mut Metadata,
        parent: InodeId,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<InodeId> {
        self.ensure_block_backend_writable_locked(meta)?;
        validate_entry_name(name)?;
        if self
            .dir_inode_locked(meta, parent)?
            .entries
            .contains_key(name)
        {
            return Err(ArgosError::AlreadyExists(name.to_string()));
        }
        let now = now_f64();
        let ino = self.alloc_inode_locked(meta)?;
        let (gid, inherit_setgid) = {
            let parent_inode = self.dir_inode_locked(meta, parent)?;
            (
                if parent_inode.mode & libc::S_ISGID != 0 {
                    parent_inode.gid
                } else {
                    gid
                },
                parent_inode.mode & libc::S_ISGID != 0,
            )
        };
        let mode = if inherit_setgid {
            mode | libc::S_ISGID
        } else {
            mode
        };
        let inherited_default_acl = meta
            .inodes
            .get(&parent)
            .and_then(acl::inherited_directory_acl);
        let inherited_access_acl = meta
            .inodes
            .get(&parent)
            .and_then(|parent| acl::inherited_access_acl(parent, mode));
        let inode = Inode {
            id: ino,
            kind: NodeKind::Directory,
            mode: libc::S_IFDIR | (mode & 0o7777),
            uid,
            gid,
            nlink: 2,
            size: 0,
            rdev: 0,
            atime: now,
            mtime: now,
            ctime: now,
            entries: BTreeMap::new(),
            target: None,
            inline_data: None,
            inline_sha256: String::new(),
            blocks: Vec::new(),
            xattrs: BTreeMap::new(),
            quota_bytes: None,
            posix_acl_access: inherited_access_acl,
            posix_acl_default: inherited_default_acl,
            nfs4_acl: None,
            access_count: 0,
            write_count: 0,
            read_bytes: 0,
            write_bytes: 0,
            storage_class: StorageTier::Warm,
            boot_critical: boot_critical_name(name),
            workload_score: 0.0,
            last_accessed_at: now,
            last_written_at: now,
        };
        meta.inodes.insert(ino, inode);
        self.dir_inode_mut_locked(meta, parent)?
            .entries
            .insert(name.to_string(), ino);
        if let Some(parent_inode) = meta.inodes.get_mut(&parent) {
            parent_inode.nlink = parent_inode.nlink.saturating_add(1);
        }
        self.touch_inode_locked(meta, parent, true, true);
        self.commit_locked(
            meta,
            "mkdir",
            json!({"parent": parent, "name": name, "inode": ino}),
        )?;
        self.remember_parent_link(parent, ino);
        self.inherit_quota_memberships(parent, ino);
        Ok(ino)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn mknod_locked(
        &self,
        meta: &mut Metadata,
        parent: InodeId,
        name: &str,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> Result<InodeId> {
        self.ensure_block_backend_writable_locked(meta)?;
        validate_entry_name(name)?;
        if self
            .dir_inode_locked(meta, parent)?
            .entries
            .contains_key(name)
        {
            return Err(ArgosError::AlreadyExists(name.to_string()));
        }
        let rollback = self.commit_previous_with_advisory_locked(meta);
        let file_type = mode & libc::S_IFMT;
        let kind = if file_type == libc::S_IFREG || file_type == 0 {
            if rdev != 0 {
                return Err(ArgosError::Invalid(
                    "regular files must not carry an rdev".to_string(),
                ));
            }
            NodeKind::File
        } else if file_type == libc::S_IFCHR
            || file_type == libc::S_IFBLK
            || file_type == libc::S_IFIFO
            || file_type == libc::S_IFSOCK
        {
            if matches!(file_type, value if value == libc::S_IFIFO || value == libc::S_IFSOCK)
                && rdev != 0
            {
                return Err(ArgosError::Invalid(
                    "fifo and socket nodes must not carry an rdev".to_string(),
                ));
            }
            NodeKind::Special
        } else {
            return Err(ArgosError::Unsupported(format!(
                "unsupported mknod mode {mode:o}"
            )));
        };
        let now = now_f64();
        let ino = self.alloc_inode_locked(meta)?;
        let gid = {
            let parent_inode = self.dir_inode_locked(meta, parent)?;
            if parent_inode.mode & libc::S_ISGID != 0 {
                parent_inode.gid
            } else {
                gid
            }
        };
        let inherited_acl = meta
            .inodes
            .get(&parent)
            .and_then(|parent| acl::inherited_access_acl(parent, mode));
        let normalized_mode = if kind == NodeKind::File && file_type == 0 {
            libc::S_IFREG | (mode & 0o7777)
        } else {
            file_type | (mode & 0o7777)
        };
        let inode = Inode {
            id: ino,
            kind,
            mode: normalized_mode,
            uid,
            gid,
            nlink: 1,
            size: 0,
            rdev,
            atime: now,
            mtime: now,
            ctime: now,
            entries: BTreeMap::new(),
            target: None,
            inline_data: None,
            inline_sha256: String::new(),
            blocks: Vec::new(),
            xattrs: BTreeMap::new(),
            quota_bytes: None,
            posix_acl_access: inherited_acl,
            posix_acl_default: None,
            nfs4_acl: None,
            access_count: 0,
            write_count: 0,
            read_bytes: 0,
            write_bytes: 0,
            storage_class: StorageTier::Warm,
            boot_critical: boot_critical_name(name),
            workload_score: 0.0,
            last_accessed_at: now,
            last_written_at: now,
        };
        meta.inodes.insert(ino, inode);
        self.dir_inode_mut_locked(meta, parent)?
            .entries
            .insert(name.to_string(), ino);
        self.touch_inode_locked(meta, parent, true, true);
        if let Err(err) = self.commit_locked_with_previous(
            meta,
            rollback.as_ref(),
            "mknod",
            json!({"parent": parent, "name": name, "inode": ino, "mode": mode, "rdev": rdev}),
        ) {
            if !Self::transaction_error_is_committed(&err)
                && !matches!(err, ArgosError::Conflict(_))
            {
                if let Some(rollback) = rollback {
                    *meta = rollback;
                }
            }
            return Err(err);
        }
        self.remember_parent_link(parent, ino);
        self.inherit_quota_memberships(parent, ino);
        Ok(ino)
    }

    pub(super) fn unlink_locked(
        &self,
        meta: &mut Metadata,
        parent: InodeId,
        name: &str,
        dir: bool,
        uid: Option<u32>,
        preserve_unlinked: bool,
    ) -> Result<()> {
        validate_entry_name(name)?;
        let child = *self
            .dir_inode_locked(meta, parent)?
            .entries
            .get(name)
            .ok_or_else(|| ArgosError::NotFound(name.to_string()))?;
        let child_inode = meta
            .inodes
            .get(&child)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {child}")))?
            .clone();
        self.check_sticky_locked(meta, parent, child, uid)?;
        self.ensure_block_backend_writable_locked(meta)?;
        if dir {
            if child_inode.kind != NodeKind::Directory {
                return Err(ArgosError::NotDirectory(name.to_string()));
            }
            if !child_inode.entries.is_empty() {
                return Err(ArgosError::DirectoryNotEmpty(name.to_string()));
            }
        } else if child_inode.kind == NodeKind::Directory {
            return Err(ArgosError::IsDirectory(name.to_string()));
        }
        self.dir_inode_mut_locked(meta, parent)?
            .entries
            .remove(name);
        self.touch_inode_locked(meta, parent, true, true);
        let mut blocks_to_delete = Vec::new();
        if child_inode.kind == NodeKind::Directory {
            if let Some(parent_inode) = meta.inodes.get_mut(&parent) {
                parent_inode.nlink = parent_inode.nlink.saturating_sub(1).max(2);
            }
            meta.inodes.remove(&child);
        } else if let Some(live) = meta.inodes.get_mut(&child) {
            live.nlink = live.nlink.saturating_sub(1);
            live.ctime = now_f64();
            if live.nlink == 0 && !preserve_unlinked {
                blocks_to_delete = live.blocks.clone();
                meta.inodes.remove(&child);
            }
        }
        self.stage_block_reclamation_locked(meta, &blocks_to_delete);
        self.commit_locked(
            meta,
            if dir { "rmdir" } else { "unlink" },
            json!({"parent": parent, "name": name, "inode": child}),
        )?;
        self.forget_parent_link(child);
        self.refresh_quota_runtime_if_active_locked(meta)?;
        self.finish_block_reclamation_locked(meta, &blocks_to_delete);
        Ok(())
    }

    pub fn reap_unlinked_inode(&self, ino: InodeId) -> Result<()> {
        // Closing a linked file is an observation, not a namespace
        // mutation. Avoid the exclusive pool lock (and cloning the entire
        // inode's block map) for the overwhelmingly common read-only close.
        {
            let meta = self.meta.read();
            // Closing a normally linked file is not a mutation. Check that
            // before the writability gate, or every last close on a read-only
            // or write-fenced pool reports EROFS for no reason.
            if meta.inodes.get(&ino).is_none_or(|inode| inode.nlink != 0) {
                return Ok(());
            }
            self.ensure_block_backend_writable_locked(&meta)?;
        }
        // Recheck after upgrading the lock: an inode could have been reaped
        // or linked again in the meantime.
        let mut meta = self.meta.write();
        self.ensure_block_backend_writable_locked(&meta)?;
        let Some(inode) = meta.inodes.get(&ino) else {
            return Ok(());
        };
        if inode.nlink != 0 {
            return Ok(());
        }
        let blocks = inode.blocks.clone();
        meta.inodes.remove(&ino);
        self.stage_block_reclamation_locked(&mut meta, &blocks);
        self.commit_locked(&mut meta, "orphan-reap", json!({"inode": ino}))?;
        self.forget_parent_link(ino);
        self.finish_block_reclamation_locked(&mut meta, &blocks);
        Ok(())
    }

    pub(super) fn rename_locked(
        &self,
        meta: &mut Metadata,
        old_parent: InodeId,
        old_name: &str,
        new_parent: InodeId,
        new_name: &str,
        policy: RenamePolicy,
    ) -> Result<()> {
        validate_entry_name(old_name)?;
        validate_entry_name(new_name)?;
        if policy.no_replace && policy.exchange {
            return Err(ArgosError::Invalid(
                "RENAME_NOREPLACE and RENAME_EXCHANGE cannot be combined".to_string(),
            ));
        }
        if old_parent == new_parent && old_name == new_name && !policy.exchange {
            return Ok(());
        }
        let child = *self
            .dir_inode_locked(meta, old_parent)?
            .entries
            .get(old_name)
            .ok_or_else(|| ArgosError::NotFound(old_name.to_string()))?;
        let child_inode = meta
            .inodes
            .get(&child)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {child}")))?
            .clone();
        let child_is_dir = child_inode.kind == NodeKind::Directory;
        self.dir_inode_locked(meta, new_parent)?;
        self.check_sticky_locked(meta, old_parent, child, policy.uid)?;
        if child_is_dir && Self::directory_contains_inode(meta, child, new_parent) {
            return Err(ArgosError::Invalid(
                "cannot move a directory into itself".to_string(),
            ));
        }
        let existing = self
            .dir_inode_locked(meta, new_parent)?
            .entries
            .get(new_name)
            .copied();
        if policy.no_replace && existing.is_some() {
            return Err(ArgosError::AlreadyExists(new_name.to_string()));
        }
        if existing == Some(child) && !policy.exchange {
            return Ok(());
        }
        self.ensure_block_backend_writable_locked(meta)?;
        if policy.exchange {
            let existing = existing.ok_or_else(|| ArgosError::NotFound(new_name.to_string()))?;
            self.check_sticky_locked(meta, new_parent, existing, policy.uid)?;
            let existing_inode = meta
                .inodes
                .get(&existing)
                .ok_or_else(|| ArgosError::NotFound(format!("inode {existing}")))?
                .clone();
            if existing_inode.kind == NodeKind::Directory
                && Self::directory_contains_inode(meta, existing, old_parent)
            {
                return Err(ArgosError::Invalid(
                    "cannot exchange a directory into itself".to_string(),
                ));
            }
            if self.has_directory_quotas() {
                let mut quota_candidate = meta.clone();
                self.dir_inode_mut_locked(&mut quota_candidate, old_parent)?
                    .entries
                    .insert(old_name.to_string(), existing);
                self.dir_inode_mut_locked(&mut quota_candidate, new_parent)?
                    .entries
                    .insert(new_name.to_string(), child);
                self.validate_all_directory_quotas_locked(&quota_candidate)?;
            }
            self.dir_inode_mut_locked(meta, old_parent)?
                .entries
                .insert(old_name.to_string(), existing);
            self.dir_inode_mut_locked(meta, new_parent)?
                .entries
                .insert(new_name.to_string(), child);
            if old_parent != new_parent {
                if child_is_dir {
                    if let Some(parent_inode) = meta.inodes.get_mut(&old_parent) {
                        parent_inode.nlink = parent_inode.nlink.saturating_sub(1).max(2);
                    }
                    if let Some(parent_inode) = meta.inodes.get_mut(&new_parent) {
                        parent_inode.nlink = parent_inode.nlink.saturating_add(1);
                    }
                }
                if existing_inode.kind == NodeKind::Directory {
                    if let Some(parent_inode) = meta.inodes.get_mut(&new_parent) {
                        parent_inode.nlink = parent_inode.nlink.saturating_sub(1).max(2);
                    }
                    if let Some(parent_inode) = meta.inodes.get_mut(&old_parent) {
                        parent_inode.nlink = parent_inode.nlink.saturating_add(1);
                    }
                }
            }
            self.touch_inode_locked(meta, old_parent, true, true);
            self.touch_inode_locked(meta, new_parent, true, true);
            self.touch_inode_locked(meta, child, false, true);
            self.touch_inode_locked(meta, existing, false, true);
            self.commit_locked(
                meta,
                "rename-exchange",
                json!({"old_parent": old_parent, "old_name": old_name, "new_parent": new_parent, "new_name": new_name, "inode": child, "exchanged_inode": existing}),
            )?;
            self.remember_parent_link(new_parent, child);
            self.remember_parent_link(old_parent, existing);
            self.refresh_quota_runtime_if_active_locked(meta)?;
            return Ok(());
        }
        let existing_inode = if let Some(existing) = existing {
            self.check_sticky_locked(meta, new_parent, existing, policy.uid)?;
            let inode = meta
                .inodes
                .get(&existing)
                .ok_or_else(|| ArgosError::NotFound(format!("inode {existing}")))?
                .clone();
            match (&child_inode.kind, &inode.kind) {
                (NodeKind::Directory, NodeKind::Directory) if !inode.entries.is_empty() => {
                    return Err(ArgosError::DirectoryNotEmpty(new_name.to_string()));
                }
                (NodeKind::Directory, NodeKind::Directory) => {}
                (NodeKind::Directory, _) => {
                    return Err(ArgosError::NotDirectory(new_name.to_string()));
                }
                (_, NodeKind::Directory) => {
                    return Err(ArgosError::IsDirectory(new_name.to_string()));
                }
                _ => {}
            }
            Some((existing, inode))
        } else {
            None
        };
        if self.has_directory_quotas() {
            let mut quota_candidate = meta.clone();
            if existing.is_some() {
                self.dir_inode_mut_locked(&mut quota_candidate, new_parent)?
                    .entries
                    .remove(new_name);
            }
            self.dir_inode_mut_locked(&mut quota_candidate, old_parent)?
                .entries
                .remove(old_name);
            self.dir_inode_mut_locked(&mut quota_candidate, new_parent)?
                .entries
                .insert(new_name.to_string(), child);
            self.validate_all_directory_quotas_locked(&quota_candidate)?;
        }
        let mut blocks_to_delete = Vec::new();
        if let Some((existing, existing_inode)) = existing_inode {
            self.dir_inode_mut_locked(meta, new_parent)?
                .entries
                .remove(new_name);
            if existing_inode.kind == NodeKind::Directory {
                meta.inodes.remove(&existing);
                if let Some(parent_inode) = meta.inodes.get_mut(&new_parent) {
                    parent_inode.nlink = parent_inode.nlink.saturating_sub(1).max(2);
                }
            } else if let Some(live) = meta.inodes.get_mut(&existing) {
                live.nlink = live.nlink.saturating_sub(1);
                live.ctime = now_f64();
                if live.nlink == 0 && !policy.preserve_replaced_inode {
                    blocks_to_delete = live.blocks.clone();
                    meta.inodes.remove(&existing);
                }
            }
        }
        self.stage_block_reclamation_locked(meta, &blocks_to_delete);
        self.dir_inode_mut_locked(meta, old_parent)?
            .entries
            .remove(old_name);
        self.dir_inode_mut_locked(meta, new_parent)?
            .entries
            .insert(new_name.to_string(), child);
        if child_is_dir && old_parent != new_parent {
            if let Some(parent_inode) = meta.inodes.get_mut(&old_parent) {
                parent_inode.nlink = parent_inode.nlink.saturating_sub(1).max(2);
            }
            if let Some(parent_inode) = meta.inodes.get_mut(&new_parent) {
                parent_inode.nlink = parent_inode.nlink.saturating_add(1);
            }
        }
        self.touch_inode_locked(meta, old_parent, true, true);
        self.touch_inode_locked(meta, new_parent, true, true);
        self.touch_inode_locked(meta, child, false, true);
        self.commit_locked(
            meta,
            "rename",
            json!({"old_parent": old_parent, "old_name": old_name, "new_parent": new_parent, "new_name": new_name, "inode": child}),
        )?;
        if let Some(existing) = existing {
            self.forget_parent_link(existing);
        }
        self.remember_parent_link(new_parent, child);
        self.refresh_quota_runtime_if_active_locked(meta)?;
        self.finish_block_reclamation_locked(meta, &blocks_to_delete);
        Ok(())
    }

    pub(super) fn resolve_path_locked(
        &self,
        meta: &Metadata,
        path: &str,
        follow_final: bool,
        limit: u32,
    ) -> Result<InodeId> {
        if limit == 0 {
            return Err(ArgosError::Invalid("too many symbolic links".to_string()));
        }
        let clean = clean_path(path);
        if clean == "/" {
            return Ok(ROOT_INO);
        }
        let mut current = ROOT_INO;
        let parts = split_path(&clean);
        let mut prefix: Vec<String> = Vec::new();
        for (idx, part) in parts.iter().enumerate() {
            let inode = self.dir_inode_locked(meta, current)?;
            let part_key = entry_name_from_str(part)?;
            let next = *inode
                .entries
                .get(part_key.as_str())
                .ok_or_else(|| ArgosError::NotFound(clean.clone()))?;
            let child = meta
                .inodes
                .get(&next)
                .ok_or_else(|| ArgosError::NotFound(format!("inode {next}")))?;
            let final_component = idx + 1 == parts.len();
            if child.kind == NodeKind::Symlink && (follow_final || !final_component) {
                let target_bytes =
                    decode_symlink_target_bytes(child.target.as_deref().unwrap_or_default());
                let target = std::str::from_utf8(&target_bytes).map_err(|_| {
                    ArgosError::Invalid(
                        "non-UTF-8 symlink targets cannot be followed by string path APIs"
                            .to_string(),
                    )
                })?;
                let rest = parts[idx + 1..].join("/");
                let new_path = if target.starts_with('/') {
                    if rest.is_empty() {
                        target.to_string()
                    } else {
                        format!("{target}/{rest}")
                    }
                } else {
                    let base = if prefix.is_empty() {
                        "/".to_string()
                    } else {
                        format!("/{}", prefix.join("/"))
                    };
                    if rest.is_empty() {
                        clean_path(&format!("{base}/{target}"))
                    } else {
                        clean_path(&format!("{base}/{target}/{rest}"))
                    }
                };
                return self.resolve_path_locked(meta, &new_path, true, limit - 1);
            }
            current = next;
            prefix.push(part.clone());
        }
        Ok(current)
    }

    pub(super) fn dir_inode_locked<'a>(
        &self,
        meta: &'a Metadata,
        ino: InodeId,
    ) -> Result<&'a Inode> {
        let inode = meta
            .inodes
            .get(&ino)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
        if inode.kind != NodeKind::Directory {
            return Err(ArgosError::NotDirectory(format!("inode {ino}")));
        }
        Ok(inode)
    }

    pub(super) fn dir_inode_mut_locked<'a>(
        &self,
        meta: &'a mut Metadata,
        ino: InodeId,
    ) -> Result<&'a mut Inode> {
        let inode = meta
            .inodes
            .get_mut(&ino)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {ino}")))?;
        if inode.kind != NodeKind::Directory {
            return Err(ArgosError::NotDirectory(format!("inode {ino}")));
        }
        Ok(inode)
    }

    /// Resolve an inode's parent for the ".." entry. `Meta.inodes` has no
    /// reverse edges, so a full scan here made every readdir O(namespace) and
    /// a tree walk O(directories * namespace). Build the inverse map once and
    /// then maintain it from the namespace mutations.
    pub(super) fn parent_inode_locked(&self, meta: &Metadata, ino: InodeId) -> Result<InodeId> {
        if ino == ROOT_INO {
            return Ok(ROOT_INO);
        }
        if let Some(parent) = self.cached_parent_inode(meta, ino) {
            return Ok(parent);
        }
        let mut index = self.runtime.parent_index.write();
        // Another reader may have built the map between the two locks.
        if let Some(parent) = index
            .as_ref()
            .and_then(|index| index.get(&ino))
            .copied()
            .filter(|parent| Self::parent_link_valid(meta, *parent, ino))
        {
            return Ok(parent);
        }
        *index = Some(Self::build_parent_index(meta));
        index
            .as_ref()
            .and_then(|index| index.get(&ino))
            .copied()
            .ok_or_else(|| ArgosError::NotFound(format!("parent of inode {ino}")))
    }

    fn cached_parent_inode(&self, meta: &Metadata, ino: InodeId) -> Option<InodeId> {
        let index = self.runtime.parent_index.read();
        let parent = *index.as_ref()?.get(&ino)?;
        // A stale hint is never trusted. The parent's directory entry is the
        // authority, and checking one directory is bounded by its own size.
        Self::parent_link_valid(meta, parent, ino).then_some(parent)
    }

    fn parent_link_valid(meta: &Metadata, parent: InodeId, ino: InodeId) -> bool {
        meta.inodes.get(&parent).is_some_and(|inode| {
            inode.kind == NodeKind::Directory && inode.entries.values().any(|child| *child == ino)
        })
    }

    fn build_parent_index(meta: &Metadata) -> BTreeMap<InodeId, InodeId> {
        let mut index = BTreeMap::new();
        for inode in meta.inodes.values() {
            if inode.kind != NodeKind::Directory {
                continue;
            }
            for child in inode.entries.values() {
                index.insert(*child, inode.id);
            }
        }
        index
    }

    pub(super) fn remember_parent_link(&self, parent: InodeId, child: InodeId) {
        if let Some(index) = self.runtime.parent_index.write().as_mut() {
            index.insert(child, parent);
        }
    }

    fn forget_parent_link(&self, child: InodeId) {
        if let Some(index) = self.runtime.parent_index.write().as_mut() {
            index.remove(&child);
        }
    }

    pub(super) fn directory_contains_inode(
        meta: &Metadata,
        ancestor: InodeId,
        needle: InodeId,
    ) -> bool {
        if ancestor == needle {
            return true;
        }
        let Some(inode) = meta.inodes.get(&ancestor) else {
            return false;
        };
        if inode.kind != NodeKind::Directory {
            return false;
        }
        inode
            .entries
            .values()
            .any(|child| Self::directory_contains_inode(meta, *child, needle))
    }

    pub(super) fn check_sticky_locked(
        &self,
        meta: &Metadata,
        parent: InodeId,
        child: InodeId,
        uid: Option<u32>,
    ) -> Result<()> {
        let Some(uid) = uid else {
            return Ok(());
        };
        if uid == 0 {
            return Ok(());
        }
        let parent_inode = self.dir_inode_locked(meta, parent)?;
        if parent_inode.mode & libc::S_ISVTX == 0 {
            return Ok(());
        }
        let child_inode = meta
            .inodes
            .get(&child)
            .ok_or_else(|| ArgosError::NotFound(format!("inode {child}")))?;
        if uid == parent_inode.uid || uid == child_inode.uid {
            Ok(())
        } else {
            Err(ArgosError::PermissionDenied(format!(
                "sticky directory denies uid {uid} removing inode {child}"
            )))
        }
    }

    pub(super) fn touch_inode_locked(
        &self,
        meta: &mut Metadata,
        ino: InodeId,
        mtime: bool,
        ctime: bool,
    ) {
        if let Some(inode) = meta.inodes.get_mut(&ino) {
            let now = now_f64();
            if mtime {
                inode.mtime = now;
            }
            if ctime {
                inode.ctime = now;
            }
        }
    }

    pub(super) fn alloc_inode_locked(&self, meta: &mut Metadata) -> Result<InodeId> {
        metadata_budget::check_new_inode(meta)?;
        let ino = meta.next_inode;
        meta.next_inode += 1;
        Ok(ino)
    }
}
