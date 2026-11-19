use super::*;
use crate::util::content_hash_matches;
use tempfile::tempdir;

#[test]
fn memory_cache_evicts_replaces_invalidates_and_reports_stats() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 6, 0);
    assert_eq!(cache.stats()["hit_ratio"], 0.0);
    cache.put("a:1", b"aaa").unwrap();
    cache.put("a:2", b"bbb").unwrap();
    assert_eq!(cache.get("a:1", None).unwrap(), b"aaa");
    cache.put("a:3", b"cccc").unwrap();
    assert!(cache.get("a:2", None).is_none());
    assert_eq!(cache.get("a:3", None).unwrap(), b"cccc");

    cache.put("a:3", b"cc").unwrap();
    assert_eq!(cache.get("a:3", None).unwrap(), b"cc");
    cache.put("b:1", b"zz").unwrap();
    cache.invalidate_prefix("a:");
    assert!(cache.get("a:1", None).is_none());
    assert!(cache.get("a:3", None).is_none());
    assert_eq!(cache.get("b:1", None).unwrap(), b"zz");
    cache.remove("b:1");
    cache.remove("missing");
    assert!(cache.get("b:1", None).is_none());

    let stats = cache.stats();
    assert!(stats["hits"].as_u64().unwrap() >= 4);
    assert!(stats["misses"].as_u64().unwrap() >= 4);
    assert!(stats["hit_ratio"].as_f64().unwrap() > 0.0);
}

#[test]
fn checksum_mismatch_removes_memory_and_l2_entries() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 1024, 1024);
    cache.put("key", b"value").unwrap();
    let l2_path = cache.l2_path("key");
    assert!(l2_path.exists());
    assert!(cache.get("key", Some(&sha256_hex(b"wrong"))).is_none());
    assert!(!l2_path.exists());
    let stats = cache.stats();
    assert_eq!(stats["memory_items"], 0);
    assert_eq!(stats["l2_items"], 0);
}

#[test]
fn l2_index_loads_existing_files_and_prunes_oldest_entries() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 0, 5);
    let first = cache.l2_path("first");
    let second = cache.l2_path("second");
    atomic_write(&first, b"1111").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    atomic_write(&second, b"2222").unwrap();
    cache.load_l2_index().unwrap();
    assert_eq!(cache.stats()["l2_items"], 2);
    cache.prune_l2().unwrap();
    let remaining = [first.exists(), second.exists()]
        .into_iter()
        .filter(|present| *present)
        .count();
    assert_eq!(remaining, 1);
    assert!(cache.stats()["l2_bytes"].as_u64().unwrap() <= 5);
    cache.prune_l2().unwrap();
}

#[test]
fn l2_hits_promote_to_memory_and_remove_cleans_index() {
    let dir = tempdir().unwrap();
    let writer = BlockCache::new(dir.path(), 0, 1024);
    writer.put("key", b"payload").unwrap();
    let cache = BlockCache::new(dir.path(), 1024, 1024);
    assert_eq!(cache.get("key", None).unwrap(), b"payload");
    assert_eq!(cache.get("key", None).unwrap(), b"payload");
    let stats = cache.stats();
    assert_eq!(stats["l2_hits"], 1);
    assert_eq!(stats["hits"], 1);
    assert_eq!(stats["memory_items"], 1);
    cache.remove("key");
    assert!(cache.get("key", None).is_none());
}

#[test]
fn l2_limit_resolution_honors_disable_override_and_invalid_values() {
    assert_eq!(resolve_l2_limit(123, None, None), 123);
    assert_eq!(resolve_l2_limit(123, None, Some("456".into())), 456);
    assert_eq!(resolve_l2_limit(123, None, Some("invalid".into())), 123);
    assert_eq!(
        resolve_l2_limit(123, Some("1".into()), Some("456".into())),
        0
    );
}

