use super::*;
use crate::types::{
    CapacitySource, DiskClass, HealthCounters, HealthDiskReport, IoMode, VolumeConfig,
};
use std::collections::BTreeMap;
use tempfile::tempdir;

fn disk(
    id: &str,
    status: DiskStatus,
    used: u64,
    capacity: u64,
    risk: f64,
    predicted_failure: bool,
    io_errors: u64,
) -> HealthDiskReport {
    HealthDiskReport {
        id: id.to_string(),
        status,
        tier: StorageTier::Warm,
        weight: 1.0,
        used_bytes: used,
        capacity_bytes: capacity,
        used_bytes_source: "metadata".to_string(),
        capacity_source: CapacitySource::UserOverride,
        available_bytes: capacity.saturating_sub(used),
        class: DiskClass::Ssd,
        backing_device: None,
        rotational: Some(false),
        numa_node: None,
        read_latency_ewma_ms: 0.0,
        write_latency_ewma_ms: 0.0,
        observed_read_mib_s: 0.0,
        observed_write_mib_s: 0.0,
        risk_score: risk,
        predicted_failure,
        smart_stale: false,
        reasons: Vec::new(),
        health: HealthCounters {
            io_errors,
            ..HealthCounters::default()
        },
    }
}

fn report(disks: Vec<HealthDiskReport>) -> HealthReport {
    HealthReport {
        volume_uuid: "volume".to_string(),
        txid: 1,
        files: 0,
        directories: 1,
        symlinks: 0,
        specials: 0,
        disks,
        cache: BTreeMap::new(),
        io_mode: IoMode::Buffered,
        encryption_enabled: false,
        metadata_inodes: 1,
        metadata_estimated_rss_bytes: 160 * 1024,
        metadata_inode_limit: None,
    }
}

#[test]
fn autopilot_defaults_are_conservative_and_versioned() {
    let config = AutopilotConfig::default();
    assert_eq!(config.probe_interval_sec, 3600);
    assert_eq!(config.smart_interval_sec, 600);
    assert_eq!(config.scrub_interval_sec, 300);
    assert_eq!(config.rebalance_interval_sec, 600);
    assert_eq!(config.risk_confirmations, 2);
    assert_eq!(config.scrub_files_per_run, 128);
    assert_eq!(config.rebalance_files_per_run, 32);
    assert_eq!(config.max_drains_per_run, 1);
    assert_eq!(config.foreground_latency_target_ms, 75.0);
    assert_eq!(autopilot_state_version(), 3);
}

#[test]
fn due_calculation_handles_disabled_new_due_and_recent_actions() {
    assert!(!autopilot_due(0.0, u64::MAX, 100.0));
    assert!(autopilot_due(0.0, 60, 100.0));
    assert!(autopilot_due(40.0, 60, 100.0));
    assert!(!autopilot_due(41.0, 60, 100.0));
}

#[test]
fn risk_memory_requires_two_healthy_observations_to_clear_streaks() {
    let mut state = AutopilotState::default();
    let mut risky_disk = disk("disk-a", DiskStatus::Online, 0, 100, 0.7, true, 0);
    risky_disk.health.last_smart_refresh_at = 10.0;
    let mut risky = report(vec![risky_disk]);
    update_autopilot_risk_memory(&mut state, &risky, 10.0);
    update_autopilot_risk_memory(&mut state, &risky, 15.0);
    assert_eq!(state.disks["disk-a"].risk_streak, 1);
    risky.disks[0].health.last_smart_refresh_at = 20.0;
    update_autopilot_risk_memory(&mut state, &risky, 20.0);
    let disk_state = state.disks.get("disk-a").unwrap();
    assert_eq!(disk_state.risk_streak, 2);
    assert_eq!(disk_state.healthy_streak, 0);
    assert!(disk_state.last_predicted_failure);

    state.disks.get_mut("disk-a").unwrap().next_action_after = 100.0;
    let mut healthy_disk = disk("disk-a", DiskStatus::Online, 0, 100, 0.0, false, 0);
    healthy_disk.health.last_smart_refresh_at = 30.0;
    let mut healthy = report(vec![healthy_disk]);
    update_autopilot_risk_memory(&mut state, &healthy, 30.0);
    assert_eq!(state.disks["disk-a"].risk_streak, 2);
    update_autopilot_risk_memory(&mut state, &healthy, 35.0);
    assert_eq!(state.disks["disk-a"].risk_streak, 2);
    healthy.disks[0].health.last_smart_refresh_at = 40.0;
    update_autopilot_risk_memory(&mut state, &healthy, 40.0);
    assert_eq!(state.disks["disk-a"].risk_streak, 0);
    assert_eq!(state.disks["disk-a"].next_action_after, 40.0);
}

