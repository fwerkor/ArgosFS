//! Dependency-free measurement plumbing shared by the benchmark targets.
//!
//! # Why not criterion
//!
//! Criterion is a fine tool, and it is deliberately not used here.
//!
//! - This crate's entire pitch is dependency hygiene. Pulling ~40 transitive
//!   crates into the dev graph to time a byte loop undercuts the argument the
//!   benchmarks exist to support, and it puts the MSRV job at the mercy of a
//!   dependency tree nobody in this project controls.
//! - The numbers that matter here differ by 10x to 400x, not by 2%. Resolving
//!   them needs a median over batched samples, which is fifty lines, not a
//!   bootstrap confidence interval.
//! - The measurements the brief actually asks for (initialization as a
//!   first-class metric, decode cost as a function of erasure count, a
//!   markdown table that can be pasted into a README) are all things a custom
//!   harness emits directly and criterion would have to be worked around to
//!   produce.
//!
//! # Method
//!
//! Each measurement calibrates a batch size so that one batch takes at least
//! [`Budget::batch_target`], warms up, then collects per-batch timings until
//! it has enough samples or runs out of budget. The reported figure is the
//! **median** batch, in nanoseconds per operation; the minimum is carried
//! alongside it because a large gap between the two is the signature of a
//! noisy machine and is worth seeing.
//!
//! Every measured object (encoder, decoder, buffers) is constructed once,
//! outside the timed region, so the steady-state numbers exclude
//! initialization. Initialization is measured separately and reported in its
//! own table, because burying it in an amortized average is exactly how a
//! multi-millisecond setup cost stays invisible.

// Each bench target uses a different subset of this module.
#![allow(dead_code)]

use std::fmt::Write as _;
use std::hint::black_box;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

/// How much time a single measurement may spend.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Time spent running the closure before any timing is recorded.
    pub warmup: Duration,
    /// Time to spend collecting samples, once `min_batches` is satisfied.
    pub measure: Duration,
    /// Hard ceiling on the sampling phase, so a slow configuration cannot make
    /// the whole sweep unusable.
    pub ceiling: Duration,
    /// Fewest batches to collect before `measure` is allowed to end sampling.
    pub min_batches: usize,
    /// Most batches to collect, whatever the clock says.
    pub max_batches: usize,
    /// Minimum duration of one batch, which sets the batch size. Keeping this
    /// well above the clock's resolution is what makes timing sub-microsecond
    /// operations meaningful.
    pub batch_target: Duration,
}

impl Budget {
    /// The default budget: about 0.3s per measurement, 1.5s worst case.
    pub fn standard() -> Self {
        Self {
            warmup: Duration::from_millis(100),
            measure: Duration::from_millis(200),
            ceiling: Duration::from_millis(1500),
            min_batches: 3,
            max_batches: 200,
            batch_target: Duration::from_millis(2),
        }
    }

    /// A coarser budget for `--quick`, useful while iterating on the harness
    /// itself. Not fit for publishing numbers.
    pub fn quick() -> Self {
        Self {
            warmup: Duration::from_millis(10),
            measure: Duration::from_millis(30),
            ceiling: Duration::from_millis(200),
            min_batches: 3,
            max_batches: 50,
            batch_target: Duration::from_micros(500),
        }
    }
}

/// The outcome of one measurement.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Median nanoseconds per operation across the collected batches.
    pub median_ns: f64,
    /// Fastest batch, in nanoseconds per operation.
    pub min_ns: f64,
    /// How many batches were collected.
    pub batches: usize,
    /// How many operations each batch performed.
    pub iters_per_batch: u64,
}

impl Sample {
    /// Payload throughput in MiB/s, given the number of payload bytes one
    /// operation covers.
    pub fn mib_per_s(&self, payload_bytes: usize) -> f64 {
        payload_bytes as f64 / (self.median_ns / 1e9) / (1024.0 * 1024.0)
    }
}