#[test]
fn range_hit_returns_only_requested_bytes_and_obeys_exact_digest_and_length() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 4 * 1024 * 1024, 0);
    let block = (0..1024 * 1024)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let digest = crate::util::content_hash_hex(&block);
    cache.put("stripe:0", &block).unwrap();
    for offset in (0..block.len()).step_by(8192) {
        let end = (offset + 4096).min(block.len());
        assert_eq!(
            cache
                .get_range("stripe:0", Some(&digest), block.len(), offset, end)
                .unwrap(),
            block[offset..end]
        );
    }
    assert_eq!(cache.stats()["hits"], 128);
    assert_eq!(cache.stats()["memory_bytes"], block.len());
    assert!(cache
        .get_range(
            "stripe:0",
            Some(&digest),
            block.len(),
            block.len() - 1,
            block.len() + 1
        )
        .is_none());
    assert!(cache
        .get_range("stripe:0", Some(&digest), block.len(), 400, 399)
        .is_none());
    assert!(cache
        .get_range("stripe:0", Some(&digest), block.len() + 1, 0, 1)
        .is_none());
    assert_eq!(cache.stats()["memory_items"], 0); // stale metadata size is rejected
}

#[test]
fn range_hit_legacy_sha256_and_digest_mismatch_are_checked() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 4096, 0);
    cache.put("key", b"trusted-data").unwrap();
    let sha = sha256_hex(b"trusted-data");
    assert_eq!(
        cache.get_range("key", Some(&sha), 12, 1, 5).unwrap(),
        b"rust"
    );
    assert_eq!(
        cache.get_range("key", Some(&sha), 12, 1, 5).unwrap(),
        b"rust"
    );
    assert!(cache
        .get_range("key", Some(&sha256_hex(b"wrong")), 12, 0, 1)
        .is_none());
    assert_eq!(cache.stats()["memory_items"], 0);
}

#[test]
fn l2_range_hit_promotes_without_trusting_damaged_l2_content() {
    let dir = tempdir().unwrap();
    let writer = BlockCache::new(dir.path(), 0, 128 * 1024);
    let block = vec![0x39u8; 32 * 1024];
    let hash = crate::util::content_hash_hex(&block);
    writer.put("piece", &block).unwrap();
    let cache = BlockCache::new(dir.path(), 64 * 1024, 128 * 1024);
    assert_eq!(
        cache
            .get_range("piece", Some(&hash), block.len(), 100, 4096)
            .unwrap(),
        &block[100..4096]
    );
    assert_eq!(cache.stats()["l2_hits"], 1);
    assert_eq!(
        cache
            .get_range("piece", Some(&hash), block.len(), 8192, 16384)
            .unwrap(),
        &block[8192..16384]
    );
    assert_eq!(cache.stats()["hits"], 1);
    cache.remove("piece");
    writer.put("piece", &block).unwrap();
    let path = cache.l2_path("piece");
    let mut disk = std::fs::read(&path).unwrap();
    disk[0] ^= 0x80;
    std::fs::write(&path, disk).unwrap();
    assert!(cache
        .get_range("piece", Some(&hash), block.len(), 100, 4096)
        .is_none());
    assert!(!path.exists());
}

#[test]
fn lru_range_hits_keep_recent_entries_and_support_wraparound() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 9, 0);
    for key in ["a", "b", "c"] {
        cache.put(key, b"123").unwrap();
    }
    assert_eq!(cache.get_range("a", None, 3, 1, 2).unwrap(), b"2");
    cache.put("d", b"123").unwrap();
    assert!(cache.get("b", None).is_none());
    assert_eq!(cache.get_range("a", None, 3, 0, 3).unwrap(), b"123");
    assert_eq!(cache.get_range("c", None, 3, 0, 3).unwrap(), b"123");
    assert_eq!(cache.get_range("d", None, 3, 0, 3).unwrap(), b"123");
    {
        cache.inner.lock().lru_clock = u64::MAX;
    }
    assert_eq!(cache.get_range("a", None, 3, 0, 3).unwrap(), b"123");
    cache.put("e", b"123").unwrap();
    assert!(cache.get("c", None).is_none());
    assert_eq!(cache.get("d", None).unwrap(), b"123");
    assert_eq!(cache.get("a", None).unwrap(), b"123");
    assert_eq!(cache.get("e", None).unwrap(), b"123");
    let guard = cache.inner.lock();
    assert_eq!(guard.items.len(), guard.order.len());
}