#[test]
fn risk_memory_counts_legacy_untimestamped_health_once() {
    let mut state = AutopilotState::default();
    let risky = report(vec![disk(
        "disk-a",
        DiskStatus::Online,
        0,
        100,
        0.7,
        true,
        0,
    )]);

    update_autopilot_risk_memory(&mut state, &risky, 10.0);
    update_autopilot_risk_memory(&mut state, &risky, 20.0);

    assert_eq!(state.disks["disk-a"].risk_streak, 1);
    assert!(state.disks["disk-a"].has_health_observation);
}

#[test]
fn explicit_health_reset_clears_prior_risk_memory_immediately() {
    let mut state = AutopilotState::default();
    let mut risky_disk = disk("disk-a", DiskStatus::Online, 0, 100, 0.7, true, 0);
    risky_disk.health.last_smart_refresh_at = 10.0;
    update_autopilot_risk_memory(&mut state, &report(vec![risky_disk]), 10.0);
    assert_eq!(state.disks["disk-a"].risk_streak, 1);

    let mut reset_disk = disk("disk-a", DiskStatus::Online, 0, 100, 0.0, false, 0);
    reset_disk.health.smart_evidence_updated_at = 20.0;
    update_autopilot_risk_memory(&mut state, &report(vec![reset_disk]), 20.0);

    assert_eq!(state.disks["disk-a"].risk_streak, 0);
    assert_eq!(state.disks["disk-a"].healthy_streak, 2);
}

#[test]
fn drain_decision_prioritizes_cooldown_critical_and_confirmation() {
    let config = AutopilotConfig::default();
    let risky = disk("disk-a", DiskStatus::Online, 0, 100, 0.9, true, 0);
    let mut state = AutopilotDiskState {
        next_action_after: 20.0,
        ..AutopilotDiskState::default()
    };
    assert_eq!(
        autopilot_drain_decision(&risky, &state, 10.0, &config),
        AutopilotDrainDecision::Cooldown
    );
    state.next_action_after = 0.0;
    assert_eq!(
        autopilot_drain_decision(&risky, &state, 10.0, &config),
        AutopilotDrainDecision::Drain
    );

    let mut io_risky = disk("disk-a", DiskStatus::Online, 0, 100, 0.1, true, 40);
    io_risky.health.recent_io_error_delta = 40;
    io_risky.health.recent_io_error_delta_at = 10.0;
    io_risky.health.smart_evidence_updated_at = 10.0;
    assert_eq!(
        autopilot_drain_decision(&io_risky, &state, 10.0, &config),
        AutopilotDrainDecision::Drain
    );

    let mut smart_failed = disk("disk-a", DiskStatus::Online, 0, 100, 0.1, true, 0);
    smart_failed.health.smart_status_failed = true;
    assert_eq!(
        autopilot_drain_decision(&smart_failed, &state, 10.0, &config),
        AutopilotDrainDecision::Drain
    );

    let moderate = disk("disk-a", DiskStatus::Online, 0, 100, 0.5, true, 0);
    assert_eq!(
        autopilot_drain_decision(&moderate, &state, 10.0, &config),
        AutopilotDrainDecision::Observe
    );
    state.risk_streak = config.risk_confirmations;
    assert_eq!(
        autopilot_drain_decision(&moderate, &state, 10.0, &config),
        AutopilotDrainDecision::Drain
    );
}

#[test]
fn rebalance_skew_handles_empty_single_capacity_and_absolute_usage() {
    assert_eq!(autopilot_rebalance_skew(&report(Vec::new())), 0.0);
    assert_eq!(
        autopilot_rebalance_skew(&report(vec![disk(
            "a",
            DiskStatus::Online,
            50,
            100,
            0.0,
            false,
            0,
        )])),
        0.0
    );
    let balanced = report(vec![
        disk("a", DiskStatus::Online, 50, 100, 0.0, false, 0),
        disk("b", DiskStatus::Online, 100, 200, 0.0, false, 0),
        disk("c", DiskStatus::Offline, 10_000, 1, 0.0, false, 0),
    ]);
    assert_eq!(autopilot_rebalance_skew(&balanced), 2.0 / 3.0);

    let zero_capacity = report(vec![
        disk("a", DiskStatus::Online, 0, 0, 0.0, false, 0),
        disk("b", DiskStatus::Online, 10, 0, 0.0, false, 0),
    ]);
    assert_eq!(autopilot_rebalance_skew(&zero_capacity), 10.0);
}

