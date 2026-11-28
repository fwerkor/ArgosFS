use anyhow::{bail, Context, Result};
use argosfs_ztest::fault::FaultConfig;
use argosfs_ztest::supervisor::{run, RunConfig};
use argosfs_ztest::workload::{run_worker, WorkerConfig};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "argosfs-ztest",
    version,
    about = "ztest-style randomized crash, bit-rot, torn-write, and flush-loss stress for ArgosFS"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Create a pool, repeatedly SIGKILL a workload, and verify recovery.
    Run(RunArgs),
    /// Run or replay an exact-state model trace on clean storage.
    Model(ModelArgs),
    /// Internal workload process. The run supervisor starts this.
    #[command(hide = true)]
    Worker(WorkerArgs),
    #[command(hide = true)]
    RecoveryWorker(WorkerArgs),
    #[command(hide = true)]
    MutationWorker(WorkerArgs),
}

#[derive(Parser)]
struct RunArgs {
    /// New directory for images, logs, and the oracle (must not exist). Removed on success unless --keep-work-dir.
    #[arg(long)]
    work_dir: Option<PathBuf>,
    #[arg(long)]
    keep_work_dir: bool,
    /// Explicitly select authenticated paged metadata (the default).
    #[arg(long, conflicts_with = "legacy_metadata")]
    paged_metadata: bool,
    /// Exercise the original checkpoint/journal format instead.
    #[arg(long)]
    legacy_metadata: bool,
    /// Number of crash/recover cycles.
    #[arg(long, default_value_t = 5)]
    cycles: usize,
    /// 64-bit seed. Zero chooses a random seed.
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, default_value_t = 5)]
    devices: usize,
    #[arg(long, default_value_t = 3)]
    k: usize,
    #[arg(long, default_value_t = 2)]
    m: usize,
    #[arg(long, default_value = "64MiB", value_parser = parse_bytes)]
    image_size: u64,
    #[arg(long, default_value_t = 32 * 1024)]
    chunk_size: usize,
    #[arg(long, default_value_t = 4)]
    threads: usize,
    #[arg(long, default_value_t = 64 * 1024)]
    max_file_bytes: usize,
    #[arg(long, default_value_t = 4)]
    files_per_epoch: usize,
    #[arg(long, default_value_t = 3)]
    keep_epochs: usize,
    #[arg(long, default_value_t = 400)]
    kill_after_ms_min: u64,
    #[arg(long, default_value_t = 2500)]
    kill_after_ms_max: u64,
    /// Fail if fewer than this many individual faults were injected.
    #[arg(long, default_value_t = 1)]
    min_faults: u64,
    /// Disable the deterministic end-to-end parity reconstruction drill.
    #[arg(long)]
    no_parity_drill: bool,
    /// Allow heuristic over-budget loss retirement in exploratory stress runs.
    #[arg(long)]
    allow_expected_loss: bool,
    /// Additional deterministic crash attempts during each recovery.
    #[arg(long, default_value_t = 0)]
    recovery_cuts: usize,
    /// One-shot EIO attempts during each recovery.
    #[arg(long, default_value_t = 0)]
    recovery_eios: usize,
    /// Reorder independent writes in the volatile cache before persistence.
    #[arg(long)]
    reorder_pending: bool,

    /// Percentage of reads that return EIO.
    #[arg(long, default_value_t = 0.10)]
    read_error_pct: f64,
    /// Percentage of reads whose returned bytes are transiently flipped.
    #[arg(long, default_value_t = 0.20)]
    read_flip_pct: f64,
    /// Percentage of reads that persist silent bit rot to the image.
    #[arg(long, default_value_t = 0.05)]
    bit_rot_pct: f64,
    /// Allow durable bit rot in metadata and journal regions as well as data.
    #[arg(long)]
    bit_rot_metadata: bool,
    /// Percentage of writes that return EIO without writing.
    #[arg(long, default_value_t = 0.10)]
    write_error_pct: f64,
    /// Percentage of writes that are applied and then return EIO.
    #[arg(long, default_value_t = 0.02)]
    write_eio_after_pct: f64,
    /// Percentage of writes that are silently corrupted before storage.
    #[arg(long, default_value_t = 0.02)]
    write_bit_rot_pct: f64,
    /// Percentage of writes truncated at a sector boundary.
    #[arg(long, default_value_t = 0.10)]
    torn_write_pct: f64,
    /// Percentage of writes silently discarded.
    #[arg(long, default_value_t = 0.05)]
    lost_write_pct: f64,
    /// Scale write-fault probabilities for metadata/journal during mount.
    #[arg(long, default_value_t = 0.10)]
    metadata_write_fault_scale: f64,
    /// Percentage of flushes acknowledged without issuing the real barrier.
    #[arg(long, default_value_t = 0.20)]
    drop_flush_pct: f64,
    /// Also discard volatile writes on a dropped flush (models a lying cache).
    #[arg(long)]
    discard_pending_on_drop_flush: bool,
    /// Bytes of writes held in the simulated volatile write-back window.
    #[arg(long, default_value_t = 256, value_parser = parse_kib)]
    volatile_window_kib: u64,
    /// Bits changed by one bit-rot event.
    #[arg(long, default_value_t = 2)]
    bits_per_flip: usize,
    /// Sector size used to choose a torn-write prefix.
    #[arg(long, default_value_t = 512)]
    torn_sector_bytes: u64,
    /// Artificial per-I/O delay ceiling, in microseconds.
    #[arg(long, default_value_t = 0)]
    max_latency_us: u64,
}