#[test]
fn concurrent_range_read_replace_and_invalidate_are_safe_and_validated() {
    use std::sync::Arc;
    let dir = tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(dir.path(), 64 * 1024, 0));
    let a = vec![11u8; 16 * 1024];
    let b = vec![22u8; 16 * 1024];
    let a_sha = crate::util::content_hash_hex(&a);
    let b_sha = crate::util::content_hash_hex(&b);
    cache.put("key", &a).unwrap();
    let writer = Arc::clone(&cache);
    let thread = std::thread::spawn(move || {
        for i in 0..200 {
            if i % 2 == 0 {
                writer.put("key", &a).unwrap();
            } else {
                writer.put("key", &b).unwrap();
            }
        }
    });
    for _ in 0..200 {
        if let Some(slice) = cache.get_range("key", Some(&a_sha), 16 * 1024, 1024, 6144) {
            assert_eq!(slice, vec![11u8; 5120]);
        }
        if let Some(slice) = cache.get_range("key", Some(&b_sha), 16 * 1024, 1024, 6144) {
            assert_eq!(slice, vec![22u8; 5120]);
        }
    }
    thread.join().unwrap();
    cache.remove("key");
    assert!(cache.get("key", None).is_none());
}

/// Manual, opt-in microbenchmark of the original whole-block clone + hash
/// versus the new validated subrange cache API. Never use timings as a CI
/// assertion: these are host/CPU/compiler dependent, not Borg throughput.
#[test]
#[ignore = "run with cargo test cache_subrange_microbench -- --ignored --nocapture"]
fn cache_subrange_microbench() {
    use std::hint::black_box;
    use std::time::Instant;
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 2 * 1024 * 1024, 0);
    let block = (0..1024 * 1024)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let hash = crate::util::content_hash_hex(&block);
    let mut oldest_first = std::collections::VecDeque::from(["stripe".to_string()]);
    cache.put("stripe", &block).unwrap();
    const COUNT: usize = 1024;
    const READ: usize = 128 * 1024;

    let start = Instant::now();
    for i in 0..COUNT {
        let offset = (i % 8) * READ;
        let whole = black_box(block.clone());
        assert!(content_hash_matches(&whole, &hash));
        oldest_first.retain(|entry| entry != "stripe");
        oldest_first.push_back("stripe".to_string());
        black_box(&whole[offset..offset + READ]);
    }
    let original = start.elapsed();

    let start = Instant::now();
    for i in 0..COUNT {
        let offset = (i % 8) * READ;
        let range = cache
            .get_range("stripe", Some(&hash), block.len(), offset, offset + READ)
            .unwrap();
        black_box(range);
    }
    let optimized = start.elapsed();
    eprintln!(
        "whole_block_clone_and_hash={original:?} cached_range_copy={optimized:?} ratio={:.2}x",
        original.as_secs_f64() / optimized.as_secs_f64()
    );
}

#[test]
fn in_flight_l2_generation_changes_are_scoped_to_key_and_prefix() {
    let mut inner = CacheInner::default();
    let first = inner.begin_l2_read("file-a:block-1");
    let same_key = inner.begin_l2_read("file-a:block-1");
    let other = inner.begin_l2_read("file-b:block-2");
    inner.note_key_mutation("file-b:block-2");
    assert!(inner.finish_l2_read("file-a:block-1", first));
    inner.note_prefix_mutation("file-a:");
    assert!(!inner.finish_l2_read("file-a:block-1", same_key));
    assert!(!inner.finish_l2_read("file-b:block-2", other));
    assert!(
        inner.inflight_l2.is_empty(),
        "completed L2 reads must free their generation tracking"
    );
    let current = inner.begin_l2_read("file-a:block-1");
    inner.note_key_mutation("unrelated");
    assert!(inner.finish_l2_read("file-a:block-1", current));
    assert!(inner.inflight_l2.is_empty());
}

#[test]
fn concurrent_unrelated_admissions_preserve_verified_l2_hits() {
    use std::sync::Arc;
    let dir = tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(dir.path(), 0, 8 * 1024 * 1024));
    let stable = vec![0x4du8; 128 * 1024];
    let stable_hash = crate::util::content_hash_hex(&stable);
    cache.put("stable:1", &stable).unwrap();
    let other = Arc::clone(&cache);
    let worker = std::thread::spawn(move || {
        for i in 0..60 {
            other.put("other:2", &vec![i as u8; 32 * 1024]).unwrap();
        }
    });
    for _ in 0..60 {
        let slice = cache
            .get_range(
                "stable:1",
                Some(&stable_hash),
                stable.len(),
                16 * 1024,
                20 * 1024,
            )
            .expect("an unrelated put must not cancel an L2 read of an immutable key");
        assert_eq!(slice, stable[16 * 1024..20 * 1024]);
    }
    worker.join().unwrap();
    assert!(cache.stats()["l2_hits"].as_u64().unwrap() >= 60);
    assert_eq!(cache.stats()["l2_errors"], 0);
}