/// Times `f`, returning nanoseconds per call.
///
/// `f` is called for effect; its return value is passed through
/// [`black_box`] so the optimizer cannot delete the work.
pub fn measure<F, R>(budget: Budget, mut f: F) -> Sample
where
    F: FnMut() -> R,
{
    // Calibrate: grow the batch until it takes at least `batch_target`. This
    // doubles as the first part of the warmup.
    let mut iters: u64 = 1;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            black_box(f());
        }
        if start.elapsed() >= budget.batch_target || iters >= 1 << 30 {
            break;
        }
        iters *= 2;
    }

    let warmup_start = Instant::now();
    while warmup_start.elapsed() < budget.warmup {
        for _ in 0..iters {
            black_box(f());
        }
    }

    let mut timings: Vec<f64> = Vec::with_capacity(budget.min_batches);
    let sampling_start = Instant::now();
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            black_box(f());
        }
        timings.push(start.elapsed().as_secs_f64() * 1e9 / iters as f64);

        let elapsed = sampling_start.elapsed();
        if timings.len() >= budget.max_batches || elapsed >= budget.ceiling {
            break;
        }
        if timings.len() >= budget.min_batches && elapsed >= budget.measure {
            break;
        }
    }

    timings.sort_by(|a, b| a.partial_cmp(b).expect("timings are finite"));
    Sample {
        median_ns: timings[timings.len() / 2],
        min_ns: timings[0],
        batches: timings.len(),
        iters_per_batch: iters,
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Formats nanoseconds as microseconds, with a precision that keeps roughly
/// three significant figures across the eight orders of magnitude this crate's
/// numbers span.
pub fn us(ns: f64) -> String {
    let value = ns / 1000.0;
    if value >= 1000.0 {
        format!("{value:.0}")
    } else if value >= 100.0 {
        format!("{value:.1}")
    } else if value >= 10.0 {
        format!("{value:.2}")
    } else if value >= 1.0 {
        format!("{value:.3}")
    } else {
        format!("{value:.4}")
    }
}

/// Formats nanoseconds, with the same three-significant-figure policy as
/// [`us`]. Used where the quantity is a single kernel call rather than a whole
/// codec operation, and microseconds would be all leading zeroes.
pub fn ns(value: f64) -> String {
    if value >= 1000.0 {
        format!("{value:.0}")
    } else if value >= 100.0 {
        format!("{value:.1}")
    } else if value >= 10.0 {
        format!("{value:.2}")
    } else {
        format!("{value:.3}")
    }
}

/// Formats a ratio as a speedup, the way every "x faster" column reads.
pub fn speedup(baseline_ns: f64, candidate_ns: f64) -> String {
    format!("{:.2}x", baseline_ns / candidate_ns)
}

/// Formats a MiB/s figure.
pub fn mib(value: f64) -> String {
    if value >= 1000.0 {
        format!("{value:.0}")
    } else if value >= 100.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

/// Formats a byte count the way the tables label shard sizes.
pub fn bytes(value: usize) -> String {
    if value >= 1024 && value % 1024 == 0 {
        format!("{}K", value / 1024)
    } else {
        format!("{value}")
    }
}

// ---------------------------------------------------------------------------
// Markdown output
// ---------------------------------------------------------------------------

/// Column alignment in the emitted markdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    /// Left-aligned, for labels.
    Left,
    /// Right-aligned, for numbers.
    Right,
}

/// A markdown table, padded so the raw text is readable too.
#[derive(Debug)]
pub struct Table {
    headers: Vec<String>,
    aligns: Vec<Align>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// Creates a table whose first column is left-aligned and whose remaining
    /// columns are right-aligned, which is what every table here wants.
    pub fn new<S: AsRef<str>>(headers: &[S]) -> Self {
        let headers: Vec<String> = headers.iter().map(|h| h.as_ref().to_string()).collect();
        let aligns = (0..headers.len())
            .map(|i| if i == 0 { Align::Left } else { Align::Right })
            .collect();
        Self {
            headers,
            aligns,
            rows: Vec::new(),
        }
    }

    /// Left-aligns one column after the fact.
    ///
    /// [`new`](Self::new) assumes a single leading label column, which is what
    /// most tables here have. A table that carries a second label column, such
    /// as a kernel name, wants that one left-aligned too.
    pub fn left_align(&mut self, column: usize) {
        self.aligns[column] = Align::Left;
    }

    /// Appends a row. Its length must match the header count.
    pub fn row<S: AsRef<str>>(&mut self, cells: &[S]) {
        assert_eq!(
            cells.len(),
            self.headers.len(),
            "row does not match the header"
        );
        self.rows
            .push(cells.iter().map(|c| c.as_ref().to_string()).collect());
    }

    /// Renders the table as padded markdown.
    pub fn render(&self) -> String {
        let widths: Vec<usize> = (0..self.headers.len())
            .map(|column| {
                self.rows
                    .iter()
                    .map(|row| row[column].chars().count())
                    .chain(std::iter::once(self.headers[column].chars().count()))
                    .max()
                    .unwrap_or(0)
                    .max(3)
            })
            .collect();

        let mut out = String::new();
        let pad = |text: &str, width: usize, align: Align| -> String {
            let fill = width.saturating_sub(text.chars().count());
            match align {
                Align::Left => format!("{text}{}", " ".repeat(fill)),
                Align::Right => format!("{}{text}", " ".repeat(fill)),
            }
        };

        out.push('|');
        for (i, header) in self.headers.iter().enumerate() {
            let _ = write!(out, " {} |", pad(header, widths[i], self.aligns[i]));
        }
        out.push('\n');

        out.push('|');
        for (i, align) in self.aligns.iter().enumerate() {
            let rule = match align {
                Align::Left => format!(":{}", "-".repeat(widths[i] + 1)),
                Align::Right => format!("{}:", "-".repeat(widths[i] + 1)),
            };
            let _ = write!(out, "{rule}|");
        }
        out.push('\n');

        for row in &self.rows {
            out.push('|');
            for (i, cell) in row.iter().enumerate() {
                let _ = write!(out, " {} |", pad(cell, widths[i], self.aligns[i]));
            }
            out.push('\n');
        }
        out
    }

    /// Prints the table, preceded by the blank line markdown needs before it.
    pub fn print(&self) {
        println!();
        print!("{}", self.render());
    }
}

/// Prints a markdown section heading.
pub fn heading(text: &str) {
    println!("\n### {text}");
}

/// Prints a paragraph of italicised commentary under a heading.
pub fn note(text: &str) {
    println!("\n_{text}_");
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

/// Options parsed from the benchmark binary's arguments.
#[derive(Debug, Clone)]
pub struct Options {
    /// Only run sections whose name contains this string.
    pub filter: Option<String>,
    /// Use [`Budget::quick`].
    pub quick: bool,
}

impl Options {
    /// Reads the arguments cargo and the user passed.
    ///
    /// `cargo bench` hands a `--bench` flag to custom-harness targets, so
    /// unknown flags are ignored rather than treated as errors.
    pub fn from_args() -> Self {
        let mut options = Self {
            filter: None,
            quick: false,
        };
        for arg in std::env::args().skip(1) {
            match arg.as_str() {
                "--quick" => options.quick = true,
                other if other.starts_with('-') => {}
                other => options.filter = Some(other.to_string()),
            }
        }
        options
    }

    /// The budget these options select.
    pub fn budget(&self) -> Budget {
        if self.quick {
            Budget::quick()
        } else {
            Budget::standard()
        }
    }

    /// Whether a section should run.
    pub fn wants(&self, section: &str) -> bool {
        match &self.filter {
            Some(filter) => section.contains(filter.as_str()),
            None => true,
        }
    }
}

/// Prints the machine and build context the numbers below depend on.
pub fn print_environment(title: &str) {
    println!("# {title}");
    println!();
    println!("- target: `{}`", std::env::consts::ARCH);
    println!("- os: `{}`", std::env::consts::OS);
    println!(
        "- profile: `{}`",
        if cfg!(debug_assertions) {
            "debug assertions ON - these numbers are meaningless, run with --release"
        } else {
            "release"
        }
    );
}
