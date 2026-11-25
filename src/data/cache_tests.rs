use super::*;
use crate::util::atomic_write;
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
fn l2_index_loads_existing_files_and_prunes_to_limit() {
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
fn l2_scan_keeps_a_reusable_sample_and_protects_reused_entries() {
    let dir = tempdir().unwrap();
    let block_size = 1024usize;
    let limit = 512 * block_size as u64;
    let cache = BlockCache::new(dir.path(), 0, limit);
    let blocks = (0..2_048)
        .map(|index| vec![(index % 251) as u8; block_size])
        .collect::<Vec<_>>();

    // Cold sequential pass: the cache cannot hold the whole stream.
    for (index, block) in blocks.iter().enumerate() {
        cache.put(&format!("stream:{index}"), block).unwrap();
    }
    assert!(cache.stats()["l2_bytes"].as_u64().unwrap() <= limit);

    // Warm pass in the same order: the retained random sample must survive
    // until it is read, instead of being only an LRU tail evicted up front.
    let mut hits = 0;
    let mut hit_key = None;
    for (index, block) in blocks.iter().enumerate() {
        let key = format!("stream:{index}");
        if cache.get(&key, None).is_some() {
            hits += 1;
            hit_key.get_or_insert(key);
        } else {
            cache.put(&key, block).unwrap();
        }
    }
    assert!(
        hits >= 100,
        "warm sequential pass should hit a reusable sample, got {hits}"
    );

    // A reused entry is protected while colder entries remain.
    let protected = hit_key.expect("warm scan hit at least one entry");
    assert!(cache.get(&protected, None).is_some());
    for index in 0..8 {
        cache
            .put(&format!("new:{index}"), &vec![0xee; block_size])
            .unwrap();
    }
    assert!(cache.get(&protected, None).is_some());
}

#[test]
fn l2_eviction_priorities_are_stable_across_prunes_and_cache_roots() {
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    let a = BlockCache::new(first.path(), 0, 64 * 1024);
    let b = BlockCache::new(second.path(), 0, 64 * 1024);
    for index in 0..256 {
        let key = format!("stream:{index}");
        a.put(&key, &[0; 1024]).unwrap();
        b.put(&key, &[0; 1024]).unwrap();
    }
    let a_inner = a.inner.lock();
    let b_inner = b.inner.lock();
    assert_eq!(a_inner.l2_evict_seed, 0);
    assert_eq!(b_inner.l2_evict_seed, 0);
    let names = |inner: &CacheInner| {
        inner
            .l2_index
            .keys()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&a_inner), names(&b_inner));
}

#[test]
fn l2_frozen_cache_preserves_sample_and_adapts_to_a_new_working_set() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 0, 10);
    cache.put("hot-a", b"aaaa").unwrap();
    cache.put("hot-b", b"bbbb").unwrap();
    for _ in 0..32 {
        assert!(cache.get("hot-a", None).is_some());
        assert!(cache.get("hot-b", None).is_some());
    }
    assert_eq!(cache.stats()["l2_stream_frozen"], 1);
    assert_eq!(cache.stats()["l2_hot_bytes"], 8);
    // There is spare space, but not enough to admit this block without
    // pruning the protected sample.
    cache.put("cold", b"cccc").unwrap();
    assert_eq!(cache.stats()["l2_bytes"], 8);
    assert_eq!(cache.stats()["l2_writes"], 2);
    for n in 0..(8 * 64) {
        assert!(cache.get(&format!("miss:{n}"), None).is_none());
    }
    assert_eq!(cache.stats()["l2_stream_frozen"], 0);
    assert_eq!(cache.stats()["l2_hot_bytes"], 0);
    // One oversized block must not evict the entire existing cache.
    cache.put("oversized", &[0; 11]).unwrap();
    assert_eq!(cache.stats()["l2_bytes"], 8);
    for n in 0..32 {
        cache.put(&format!("next:{n}"), b"next").unwrap();
    }
    assert!(cache.stats()["l2_bytes"].as_u64().unwrap() <= 10);
    assert!((0..32).any(|n| cache.get(&format!("next:{n}"), None).is_some()));
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