#[test]
fn adaptive_budget_scales_with_utility_and_respects_zero() {
    assert_eq!(adaptive_autopilot_budget(0, None), 0);
    assert_eq!(adaptive_autopilot_budget(10, None), 10);
    let high = AutopilotActionStats {
        utility_ewma: 4.0,
        ..AutopilotActionStats::default()
    };
    assert_eq!(adaptive_autopilot_budget(10, Some(&high)), 20);
    let low = AutopilotActionStats {
        utility_ewma: -1.0,
        ..AutopilotActionStats::default()
    };
    assert_eq!(adaptive_autopilot_budget(9, Some(&low)), 5);
    let extreme = AutopilotActionStats {
        utility_ewma: 100.0,
        ..AutopilotActionStats::default()
    };
    assert_eq!(
        adaptive_autopilot_budget(usize::MAX, Some(&extreme)),
        usize::MAX
    );
}

#[test]
fn policy_conversion_covers_every_mode() {
    let observe = policy_to_config(&AutopilotPolicy {
        mode: AutopilotMode::Observe,
        max_drains_per_day: 99,
        ..AutopilotPolicy::default()
    });
    assert_eq!(observe.max_drains_per_run, 0);
    assert_eq!(observe.scrub_files_per_run, 0);
    assert_eq!(observe.rebalance_files_per_run, 0);

    let safe = policy_to_config(&AutopilotPolicy::default());
    assert_eq!(safe.scrub_files_per_run, 128);

    let balanced = policy_to_config(&AutopilotPolicy {
        mode: AutopilotMode::Balanced,
        ..AutopilotPolicy::default()
    });
    assert_eq!(balanced.scrub_files_per_run, 256);
    assert_eq!(balanced.rebalance_files_per_run, 64);

    let aggressive = policy_to_config(&AutopilotPolicy {
        mode: AutopilotMode::Aggressive,
        ..AutopilotPolicy::default()
    });
    assert_eq!(aggressive.risk_confirmations, 1);
    assert_eq!(aggressive.scrub_files_per_run, 512);
    assert_eq!(aggressive.rebalance_files_per_run, 128);
    assert_eq!(aggressive.foreground_latency_target_ms, 112.5);

    let runtime = runtime_policy_from_config(&AutopilotConfig {
        max_drains_per_run: 7,
        foreground_latency_target_ms: 44.0,
        ..AutopilotConfig::default()
    });
    assert_eq!(runtime.max_drains_per_day, 7);
    assert_eq!(runtime.background_io.target_foreground_p99_ms, 44.0);
}

#[test]
fn action_statistics_track_counts_totals_and_ewma() {
    let mut state = AutopilotState::default();
    record_autopilot_action(&mut state, "scrub", true, 10.0, 2, 3);
    let stats = &state.action_stats["scrub"];
    assert_eq!(stats.runs, 1);
    assert_eq!(stats.successes, 1);
    assert_eq!(stats.failures, 0);
    assert_eq!(stats.rewritten_files, 2);
    assert_eq!(stats.repaired_files, 3);
    assert_eq!(stats.utility_ewma, 10.0);

    record_autopilot_action(&mut state, "scrub", false, 0.0, 4, 5);
    let stats = &state.action_stats["scrub"];
    assert_eq!(stats.runs, 2);
    assert_eq!(stats.successes, 1);
    assert_eq!(stats.failures, 1);
    assert_eq!(stats.rewritten_files, 6);
    assert_eq!(stats.repaired_files, 8);
    assert!((stats.utility_ewma - 8.5).abs() < f64::EPSILON);
}

#[test]
fn adaptive_mode_reduces_after_repeated_failures() {
    let mut state = AutopilotState::default();
    assert_eq!(adaptive_autopilot_mode(&state), "normal");
    record_autopilot_action(&mut state, "probe", false, -1.0, 0, 0);
    assert_eq!(adaptive_autopilot_mode(&state), "normal");
    record_autopilot_action(&mut state, "smart", false, -1.0, 0, 0);
    assert_eq!(adaptive_autopilot_mode(&state), "reduced");
    record_autopilot_action(&mut state, "scrub", true, 1.0, 0, 0);
    record_autopilot_action(&mut state, "verify", true, 1.0, 0, 0);
    assert_eq!(adaptive_autopilot_mode(&state), "normal");
}

