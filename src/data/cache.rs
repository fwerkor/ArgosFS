use crate::error::Result;
use crate::util::{ensure_dir, sha256_hex};
use parking_lot::Mutex;
use serde_json::json;
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Once admitted, cached block bytes are immutable. Compute their trusted
/// digest *once* on admission rather than re-hashing every FUSE subrange read.
struct MemoryItem {
    data: Arc<[u8]>,
    blake3: String,
    legacy_sha256: Option<String>,
    last_used: u64,
}

impl MemoryItem {
    fn new(data: Arc<[u8]>, last_used: u64) -> Self {
        let blake3 = crate::util::content_hash_hex(&data);
        Self {
            data,
            blake3,
            legacy_sha256: None,
            last_used,
        }
    }

    fn matches(&mut self, expected: Option<&str>) -> bool {
        let Some(expected) = expected else {
            return true;
        };
        if self.blake3 == expected {
            return true;
        }
        // Existing pools may use SHA-256 block hashes. Compute the legacy
        // digest at most once; a mismatched request is still invalidated.
        self.legacy_sha256
            .get_or_insert_with(|| sha256_hex(&self.data))
            == expected
    }
}

#[derive(Default)]
struct CacheInner {
    bytes: usize,
    items: BTreeMap<String, MemoryItem>,
    // Two ordered indices provide O(log N) hits, evictions and invalidation.
    // A VecDeque::retain scan on every hit used to become O(cache entries).
    order: BTreeMap<u64, String>,
    lru_clock: u64,
    // Track only in-flight L2 keys. Unrelated admissions must not invalidate
    // another key's verified L2 read, and tombstones cannot grow unbounded.
    inflight_l2: BTreeMap<String, InflightL2>,
    l2_loaded: bool,
    l2_bytes: u64,
    l2_hot_bytes: u64,
    l2_index: BTreeMap<PathBuf, L2Entry>,
    l2_clock: u64,
    l2_evict_seed: u64,
    l2_window_requests: u64,
    l2_window_hits: u64,
    l2_low_reuse_windows: u8,
    l2_freeze_stream: bool,
    hits: u64,
    misses: u64,
    l2_hits: u64,
    l2_writes: u64,
    l2_errors: u64,
    l2_admission_frozen: u64,
    l2_admission_oversize: u64,
    l2_evictions: u64,
    l2_loaded_items: u64,
    l2_loaded_bytes: u64,
}

impl CacheInner {
    fn begin_l2_read(&mut self, key: &str) -> u64 {
        let in_flight = self.inflight_l2.entry(key.to_owned()).or_default();
        in_flight.readers += 1;
        in_flight.generation
    }

    fn finish_l2_read(&mut self, key: &str, generation: u64) -> bool {
        let in_flight = self
            .inflight_l2
            .get_mut(key)
            .expect("L2 read must be registered");
        let current = in_flight.generation == generation;
        in_flight.readers -= 1;
        if in_flight.readers == 0 {
            self.inflight_l2.remove(key);
        }
        current
    }

    fn note_key_mutation(&mut self, key: &str) {
        if let Some(in_flight) = self.inflight_l2.get_mut(key) {
            in_flight.generation = in_flight.generation.wrapping_add(1);
        }
    }

    fn note_prefix_mutation(&mut self, prefix: &str) {
        for (key, in_flight) in &mut self.inflight_l2 {
            if key.starts_with(prefix) {
                in_flight.generation = in_flight.generation.wrapping_add(1);
            }
        }
    }

    fn remove_memory(&mut self, key: &str) {
        if let Some(item) = self.items.remove(key) {
            self.bytes = self.bytes.saturating_sub(item.data.len());
            self.order.remove(&item.last_used);
        }
    }

