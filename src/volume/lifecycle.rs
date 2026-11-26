use super::*;

impl ArgosFs {
    pub fn create(
        root: impl AsRef<Path>,
        mut config: VolumeConfig,
        disk_count: usize,
        force: bool,
    ) -> Result<Self> {
        if config.k == 0 {
            return Err(ArgosError::Invalid("k must be positive".to_string()));
        }
        let shard_count = checked_layout_total(config.k, config.m)?;
        if disk_count < shard_count {
            return Err(ArgosError::NotEnoughDisks {
                need: shard_count,
                have: disk_count,
            });
        }
        if config.chunk_size == 0 {
            config.chunk_size = VolumeConfig::default().chunk_size;
        }
        validate_commit_policy(&config)?;
        let _ = RsCodec::new(config.k, config.m)?;
        let root = root.as_ref().to_path_buf();
        let system = root.join(".argosfs");
        if force && system.exists() {
            fs::remove_dir_all(&system)?;
        }
        ensure_dir(&root)?;
        if system.exists() {
            return Err(ArgosError::AlreadyExists(root.display().to_string()));
        }
        ensure_private_dir(&system)?;
        ensure_private_dir(&system.join("devices"))?;
        ensure_private_dir(&system.join("snapshots"))?;
        ensure_private_dir(&system.join("cache"))?;
        let uuid = Uuid::new_v4().to_string();
        let created_at = now_f64();
        let mut disks = BTreeMap::new();
        for index in 0..disk_count {
            let id = format!("disk-{index:04}");
            let path = PathBuf::from(format!(".argosfs/devices/{id}"));
            let disk_root = root.join(&path);
            ensure_private_dir(&disk_root)?;
            ensure_private_dir(&disk_root.join("shards"))?;
            atomic_write(
                &disk_root.join("argosfs-disk.json"),
                serde_json::to_vec_pretty(&json!({
                    "format": FORMAT_VERSION,
                    "volume_uuid": uuid,
                    "disk_id": id,
                    "created_at": created_at
                }))?
                .as_slice(),
            )?;
            let probe = probe_disk_path(&disk_root, 1024 * 1024);
            disks.insert(
                id.clone(),
                Disk {
                    id,
                    path,
                    tier: probe.recommended_tier,
                    weight: probe.recommended_weight,
                    status: DiskStatus::Online,
                    manually_offline: false,
                    offline_since: None,
                    capacity_bytes: probe.capacity_bytes,
                    capacity_source: CapacitySource::AutoProbe,
                    used_bytes: 0,
                    health: HealthCounters {
                        temperature_c: 30.0,
                        ..HealthCounters::default()
                    },
                    class: probe.class,
                    backing_device: probe.backing_device.clone(),
                    backing_fs_id: probe.backing_fs_id.clone(),
                    failure_domain: probe
                        .backing_fs_id
                        .clone()
                        .unwrap_or_else(|| format!("disk-{index:04}")),
                    sysfs_block: probe.sysfs_block.clone(),
                    rotational: probe.rotational,
                    numa_node: probe.numa_node,
                    read_latency_ewma_ms: probe.measured_read_latency_ms,
                    write_latency_ewma_ms: probe.measured_write_latency_ms,
                    observed_read_mib_s: probe.measured_read_mib_s,
                    observed_write_mib_s: probe.measured_write_mib_s,
                    io_samples: u64::from(
                        probe.measured_read_mib_s > 0.0 || probe.measured_write_mib_s > 0.0,
                    ),
                    last_probe: probe,
                    created_at,
                },
            );
        }
        let root_inode = root_inode(created_at);
        let mut inodes = BTreeMap::new();
        inodes.insert(ROOT_INO, root_inode);
        let meta = Metadata {
            format: FORMAT_VERSION.to_string(),
            uuid,
            backend: BackendKind::Host,
            raw_pool: RawPoolMetadata::default(),
            created_at,
            updated_at: created_at,
            txid: 0,
            next_inode: ROOT_INO + 1,
            next_stripe: 1,
            config,
            layouts: BTreeMap::new(),
            current_write_layout: String::new(),
            reshape: None,
            encryption: EncryptionConfig::default(),
            integrity: MetadataIntegrity::default(),
            disks,
            inodes: inodes.into_iter().collect(),
            checkpoint_member_catalog: None,
        };
        let mut meta = meta;
        normalize_metadata_layouts(&mut meta);
        journal::initialize_volume(&root, &mut meta, created_at)?;
        Self::open(root)
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let recovered = journal::load_or_recover(&root)?;
        let mut meta = recovered.metadata;
        meta.backend = BackendKind::Host;
        normalize_metadata_layouts(&mut meta);
        recompute_disk_usage_from_metadata(&mut meta);
        if meta.format != FORMAT_VERSION {
            return Err(ArgosError::Invalid(format!(
                "unsupported format {}",
                meta.format
            )));
        }
        let _ = RsCodec::new(meta.config.k, meta.config.m)?;
        harden_host_storage_permissions(&root, &meta)?;
        meta.inodes.initialize_member_catalog();
        let cache = BlockCache::new_async(
            root.join(".argosfs/cache/l2"),
            64 * 1024 * 1024,
            meta.config.l2_cache_bytes,
        );
        Ok(Self {
            backend: Arc::new(HostFsBackend::new(&root)),
            backend_writable: true,
            raw_superblocks: Arc::new(Vec::new()),
            root: Arc::new(root),
            deferred_commit: Arc::new(Mutex::new(DeferredCommitState::new(&meta))),
            quarantined_devices: Arc::new(Mutex::new(BTreeSet::new())),
            quarantine_since: Arc::new(Mutex::new(BTreeMap::new())),
            write_fence: Arc::new(Mutex::new(None)),
            quota_runtime: Arc::new(RwLock::new(QuotaRuntime::from_metadata(&meta)?)),
            meta: Arc::new(MetadataLock::new(meta)),
            capacity_snapshot: Arc::new(RwLock::new(None)),
            cache: Arc::new(cache),
            verify_written: None,
            runtime: Arc::new(MountRuntime::new()),
        })
    }