#[test]
fn direct_copy_checks_checksum_bounds_and_l1_eviction() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 64, 0);
    let payload = b"validated-block-content";
    let hash = crate::util::content_hash_hex(payload);
    cache.put("stripe", payload).unwrap();
    let mut destination = [0xa5u8; 7];
    assert!(cache.copy_range("stripe", Some(&hash), payload.len(), 10, &mut destination));
    assert_eq!(&destination, &payload[10..17]);
    destination.fill(0xa5);
    assert!(!cache.copy_range(
        "stripe",
        Some(&hash),
        payload.len(),
        payload.len() - 2,
        &mut destination
    ));
    assert_eq!(
        destination, [0xa5; 7],
        "failed bounds validation must not mutate the caller's buffer"
    );
    assert!(!cache.copy_range(
        "stripe",
        Some(&hash),
        payload.len(),
        usize::MAX,
        &mut destination
    ));
    assert_eq!(destination, [0xa5; 7]);
    assert!(!cache.copy_range(
        "stripe",
        Some(&hash),
        payload.len() + 1,
        0,
        &mut destination
    ));
    assert_eq!(destination, [0xa5; 7]);
    cache.put("stripe", payload).unwrap();
    assert!(!cache.copy_range(
        "stripe",
        Some(&sha256_hex(b"tampered")),
        payload.len(),
        0,
        &mut destination
    ));
    assert_eq!(destination, [0xa5; 7]);
    assert_eq!(cache.stats()["memory_items"], 0);
    assert!(!cache.copy_range("stripe", Some(&hash), payload.len(), 0, &mut destination));
    cache.put("stripe", payload).unwrap();
    assert!(cache.copy_range("stripe", Some(&hash), payload.len(), 0, &mut destination));
    assert_eq!(&destination, &payload[..7]);
    cache.remove("stripe");
    assert!(!cache.copy_range("stripe", Some(&hash), payload.len(), 0, &mut destination));
    assert_eq!(cache.stats()["memory_bytes"], 0);
}

#[test]
fn direct_copy_validates_l2_and_keeps_cache_within_limit() {
    let dir = tempdir().unwrap();
    let payload = vec![0x73; 24 * 1024];
    let hash = crate::util::content_hash_hex(&payload);
    let writer = BlockCache::new(dir.path(), 0, 128 * 1024);
    writer.put("stripe", &payload).unwrap();
    let cache = BlockCache::new(dir.path(), 32 * 1024, 128 * 1024);
    let mut destination = vec![0xabu8; 4096];
    assert!(cache.copy_range("stripe", Some(&hash), payload.len(), 777, &mut destination));
    assert_eq!(destination, payload[777..777 + 4096]);
    assert_eq!(cache.stats()["l2_hits"], 1);
    assert!(cache.copy_range(
        "stripe",
        Some(&hash),
        payload.len(),
        12_000,
        &mut destination
    ));
    assert_eq!(cache.stats()["hits"], 1);
    cache.put("second", &payload).unwrap();
    assert!(cache.stats()["memory_bytes"].as_u64().unwrap() <= 32 * 1024);
    assert!(cache.copy_range("stripe", Some(&hash), payload.len(), 777, &mut destination));
    assert_eq!(destination, payload[777..777 + 4096]);
    assert_eq!(cache.stats()["l2_errors"], 0);
}

#[test]
fn direct_copy_concurrent_cache_eviction_does_not_leak_or_return_wrong_bytes() {
    use std::sync::Arc;
    let dir = tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(dir.path(), 8192, 0));
    let payload = vec![0x6bu8; 4096];
    let hash = crate::util::content_hash_hex(&payload);
    cache.put("stripe", &payload).unwrap();
    let writer_cache = Arc::clone(&cache);
    let writer_payload = payload.clone();
    let writer = std::thread::spawn(move || {
        for _ in 0..500 {
            writer_cache.remove("stripe");
            writer_cache.put("stripe", &writer_payload).unwrap();
        }
    });
    for _ in 0..500 {
        let mut out = vec![0xcc; 1024];
        if cache.copy_range("stripe", Some(&hash), payload.len(), 3072, &mut out) {
            assert_eq!(out, payload[3072..]);
        } else {
            assert_eq!(out, vec![0xcc; 1024]);
        }
    }
    writer.join().unwrap();
    assert!(cache.stats()["memory_bytes"].as_u64().unwrap() <= 8192);
}