#[test]
fn state_loading_resets_corrupt_files_and_save_round_trips() {
    let dir = tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    let (fresh, warning) = fs.load_autopilot_state();
    assert_eq!(fresh.version, autopilot_state_version());
    assert!(warning.is_none());

    let mut state = AutopilotState {
        runs: 3,
        ..AutopilotState::default()
    };
    fs.save_autopilot_state(&state).unwrap();
    let (loaded, warning) = fs.load_autopilot_state();
    assert_eq!(loaded.runs, 3);
    assert_eq!(loaded.version, autopilot_state_version());
    assert!(warning.is_none());

    std::fs::write(dir.path().join(".argosfs/autopilot-state.json"), b"{").unwrap();
    let (reset, warning) = fs.load_autopilot_state();
    assert_eq!(reset.runs, 0);
    assert!(warning.is_some());

    state.version = 1;
    fs.save_autopilot_state(&state).unwrap();
    let (upgraded, _) = fs.load_autopilot_state();
    assert_eq!(upgraded.version, autopilot_state_version());
}

fn autopilot_host_volume(disks: usize) -> (tempfile::TempDir, ArgosFs) {
    let dir = tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        disks,
        false,
    )
    .unwrap();
    (dir, fs)
}

#[test]
fn public_autopilot_wrappers_run_and_reject_invalid_policies() {
    let (_dir, fs) = autopilot_host_volume(2);
    let once = fs.autopilot_once().unwrap();
    assert!(once["actions"].is_array());
    let policy_once = fs
        .autopilot_once_with_policy(AutopilotPolicy {
            mode: AutopilotMode::Observe,
            ..AutopilotPolicy::default()
        })
        .unwrap();
    assert!(policy_once["planner"]["background_io"]["throttle_decisions"].is_array());

    let config = AutopilotConfig {
        probe_interval_sec: u64::MAX,
        smart_interval_sec: u64::MAX,
        scrub_interval_sec: u64::MAX,
        rebalance_interval_sec: u64::MAX,
        ..AutopilotConfig::default()
    };
    assert!(fs.autopilot_once_with_config(config.clone()).is_ok());
    assert!(fs.autopilot_dry_run().unwrap()["decisions"].is_array());
    assert!(fs
        .autopilot_dry_run_with_policy(AutopilotPolicy::default())
        .unwrap()["decisions"]
        .is_array());
    assert!(fs.autopilot_dry_run_with_config(config).unwrap()["decisions"].is_array());

    let invalid = AutopilotPolicy {
        background_io: crate::autopilot::BackgroundIoPolicy {
            target_foreground_p99_ms: 0.0,
            ..Default::default()
        },
        ..AutopilotPolicy::default()
    };
    assert!(fs.autopilot_once_with_policy(invalid.clone()).is_err());
    assert!(fs.autopilot_dry_run_with_policy(invalid).is_err());
}

#[test]
fn autopilot_resets_corrupt_state_and_records_probe_smart_and_paused_actions() {
    let (dir, fs) = autopilot_host_volume(1);
    std::fs::write(dir.path().join(".argosfs/autopilot-state.json"), b"{").unwrap();
    let config = AutopilotConfig {
        probe_interval_sec: 1,
        smart_interval_sec: 1,
        scrub_interval_sec: 1,
        rebalance_interval_sec: 1,
        max_drains_per_run: 0,
        scrub_files_per_run: 0,
        rebalance_files_per_run: 0,
        rebalance_min_skew: 0.0,
        ..AutopilotConfig::default()
    };
    let result = fs
        .autopilot_once_with_config_and_policy(
            config,
            AutopilotPolicy {
                mode: AutopilotMode::Observe,
                ..AutopilotPolicy::default()
            },
        )
        .unwrap();
    let actions = result["actions"].as_array().unwrap();
    assert!(actions
        .iter()
        .any(|action| action["action"] == "autopilot-state-reset"));
    assert!(actions
        .iter()
        .any(|action| { action["action"] == "probe" || action["action"] == "probe-skipped" }));
    assert!(actions.iter().any(|action| {
        action["action"] == "smart-refresh" || action["action"] == "smart-refresh-skipped"
    }));
    assert!(actions
        .iter()
        .any(|action| action["action"] == "background-io-paused"));
    assert!(actions
        .iter()
        .any(|action| action["action"] == "scrub-paused"));
    assert!(actions
        .iter()
        .any(|action| action["action"] == "rebalance-paused"));
    assert!(actions
        .iter()
        .any(|action| action["action"] == "verify-actions"));
}