    pub fn create_loop(
        images: &[PathBuf],
        config: VolumeConfig,
        image_size: u64,
        pool_name: &str,
        force: bool,
    ) -> Result<Self> {
        if force {
            remove_stale_pool_cache(BackendKind::LoopBlock, images);
        }
        prepare_loop_images(images, image_size, force)?;
        Self::create_block_backend(BackendKind::LoopBlock, images, config, pool_name, force)
    }

    pub fn create_raw(
        devices: &[PathBuf],
        config: VolumeConfig,
        pool_name: &str,
        force: bool,
    ) -> Result<Self> {
        if force {
            remove_stale_pool_cache(BackendKind::RawBlock, devices);
        }
        Self::create_block_backend(BackendKind::RawBlock, devices, config, pool_name, force)
    }

    pub fn open_loop(images: &[PathBuf], write: bool) -> Result<Self> {
        Self::open_block_backend(BackendKind::LoopBlock, images, write)
    }

    pub fn open_raw(devices: &[PathBuf], write: bool) -> Result<Self> {
        Self::open_block_backend(BackendKind::RawBlock, devices, write)
    }

    pub(super) fn create_block_backend(
        kind: BackendKind,
        paths: &[PathBuf],
        mut config: VolumeConfig,
        pool_name: &str,
        force: bool,
    ) -> Result<Self> {
        if config.k == 0 {
            return Err(ArgosError::Invalid("k must be positive".to_string()));
        }
        let shard_count = checked_layout_total(config.k, config.m)?;
        if paths.len() < shard_count {
            return Err(ArgosError::NotEnoughDisks {
                need: shard_count,
                have: paths.len(),
            });
        }
        if config.chunk_size == 0 {
            config.chunk_size = VolumeConfig::default().chunk_size;
        }
        validate_commit_policy(&config)?;
        let backend_file = match kind {
            BackendKind::LoopBlock => FileBlockBackend::open_loop(paths, true)?,
            BackendKind::RawBlock => FileBlockBackend::open_raw(paths, true)?,
            BackendKind::Host => {
                return Err(ArgosError::Unsupported(
                    "create_block_backend requires loop or raw backend".to_string(),
                ))
            }
        };
        let pool_uuid = Uuid::new_v4();
        let created_at = now_f64();
        let mut superblocks = Vec::new();
        let mut disks = BTreeMap::new();
        let mut allocators = BTreeMap::new();
        for (index, info) in backend_file.list_devices()?.into_iter().enumerate() {
            let id = format!("disk-{index:04}");
            let sb = raw_store::superblock_for_device(
                pool_uuid,
                index,
                &id,
                config.k,
                config.m,
                config.chunk_size,
                info.capacity,
                pool_name,
            )?;
            let allocator = allocator::init_allocator(
                sb.data.offset,
                sb.data.length,
                raw_format::RAW_BLOCK_SIZE,
            );
            allocators.insert(id.clone(), allocator);
            disks.insert(
                id.clone(),
                Disk {
                    id,
                    path: info.path,
                    tier: StorageTier::Warm,
                    weight: 1.0,
                    status: DiskStatus::Online,
                    manually_offline: false,
                    offline_since: None,
                    capacity_bytes: info.capacity,
                    capacity_source: CapacitySource::UserOverride,
                    used_bytes: 0,
                    health: HealthCounters {
                        temperature_c: 30.0,
                        ..HealthCounters::default()
                    },
                    class: DiskClass::Unknown,
                    backing_device: None,
                    backing_fs_id: None,
                    failure_domain: format!("raw-device-{index:04}"),
                    sysfs_block: None,
                    rotational: None,
                    numa_node: None,
                    read_latency_ewma_ms: 0.0,
                    write_latency_ewma_ms: 0.0,
                    observed_read_mib_s: 0.0,
                    observed_write_mib_s: 0.0,
                    io_samples: 0,
                    last_probe: DiskProbe::default(),
                    created_at,
                },
            );
            superblocks.push(sb);
        }
        let root_inode = root_inode(created_at);
        let mut inodes = BTreeMap::new();
        inodes.insert(ROOT_INO, root_inode);
        let mut meta = Metadata {
            format: FORMAT_VERSION.to_string(),
            uuid: pool_uuid.to_string(),
            backend: kind,
            raw_pool: RawPoolMetadata {
                pool_name: pool_name.to_string(),
                format_version: RAW_FORMAT_VERSION,
                clean: true,
                dirty_since_txid: 0,
                mount_generation: 1,
                allocators,
            },
            created_at,
            updated_at: created_at,
            txid: 0,
            next_inode: ROOT_INO + 1,
            next_stripe: 1,
            config,
            layouts: BTreeMap::new(),
            current_write_layout: String::new(),
            reshape: None,
            encryption: EncryptionConfig::default(),
            integrity: MetadataIntegrity::default(),
            disks,
            inodes: inodes.into_iter().collect(),
            checkpoint_member_catalog: None,
        };
        normalize_metadata_layouts(&mut meta);
        journal::prepare_metadata_integrity_for_external_store(&mut meta)?;
        let backend: Arc<dyn StorageBackend> =
            Arc::new(GuardedBackend::new(Arc::new(backend_file)));
        raw_store::initialize_pool(backend.clone(), &superblocks, &mut meta, force)?;
        raw_store::write_superblock_clean_state(&*backend, &superblocks, false)?;
        Self::from_block_parts(paths, backend, true, superblocks, meta, false, None)
    }