    /// Detect a workload with real cache reuse. A one-shot sequential scan
    /// has a very low L2 hit ratio; a re-read pass hits a substantial sample.
    /// Freeze cold admissions only once reuse is demonstrated, then let the
    /// ratio fall again if a different workload arrives.
    fn record_l2_lookup(&mut self, hit: bool) {
        const WINDOW: u64 = 64;
        const FREEZE_RATIO: f64 = 0.03;
        self.l2_window_requests = self.l2_window_requests.saturating_add(1);
        if hit {
            self.l2_window_hits = self.l2_window_hits.saturating_add(1);
        }
        if self.l2_window_requests >= WINDOW {
            let ratio = self.l2_window_hits as f64 / self.l2_window_requests as f64;
            self.l2_freeze_stream = ratio >= FREEZE_RATIO;
            // A working set that has stopped reusing L2 must be able to
            // replace historically hot entries. Otherwise every cold
            // admission can evict itself forever when the cache is all hot.
            if self.l2_freeze_stream {
                self.l2_low_reuse_windows = 0;
            } else {
                self.l2_low_reuse_windows = self.l2_low_reuse_windows.saturating_add(1);
            }
            if self.l2_low_reuse_windows >= 8 {
                // Keep cold priorities stable across pruning batches, rotating
                // only after sustained low reuse so a new workload can adapt.
                // During the initial cold scan there is no hot population
                // to demote. Rotating its priority destroys the stable
                // sample and biases residency towards the tail of the scan.
                if self.l2_hot_bytes > 0 {
                    self.l2_evict_seed = self.l2_evict_seed.wrapping_add(1);
                    for entry in self.l2_index.values_mut() {
                        entry.reused = false;
                    }
                    self.l2_hot_bytes = 0;
                }
                self.l2_low_reuse_windows = 0;
            }
            self.l2_window_requests = 0;
            self.l2_window_hits = 0;
        }
    }

    fn next_tick(&mut self) -> u64 {
        if self.lru_clock == u64::MAX {
            // Extremely long-running mounts must not alias a prior tick.
            // Rebase the ordered indices without changing the LRU order.
            let mut rebased = BTreeMap::new();
            for (index, (_, key)) in std::mem::take(&mut self.order).into_iter().enumerate() {
                let tick = index as u64 + 1;
                if let Some(item) = self.items.get_mut(&key) {
                    item.last_used = tick;
                    rebased.insert(tick, key);
                }
            }
            self.order = rebased;
            self.lru_clock = self.order.len() as u64;
        }
        self.lru_clock += 1;
        self.lru_clock
    }

    fn touch(&mut self, key: &str) {
        let old_tick = self.items[key].last_used;
        self.order.remove(&old_tick);
        let tick = self.next_tick();
        self.items.get_mut(key).unwrap().last_used = tick;
        self.order.insert(tick, key.to_owned());
    }

    fn insert_memory(&mut self, key: &str, mut item: MemoryItem, limit: usize) {
        if limit == 0 {
            return;
        }
        self.remove_memory(key);
        item.last_used = self.next_tick();
        let tick = item.last_used;
        self.bytes = self.bytes.saturating_add(item.data.len());
        self.items.insert(key.to_owned(), item);
        self.order.insert(tick, key.to_owned());
        while self.bytes > limit {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.remove_memory(&oldest);
        }
    }
}

#[derive(Default)]
struct InflightL2 {
    readers: usize,
    generation: u64,
}

#[derive(Clone, Copy, Default)]
struct L2Entry {
    size: u64,
    touched: u64,
    /// Set when the entry was served from L2 at least once. Reused entries are
    /// protected from the random cold-entry eviction used for streaming scans.
    reused: bool,
}

fn eviction_score(path: &Path, seed: u64) -> u64 {
    let mut hasher = DefaultHasher::new();
    // The filename is the digest of the cache key; installation paths must
    // not influence which logical blocks survive a scan.
    path.file_name().hash(&mut hasher);
    seed.hash(&mut hasher);
    hasher.finish()
}

