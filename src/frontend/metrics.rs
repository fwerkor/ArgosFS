use crate::advanced_io;
use crate::error::Result;
use crate::types::DiskStatus;
use crate::volume::ArgosFs;
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

pub fn serve(volume: ArgosFs, listen: &str) -> Result<()> {
    let listener = TcpListener::bind(listen)?;
    for stream in listener.incoming() {
        // Standalone serving is intentionally long-lived; mounted servers
        // track and drain their request workers during unmount.
        let _ = spawn_client(&volume, stream?);
    }
    Ok(())
}

/// Optional HTTP server sharing the mounted volume's counters. Dropping it
/// stops the listener and drains accepted request workers before releasing the
/// final mounted-volume handle, so clean-unmount cannot race a live scrape.
pub struct MountedMetricsServer {
    stop: Sender<()>,
    join: Option<thread::JoinHandle<()>>,
    address: SocketAddr,
}

impl MountedMetricsServer {
    pub fn start(volume: &ArgosFs, listen: &str) -> Result<Self> {
        // Bind synchronously so a port conflict fails the mount cleanly.
        let listener = TcpListener::bind(listen)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let (stop, receiver) = mpsc::channel();
        let volume = volume.clone();
        let join = thread::Builder::new()
            .name("argosfs-metrics".to_string())
            .spawn(move || {
                let mut clients: Vec<thread::JoinHandle<()>> = Vec::new();
                loop {
                    // Check the stop signal even under a continuous stream
                    // of requests. Otherwise shutdown could starve in accept.
                    match receiver.try_recv() {
                        Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            clients.push(spawn_client(&volume, stream));
                            // Reap completed workers during long mounts. Never
                            // let the owned JoinHandle list grow forever.
                            clients.retain(|client| !client.is_finished());
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            match receiver.recv_timeout(Duration::from_millis(50)) {
                                Err(RecvTimeoutError::Timeout) => continue,
                                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                            }
                        }
                        Err(err) => {
                            eprintln!("argosfs: metrics listener failed: {err}");
                            break;
                        }
                    }
                }
                // An accepted client's ArgosFs clone must be dropped before
                // the mount can finish its final clean-unmount checkpoint.
                // Read/write timeouts bound requests stalled on network I/O.
                for client in clients {
                    let _ = client.join();
                }
            })?;
        Ok(Self {
            stop,
            join: Some(join),
            address,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for MountedMetricsServer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn spawn_client(volume: &ArgosFs, stream: TcpStream) -> thread::JoinHandle<()> {
    let volume = volume.clone();
    thread::spawn(move || {
        let mut stream = stream;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = handle_client(&volume, &mut stream);
    })
}

pub fn render(volume: &ArgosFs) -> String {
    let meta = volume.metadata_snapshot();
    let health = volume.health_report();
    let mut out = String::new();
    let mut emitter = MetricEmitter::new(&mut out);
    emitter.metric(
        "argosfs_txid",
        "Volume metadata transaction id",
        meta.txid as f64,
        &[],
    );
    emitter.metric(
        "argosfs_files",
        "Number of regular files",
        health.files as f64,
        &[],
    );
    emitter.metric(
        "argosfs_encryption_enabled",
        "Whether built-in encryption is enabled",
        u8::from(meta.encryption.enabled) as f64,
        &[],
    );
    emitter.metric(
        "argosfs_io_uring_available",
        "Whether io_uring can be initialized",
        u8::from(advanced_io::io_uring_available()) as f64,
        &[],
    );
    for disk in health.disks {
        let labels = [("disk", disk.id.as_str())];
        emitter.metric(
            "argosfs_disk_used_bytes",
            "Used shard bytes",
            disk.used_bytes as f64,
            &labels,
        );
        emitter.metric(
            "argosfs_disk_capacity_bytes",
            "Recorded disk capacity bytes",
            disk.capacity_bytes as f64,
            &labels,
        );
        emitter.metric(
            "argosfs_disk_risk_score",
            "Self-driving disk risk score",
            disk.risk_score,
            &labels,
        );
        emitter.metric(
            "argosfs_disk_online",
            "Disk online status",
            u8::from(disk.status == DiskStatus::Online) as f64,
            &labels,
        );
        emitter.metric(
            "argosfs_disk_read_latency_ms",
            "Disk read latency EWMA",
            disk.read_latency_ewma_ms,
            &labels,
        );
        emitter.metric(
            "argosfs_disk_write_latency_ms",
            "Disk write latency EWMA",
            disk.write_latency_ewma_ms,
            &labels,
        );
    }
    emit_read_cache_metrics(volume, &mut emitter);
    emit_commit_metrics(volume, &mut emitter);
    out
}

/// A metadata-lock-independent scrape for diagnosing long-held metadata writers.
/// Unlike full /metrics, this does not call metadata_snapshot or health_report.
pub fn render_commit_metrics(volume: &ArgosFs) -> String {
    let mut out = String::new();
    let mut emitter = MetricEmitter::new(&mut out);
    emit_read_cache_metrics(volume, &mut emitter);
    emit_commit_metrics(volume, &mut emitter);
    out
}

fn emit_read_cache_metrics(volume: &ArgosFs, emitter: &mut MetricEmitter<'_>) {
    let cache = volume.read_cache_stats();
    for (kind, key) in [
        ("memory_hit", "hits"),
        ("l2_hit", "l2_hits"),
        ("miss", "misses"),
    ] {
        if let Some(value) = cache.get(key).and_then(serde_json::Value::as_u64) {
            emitter.typed_metric(
                "argosfs_read_cache_requests_total",
                "Verified decoded-stripe cache lookups by outcome",
                value as f64,
                &[("result", kind)],
                "counter",
            );
        }
    }
    for (name, key) in [
        (
            "argosfs_read_cache_direct_copy_bytes_total",
            "direct_copy_bytes",
        ),
        ("argosfs_read_cache_l2_read_bytes_total", "l2_read_bytes"),
    ] {
        if let Some(value) = cache.get(key).and_then(serde_json::Value::as_u64) {
            emitter.typed_metric(
                name,
                "Read-cache data bytes since volume open",
                value as f64,
                &[],
                "counter",
            );
        }
    }
    for (name, key) in [
        (
            "argosfs_read_cache_l2_admission_frozen_total",
            "l2_admission_frozen",
        ),
        (
            "argosfs_read_cache_l2_admission_oversize_total",
            "l2_admission_oversize",
        ),
        ("argosfs_read_cache_l2_evictions_total", "l2_evictions"),
        ("argosfs_read_cache_l2_writes_total", "l2_writes"),
        ("argosfs_read_cache_l2_errors_total", "l2_errors"),
    ] {
        if let Some(value) = cache.get(key).and_then(serde_json::Value::as_u64) {
            emitter.typed_metric(
                name,
                "L2 cache admission and eviction events since volume open",
                value as f64,
                &[],
                "counter",
            );
        }
    }
    for (name, key) in [
        ("argosfs_read_cache_l2_loaded_items", "l2_loaded_items"),
        ("argosfs_read_cache_l2_loaded_bytes", "l2_loaded_bytes"),
        ("argosfs_read_cache_memory_bytes", "memory_bytes"),
        ("argosfs_read_cache_memory_items", "memory_items"),
        ("argosfs_read_cache_l2_bytes", "l2_bytes"),
        ("argosfs_read_cache_l2_items", "l2_items"),
        ("argosfs_read_cache_l2_hot_bytes", "l2_hot_bytes"),
        ("argosfs_read_cache_l2_hot_items", "l2_hot_items"),
        ("argosfs_read_cache_l2_stream_frozen", "l2_stream_frozen"),
    ] {
        if let Some(value) = cache.get(key).and_then(serde_json::Value::as_u64) {
            emitter.metric(
                name,
                "Current decoded-stripe cache residency",
                value as f64,
                &[],
            );
        }
    }
}

fn emit_commit_metrics(volume: &ArgosFs, emitter: &mut MetricEmitter<'_>) {
    // The counter families share the non-blocking commit-only metrics endpoint.
    // `shard_payload_written` covers all data-plane writers (including
    // repair/rebalance), while `range_accepted` covers successful range writes.
    // Their ratio is only meaningful over an isolated foreground benchmark.
    for (kind, bytes) in volume.commit_metrics().data_plane_byte_snapshots() {
        emitter.typed_metric(
            "argosfs_data_plane_bytes_total",
            "Successful data-plane payload bytes by accounting stage (metadata I/O excluded)",
            bytes as f64,
            &[("kind", kind)],
            "counter",
        );
    }
    for (reason, count, errors) in volume.commit_metrics().writeback_flush_snapshots() {
        emitter.typed_metric(
            "argosfs_writeback_flushes_total",
            "Dirty FUSE extent flush attempts by reason (full or prefix)",
            count as f64,
            &[("reason", reason)],
            "counter",
        );
        emitter.typed_metric(
            "argosfs_writeback_flush_errors_total",
            "Failed dirty FUSE extent flush attempts by reason",
            errors as f64,
            &[("reason", reason)],
            "counter",
        );
    }
    for (kind, bytes) in volume.commit_metrics().raw_write_byte_snapshots() {
        emitter.typed_metric(
            "argosfs_raw_metadata_write_bytes_total",
            "Successful mounted-volume raw metadata write payload bytes; excludes mount-time recovery",
            bytes as f64,
            &[("kind", kind)],
            "counter",
        );
    }
    for (kind, bytes) in volume.commit_metrics().checkpoint_cow_byte_snapshots() {
        emitter.typed_metric(
            "argosfs_checkpoint_cow_bytes_total",
            "Raw rollover checkpoint target bytes physically read or safely reused",
            bytes as f64,
            &[("kind", kind)],
            "counter",
        );
    }
    for (strategy, attempts) in volume.commit_metrics().rollover_member_strategy_snapshots() {
        emitter.typed_metric(
            "argosfs_checkpoint_rollover_member_strategy_total",
            "Attempted raw journal rollover member writes by checkpoint strategy",
            attempts as f64,
            &[("strategy", strategy)],
            "counter",
        );
    }
    for (reason, attempts) in volume.commit_metrics().checkpoint_reason_snapshots() {
        emitter.typed_metric(
            "argosfs_checkpoint_reason_total",
            "Mounted-volume full checkpoint attempts by cause; excludes mount-time recovery",
            attempts as f64,
            &[("reason", reason)],
            "counter",
        );
    }
    for (reason, buckets, total_ns) in volume.commit_metrics().writeback_flush_duration_snapshots()
    {
        emitter.writeback_flush_histogram(reason, &buckets, total_ns);
    }
    for (phase, snapshot) in volume.commit_metrics().snapshots() {
        let labels = [("phase", phase)];
        emit_commit_phase(emitter, snapshot, &labels);
    }
    for (disk_id, phase, snapshot) in volume.commit_metrics().member_snapshots() {
        let labels = [("phase", phase), ("disk", disk_id.as_str())];
        emit_commit_phase(emitter, snapshot, &labels);
    }
}

fn emit_commit_phase(
    emitter: &mut MetricEmitter<'_>,
    snapshot: crate::commit_metrics::PhaseSnapshot,
    labels: &[(&str, &str)],
) {
    emitter.typed_metric(
        "argosfs_commit_phase_duration_seconds_count",
        "Number of completed metadata commit phase timings (including failures)",
        snapshot.count as f64,
        labels,
        "counter",
    );
    emitter.typed_metric(
        "argosfs_commit_phase_duration_seconds_sum",
        "Total observed metadata commit phase wall time in seconds",
        snapshot.total_ns as f64 / 1e9,
        labels,
        "counter",
    );
    emitter.metric(
        "argosfs_commit_phase_duration_seconds_max",
        "Maximum observed metadata commit phase wall time in seconds since volume opened",
        snapshot.max_ns as f64 / 1e9,
        labels,
    );
}

fn handle_client(volume: &ArgosFs, stream: &mut TcpStream) -> Result<()> {
    let mut request = [0u8; 1024];
    let n = stream.read(&mut request)?;
    let first_line = std::str::from_utf8(&request[..n]).unwrap_or_default();
    let commit_only = first_line.starts_with("GET /commit-metrics ");
    let full = first_line.starts_with("GET /metrics ") || first_line.starts_with("GET / ");
    let body = if commit_only {
        render_commit_metrics(volume)
    } else if full {
        render(volume)
    } else {
        "# unsupported path\n".to_string()
    };
    let status = if commit_only || full {
        "200 OK"
    } else {
        "404 Not Found"
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )?;
    Ok(())
}

struct MetricEmitter<'a> {
    out: &'a mut String,
    emitted: BTreeSet<String>,
}

impl<'a> MetricEmitter<'a> {
    fn new(out: &'a mut String) -> Self {
        Self {
            out,
            emitted: BTreeSet::new(),
        }
    }

    fn metric(&mut self, name: &str, help: &str, value: f64, labels: &[(&str, &str)]) {
        self.typed_metric(name, help, value, labels, "gauge");
    }

    fn writeback_flush_histogram(
        &mut self,
        reason: &str,
        buckets: &[u64; crate::commit_metrics::FLUSH_LATENCY_BUCKETS.len() + 1],
        total_ns: u64,
    ) {
        let base = "argosfs_writeback_flush_duration_seconds";
        if self.emitted.insert(base.to_string()) {
            self.out.push_str(
                "# HELP argosfs_writeback_flush_duration_seconds Writeback flush duration including per-inode queue wait\n# TYPE argosfs_writeback_flush_duration_seconds histogram\n",
            );
        }
        for (index, (le, _)) in crate::commit_metrics::FLUSH_LATENCY_BUCKETS
            .iter()
            .enumerate()
        {
            self.out.push_str(&format!(
                "{base}_bucket{{reason=\"{reason}\",le=\"{le}\"}} {}\n",
                buckets[index]
            ));
        }
        let count = buckets[crate::commit_metrics::FLUSH_LATENCY_BUCKETS.len()];
        self.out.push_str(&format!(
            "{base}_bucket{{reason=\"{reason}\",le=\"+Inf\"}} {count}\n{base}_count{{reason=\"{reason}\"}} {count}\n{base}_sum{{reason=\"{reason}\"}} {}\n",
            total_ns as f64 / 1e9
        ));
    }

    fn typed_metric(
        &mut self,
        name: &str,
        help: &str,
        value: f64,
        labels: &[(&str, &str)],
        metric_type: &str,
    ) {
        if self.emitted.insert(name.to_string()) {
            self.out.push_str(&format!(
                "# HELP {name} {help}\n# TYPE {name} {metric_type}\n"
            ));
        }
        self.out.push_str(name);
        if !labels.is_empty() {
            self.out.push('{');
            for (idx, (key, value)) in labels.iter().enumerate() {
                if idx > 0 {
                    self.out.push(',');
                }
                self.out
                    .push_str(&format!("{key}=\"{}\"", escape_label_value(value)));
            }
            self.out.push('}');
        }
        self.out.push_str(&format!(" {value}\n"));
    }
}

fn escape_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