    pub(super) fn open_block_backend(
        kind: BackendKind,
        paths: &[PathBuf],
        write: bool,
    ) -> Result<Self> {
        let opened = raw_store::open_pool(kind, paths, write)?;
        let reanchor_can_wait = opened.recovery_verified
            && opened.superblocks.len()
                == opened
                    .metadata
                    .disks
                    .values()
                    .filter(|disk| disk.status != DiskStatus::Removed)
                    .count()
            && opened
                .metadata
                .disks
                .values()
                .all(|disk| matches!(disk.status, DiskStatus::Online | DiskStatus::Removed));
        let durable_metadata = if reanchor_can_wait {
            opened.durable_metadata
        } else {
            None
        };
        let mut meta = opened.metadata;
        normalize_metadata_layouts(&mut meta);
        recompute_disk_usage_from_metadata(&mut meta);
        Self::from_block_parts(
            paths,
            opened.backend,
            write,
            opened.superblocks,
            meta,
            reanchor_can_wait,
            durable_metadata,
        )
    }

    pub(super) fn from_block_parts(
        paths: &[PathBuf],
        backend: Arc<dyn StorageBackend>,
        backend_writable: bool,
        superblocks: Vec<RawSuperblock>,
        meta: Metadata,
        reanchor_can_wait: bool,
        durable_metadata: Option<Metadata>,
    ) -> Result<Self> {
        if meta.format != FORMAT_VERSION {
            return Err(ArgosError::Invalid(format!(
                "unsupported format {}",
                meta.format
            )));
        }
        let _ = RsCodec::new(meta.config.k, meta.config.m)?;
        let cache_root = block_cache_root(&meta.uuid, meta.backend, paths);
        let cache = BlockCache::new_async(cache_root, 64 * 1024 * 1024, meta.config.l2_cache_bytes);
        let root = paths
            .first()
            .and_then(|path| path.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        let raw_uncommitted_metadata_dirty = durable_metadata.is_none()
            && meta.backend != BackendKind::Host
            && (meta.integrity.meta_hash.is_empty()
                || meta.integrity.hash_scheme == Some(journal::DELTA_INTEGRITY_SCHEME)
                || (meta.integrity.hash_scheme.is_none()
                    && journal::canonical_metadata_hash(&meta)? != meta.integrity.meta_hash));
        let quarantined_devices = Arc::new(Mutex::new(BTreeSet::new()));
        let write_fence = Arc::new(Mutex::new(None));
        if backend_writable {
            meta.inodes.initialize_member_catalog();
        }
        let meta = Arc::new(MetadataLock::new(meta));
        let mut state =
            DeferredCommitState::new_with_raw_dirty(&meta.read(), raw_uncommitted_metadata_dirty);
        // Keep the exact authenticated v2 image, not the normalized live view.
        // The first real delta must account for any changed member paths,
        // allocation counters and layout normalization relative to that image.
        // A missing or unverified member keeps the conservative reanchor path.
        if let Some(durable) = durable_metadata {
            state.durable_metadata = Some(Arc::new(durable));
        }
        // A fully verified, complete pool has a durable recovery base even
        // after an unclean shutdown. Mount-time normalization can be anchored
        // with the first real mutation; advisory reads and a clean unmount do
        // not need to publish it. Missing members or recovery errors retain
        // the conservative eager checkpoint.
        state.raw_mount_reanchor_pending = reanchor_can_wait
            && raw_uncommitted_metadata_dirty
            && !meta.read().integrity.meta_hash.is_empty();
        let deferred_commit = Arc::new(Mutex::new(state));
        let quota_runtime = Arc::new(RwLock::new(QuotaRuntime::from_metadata(&meta.read())?));
        let capacity_snapshot = Arc::new(RwLock::new(Some(CapacitySnapshot::from_metadata(
            &root,
            &meta.read(),
            BTreeSet::new(),
        ))));
        let verify_written = if backend_writable && meta.read().backend != BackendKind::Host {
            VerifyAfterWrite::configured_delay().map(|delay| {
                VerifyAfterWrite::start(
                    backend.clone(),
                    delay,
                    VerifyAfterWrite::configured_retries(),
                    meta.clone(),
                    quarantined_devices.clone(),
                    write_fence.clone(),
                )
            })
        } else {
            None
        };
        Ok(Self {
            root: Arc::new(root),
            backend,
            backend_writable,
            raw_superblocks: Arc::new(superblocks),
            deferred_commit,
            quarantined_devices,
            quarantine_since: Arc::new(Mutex::new(BTreeMap::new())),
            write_fence,
            quota_runtime,
            meta,
            capacity_snapshot,
            cache: Arc::new(cache),
            verify_written,
            runtime: Arc::new(MountRuntime::new()),
        })
    }
}

/// `mkfs --force` replaces the pool UUID, so the previous pool's L2 cache
/// directory would otherwise be orphaned forever. Scan the members for the old
/// UUID before they are reformatted and remove only that disposable cache.
fn remove_stale_pool_cache(kind: BackendKind, paths: &[PathBuf]) {
    let Some(pool_uuid) = raw_store::scan_paths(kind, paths)
        .into_iter()
        .find_map(|device| device.pool_uuid)
    else {
        return;
    };
    let base = match kind {
        BackendKind::LoopBlock => super::helpers::loop_cache_base(paths),
        BackendKind::RawBlock => super::helpers::default_raw_cache_base(),
        BackendKind::Host => return,
    };
    let _ = crate::cache::clear_cache_dir(&base, &pool_uuid);
}