pub struct BlockCache {
    root: PathBuf,
    memory_limit: usize,
    l2_limit: u64,
    inner: Mutex<CacheInner>,
    // These byte counters must not take a second global cache lock on the
    // very hot direct-copy path. They reset on each volume open.
    direct_copy_bytes: AtomicU64,
    l2_read_bytes: AtomicU64,
}

impl BlockCache {
    pub fn new(root: impl AsRef<Path>, memory_limit: usize, l2_limit: u64) -> Self {
        let root = root.as_ref().to_path_buf();
        let _ = ensure_dir(&root);
        let l2_limit = configured_l2_limit(l2_limit);
        Self {
            root,
            memory_limit,
            l2_limit,
            inner: Mutex::new(CacheInner::default()),
            direct_copy_bytes: AtomicU64::new(0),
            l2_read_bytes: AtomicU64::new(0),
        }
    }

    /// Legacy full-block caller. Most FUSE reads should call `get_range`,
    /// which avoids allocating/copying a whole decoded stripe on cache hits.
    pub fn get(&self, key: &str, expected_sha: Option<&str>) -> Option<Vec<u8>> {
        self.get_shared(key, expected_sha).map(|data| data.to_vec())
    }

    /// Return only a validated contiguous section of an immutable cached
    /// decoded block. The expected total length prevents returning a truncated
    /// or outdated block. Bounds are checked even on L2 promotions.
    pub fn get_range(
        &self,
        key: &str,
        expected_sha: Option<&str>,
        expected_len: usize,
        start: usize,
        end: usize,
    ) -> Option<Vec<u8>> {
        if end < start || end > expected_len {
            return None;
        }
        let data = self.get_shared(key, expected_sha)?;
        if data.len() != expected_len {
            self.remove(key);
            return None;
        }
        Some(data[start..end].to_vec())
    }

    /// Copy directly into the caller's output. The cache mutex protects the
    /// lookup and LRU update only; the immutable Arc keeps the bytes alive
    /// after eviction or concurrent invalidation, without a temporary Vec.
    pub fn copy_range(
        &self,
        key: &str,
        expected_sha: Option<&str>,
        expected_len: usize,
        start: usize,
        destination: &mut [u8],
    ) -> bool {
        let Some(end) = start.checked_add(destination.len()) else {
            return false;
        };
        if end > expected_len {
            return false;
        }
        let Some(data) = self.get_shared(key, expected_sha) else {
            return false;
        };
        if data.len() != expected_len {
            self.remove(key);
            return false;
        }
        destination.copy_from_slice(&data[start..end]);
        self.direct_copy_bytes
            .fetch_add(destination.len() as u64, Ordering::Relaxed);
        true
    }