#[test]
#[ignore = "run with cargo test cache_copy_range_microbench -- --ignored --nocapture"]
fn cache_copy_range_microbench() {
    use std::hint::black_box;
    use std::time::Instant;
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 2 * 1024 * 1024, 0);
    let block = (0..1024 * 1024)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    let hash = crate::util::content_hash_hex(&block);
    cache.put("stripe", &block).unwrap();
    const COUNT: usize = 4096;
    const READ: usize = 128 * 1024;
    let mut dst = vec![0u8; READ];
    let start = Instant::now();
    for i in 0..COUNT {
        let off = (i % 8) * READ;
        dst.copy_from_slice(
            &cache
                .get_range("stripe", Some(&hash), block.len(), off, off + READ)
                .unwrap(),
        );
        black_box(&dst);
    }
    let before = start.elapsed();
    let start = Instant::now();
    for i in 0..COUNT {
        let off = (i % 8) * READ;
        assert!(cache.copy_range("stripe", Some(&hash), block.len(), off, &mut dst));
        black_box(&dst);
    }
    let after = start.elapsed();
    eprintln!(
        "cached_range_then_copy={before:?} direct_cached_range_copy={after:?} speedup={:.2}x",
        before.as_secs_f64() / after.as_secs_f64()
    );
}

#[test]
fn direct_copy_byte_counters_cover_hits_only_and_l2_promotion() {
    let dir = tempdir().unwrap();
    let block = vec![0x6a; 8192];
    let hash = crate::util::content_hash_hex(&block);
    let writer = BlockCache::new(dir.path(), 0, 128 * 1024);
    writer.put("stripe", &block).unwrap();
    let cache = BlockCache::new(dir.path(), 128 * 1024, 128 * 1024);
    let mut buffer = vec![0; 512];
    assert!(cache.copy_range("stripe", Some(&hash), block.len(), 17, &mut buffer));
    assert_eq!(buffer, block[17..529]);
    assert!(cache.copy_range("stripe", Some(&hash), block.len(), 128, &mut buffer));
    assert_eq!(buffer, block[128..640]);
    assert!(!cache.copy_range("stripe", Some(&hash), block.len(), 8000, &mut buffer));
    assert_eq!(cache.stats()["direct_copy_bytes"], 1024);
    assert_eq!(cache.stats()["l2_read_bytes"], 8192);
    assert_eq!(cache.stats()["l2_hits"], 1);
    assert_eq!(cache.stats()["hits"], 1);
    assert_eq!(cache.stats()["memory_bytes"], 8192);
    cache.remove("stripe");
    assert!(!cache.copy_range("stripe", Some(&hash), block.len(), 0, &mut buffer));
    assert_eq!(cache.stats()["direct_copy_bytes"], 1024);
}

#[test]
fn concurrent_read_cache_copy_is_bounded_and_byte_accounted() {
    use std::sync::Arc;
    let dir = tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(dir.path(), 64 * 1024, 0));
    let block = (0..16384)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let digest = crate::util::content_hash_hex(&block);
    cache.put("immutable", &block).unwrap();
    std::thread::scope(|scope| {
        for thread_id in 0..8 {
            let cache = Arc::clone(&cache);
            let block = &block;
            let digest = &digest;
            scope.spawn(move || {
                let mut output = vec![0; 128];
                for i in 0..128 {
                    let offset = (i * 97 + thread_id * 17) % (block.len() - output.len());
                    assert!(cache.copy_range(
                        "immutable",
                        Some(digest),
                        block.len(),
                        offset,
                        &mut output
                    ));
                    assert_eq!(&output, &block[offset..offset + 128]);
                }
            });
        }
    });
    assert_eq!(cache.stats()["direct_copy_bytes"], 8 * 128 * 128);
    assert_eq!(cache.stats()["memory_bytes"], block.len());
}

