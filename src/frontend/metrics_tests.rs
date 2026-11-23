use super::*;
use crate::types::VolumeConfig;
use std::net::Shutdown;
use tempfile::tempdir;

fn volume() -> ArgosFs {
    let dir = tempdir().unwrap();
    let path = dir.keep();
    ArgosFs::create(
        &path,
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap()
}

fn request(volume: &ArgosFs, request: &[u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let request = request.to_vec();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(&request).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (mut server, _) = listener.accept().unwrap();
    handle_client(volume, &mut server).unwrap();
    drop(server);
    client.join().unwrap()
}

#[test]
fn rendered_metrics_include_help_types_and_disk_series_once() {
    let volume = volume();
    let output = render(&volume);
    assert!(output.contains("# HELP argosfs_txid"));
    assert!(output.contains("# TYPE argosfs_txid gauge"));
    assert!(output.contains("argosfs_files"));
    assert!(output.contains("argosfs_encryption_enabled 0"));
    assert!(output.contains("argosfs_disk_used_bytes{disk=\"disk-"));
    assert_eq!(output.matches("# HELP argosfs_disk_used_bytes").count(), 1);
}

#[test]
fn metric_emitter_escapes_labels_and_deduplicates_metadata() {
    let mut output = String::new();
    let mut emitter = MetricEmitter::new(&mut output);
    let label = "slash\\quote\"line\nend";
    emitter.metric("sample_metric", "sample help", 1.5, &[("disk", label)]);
    emitter.metric("sample_metric", "sample help", 2.5, &[]);
    drop(emitter);
    assert_eq!(output.matches("# HELP sample_metric").count(), 1);
    assert_eq!(output.matches("# TYPE sample_metric gauge").count(), 1);
    assert!(output.contains("disk=\"slash\\\\quote\\\"line\\nend\""));
    assert!(output.contains("sample_metric 2.5"));
    assert_eq!(escape_label_value("plain"), "plain");
    assert_eq!(escape_label_value("\\\"\n"), "\\\\\\\"\\n");
}

#[test]
fn http_handler_serves_metrics_root_and_not_found() {
    let volume = volume();
    for path in ["/metrics", "/"] {
        let response = request(
            &volume,
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes(),
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Content-Type: text/plain; version=0.0.4"));
        assert!(response.contains("argosfs_txid"));
    }

    let response = request(&volume, b"GET /missing HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 404 Not Found"));
    assert!(response.ends_with("# unsupported path\n"));

    let response = request(&volume, b"\xff\xfe\r\n\r\n");
    assert!(response.starts_with("HTTP/1.1 404 Not Found"));
}

#[test]
fn metrics_server_reports_bind_errors() {
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = occupied.local_addr().unwrap();
    let error = serve(volume(), &address.to_string()).unwrap_err();
    assert!(matches!(error, crate::error::ArgosError::Io(_)));
}

#[test]
fn commit_only_metrics_are_available_during_metadata_write_lock() {
    let volume = volume();
    let snapshot = volume.commit_metrics().snapshots().collect::<Vec<_>>();
    assert!(snapshot.iter().all(|(_, counters)| counters.count == 0));
    volume.test_with_metadata_write_lock(|| {
        let output = render_commit_metrics(&volume);
        assert!(output
            .contains("argosfs_commit_phase_duration_seconds_count{phase=\"group_commit\"} 0"));
        assert!(output.contains("# TYPE argosfs_commit_phase_duration_seconds_sum counter"));
        assert!(!output.contains("argosfs_files"));
        let response = request(
            &volume,
            b"GET /commit-metrics HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("phase=\"sync_lock_hold\""));
    });
}

#[test]
fn commit_metrics_are_shared_across_cloned_handles() {
    let volume = volume();
    volume.commit_metrics().observe(
        crate::commit_metrics::CommitPhase::GroupCommit,
        Duration::from_millis(25),
    );
    let clone = volume.clone();
    let output = render_commit_metrics(&clone);
    assert!(
        output.contains("argosfs_commit_phase_duration_seconds_count{phase=\"group_commit\"} 1")
    );
    assert!(
        output.contains("argosfs_commit_phase_duration_seconds_sum{phase=\"group_commit\"} 0.025")
    );
}

#[test]
fn mounted_metrics_server_shares_live_counters_and_releases_listener() {
    let volume = volume();
    let server = MountedMetricsServer::start(&volume, "127.0.0.1:0").unwrap();
    let address = server.local_addr();
    volume.commit_metrics().observe(
        crate::commit_metrics::CommitPhase::Journal,
        Duration::from_millis(42),
    );
    volume.test_with_metadata_write_lock(|| {
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .write_all(b"GET /commit-metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(
            response.contains("argosfs_commit_phase_duration_seconds_count{phase=\"journal\"} 1")
        );
    });
    drop(server);
    assert!(TcpListener::bind(address).is_ok());
}

#[test]
fn commit_metrics_expose_member_labels_during_global_writer_stalls() {
    let volume = volume();
    {
        let _checkpoint = volume.commit_metrics().member_timer(
            crate::commit_metrics::CommitPhase::CheckpointMemberIo,
            "disk-0009",
        );
    }
    volume.test_with_metadata_write_lock(|| {
        let output = render_commit_metrics(&volume);
        assert!(output.contains(
            "argosfs_commit_phase_duration_seconds_count{phase=\"checkpoint_member_io\",disk=\"disk-0009\"} 1"
        ));
        assert!(output.contains(
            "argosfs_commit_phase_duration_seconds_count{phase=\"checkpoint_member_io\"} 1"
        ));
        assert!(output.contains("phase=\"journal_quorum\""));
        assert!(output.contains("phase=\"journal_flush\""));
        assert!(output.contains("phase=\"journal_tip\""));
        assert_eq!(output.matches("# HELP argosfs_commit_phase_duration_seconds_count").count(), 1);
    });
}

#[test]
fn mounted_server_drains_inflight_clients_before_unmount_returns() {
    let volume = volume();
    let server = MountedMetricsServer::start(&volume, "127.0.0.1:0").unwrap();
    let address = server.local_addr();
    let mut client = TcpStream::connect(address).unwrap();
    // Leave a connected client blocked in its request read. The listener
    // accepts independently and shutdown must wait for that worker.
    thread::sleep(Duration::from_millis(100));
    let (finished_tx, finished_rx) = mpsc::channel();
    let shutdown = thread::spawn(move || {
        drop(server);
        finished_tx.send(()).unwrap();
    });
    assert!(matches!(
        finished_rx.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout)
    ));
    client
        .write_all(b"GET /commit-metrics HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    client.shutdown(Shutdown::Write).unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"));
    assert!(finished_rx.recv_timeout(Duration::from_secs(3)).is_ok());
    shutdown.join().unwrap();
    assert!(TcpListener::bind(address).is_ok());
}

