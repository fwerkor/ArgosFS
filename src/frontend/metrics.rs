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
        spawn_client(&volume, stream?);
    }
    Ok(())
}

/// Optional HTTP server sharing the mounted volume's counters. Dropping it
/// stops its listener before the last mount handle is released.
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
            .spawn(move || loop {
                match listener.accept() {
                    Ok((stream, _)) => spawn_client(&volume, stream),
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

fn spawn_client(volume: &ArgosFs, stream: TcpStream) {
    let volume = volume.clone();
    thread::spawn(move || {
        let mut stream = stream;
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let _ = handle_client(&volume, &mut stream);
    });
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
    emit_commit_metrics(volume, &mut emitter);
    out
}

/// A lock-free scrape for diagnosing long-held metadata writer locks: unlike
/// full /metrics, this endpoint does not call metadata_snapshot or health_report.
pub fn render_commit_metrics(volume: &ArgosFs) -> String {
    let mut out = String::new();
    let mut emitter = MetricEmitter::new(&mut out);
    emit_commit_metrics(volume, &mut emitter);
    out
}

fn emit_commit_metrics(volume: &ArgosFs, emitter: &mut MetricEmitter<'_>) {
    for (phase, snapshot) in volume.commit_metrics().snapshots() {
        let labels = [("phase", phase)];
        emitter.typed_metric(
            "argosfs_commit_phase_duration_seconds_count",
            "Number of completed metadata commit phase timings (including failures)",
            snapshot.count as f64,
            &labels,
            "counter",
        );
        emitter.typed_metric(
            "argosfs_commit_phase_duration_seconds_sum",
            "Total observed metadata commit phase wall time in seconds",
            snapshot.total_ns as f64 / 1e9,
            &labels,
            "counter",
        );
        emitter.metric(
            "argosfs_commit_phase_duration_seconds_max",
            "Maximum observed metadata commit phase wall time in seconds since volume opened",
            snapshot.max_ns as f64 / 1e9,
            &labels,
        );
    }
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