#[test]
fn dry_run_explains_budget_redundancy_cooldown_and_low_risk_rejections() {
    let (dir, fs) = autopilot_host_volume(2);
    let ids = fs
        .metadata_snapshot()
        .disks
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    fs.set_disk_health(
        &ids[0],
        HealthCounters {
            reallocated_sectors: 500,
            pending_sectors: 10,
            io_errors: 50,
            ..HealthCounters::default()
        },
    )
    .unwrap();

    let observe = fs
        .autopilot_dry_run_with_policy(AutopilotPolicy {
            mode: AutopilotMode::Observe,
            ..AutopilotPolicy::default()
        })
        .unwrap();
    assert!(observe["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|decision| {
            decision["rejected_actions"]
                .as_array()
                .is_some_and(|items| {
                    items.iter().any(|item| {
                        item["reason"]
                            .as_str()
                            .is_some_and(|reason| reason.contains("observe"))
                    })
                })
        }));

    let mut state = AutopilotState::default();
    state.disks.insert(
        ids[0].clone(),
        AutopilotDiskState {
            risk_streak: 10,
            next_action_after: now_f64() + 3600.0,
            ..AutopilotDiskState::default()
        },
    );
    fs.save_autopilot_state(&state).unwrap();
    let cooldown = fs.autopilot_dry_run().unwrap();
    assert!(cooldown["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|decision| {
            decision["rejected_actions"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["reason"] == "cooldown"))
        }));

    std::fs::remove_file(dir.path().join(".argosfs/autopilot-state.json")).unwrap();
    let low_risk = fs.autopilot_dry_run().unwrap();
    assert!(low_risk["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|decision| {
            decision["rejected_actions"]
                .as_array()
                .is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item["reason"] == "risk below threshold")
                })
        }));
}