    fn get_shared(&self, key: &str, expected_sha: Option<&str>) -> Option<Arc<[u8]>> {
        let mut inner = self.inner.lock();
        if let Some(item) = inner.items.get_mut(key) {
            if item.matches(expected_sha) {
                let data = Arc::clone(&item.data);
                inner.hits = inner.hits.saturating_add(1);
                inner.touch(key);
                return Some(data);
            }
            inner.note_key_mutation(key);
            inner.remove_memory(key);
        }
        if self.l2_limit == 0 {
            inner.misses = inner.misses.saturating_add(1);
            return None;
        }
        let generation = inner.begin_l2_read(key);
        drop(inner);

        let _ = self.load_l2_index();
        let path = self.l2_path(key);
        // Disk I/O and all cryptographic work are done outside the global
        // cache lock. Immutable bytes cannot be mutated after admission.
        let candidate = std::fs::read(&path).ok().map(|data| {
            self.l2_read_bytes
                .fetch_add(data.len() as u64, Ordering::Relaxed);
            let mut item = MemoryItem::new(Arc::from(data), 0);
            let valid = item.matches(expected_sha);
            (item, valid)
        });

        let mut inner = self.inner.lock();
        if !inner.finish_l2_read(key, generation) {
            // This exact key was removed, replaced or prefix-invalidated
            // while its L2 file was read. Other keys do not affect this read.
            inner.misses = inner.misses.saturating_add(1);
            inner.record_l2_lookup(false);
            return None;
        }
        if let Some((item, valid)) = candidate {
            if valid {
                if let Some(latest) = inner.items.get_mut(key) {
                    if latest.matches(expected_sha) {
                        let data = Arc::clone(&latest.data);
                        inner.hits = inner.hits.saturating_add(1);
                        inner.touch(key);
                        return Some(data);
                    }
                    inner.remove_memory(key);
                }
                inner.l2_hits = inner.l2_hits.saturating_add(1);
                inner.l2_clock = inner.l2_clock.saturating_add(1);
                let touched = inner.l2_clock;
                let promoted = inner.l2_index.get_mut(&path).map(|entry| {
                    entry.touched = touched;
                    if entry.reused {
                        None
                    } else {
                        entry.reused = true;
                        Some(entry.size)
                    }
                });
                if let Some(Some(size)) = promoted {
                    inner.l2_hot_bytes = inner.l2_hot_bytes.saturating_add(size);
                }
                inner.record_l2_lookup(true);
                let data = Arc::clone(&item.data);
                inner.insert_memory(key, item, self.memory_limit);
                return Some(data);
            }
            // A corrupted on-disk L2 entry is never promoted. Keep the
            // key's mutation lock across deletion, so a concurrent put()
            // cannot have replaced it with a new key version meanwhile.
            let _ = std::fs::remove_file(&path);
            if let Some(entry) = inner.l2_index.remove(&path) {
                inner.l2_bytes = inner.l2_bytes.saturating_sub(entry.size);
                if entry.reused {
                    inner.l2_hot_bytes = inner.l2_hot_bytes.saturating_sub(entry.size);
                }
            }
        }
        inner.misses = inner.misses.saturating_add(1);
        inner.record_l2_lookup(false);
        None
    }