#[derive(Parser)]
struct ModelArgs {
    #[arg(long)]
    work_dir: PathBuf,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long, default_value_t = 64)]
    operations: usize,
    #[arg(long, conflicts_with = "legacy_metadata")]
    paged_metadata: bool,
    #[arg(long)]
    legacy_metadata: bool,
    #[arg(long)]
    topology: bool,
    /// Combine feature checks with a varied pool configuration and repeated topology rounds.
    #[arg(long, value_enum, conflicts_with = "replay")]
    profile: Option<argosfs_ztest::combinations::Profile>,
    /// Additional files in the quota subtree (0–4096), retained across maintenance.
    #[arg(
        long,
        default_value_t = 0,
        requires = "profile",
        conflicts_with = "replay"
    )]
    directory_entries: usize,
    #[arg(long)]
    replay: Option<PathBuf>,
    #[arg(long)]
    minimize: bool,
}

#[derive(Parser)]
struct WorkerArgs {
    #[arg(long)]
    config: PathBuf,
}

fn main() {
    if let Err(err) = dispatch() {
        eprintln!("argosfs-ztest: {err:#}");
        std::process::exit(1);
    }
}

fn dispatch() -> Result<()> {
    match Cli::parse().command {
        Command::Run(args) => {
            let outcome = run(run_config(args)?)?;
            println!("{}", serde_json::to_string_pretty(&outcome)?);
            Ok(())
        }
        Command::Model(args) => {
            let trace = if let Some(path) = args.replay {
                serde_json::from_slice(&std::fs::read(path)?)?
            } else {
                let trace = argosfs_ztest::model::Trace::generate(
                    args.seed,
                    !args.legacy_metadata,
                    args.operations,
                    args.topology,
                );
                if let Some(profile) = args.profile {
                    let mut trace = trace.with_profile(profile, args.topology);
                    trace.directory_entries = args.directory_entries;
                    trace
                } else {
                    trace
                }
            };
            if trace
                .profile
                .is_some_and(argosfs_ztest::combinations::Profile::encrypted)
            {
                // Only this CLI process and its disposable test children use the fixture key.
                std::env::remove_var("ARGOSFS_KEY_FILE");
                std::env::set_var("ARGOSFS_KEY", argosfs_ztest::combinations::TEST_KEY);
            }
            argosfs_ztest::model::run(trace, &args.work_dir, args.minimize)
        }
        Command::MutationWorker(args) => {
            let bytes = std::fs::read(&args.config)?;
            argosfs_ztest::atomic::worker(serde_json::from_slice(&bytes)?)
        }
        Command::RecoveryWorker(args) => {
            let bytes = std::fs::read(&args.config)?;
            argosfs_ztest::supervisor::run_recovery_worker(serde_json::from_slice(&bytes)?)
        }
        Command::Worker(args) => {
            // A panic in a chaos thread must terminate the whole child, not
            // leave a broken worker running until the supervisor's timeout.
            std::panic::set_hook(Box::new(|info| {
                eprintln!("ztest worker panic: {info}");
                std::process::abort();
            }));
            let bytes = std::fs::read(&args.config)
                .with_context(|| format!("read worker config {}", args.config.display()))?;
            let config: WorkerConfig = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse worker config {}", args.config.display()))?;
            run_worker(config)
        }
    }
}