#[test]
fn initial_cold_scan_keeps_stable_sample_across_lookup_windows_and_reopen() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 0, 64 * 1024);
    for n in 0..1024 {
        let key = format!("cold:{n}");
        assert!(cache.get(&key, None).is_none());
        cache.put(&key, &[0x71; 1024]).unwrap();
    }
    assert_eq!(cache.inner.lock().l2_evict_seed, 0);
    let resident = cache.stats()["l2_items"].as_u64().unwrap();
    assert!(resident >= 48);
    assert!(cache.stats()["l2_evictions"].as_u64().unwrap() > 0);
    drop(cache);
    // An unrelated file from an interrupted write is not a cache block.
    let foreign = dir.path().join("unrelated.tmp");
    std::fs::write(&foreign, [0; 8192]).unwrap();
    let cache = BlockCache::new(dir.path(), 0, 64 * 1024);
    cache.load_l2_index().unwrap();
    assert_eq!(cache.stats()["l2_loaded_items"], resident);
    assert_eq!(cache.stats()["l2_loaded_bytes"], resident * 1024);
    let mut hits = 0;
    for n in 0..1024 {
        let key = format!("cold:{n}");
        if cache.get(&key, None).is_some() {
            hits += 1;
        } else {
            cache.put(&key, &[0x71; 1024]).unwrap();
        }
    }
    assert!(
        hits >= 32,
        "retained sample should survive remount, hits={hits}"
    );
    assert!(foreign.exists());
    cache.put("oversize", &[0; 65537]).unwrap();
    assert_eq!(cache.stats()["l2_admission_oversize"], 1);
}

#[test]
fn cache_admin_lists_clears_and_gcs_pool_directories() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let first = uuid::Uuid::new_v4().to_string();
    let second = uuid::Uuid::new_v4().to_string();
    for (pool, bytes) in [(&first, 3usize), (&second, 5usize)] {
        let pool_dir = base.join(pool).join("ab");
        std::fs::create_dir_all(&pool_dir).unwrap();
        std::fs::write(pool_dir.join("block.blk"), vec![0u8; bytes]).unwrap();
    }
    // A non-UUID directory must be ignored, not summarized or removed.
    std::fs::create_dir_all(base.join("not-a-pool")).unwrap();
    std::fs::write(base.join("not-a-pool").join("keep.blk"), b"x").unwrap();

    let listed = list_cache_dirs(base).unwrap();
    assert_eq!(listed.len(), 2);
    assert!(list_cache_dirs(&base.join("missing")).unwrap().is_empty());
    let mut ids: Vec<_> = listed.iter().map(|info| info.pool_uuid.clone()).collect();
    ids.sort();
    let mut expected = vec![first.clone(), second.clone()];
    expected.sort();
    assert_eq!(ids, expected);
    let first_info = listed.iter().find(|info| info.pool_uuid == first).unwrap();
    assert_eq!(first_info.files, 1);
    assert_eq!(first_info.bytes, 3);

    let removed = clear_cache_dir(base, &first).unwrap().unwrap();
    assert_eq!(removed.files, 1);
    assert_eq!(removed.bytes, 3);
    assert!(!base.join(&first).exists());
    assert!(clear_cache_dir(base, &first).unwrap().is_none());
    assert!(clear_cache_dir(base, "not-a-uuid").is_err());

    assert!(gc_cache_dirs(base, &BTreeSet::new()).is_err());
    assert!(gc_cache_dirs(base, &BTreeSet::from(["invalid".to_string()])).is_err());
    assert!(base.join(&second).exists());
    let kept_uppercase = BTreeSet::from([second.to_uppercase()]);
    assert!(gc_cache_dirs(base, &kept_uppercase).unwrap().is_empty());
    let mut keep = BTreeSet::new();
    keep.insert(first);
    let removed = gc_cache_dirs(base, &keep).unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].pool_uuid, second);
    assert!(!base.join(&second).exists());
    assert!(base.join("not-a-pool").exists());
}

#[test]
fn clear_drops_memory_and_l2_entries_and_recreates_the_root() {
    let dir = tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 1024, 1024);
    cache.put("a:1", b"aaaa").unwrap();
    cache.put("a:2", b"bbbb").unwrap();
    assert_eq!(cache.get("a:1", None).unwrap(), b"aaaa");
    assert_eq!(cache.stats()["l2_items"], 2);
    assert!(cache.disk_footprint().unwrap().0 >= 2);

    cache.clear().unwrap();
    let stats = cache.stats();
    assert_eq!(stats["memory_items"], 0);
    assert_eq!(stats["memory_bytes"], 0);
    assert_eq!(stats["l2_items"], 0);
    assert_eq!(stats["l2_bytes"], 0);
    assert_eq!(stats["l2_loaded_items"], 0);
    assert_eq!(cache.disk_footprint().unwrap(), (0, 0));
    assert!(dir.path().is_dir());
    assert!(cache.get("a:1", None).is_none());
}