#[test]
fn raw_autopilot_moves_bounded_verified_shards_to_returning_member() {
    let dir = tempdir().unwrap();
    let paths = (0..4)
        .map(|i| dir.path().join(format!("member-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop_paged(
        &paths,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            ..Default::default()
        },
        64 * 1024 * 1024,
        "autopilot-member",
        false,
    )
    .unwrap();
    let target = "disk-0003";
    fs.mark_disk(target, DiskStatus::Offline).unwrap();
    fs.write_file("/empty", b"", 0o600).unwrap();
    let data = vec![73; 128 * 1024];
    fs.write_file("/payload", &data, 0o600).unwrap();
    let config = AutopilotConfig {
        probe_interval_sec: u64::MAX,
        smart_interval_sec: u64::MAX,
        scrub_interval_sec: u64::MAX,
        rebalance_interval_sec: 0,
        max_drains_per_run: 0,
        rebalance_files_per_run: 1,
        ..Default::default()
    };
    assert!(fs
        .autopilot_member_target(&fs.health_report(), 0.08, 0.85, None, &BTreeSet::new())
        .is_none());
    fs.mark_disk(target, DiskStatus::Online).unwrap();
    fs.sync().unwrap();
    let before = fs.metadata_snapshot();
    let ino = fs.resolve_path("/payload", true).unwrap();
    let plan = fs.autopilot_dry_run_with_config(config.clone()).unwrap();
    assert_eq!(plan["planner"]["rebalance"]["member_target"], target);
    assert_eq!(plan["planner"]["rebalance"]["budget_shards"], 1);
    assert_eq!(before.disks[target].used_bytes, 0);
    let observe = fs
        .autopilot_once_with_config_and_policy(
            config.clone(),
            AutopilotPolicy {
                mode: AutopilotMode::Observe,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(observe["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["action"] == "rebalance-paused"));
    assert_eq!(fs.metadata_snapshot().disks[target].used_bytes, 0);
    let first = fs.autopilot_once_with_config(config.clone()).unwrap();
    let action = first["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["action"] == "rebalance-member-incremental")
        .unwrap();
    assert_eq!(action["report"]["scanned_files"], 1);
    assert_eq!(action["report"]["moved_shards"], 0);
    for position in 1..=2 {
        let result = fs.autopilot_once_with_config(config.clone()).unwrap();
        let action = result["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["action"] == "rebalance-member-incremental")
            .unwrap();
        assert_eq!(action["report"]["scanned_files"], 1);
        assert_eq!(action["report"]["moved_shards"], 1);
        assert_eq!(action["report"]["scanned_blocks"], 1);
        assert_eq!(action["report"]["resume_block"], json!([ino, position]));
        assert!(!result["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["action"] == "rebalance-incremental"));
    }
    // The already-balanced prefix is skipped within the scan budget,
    // without rescanning it indefinitely or exceeding the shard budget.
    let empty = fs.resolve_path("/empty", true).unwrap();
    let mut window = super::super::member_balance::MemberBalanceWindow {
        max_files: 1,
        max_blocks: 1,
        cursor: Some(empty),
        resume_block: None,
    };
    for position in 1..=3 {
        let batch = fs.rebalance_member_window(target, 10, window).unwrap();
        assert_eq!(batch.scanned_blocks, 1);
        assert_eq!(batch.moved_shards, u64::from(position == 3));
        assert_eq!(batch.resume_block, Some((ino, position)));
        window.resume_block = batch.resume_block;
    }
    let after = fs.metadata_snapshot();
    assert!(after.disks[target].used_bytes > 0);
    assert_eq!(before.inodes[&ino].mtime, after.inodes[&ino].mtime);
    assert_eq!(before.inodes[&ino].ctime, after.inodes[&ino].ctime);
    assert_eq!(
        before.inodes[&ino].blocks.len(),
        after.inodes[&ino].blocks.len()
    );
    assert_eq!(fs.read_file("/payload", false).unwrap(), data);
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(fs.read_file("/payload", false).unwrap(), data);
}

#[test]
fn raw_autopilot_rotates_after_a_complete_unfillable_pass() {
    let dir = tempdir().unwrap();
    let paths = (0..4)
        .map(|i| {
            let path = dir.path().join(format!("member-{i}.img"));
            let mib = if i == 0 { 512 } else { 64 };
            std::fs::File::create(&path)
                .unwrap()
                .set_len(mib * 1024 * 1024)
                .unwrap();
            path
        })
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_raw_paged(
        &paths,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            ..Default::default()
        },
        "autopilot-rotation",
        false,
    )
    .unwrap();
    let unfillable = "disk-0000";
    let returning = "disk-0003";
    fs.mark_disk(returning, DiskStatus::Offline).unwrap();
    let data = vec![37; 64 * 1024];
    fs.write_file("/first", &data, 0o600).unwrap();
    fs.write_file("/second", &data, 0o600).unwrap();
    fs.mark_disk(returning, DiskStatus::Online).unwrap();
    fs.sync().unwrap();
    // The largest member is already in every stripe, while the returning
    // member has a smaller proportional deficit but still accepts shards.
    assert_eq!(fs.rebalance_member(returning, 3).unwrap().moved_shards, 3);
    let config = AutopilotConfig {
        probe_interval_sec: u64::MAX,
        smart_interval_sec: u64::MAX,
        scrub_interval_sec: u64::MAX,
        rebalance_interval_sec: 0,
        max_drains_per_run: 0,
        rebalance_files_per_run: 1,
        ..Default::default()
    };
    let plan = || fs.autopilot_dry_run_with_config(config.clone()).unwrap();
    assert_eq!(plan()["planner"]["rebalance"]["member_target"], unfillable);
    for completed in [false, true] {
        let result = fs.autopilot_once_with_config(config.clone()).unwrap();
        let action = result["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["action"] == "rebalance-member-incremental")
            .unwrap();
        assert_eq!(action["target"], unfillable);
        assert_eq!(action["report"]["moved_shards"], 0);
        assert_eq!(action["report"]["pass_complete"], completed);
    }
    assert_eq!(plan()["planner"]["rebalance"]["member_target"], returning);
    let result = fs.autopilot_once_with_config(config).unwrap();
    let action = result["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["action"] == "rebalance-member-incremental")
        .unwrap();
    assert_eq!(action["target"], returning);
    assert_eq!(action["report"]["moved_shards"], 1);
    assert_eq!(fs.read_file("/first", false).unwrap(), data);
    assert_eq!(fs.read_file("/second", false).unwrap(), data);
}