fn run_config(args: RunArgs) -> Result<RunConfig> {
    validate_pct("read-error-pct", args.read_error_pct)?;
    validate_pct("read-flip-pct", args.read_flip_pct)?;
    validate_pct("bit-rot-pct", args.bit_rot_pct)?;
    validate_pct("write-error-pct", args.write_error_pct)?;
    validate_pct("write-eio-after-pct", args.write_eio_after_pct)?;
    validate_pct("write-bit-rot-pct", args.write_bit_rot_pct)?;
    validate_pct("torn-write-pct", args.torn_write_pct)?;
    validate_pct("lost-write-pct", args.lost_write_pct)?;
    validate_pct("drop-flush-pct", args.drop_flush_pct)?;
    if !args.metadata_write_fault_scale.is_finite()
        || !(0.0..=1.0).contains(&args.metadata_write_fault_scale)
    {
        bail!("--metadata-write-fault-scale must be between 0 and 1");
    }
    if args.bits_per_flip == 0 {
        bail!("--bits-per-flip must be positive");
    }
    if args.torn_sector_bytes == 0 {
        bail!("--torn-sector-bytes must be positive");
    }
    if args.volatile_window_kib > 1024 * 1024 {
        bail!("--volatile-window-kib is unreasonably large");
    }
    let fault = FaultConfig {
        seed: args.seed,
        read_error: args.read_error_pct / 100.0,
        read_bit_flip: args.read_flip_pct / 100.0,
        durable_bit_rot: args.bit_rot_pct / 100.0,
        write_error: args.write_error_pct / 100.0,
        write_error_after_write: args.write_eio_after_pct / 100.0,
        write_bit_rot: args.write_bit_rot_pct / 100.0,
        torn_write: args.torn_write_pct / 100.0,
        lost_write: args.lost_write_pct / 100.0,
        drop_flush: args.drop_flush_pct / 100.0,
        discard_pending_on_drop_flush: args.discard_pending_on_drop_flush,
        bits_per_flip: args.bits_per_flip,
        torn_sector_bytes: args.torn_sector_bytes,
        volatile_window_bytes: args.volatile_window_kib * 1024,
        reorder_pending: args.reorder_pending,
        bit_rot_data_only: !args.bit_rot_metadata,
        metadata_write_fault_scale: args.metadata_write_fault_scale,
        data_floor: 16 * 1024 * 1024,
        max_latency_us: args.max_latency_us,
        error_errno: libc::EIO,
    };
    Ok(RunConfig {
        work_dir: args.work_dir,
        keep_work_dir: args.keep_work_dir,
        paged_metadata: !args.legacy_metadata,
        cycles: args.cycles,
        seed: args.seed,
        devices: args.devices,
        k: args.k,
        m: args.m,
        image_size: args.image_size,
        chunk_size: args.chunk_size,
        threads: args.threads,
        max_file_bytes: args.max_file_bytes,
        files_per_epoch: args.files_per_epoch,
        keep_epochs: args.keep_epochs,
        kill_after_min: Duration::from_millis(args.kill_after_ms_min),
        kill_after_max: Duration::from_millis(args.kill_after_ms_max),
        min_faults: args.min_faults,
        parity_drill: !args.no_parity_drill,
        recovery_cuts: args.recovery_cuts,
        recovery_eios: args.recovery_eios,
        allow_expected_loss: args.allow_expected_loss,
        fault,
    })
}

fn validate_pct(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        bail!("--{name} must be between 0 and 100");
    }
    Ok(())
}

fn parse_bytes(value: &str) -> std::result::Result<u64, String> {
    let value = value.trim();
    let (digits, multiplier) = if let Some(prefix) = value.strip_suffix("KiB") {
        (prefix, 1024u64)
    } else if let Some(prefix) = value.strip_suffix("MiB") {
        (prefix, 1024u64 * 1024)
    } else if let Some(prefix) = value.strip_suffix("GiB") {
        (prefix, 1024u64 * 1024 * 1024)
    } else if let Some(prefix) = value.strip_suffix("KB") {
        (prefix, 1000u64)
    } else if let Some(prefix) = value.strip_suffix("MB") {
        (prefix, 1000u64 * 1000)
    } else {
        (value, 1u64)
    };
    let number: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid byte size: {value:?}"))?;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte size overflows u64: {value:?}"))
}

fn parse_kib(value: &str) -> std::result::Result<u64, String> {
    value
        .trim()
        .parse()
        .map_err(|_| format!("invalid KiB value: {value:?}"))
}