#[test]
fn data_plane_byte_counters_are_live_and_scrapable_without_metadata_lock() {
    let volume = volume();
    volume.commit_metrics().add_data_plane_bytes(
        crate::commit_metrics::DataPlaneByteKind::RangeAccepted,
        1024,
    );
    volume.commit_metrics().add_data_plane_bytes(
        crate::commit_metrics::DataPlaneByteKind::ShardPayloadWritten,
        2048,
    );
    volume.test_with_metadata_write_lock(|| {
        let output = render_commit_metrics(&volume);
        assert!(output.contains("argosfs_data_plane_bytes_total{kind=\"range_accepted\"} 1024"));
        assert!(output.contains("argosfs_data_plane_bytes_total{kind=\"range_aligned_append\"} 0"));
        assert!(
            output.contains("argosfs_data_plane_bytes_total{kind=\"shard_payload_written\"} 2048")
        );
        assert_eq!(
            output
                .matches("# TYPE argosfs_data_plane_bytes_total counter")
                .count(),
            1
        );
        assert!(output.contains("argosfs_writeback_flushes_total{reason=\"read_fallback\"} 0"));
        assert!(output.contains("argosfs_writeback_flush_errors_total{reason=\"sync\"} 0"));
        let clone = volume.clone();
        assert!(render_commit_metrics(&clone).contains("kind=\"range_accepted\"} 1024"));
    });
}

#[test]
fn raw_checkpoint_causes_and_write_bytes_are_exposed_without_metadata_lock() {
    use crate::commit_metrics::{CheckpointReason, RawWriteKind};
    let volume = volume();
    let metrics = volume.commit_metrics();
    metrics.add_raw_write_bytes(RawWriteKind::Checkpoint, 8192);
    metrics.add_raw_write_bytes(RawWriteKind::Journal, 1024);
    metrics.record_checkpoint_reason(CheckpointReason::JournalRollover);
    volume.test_with_metadata_write_lock(|| {
        let output = render_commit_metrics(&volume);
        assert!(output.contains("argosfs_raw_metadata_write_bytes_total{kind=\"checkpoint\"} 8192"));
        assert!(output.contains("argosfs_raw_metadata_write_bytes_total{kind=\"journal\"} 1024"));
        assert!(output.contains("argosfs_raw_metadata_write_bytes_total{kind=\"clean_state\"} 0"));
        assert!(output.contains("argosfs_checkpoint_reason_total{reason=\"journal_rollover\"} 1"));
        assert!(output.contains("argosfs_checkpoint_reason_total{reason=\"recovery_or_format\"} 0"));
    });
}