#[test]
fn volatile_l2_publication_replaces_complete_blocks_and_ignores_interrupted_writes() {
    let dir = tempfile::tempdir().unwrap();
    let cache = BlockCache::new(dir.path(), 0, 1024);
    let path = cache.l2_path("entry");
    write_cache_block(&path, b"old").unwrap();
    write_cache_block(&path, b"new content").unwrap();
    std::fs::write(path.with_extension("cache-tmp-interrupted"), b"partial").unwrap();
    cache.load_l2_index().unwrap();
    assert_eq!(cache.stats()["l2_items"], 1);
    assert_eq!(
        cache
            .get("entry", Some(&sha256_hex(b"new content")))
            .unwrap(),
        b"new content"
    );
    std::fs::write(&path, b"truncated").unwrap();
    assert!(cache
        .get("entry", Some(&sha256_hex(b"new content")))
        .is_none());
    assert!(!path.exists());
}

#[test]
fn async_l2_drains_on_shutdown_and_removed_keys_do_not_reappear() {
    let dir = tempdir().unwrap();
    let mut cache = BlockCache::new_async(dir.path(), 0, 1024 * 1024);
    assert!(cache.writer.is_some());
    cache.put("removed", b"stale").unwrap();
    cache.remove("removed");
    cache.put("retained", b"previous").unwrap();
    cache.put("retained", b"current").unwrap();
    // Join the real worker before asserting the persistent state.
    drop(cache.writer.take());
    assert_eq!(cache.stats()["l2_pending_bytes"], 0);
    assert_eq!(cache.stats()["l2_errors"], 0);
    assert!(cache.inner.lock().inflight_l2.is_empty());
    assert!(!cache.l2_path("removed").exists());
    drop(cache);
    let reopened = BlockCache::new(dir.path(), 0, 1024 * 1024);
    assert_eq!(
        reopened
            .get("retained", Some(&sha256_hex(b"current")))
            .unwrap(),
        b"current"
    );
}

#[test]
fn async_admission_byte_budget_and_invalidation_do_not_block_producer() {
    let dir = tempdir().unwrap();
    let mut cache = BlockCache::new(dir.path(), 0, 128 * 1024 * 1024);
    // Stall consumption deterministically without blocking the cache lock.
    let (sender, receiver) = mpsc::sync_channel(L2_QUEUE_ITEMS);
    cache.writer = Some(L2Writer {
        sender: Some(sender),
        thread: None,
    });
    let data = vec![9; 8 * 1024 * 1024];
    for i in 0..6 {
        cache.put(&format!("key:{i}"), &data).unwrap();
    }
    assert_eq!(cache.stats()["l2_pending_bytes"], L2_QUEUE_BYTES);
    assert_eq!(cache.stats()["l2_queue_dropped"], 2);
    cache.invalidate_prefix("key:0");
    cache.remove("key:1");
    drop(cache.writer.take());
    let mut jobs = receiver.into_iter();
    for _ in 0..2 {
        let job = jobs.next().unwrap();
        assert!(!cache
            .inner
            .lock()
            .l2_generation_current(&job.key, job.generation));
        cache.put_l2(&job.key, &job.data, job.generation).unwrap();
        assert!(!cache.l2_path(&job.key).exists());
    }
    assert_eq!(jobs.count(), 2);
}

#[test]
fn clear_invalidates_queued_l2_admissions() {
    let dir = tempdir().unwrap();
    let mut cache = BlockCache::new(dir.path(), 1024, 1024);
    let (sender, receiver) = mpsc::sync_channel(L2_QUEUE_ITEMS);
    cache.writer = Some(L2Writer {
        sender: Some(sender),
        thread: None,
    });
    cache.put("secret", b"plaintext").unwrap();
    cache.clear().unwrap();
    drop(cache.writer.take());
    let job = receiver.recv().unwrap();
    cache.put_l2(&job.key, &job.data, job.generation).unwrap();
    assert_eq!(cache.disk_footprint().unwrap(), (0, 0));
    assert!(cache.get("secret", None).is_none());
}