    pub fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        // Hash once before taking the global cache mutex, even on a cold
        // stripe admission while other FUSE threads are serving warm hits.
        let admission = (self.memory_limit > 0).then(|| MemoryItem::new(Arc::from(data), 0));
        {
            let mut inner = self.inner.lock();
            inner.note_key_mutation(key);
            if let Some(item) = admission {
                inner.insert_memory(key, item, self.memory_limit);
            }
        }
        if self.l2_limit > 0 && self.put_l2(key, data).is_err() {
            self.inner.lock().l2_errors += 1;
        }
        Ok(())
    }

    pub fn invalidate_prefix(&self, prefix: &str) {
        let mut inner = self.inner.lock();
        inner.note_prefix_mutation(prefix);
        let keys = inner
            .items
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            inner.remove_memory(&key);
        }
    }

    pub fn remove(&self, key: &str) {
        {
            let mut inner = self.inner.lock();
            inner.note_key_mutation(key);
            inner.remove_memory(key);
        }
        if self.l2_limit > 0 {
            let path = self.l2_path(key);
            let _ = std::fs::remove_file(&path);
            let mut inner = self.inner.lock();
            if let Some(entry) = inner.l2_index.remove(&path) {
                inner.l2_bytes = inner.l2_bytes.saturating_sub(entry.size);
                if entry.reused {
                    inner.l2_hot_bytes = inner.l2_hot_bytes.saturating_sub(entry.size);
                }
            }
        }
    }

    pub fn stats(&self) -> BTreeMap<String, serde_json::Value> {
        let inner = self.inner.lock();
        let total = inner.hits + inner.misses + inner.l2_hits;
        BTreeMap::from([
            ("memory_items".to_string(), json!(inner.items.len())),
            ("memory_bytes".to_string(), json!(inner.bytes)),
            ("hits".to_string(), json!(inner.hits)),
            ("misses".to_string(), json!(inner.misses)),
            ("l2_hits".to_string(), json!(inner.l2_hits)),
            ("l2_writes".to_string(), json!(inner.l2_writes)),
            ("l2_errors".to_string(), json!(inner.l2_errors)),
            (
                "l2_admission_frozen".to_string(),
                json!(inner.l2_admission_frozen),
            ),
            (
                "l2_admission_oversize".to_string(),
                json!(inner.l2_admission_oversize),
            ),
            ("l2_evictions".to_string(), json!(inner.l2_evictions)),
            ("l2_loaded_items".to_string(), json!(inner.l2_loaded_items)),
            ("l2_loaded_bytes".to_string(), json!(inner.l2_loaded_bytes)),
            ("l2_bytes".to_string(), json!(inner.l2_bytes)),
            ("l2_items".to_string(), json!(inner.l2_index.len())),
            ("l2_hot_bytes".to_string(), json!(inner.l2_hot_bytes)),
            (
                "l2_hot_items".to_string(),
                json!(inner.l2_index.values().filter(|entry| entry.reused).count()),
            ),
            (
                "l2_stream_frozen".to_string(),
                json!(u8::from(inner.l2_freeze_stream)),
            ),
            (
                "direct_copy_bytes".to_string(),
                json!(self.direct_copy_bytes.load(Ordering::Relaxed)),
            ),
            (
                "l2_read_bytes".to_string(),
                json!(self.l2_read_bytes.load(Ordering::Relaxed)),
            ),
            (
                "hit_ratio".to_string(),
                json!(if total == 0 {
                    0.0
                } else {
                    (inner.hits + inner.l2_hits) as f64 / total as f64
                }),
            ),
        ])
    }

    fn l2_path(&self, key: &str) -> PathBuf {
        let digest = sha256_hex(key.as_bytes());
        self.root.join(&digest[..2]).join(format!("{digest}.blk"))
    }

    fn put_l2(&self, key: &str, data: &[u8]) -> Result<()> {
        self.load_l2_index()?;
        let path = self.l2_path(key);
        {
            let mut inner = self.inner.lock();
            // Once the recent hit ratio shows real reuse, a new streaming
            // admission would start evicting the reusable sample before the
            // rest of the second pass can read it. Freeze cold admissions at
            // that point; existing keys can always be refreshed. A different
            // workload with a low hit ratio eventually unfreezes the cache.
            if !inner.l2_index.contains_key(&path) {
                if data.len() as u64 > self.l2_limit {
                    inner.l2_admission_oversize += 1;
                    return Ok(());
                }
                if inner.l2_freeze_stream
                    && inner.l2_bytes.saturating_add(data.len() as u64) > self.l2_limit
                {
                    inner.l2_admission_frozen += 1;
                    return Ok(());
                }
            }
        }
        write_cache_block(&path, data)?;
        let mut inner = self.inner.lock();
        inner.l2_clock = inner.l2_clock.saturating_add(1);
        let touched = inner.l2_clock;
        let reused = inner.l2_index.get(&path).is_some_and(|entry| entry.reused);
        let old = inner.l2_index.insert(
            path,
            L2Entry {
                size: data.len() as u64,
                touched,
                reused,
            },
        );
        if let Some(old) = old {
            inner.l2_bytes = inner.l2_bytes.saturating_sub(old.size);
            if old.reused {
                inner.l2_hot_bytes = inner.l2_hot_bytes.saturating_sub(old.size);
            }
        }
        inner.l2_bytes = inner.l2_bytes.saturating_add(data.len() as u64);
        if reused {
            inner.l2_hot_bytes = inner.l2_hot_bytes.saturating_add(data.len() as u64);
        }
        inner.l2_writes += 1;
        drop(inner);
        self.prune_l2()
    }

    fn load_l2_index(&self) -> Result<()> {
        let mut inner = self.inner.lock();
        if inner.l2_loaded {
            return Ok(());
        }

        let mut files = Vec::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .min_depth(1)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            if !entry.file_type().is_file()
                || !entry
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.strip_suffix(".blk").is_some_and(|digest| {
                            digest.len() == 64
                                && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                    })
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let modified_secs = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs())
                .unwrap_or(0);
            files.push((modified_secs, entry.path().to_path_buf(), metadata.len()));
        }

        files.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        let mut index = BTreeMap::new();
        let mut total = 0u64;
        let mut clock = 0u64;
        for (_, path, size) in files {
            clock = clock.saturating_add(1);
            total = total.saturating_add(size);
            index.insert(
                path,
                L2Entry {
                    size,
                    touched: clock,
                    reused: false,
                },
            );
        }

        inner.l2_loaded = true;
        inner.l2_loaded_items = index.len() as u64;
        inner.l2_loaded_bytes = total;
        inner.l2_index = index;
        inner.l2_bytes = total;
        inner.l2_hot_bytes = 0;
        inner.l2_clock = clock.max(inner.l2_clock);
        Ok(())
    }

    fn prune_l2(&self) -> Result<()> {
        self.load_l2_index()?;
        let mut inner = self.inner.lock();
        if inner.l2_bytes <= self.l2_limit {
            return Ok(());
        }
        // Prune with hysteresis so a scan does not sort and evict on every
        // admission. Cold entries are evicted in a deterministic hash order
        // (a stable priority sample within each reuse epoch), while
        // entries that have already been reused are kept until no cold entry
        // remains. This lets a second sequential pass hit a useful sample of
        // the first pass instead of finding only an already-evicted LRU tail.
        let target = self.l2_limit.saturating_sub(self.l2_limit / 16).max(1);
        let seed = inner.l2_evict_seed;
        let mut cold = Vec::new();
        let mut hot = Vec::new();
        for (path, entry) in &inner.l2_index {
            if entry.reused {
                hot.push((entry.touched, path.clone()));
            } else {
                cold.push((eviction_score(path, seed), path.clone()));
            }
        }
        cold.sort_unstable();
        hot.sort_unstable();
        for (_, path) in cold.into_iter().chain(hot) {
            if inner.l2_bytes <= target {
                break;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    if let Some(entry) = inner.l2_index.remove(&path) {
                        inner.l2_evictions += 1;
                        inner.l2_bytes = inner.l2_bytes.saturating_sub(entry.size);
                        if entry.reused {
                            inner.l2_hot_bytes = inner.l2_hot_bytes.saturating_sub(entry.size);
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(entry) = inner.l2_index.remove(&path) {
                        inner.l2_evictions += 1;
                        inner.l2_bytes = inner.l2_bytes.saturating_sub(entry.size);
                        if entry.reused {
                            inner.l2_hot_bytes = inner.l2_hot_bytes.saturating_sub(entry.size);
                        }
                    }
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }
}

/// L2 is a disposable, content-verified acceleration tier. Atomic publication
/// prevents readers seeing a partial write; unlike metadata, cache blocks need
/// not survive power loss. Never fsync a cache block or its parent directory.
fn write_cache_block(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let temporary = path.with_extension(format!("cache-tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(data)?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn configured_l2_limit(default_limit: u64) -> u64 {
    resolve_l2_limit(
        default_limit,
        std::env::var_os("ARGOSFS_DISABLE_L2_CACHE"),
        std::env::var_os("ARGOSFS_L2_CACHE_BYTES"),
    )
}

fn resolve_l2_limit(
    default_limit: u64,
    disable_override: Option<std::ffi::OsString>,
    size_override: Option<std::ffi::OsString>,
) -> u64 {
    if disable_override.is_some() {
        return 0;
    }
    size_override
        .and_then(|value| value.to_string_lossy().parse::<u64>().ok())
        .unwrap_or(default_limit)
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
