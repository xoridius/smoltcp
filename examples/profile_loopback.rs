//! In-process smoltcp throughput / latency profiler.
//!
//! Wires two `Interface`s back-to-back through a pair of in-memory packet
//! queues (no tun/tap, no syscalls per packet), then drives different
//! traffic shapes through them and reports a comprehensive metrics block:
//!
//!   * throughput (Gbps app, Gbps wire, Mpps)
//!   * per-poll latency: mean / p50 / p90 / p99 / max
//!   * allocation count + bytes allocated (instrumented allocator)
//!   * RSS / footprint (Linux `/proc`; macOS Mach task info)
//!   * smoltcp Socket footprint of relevant sockets
//!   * `cycles_estimate` per packet from a 2.4 GHz reference
//!
//! Designed to run under `perf record`, `valgrind --tool=massif`, or
//! `heaptrack` with no external setup.
//!
//! Usage:
//!   cargo run --release --example profile_loopback -- [--mode bench|trace] <shape> <seconds> [opts...]
//!
//! Shapes:
//!   udp           - 1400B UDP packet forwarding (tunnel analogue)
//!   small         - many small TCP segments, measures per-packet overhead
//!   pingpong      - 128B request/response, latency-bound
//!   firehose      - one-way TCP bulk transfer (cwnd-limited)
//!   many_tcp      - N concurrent TCP echo flows; stresses throughput,
//!                   memory growth, and starvation. Usage:
//!                     profile_loopback many_tcp 5 200 [offload]
//!   many_tcp_fair - N concurrent TCP flows with deterministic per-flow
//!                   scheduling. Usage:
//!                     profile_loopback many_tcp_fair 5 200 [offload]
//!   many_udp      - N concurrent UDP flows; same fairness + memory metrics.
//!                     profile_loopback many_udp 5 200 [offload]
//!   multi_tcp     - dynamic-buffer multi-thread TCP echo workload.
//!   multi_tcp_sink - dynamic-buffer multi-thread one-way TCP sink workload.
//!   all           - runs udp + small + pingpong back-to-back
//!
//! Recommended profiling recipes:
//!   perf record -F 999 --call-graph dwarf -- \
//!     target/release/examples/profile_loopback udp 5
//!   perf report --no-children --stdio --percent-limit 1
//!   valgrind --tool=massif --pages-as-heap=no -- \
//!     target/release/examples/profile_loopback udp 2

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::num::{NonZeroU64, NonZeroUsize};
use std::process::ExitCode;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant as StdInstant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunMode {
    Bench,
    Trace,
}

impl FromStr for RunMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "bench" => Ok(Self::Bench),
            "trace" => Ok(Self::Trace),
            "" => Err("mode cannot be empty; expected bench|trace".to_owned()),
            other => Err(format!("invalid mode '{other}', expected bench|trace")),
        }
    }
}

impl RunMode {
    fn label(self) -> &'static str {
        match self {
            Self::Bench => "bench",
            Self::Trace => "trace",
        }
    }

    fn sample_memory(self) -> bool {
        matches!(self, Self::Bench)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Config {
    mode: RunMode,
    seconds: NonZeroU64,
    shape: TrafficShape,
    offload_checksums: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrafficShape {
    Udp,
    Firehose,
    PingPong,
    Small,
    All,
    ManyTcp {
        flows: NonZeroUsize,
    },
    ManyTcpFair {
        flows: NonZeroUsize,
    },
    ManyUdp {
        flows: NonZeroUsize,
    },
    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    MultiTcp {
        threads: NonZeroUsize,
        flows_per_thread: NonZeroUsize,
    },
    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    MultiTcpSink {
        threads: NonZeroUsize,
        flows_per_thread: NonZeroUsize,
    },
    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    Churn {
        rate: NonZeroUsize,
    },
    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    IdleHot {
        idle: usize,
        active: usize,
    },
}

impl TrafficShape {
    fn flow_count(self) -> Option<usize> {
        match self {
            Self::ManyTcp { flows } | Self::ManyTcpFair { flows } | Self::ManyUdp { flows } => {
                Some(flows.get())
            }
            _ => None,
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Config, String> {
    let mut args = args.into_iter().peekable();
    let mut explicit_mode = false;
    let mode = match args.peek().map(String::as_str) {
        Some("--mode") => {
            explicit_mode = true;
            args.next();
            args.next()
                .ok_or_else(|| "missing value for --mode, expected bench|trace".to_owned())?
                .parse()?
        }
        Some(arg) if arg.starts_with("--mode=") => {
            explicit_mode = true;
            let value = arg["--mode=".len()..].to_owned();
            args.next();
            value.parse()?
        }
        _ => RunMode::Bench,
    };

    let shape_name = args
        .next()
        .ok_or_else(|| "missing traffic shape".to_owned())?;
    if is_mode_option(&shape_name) {
        return Err(mode_position_error(explicit_mode));
    }
    if shape_name.starts_with('-') {
        return Err(format!("unknown option '{shape_name}'"));
    }

    let seconds = next_nonzero_u64(&mut args, "seconds", explicit_mode)?;

    let shape = match shape_name.as_str() {
        "udp" => TrafficShape::Udp,
        "firehose" => TrafficShape::Firehose,
        "pingpong" => TrafficShape::PingPong,
        "small" => TrafficShape::Small,
        "all" => TrafficShape::All,
        "many_tcp" => TrafficShape::ManyTcp {
            flows: next_nonzero_usize(&mut args, "flows", explicit_mode)?,
        },
        "many_tcp_fair" => TrafficShape::ManyTcpFair {
            flows: next_nonzero_usize(&mut args, "flows", explicit_mode)?,
        },
        "many_udp" => TrafficShape::ManyUdp {
            flows: next_nonzero_usize(&mut args, "flows", explicit_mode)?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "multi_tcp" => TrafficShape::MultiTcp {
            threads: next_nonzero_usize(&mut args, "threads", explicit_mode)?,
            flows_per_thread: next_nonzero_usize(&mut args, "flows per thread", explicit_mode)?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "multi_tcp_sink" => TrafficShape::MultiTcpSink {
            threads: next_nonzero_usize(&mut args, "threads", explicit_mode)?,
            flows_per_thread: next_nonzero_usize(&mut args, "flows per thread", explicit_mode)?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "churn" => TrafficShape::Churn {
            rate: next_nonzero_usize(&mut args, "rate", explicit_mode)?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "idle_hot" => {
            let idle = next_usize(&mut args, "idle flows", explicit_mode)?;
            let active = next_usize(&mut args, "active flows", explicit_mode)?;
            if idle == 0 && active == 0 {
                return Err("idle_hot requires at least one idle or active flow".to_owned());
            }
            TrafficShape::IdleHot { idle, active }
        }
        #[cfg(not(feature = "socket-tcp-dynamic-buffer"))]
        "multi_tcp" | "multi_tcp_sink" | "churn" | "idle_hot" => {
            return Err(format!(
                "traffic shape '{shape_name}' requires feature 'socket-tcp-dynamic-buffer'"
            ));
        }
        other => return Err(format!("unknown traffic shape '{other}'")),
    };

    let offload_checksums = parse_offload(&mut args, explicit_mode)?;
    Ok(Config {
        mode,
        seconds,
        shape,
        offload_checksums,
    })
}

fn next_value(
    args: &mut impl Iterator<Item = String>,
    name: &str,
    explicit_mode: bool,
) -> Result<String, String> {
    let value = args.next().ok_or_else(|| format!("missing {name}"))?;
    if is_mode_option(&value) {
        return Err(mode_position_error(explicit_mode));
    }
    if value.starts_with('-') {
        return Err(format!("unknown option '{value}'"));
    }
    Ok(value)
}

fn next_nonzero_u64(
    args: &mut impl Iterator<Item = String>,
    name: &str,
    explicit_mode: bool,
) -> Result<NonZeroU64, String> {
    let value = next_value(args, name, explicit_mode)?;
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("invalid {name} '{value}': expected a non-zero integer"))?;
    NonZeroU64::new(parsed).ok_or_else(|| format!("{name} must be non-zero"))
}

fn next_nonzero_usize(
    args: &mut impl Iterator<Item = String>,
    name: &str,
    explicit_mode: bool,
) -> Result<NonZeroUsize, String> {
    let value = next_value(args, name, explicit_mode)?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid {name} '{value}': expected a non-zero integer"))?;
    NonZeroUsize::new(parsed).ok_or_else(|| format!("{name} must be non-zero"))
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn next_usize(
    args: &mut impl Iterator<Item = String>,
    name: &str,
    explicit_mode: bool,
) -> Result<usize, String> {
    let value = next_value(args, name, explicit_mode)?;
    value
        .parse()
        .map_err(|_| format!("invalid {name} '{value}': expected a non-negative integer"))
}

fn parse_offload(
    args: &mut impl Iterator<Item = String>,
    explicit_mode: bool,
) -> Result<bool, String> {
    let Some(value) = args.next() else {
        return Ok(false);
    };
    if is_mode_option(&value) {
        return Err(mode_position_error(explicit_mode));
    }
    if value.starts_with('-') {
        return Err(format!("unknown option '{value}'"));
    }
    if !matches!(value.as_str(), "offload" | "1" | "true") {
        return Err(format!(
            "invalid offload value '{value}': expected offload|1|true"
        ));
    }
    if let Some(trailing) = args.next() {
        if is_mode_option(&trailing) {
            return Err(mode_position_error(explicit_mode));
        }
        if trailing.starts_with('-') {
            return Err(format!("unknown option '{trailing}'"));
        }
        return Err(format!("unexpected trailing argument '{trailing}'"));
    }
    Ok(true)
}

fn is_mode_option(value: &str) -> bool {
    value == "--mode" || value.starts_with("--mode=")
}

fn mode_position_error(explicit_mode: bool) -> String {
    if explicit_mode {
        "--mode may be specified only once and must appear before the traffic shape".to_owned()
    } else {
        "--mode must appear before the traffic shape".to_owned()
    }
}

/// Tracks every allocation routed through the global allocator. We only count
/// counter atomics (Relaxed), so the overhead is two adds per alloc/free.
#[allow(dead_code)] // unused when `dhat-heap` feature swaps the global allocator
struct CountingAlloc;
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static FREE_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static A: CountingAlloc = CountingAlloc;

// dhat::Alloc wraps System and captures per-callstack allocation attribution.
// When this feature is on, CountingAlloc is unused (kept compiled so the rest
// of the file still references its ALLOC_* counters; they just stay at zero).
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static A: dhat::Alloc = dhat::Alloc;

/// Read process resident memory in bytes.
fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kib: u64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                return kib * 1024;
            }
        }
        0
    }
    #[cfg(target_os = "macos")]
    {
        macos_phys_footprint_bytes().unwrap_or(0)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn mach_task_self() -> u32;
    fn task_info(task: u32, flavor: i32, info: *mut i32, count: *mut u32) -> i32;
}

#[cfg(target_os = "macos")]
fn macos_phys_footprint_bytes() -> Option<u64> {
    const KERN_SUCCESS: i32 = 0;
    const TASK_VM_INFO: i32 = 22;

    let mut info = TaskVmInfo::default();
    let mut count = (core::mem::size_of::<TaskVmInfo>() / core::mem::size_of::<i32>()) as u32;
    let kr = unsafe {
        task_info(
            mach_task_self(),
            TASK_VM_INFO,
            (&mut info as *mut TaskVmInfo).cast::<i32>(),
            &mut count,
        )
    };
    if kr == KERN_SUCCESS {
        Some(info.phys_footprint.max(info.resident_size))
    } else {
        None
    }
}

/// Read voluntary + involuntary context-switch counts from
/// /proc/self/status on Linux. macOS users should use Instruments System
/// Trace for per-thread context-switch analysis.
/// Returns `(voluntary, nonvoluntary)`.
/// Voluntary = process blocked / yielded (rare in our spin loops);
/// nonvoluntary = preempted by the scheduler. Multi-thread shapes
/// expect a small voluntary count and a nonvoluntary count proportional
/// to N_threads × wall_time / time_slice.
fn ctxsw_counts() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let mut vol = 0u64;
        let mut nvol = 0u64;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("voluntary_ctxt_switches:") {
                vol = rest.trim().parse().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("nonvoluntary_ctxt_switches:") {
                nvol = rest.trim().parse().unwrap_or(0);
            }
        }
        (vol, nvol)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (0, 0)
    }
}

/// Log-linear latency histogram: each power-of-two range [2^k, 2^(k+1)) is
/// split into `SUBBUCKETS=16` linear sub-buckets of width `2^k / SUBBUCKETS`,
/// giving ~6% relative error across the full range. Constant per-sample cost,
/// fixed-size array, no allocations.
struct Histo {
    /// `buckets[k][s]` covers `[2^k + s*(2^k/SUBBUCKETS), 2^k + (s+1)*(2^k/SUBBUCKETS))`
    /// for k >= 1. Row k=0 covers `[0, 1)` (effectively unused).
    buckets: [[u64; Self::SUBBUCKETS]; 30],
    samples: u64,
    sum_ns: u64,
    max_ns: u64,
    min_ns: u64,
}

impl Histo {
    const SUBBUCKET_BITS: u32 = 4;
    const SUBBUCKETS: usize = 1 << Self::SUBBUCKET_BITS; // 16

    fn new() -> Self {
        Self {
            buckets: [[0; Self::SUBBUCKETS]; 30],
            samples: 0,
            sum_ns: 0,
            max_ns: 0,
            min_ns: u64::MAX,
        }
    }

    /// Locate (major, sub) for a value. `major` is `floor(log2(ns))` clamped
    /// into the table; `sub` is the linear index within the major range.
    #[inline]
    fn locate(ns: u64) -> (usize, usize) {
        if ns <= 1 {
            return (0, 0);
        }
        let major = 63 - ns.leading_zeros();
        let major = (major as usize).min(29);
        let span = 1u64 << major;
        // Sub-bucket width within [span, 2*span):
        //   width = span / SUBBUCKETS = span >> SUBBUCKET_BITS
        // For major < SUBBUCKET_BITS, width rounds down to 0; just route to sub=0.
        let sub = if major >= Self::SUBBUCKET_BITS as usize {
            (((ns - span) << Self::SUBBUCKET_BITS) / span) as usize
        } else {
            (ns - span) as usize
        };
        let sub = sub.min(Self::SUBBUCKETS - 1);
        (major, sub)
    }

    /// Upper bound (exclusive) of a (major, sub) bucket — used to print
    /// percentiles as nanosecond values.
    #[inline]
    fn upper(major: usize, sub: usize) -> u64 {
        if major == 0 {
            return 1;
        }
        let span = 1u64 << major;
        if major >= Self::SUBBUCKET_BITS as usize {
            span + (sub as u64 + 1) * (span >> Self::SUBBUCKET_BITS)
        } else {
            span + sub as u64 + 1
        }
    }

    #[inline]
    fn record(&mut self, ns: u64) {
        self.samples += 1;
        self.sum_ns += ns;
        if ns > self.max_ns {
            self.max_ns = ns;
        }
        if ns < self.min_ns {
            self.min_ns = ns;
        }
        let (i, j) = Self::locate(ns);
        self.buckets[i][j] += 1;
    }

    fn percentile(&self, p: f64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let target = ((self.samples as f64 * p).ceil() as u64).max(1);
        let mut cum = 0u64;
        for (i, row) in self.buckets.iter().enumerate() {
            for (j, &c) in row.iter().enumerate() {
                cum += c;
                if cum >= target {
                    return Self::upper(i, j).saturating_sub(1);
                }
            }
        }
        self.max_ns
    }

    fn mean(&self) -> u64 {
        self.sum_ns.checked_div(self.samples).unwrap_or(0)
    }
}

/// Reference CPU frequency used to estimate cycles from elapsed wall time.
/// Reasonable approximation for typical x86_64/aarch64 server CPUs in this era.
const REF_CPU_GHZ: f64 = 2.4;

/// Per-flow throughput statistics computed at the end of a many-flow run.
///
/// `jain` is Jain's fairness index (Jain, Chiu, Hawe, 1984), defined as
/// `(Σ xᵢ)² / (n · Σ xᵢ²)`. 1.0 means every flow received exactly the same
/// number of bytes; 1/n means one flow got everything. >0.95 is generally
/// considered "fair" in network-research literature.
struct Fairness {
    n: usize,
    total: u64,
    min: u64,
    max: u64,
    /// Index of the flow with the smallest byte count (useful to check whether
    /// starvation lands on the same handle across runs).
    min_flow: usize,
    /// Index of the flow with the largest byte count.
    max_flow: usize,
    mean: f64,
    stddev: f64,
    /// Coefficient of variation = stddev / mean.
    cv: f64,
    jain: f64,
    /// Per-flow byte counts, sorted ascending — used to print percentiles and
    /// to identify starved flows.
    sorted: Vec<u64>,
    /// Flows below 10% of the mean; nonzero values are a starvation flag.
    starved: usize,
    /// Flows that received zero bytes — a strong starvation signal.
    zero_flows: usize,
}

impl Fairness {
    fn from(per_flow: &[u64]) -> Self {
        let n = per_flow.len();
        let total: u64 = per_flow.iter().sum();
        let (min_flow, &min) = per_flow
            .iter()
            .enumerate()
            .min_by_key(|&(_, &v)| v)
            .unwrap_or((0, &0));
        let (max_flow, &max) = per_flow
            .iter()
            .enumerate()
            .max_by_key(|&(_, &v)| v)
            .unwrap_or((0, &0));
        let mean = total as f64 / n.max(1) as f64;
        let var = if n == 0 {
            0.0
        } else {
            per_flow
                .iter()
                .map(|&x| (x as f64 - mean).powi(2))
                .sum::<f64>()
                / n as f64
        };
        let stddev = var.sqrt();
        let cv = if mean > 0.0 { stddev / mean } else { 0.0 };
        // Jain's fairness index.
        let sum_sq: f64 = per_flow.iter().map(|&x| (x as f64).powi(2)).sum();
        let jain = if sum_sq > 0.0 {
            let s = total as f64;
            (s * s) / (n as f64 * sum_sq)
        } else {
            0.0
        };
        let mut sorted = per_flow.to_vec();
        sorted.sort_unstable();
        let starved = per_flow
            .iter()
            .filter(|&&x| (x as f64) < 0.1 * mean)
            .count();
        let zero_flows = per_flow.iter().filter(|&&x| x == 0).count();
        Self {
            n,
            total,
            min,
            max,
            min_flow,
            max_flow,
            mean,
            stddev,
            cv,
            jain,
            sorted,
            starved,
            zero_flows,
        }
    }

    fn at(&self, p: f64) -> u64 {
        if self.sorted.is_empty() {
            return 0;
        }
        let idx = ((self.sorted.len() as f64 * p) as usize).min(self.sorted.len() - 1);
        self.sorted[idx]
    }

    fn print(&self, label: &str) {
        println!();
        println!("  per-flow {label} (bytes):");
        println!(
            "    flows: {:>5}     total: {:>14}     mean: {:>12.1}",
            self.n, self.total, self.mean
        );
        println!(
            "    min:   {:>14} (flow #{:<5})  p10:   {:>14}  p50: {:>12}",
            self.min,
            self.min_flow,
            self.at(0.10),
            self.at(0.50)
        );
        println!(
            "    p90:   {:>14}                  p99:   {:>14}  max: {:>12} (flow #{})",
            self.at(0.90),
            self.at(0.99),
            self.max,
            self.max_flow
        );
        println!(
            "    stddev:{:>14.1}     CV:    {:>14.4}     Jain: {:>12.4}",
            self.stddev, self.cv, self.jain
        );
        let fairness_verdict = if self.jain >= 0.95 {
            "FAIR"
        } else if self.jain >= 0.80 {
            "uneven"
        } else {
            "UNFAIR"
        };
        let starve_verdict = if self.zero_flows > 0 {
            "STARVATION (zero-byte flows present)"
        } else if self.starved > 0 {
            "mild starvation (some flows < 10% of mean)"
        } else {
            "no starvation"
        };
        println!(
            "    verdict: {fairness_verdict} ({starve_verdict}); zero_flows: {}, <10%-of-mean: {}",
            self.zero_flows, self.starved
        );
    }
}

/// Periodic RSS sample. We collect (elapsed_ms, rss, alloc_bytes) snapshots
/// during a many-flow run so we can see whether memory grows over time
/// (= leak) or plateaus (= bounded, healthy).
struct MemTrace {
    samples: Vec<(u64, u64, u64)>, // (ms_since_start, rss_bytes, alloc_bytes_delta)
    start_wall: StdInstant,
    start_alloc: u64,
}

impl MemTrace {
    fn start() -> Self {
        Self {
            samples: Vec::with_capacity(64),
            start_wall: StdInstant::now(),
            start_alloc: ALLOC_BYTES.load(Ordering::Relaxed),
        }
    }
    /// Cheap to call from the hot loop: only takes a sample if at least
    /// `interval_ms` have elapsed since the last one.
    fn maybe_sample(&mut self, interval_ms: u64) {
        let now = StdInstant::now();
        let elapsed = now.duration_since(self.start_wall).as_millis() as u64;
        let last = self.samples.last().map(|s| s.0).unwrap_or(0);
        if self.samples.is_empty() || elapsed >= last + interval_ms {
            let rss = rss_bytes();
            let alloc_now = ALLOC_BYTES.load(Ordering::Relaxed);
            self.samples
                .push((elapsed, rss, alloc_now - self.start_alloc));
        }
    }
    fn print(&self) {
        if self.samples.is_empty() {
            return;
        }
        println!();
        println!("  memory trace (snapshot every ~250 ms):");
        println!(
            "    {:>8}   {:>10}   {:>10}",
            "t_ms", "rss_bytes", "alloc_delta"
        );
        for (t, rss, alloc) in &self.samples {
            println!("    {:>8}   {:>10}   {:>10}", t, rss, alloc);
        }
        // Detect monotonic RSS growth as a leak signal: if last RSS is
        // > 1.5× the median RSS, flag it.
        let mut rss_sorted: Vec<u64> = self.samples.iter().map(|s| s.1).collect();
        rss_sorted.sort_unstable();
        let median = rss_sorted[rss_sorted.len() / 2];
        let last = self.samples.last().unwrap().1;
        let verdict = if last as f64 > 1.5 * median as f64 {
            "GROWTH (possible leak)"
        } else {
            "bounded"
        };
        println!("    RSS verdict: {verdict} (last={last}, median={median})");
    }
}

/// Take a wall-clock time sample roughly every `LAT_SAMPLE_EVERY` polls. We
/// don't sample every poll because `Instant::now()` is ~30 cycles (via vDSO),
/// large enough that recording 6M+ samples per run measurably inflates the
/// throughput numbers. Sampling 1-in-32 keeps the latency histogram dense
/// (hundreds of thousands of samples per second) at <0.5% overhead.
const LAT_SAMPLE_EVERY: u64 = 32;

/// Helper around `Histo` that throttles sample acquisition. The wall-clock
/// timing path is gated on `iter & (LAT_SAMPLE_EVERY-1) == 0` so the hot loop
/// has a single predictable branch.
struct SampledTimer {
    iter: u64,
    histo: Histo,
}

impl SampledTimer {
    fn new() -> Self {
        Self {
            iter: 0,
            histo: Histo::new(),
        }
    }
    /// Run `body`; on sampling iterations, time it with `StdInstant::now()` and
    /// add the result to the histogram. The closure form lets us skip the
    /// vDSO clock-read entirely on the 31-in-32 non-sampling iterations.
    #[inline(always)]
    fn measure<F: FnOnce()>(&mut self, body: F) {
        if self.iter & (LAT_SAMPLE_EVERY - 1) == 0 {
            let t0 = StdInstant::now();
            body();
            self.histo.record(t0.elapsed().as_nanos() as u64);
        } else {
            body();
        }
        self.iter = self.iter.wrapping_add(1);
    }
}

use smoltcp::iface::{Config as InterfaceConfig, Interface, SocketSet};
use smoltcp::phy::{self, ChecksumCapabilities, Device, DeviceCapabilities, Medium};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};

/// One in-flight packet. Buffer capacity is reused for the lifetime of the
/// test: we set `len` to the on-wire size on emit, and the consumer reads up
/// to `len` and recycles the empty buffer back through `Lane::pool`.
struct Packet {
    buf: Vec<u8>,
    len: usize,
}

impl Packet {
    fn with_capacity(cap: usize) -> Self {
        Self {
            buf: vec![0u8; cap],
            len: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct LaneStats {
    tx_backpressure: u64,
    rx_backpressure: u64,
    max_queue_depth: usize,
    max_pool_depth: usize,
    reserved_payload_bytes: usize,
    reserved_packet_slot_bytes: usize,
}

impl LaneStats {
    fn merge(&mut self, other: Self) {
        self.tx_backpressure += other.tx_backpressure;
        self.rx_backpressure += other.rx_backpressure;
        self.max_queue_depth = self.max_queue_depth.max(other.max_queue_depth);
        self.max_pool_depth = self.max_pool_depth.max(other.max_pool_depth);
        self.reserved_payload_bytes += other.reserved_payload_bytes;
        self.reserved_packet_slot_bytes += other.reserved_packet_slot_bytes;
    }

    fn reserved_total_bytes(&self) -> usize {
        self.reserved_payload_bytes + self.reserved_packet_slot_bytes
    }
}

/// One direction of the paired link.
///
/// `queue` holds packets in flight (FIFO). `pool` holds empty buffers we
/// rotate through, so steady-state runs do zero allocations.
struct Lane {
    queue: VecDeque<Packet>,
    pool: Vec<Packet>,
    stats: LaneStats,
}

impl Lane {
    fn new(mtu: usize, depth: usize) -> Self {
        let queue = VecDeque::with_capacity(depth);
        let mut pool = Vec::with_capacity(depth);
        for _ in 0..depth {
            pool.push(Packet::with_capacity(mtu));
        }
        let stats = LaneStats {
            max_pool_depth: pool.len(),
            reserved_payload_bytes: pool.iter().map(|packet| packet.buf.capacity()).sum(),
            reserved_packet_slot_bytes: (queue.capacity() + pool.capacity())
                * core::mem::size_of::<Packet>(),
            ..LaneStats::default()
        };
        Self { queue, pool, stats }
    }

    fn try_take_packet(&mut self) -> Option<Packet> {
        self.pool.pop()
    }

    fn queue_pkt(&mut self, pkt: Packet) {
        self.queue.push_back(pkt);
        self.stats.max_queue_depth = self.stats.max_queue_depth.max(self.queue.len());
    }

    fn return_pkt(&mut self, mut pkt: Packet) {
        pkt.len = 0;
        self.pool.push(pkt);
        self.stats.max_pool_depth = self.stats.max_pool_depth.max(self.pool.len());
    }

    fn stats(&self) -> LaneStats {
        self.stats
    }
}

type LaneRc = Rc<RefCell<Lane>>;

fn collect_lane_stats(lanes: &[&LaneRc]) -> LaneStats {
    let mut stats = LaneStats::default();
    for lane in lanes {
        stats.merge(lane.borrow().stats());
    }
    stats
}

fn print_lane_stats(label: &str, stats: LaneStats) {
    println!();
    println!("  lane stats ({label}):");
    println!("    TX backpressure:      {}", stats.tx_backpressure);
    println!("    RX backpressure:      {}", stats.rx_backpressure);
    println!("    max queue depth:      {}", stats.max_queue_depth);
    println!("    max pool depth:       {}", stats.max_pool_depth);
    println!(
        "    reserved payload:     {} bytes ({} KiB)",
        stats.reserved_payload_bytes,
        stats.reserved_payload_bytes / 1024
    );
    println!(
        "    reserved pkt slots:   {} bytes ({} KiB)",
        stats.reserved_packet_slot_bytes,
        stats.reserved_packet_slot_bytes / 1024
    );
    println!(
        "    reserved total:       {} bytes ({} KiB)  (harness packet pool, not smoltcp socket memory)",
        stats.reserved_total_bytes(),
        stats.reserved_total_bytes() / 1024
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Result<Config, String> {
        parse_args(values.iter().map(|value| (*value).to_owned()))
    }

    fn nz64(value: u64) -> NonZeroU64 {
        std::num::NonZeroU64::new(value).unwrap()
    }

    fn nz(value: usize) -> NonZeroUsize {
        std::num::NonZeroUsize::new(value).unwrap()
    }

    fn config(mode: RunMode, seconds: u64, shape: TrafficShape, offload: bool) -> Config {
        Config {
            mode,
            seconds: nz64(seconds),
            shape,
            offload_checksums: offload,
        }
    }

    fn assert_commands_parse(commands: &[&str], expected_count: usize) {
        assert_eq!(commands.len(), expected_count);
        for command in commands {
            let result = parse_args(command.split_ascii_whitespace().map(str::to_owned));
            assert!(result.is_ok(), "command {command:?}: {result:?}");
        }
    }

    fn assert_errors(cases: Vec<(Vec<&str>, &str)>) {
        for (input, expected) in cases {
            let error = args(&input).unwrap_err();
            assert!(
                error.contains(expected),
                "input {input:?}: expected {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn parse_args_returns_complete_config_for_every_static_shape() {
        let cases: &[(&[&str], Config)] = &[
            (
                &["udp", "1"],
                config(RunMode::Bench, 1, TrafficShape::Udp, false),
            ),
            (
                &["--mode", "trace", "firehose", "2", "offload"],
                config(RunMode::Trace, 2, TrafficShape::Firehose, true),
            ),
            (
                &["--mode=bench", "pingpong", "3", "1"],
                config(RunMode::Bench, 3, TrafficShape::PingPong, true),
            ),
            (
                &["small", "4", "true"],
                config(RunMode::Bench, 4, TrafficShape::Small, true),
            ),
            (
                &["all", "5"],
                config(RunMode::Bench, 5, TrafficShape::All, false),
            ),
            (
                &["many_tcp", "6", "7"],
                config(
                    RunMode::Bench,
                    6,
                    TrafficShape::ManyTcp { flows: nz(7) },
                    false,
                ),
            ),
            (
                &["many_tcp_fair", "8", "9", "offload"],
                config(
                    RunMode::Bench,
                    8,
                    TrafficShape::ManyTcpFair { flows: nz(9) },
                    true,
                ),
            ),
            (
                &["many_udp", "10", "11", "1"],
                config(
                    RunMode::Bench,
                    10,
                    TrafficShape::ManyUdp { flows: nz(11) },
                    true,
                ),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(args(input), Ok(*expected), "input: {input:?}");
        }
    }

    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    #[test]
    fn parse_args_returns_complete_config_for_every_dynamic_shape() {
        let cases: &[(&[&str], Config)] = &[
            (
                &["multi_tcp", "1", "2", "3"],
                config(
                    RunMode::Bench,
                    1,
                    TrafficShape::MultiTcp {
                        threads: nz(2),
                        flows_per_thread: nz(3),
                    },
                    false,
                ),
            ),
            (
                &["multi_tcp_sink", "4", "5", "6", "offload"],
                config(
                    RunMode::Bench,
                    4,
                    TrafficShape::MultiTcpSink {
                        threads: nz(5),
                        flows_per_thread: nz(6),
                    },
                    true,
                ),
            ),
            (
                &["churn", "7", "8", "1"],
                config(RunMode::Bench, 7, TrafficShape::Churn { rate: nz(8) }, true),
            ),
            (
                &["idle_hot", "9", "10", "0", "true"],
                config(
                    RunMode::Bench,
                    9,
                    TrafficShape::IdleHot {
                        idle: 10,
                        active: 0,
                    },
                    true,
                ),
            ),
            (
                &["idle_hot", "9", "0", "10"],
                config(
                    RunMode::Bench,
                    9,
                    TrafficShape::IdleHot {
                        idle: 0,
                        active: 10,
                    },
                    false,
                ),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(args(input), Ok(*expected), "input: {input:?}");
        }
    }

    #[test]
    fn parse_args_accepts_each_mode_and_offload_spelling() {
        let cases: &[(&[&str], RunMode, bool)] = &[
            (&["udp", "1"], RunMode::Bench, false),
            (&["--mode", "bench", "udp", "1"], RunMode::Bench, false),
            (&["--mode=bench", "udp", "1"], RunMode::Bench, false),
            (&["--mode", "trace", "udp", "1"], RunMode::Trace, false),
            (&["--mode=trace", "udp", "1"], RunMode::Trace, false),
            (&["udp", "1", "offload"], RunMode::Bench, true),
            (&["udp", "1", "1"], RunMode::Bench, true),
            (&["udp", "1", "true"], RunMode::Bench, true),
        ];

        for (input, expected_mode, expected_offload) in cases {
            let config = args(input).unwrap();
            assert_eq!(config.mode, *expected_mode, "input: {input:?}");
            assert_eq!(
                config.offload_checksums, *expected_offload,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn parse_args_accepts_maximum_representable_numbers() {
        assert_eq!(
            parse_args(vec![
                "many_tcp".to_owned(),
                u64::MAX.to_string(),
                usize::MAX.to_string(),
            ]),
            Ok(config(
                RunMode::Bench,
                u64::MAX,
                TrafficShape::ManyTcp {
                    flows: nz(usize::MAX),
                },
                false,
            ))
        );
    }

    #[test]
    fn full_gate_static_command_list_has_26_parseable_commands() {
        assert_commands_parse(
            &[
                "--mode bench udp 3",
                "--mode bench udp 3 offload",
                "--mode bench firehose 3",
                "--mode bench firehose 3 offload",
                "--mode bench pingpong 3",
                "--mode bench pingpong 3 offload",
                "--mode bench small 3",
                "--mode bench small 3 offload",
                "--mode bench many_tcp 3 8",
                "--mode bench many_tcp 3 8 offload",
                "--mode bench many_tcp_fair 3 8",
                "--mode bench many_tcp_fair 3 8 offload",
                "--mode bench many_udp 3 8",
                "--mode bench many_udp 3 8 offload",
                "--mode bench many_tcp 3 50",
                "--mode bench many_tcp 3 50 offload",
                "--mode bench many_tcp_fair 3 50",
                "--mode bench many_tcp_fair 3 50 offload",
                "--mode bench many_udp 3 50",
                "--mode bench many_udp 3 50 offload",
                "--mode bench many_tcp 3 100",
                "--mode bench many_tcp 3 100 offload",
                "--mode bench many_tcp_fair 3 100",
                "--mode bench many_tcp_fair 3 100 offload",
                "--mode bench many_udp 3 100",
                "--mode bench many_udp 3 100 offload",
            ],
            26,
        );
    }

    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    #[test]
    fn full_gate_dynamic_command_list_has_10_parseable_commands() {
        assert_commands_parse(
            &[
                "--mode bench multi_tcp 3 2 50",
                "--mode bench multi_tcp 3 2 50 offload",
                "--mode bench multi_tcp_sink 3 2 50",
                "--mode bench multi_tcp_sink 3 2 50 offload",
                "--mode bench churn 3 500",
                "--mode bench churn 3 500 offload",
                "--mode bench idle_hot 3 1000 0",
                "--mode bench idle_hot 3 1000 0 offload",
                "--mode bench idle_hot 3 1000 10",
                "--mode bench idle_hot 3 1000 10 offload",
            ],
            10,
        );
    }

    #[test]
    fn parse_args_rejects_invalid_static_commands() {
        let usize_overflow = (usize::MAX as u128 + 1).to_string();
        assert_errors(vec![
            (vec![], "missing traffic shape"),
            (vec!["udp"], "missing seconds"),
            (vec!["udp", "0"], "seconds must be non-zero"),
            (vec!["udp", ""], "invalid seconds ''"),
            (vec!["udp", "nope"], "invalid seconds 'nope'"),
            (
                vec!["udp", "18446744073709551616"],
                "invalid seconds '18446744073709551616'",
            ),
            (vec!["unknown", "1"], "unknown traffic shape 'unknown'"),
            (vec!["", "1"], "unknown traffic shape ''"),
            (vec!["--wat", "udp", "1"], "unknown option '--wat'"),
            (vec!["--mode"], "missing value for --mode"),
            (vec!["--mode=", "udp", "1"], "mode cannot be empty"),
            (vec!["--mode", "fast", "udp", "1"], "invalid mode 'fast'"),
            (
                vec!["--mode", "bench", "--mode", "trace", "udp", "1"],
                "--mode may be specified only once",
            ),
            (
                vec!["udp", "1", "--mode", "trace"],
                "--mode must appear before the traffic shape",
            ),
            (vec!["udp", "1", "false"], "invalid offload value 'false'"),
            (
                vec!["udp", "1", "offload", "extra"],
                "unexpected trailing argument 'extra'",
            ),
            (
                vec!["udp", "1", "offload", "--wat"],
                "unknown option '--wat'",
            ),
            (vec!["udp", "offload", "1"], "invalid seconds 'offload'"),
            (vec!["many_tcp", "1"], "missing flows"),
            (vec!["many_tcp", "1", "0"], "flows must be non-zero"),
            (vec!["many_tcp", "1", "nope"], "invalid flows 'nope'"),
            (
                vec!["many_tcp", "1", usize_overflow.as_str()],
                "invalid flows",
            ),
            (
                vec!["many_udp", "1", "2", "TRUE"],
                "invalid offload value 'TRUE'",
            ),
        ]);
    }

    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    #[test]
    fn parse_args_rejects_invalid_dynamic_commands() {
        let usize_overflow = (usize::MAX as u128 + 1).to_string();
        assert_errors(vec![
            (vec!["multi_tcp", "1"], "missing threads"),
            (vec!["multi_tcp", "1", "0", "2"], "threads must be non-zero"),
            (
                vec!["multi_tcp", "1", "nope", "2"],
                "invalid threads 'nope'",
            ),
            (
                vec!["multi_tcp", "1", usize_overflow.as_str(), "2"],
                "invalid threads",
            ),
            (vec!["multi_tcp_sink", "1", "2"], "missing flows per thread"),
            (
                vec!["multi_tcp_sink", "1", "2", "0"],
                "flows per thread must be non-zero",
            ),
            (
                vec!["multi_tcp_sink", "1", "2", "nope"],
                "invalid flows per thread 'nope'",
            ),
            (vec!["churn", "1"], "missing rate"),
            (vec!["churn", "1", "0"], "rate must be non-zero"),
            (vec!["churn", "1", "nope"], "invalid rate 'nope'"),
            (vec!["idle_hot", "1"], "missing idle flows"),
            (vec!["idle_hot", "1", "2"], "missing active flows"),
            (
                vec!["idle_hot", "1", "nope", "2"],
                "invalid idle flows 'nope'",
            ),
            (
                vec!["idle_hot", "1", "2", "nope"],
                "invalid active flows 'nope'",
            ),
            (
                vec!["idle_hot", "1", "0", "0"],
                "idle_hot requires at least one idle or active flow",
            ),
            (
                vec!["idle_hot", "1", "2", "3", "offload", "extra"],
                "unexpected trailing argument 'extra'",
            ),
        ]);
    }

    #[cfg(not(feature = "socket-tcp-dynamic-buffer"))]
    #[test]
    fn parse_args_reports_the_required_feature_for_dynamic_shapes() {
        for shape in ["multi_tcp", "multi_tcp_sink", "churn", "idle_hot"] {
            let input = [shape, "1", "1", "1"];
            assert_eq!(
                args(&input),
                Err(format!(
                    "traffic shape '{shape}' requires feature 'socket-tcp-dynamic-buffer'"
                ))
            );
        }
    }

    fn lane(mtu: usize, depth: usize) -> LaneRc {
        Rc::new(RefCell::new(Lane::new(mtu, depth)))
    }

    fn queue_packet(lane: &LaneRc, bytes: &[u8]) {
        let mut lane = lane.borrow_mut();
        let mut packet = lane
            .try_take_packet()
            .expect("packet pool exhausted in test setup");
        packet.buf[..bytes.len()].copy_from_slice(bytes);
        packet.len = bytes.len();
        lane.queue_pkt(packet);
    }

    fn device(tx: &LaneRc, rx: &LaneRc, mtu: usize) -> PairedDevice {
        PairedDevice::new(tx.clone(), rx.clone(), mtu, false)
    }

    #[test]
    fn transmit_reserves_packet_before_returning_token() {
        let tx = lane(64, 2);
        let rx = lane(64, 2);
        let mut device = device(&tx, &rx, 64);

        let token = device.transmit(Instant::from_millis(0)).unwrap();

        assert_eq!(tx.borrow().pool.len(), 1);
        drop(token);
    }

    #[test]
    fn standalone_transmit_preserves_last_response_credit() {
        let tx = lane(64, 2);
        let rx = lane(64, 2);
        queue_packet(&tx, &[1]);
        let mut device = device(&tx, &rx, 64);

        assert!(device.transmit(Instant::from_millis(0)).is_none());
        assert_eq!(tx.borrow().pool.len(), 1);
        assert_eq!(tx.borrow().queue.len(), 1);
        assert_eq!(tx.borrow().stats.tx_backpressure, 1);
    }

    #[test]
    fn unused_transmit_token_refunds_reservation() {
        let tx = lane(64, 2);
        let rx = lane(64, 2);
        let mut device = device(&tx, &rx, 64);

        let token = device.transmit(Instant::from_millis(0)).unwrap();
        assert_eq!(tx.borrow().pool.len(), 1);

        drop(token);
        assert_eq!(tx.borrow().pool.len(), 2);
        assert!(tx.borrow().queue.is_empty());
    }

    #[test]
    fn transmit_consume_reuses_reserved_packet_and_queue_storage() {
        let tx = lane(64, 2);
        let rx = lane(64, 2);
        let mut device = device(&tx, &rx, 64);
        let token = device.transmit(Instant::from_millis(0)).unwrap();
        let packet_buffer = token.packet.as_ref().unwrap().buf.as_ptr();
        let packet_capacity = token.packet.as_ref().unwrap().buf.capacity();
        let queue_capacity = tx.borrow().queue.capacity();

        phy::TxToken::consume(token, 4, |buffer| buffer.copy_from_slice(&[1, 2, 3, 4]));

        let tx = tx.borrow();
        assert_eq!(tx.queue.capacity(), queue_capacity);
        assert_eq!(tx.queue[0].buf.as_ptr(), packet_buffer);
        assert_eq!(tx.queue[0].buf.capacity(), packet_capacity);
        assert_eq!(&tx.queue[0].buf[..tx.queue[0].len], &[1, 2, 3, 4]);
    }

    #[test]
    fn paired_receive_backpressure_leaves_rx_queued() {
        let tx = lane(64, 1);
        let rx = lane(64, 1);
        queue_packet(&rx, &[1, 2, 3]);
        let reserved = tx.borrow_mut().try_take_packet().unwrap();
        let mut device = device(&tx, &rx, 64);

        assert!(device.receive(Instant::from_millis(0)).is_none());
        assert_eq!(rx.borrow().queue.len(), 1);
        assert_eq!(tx.borrow().stats.rx_backpressure, 1);

        tx.borrow_mut().return_pkt(reserved);
    }

    #[test]
    fn paired_receive_and_unused_tx_refund_both_packets() {
        let tx = lane(64, 1);
        let rx = lane(64, 1);
        queue_packet(&rx, &[1, 2, 3]);
        let mut device = device(&tx, &rx, 64);

        let (rx_token, tx_token) = device.receive(Instant::from_millis(0)).unwrap();
        assert!(tx.borrow().pool.is_empty());
        assert!(rx.borrow().queue.is_empty());

        phy::RxToken::consume(rx_token, |bytes| assert_eq!(bytes, [1, 2, 3]));
        drop(tx_token);

        assert_eq!(tx.borrow().pool.len(), 1);
        assert_eq!(rx.borrow().pool.len(), 1);
    }

    #[test]
    fn oversized_transmit_panics_and_refunds_reservation() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let tx = lane(64, 2);
        let rx = lane(64, 2);
        let mut device = device(&tx, &rx, 64);
        let token = device.transmit(Instant::from_millis(0)).unwrap();

        let result = catch_unwind(AssertUnwindSafe(|| {
            phy::TxToken::consume(token, 65, |_| ());
        }));

        assert!(result.is_err());
        assert_eq!(tx.borrow().pool.len(), 2);
        assert!(tx.borrow().queue.is_empty());
    }

    #[test]
    fn packet_ownership_is_conserved_across_live_token_and_queue() {
        let tx = lane(64, 2);
        let rx = lane(64, 2);
        let mut device = device(&tx, &rx, 64);

        let token = device.transmit(Instant::from_millis(0)).unwrap();
        assert_eq!(tx.borrow().pool.len() + tx.borrow().queue.len() + 1, 2);

        phy::TxToken::consume(token, 1, |buffer| buffer[0] = 1);
        assert_eq!(tx.borrow().pool.len() + tx.borrow().queue.len(), 2);
    }

    #[test]
    fn symmetrically_saturated_lanes_make_response_progress() {
        let lane_a = lane(64, 2);
        let lane_b = lane(64, 2);
        queue_packet(&lane_a, &[1]);
        queue_packet(&lane_b, &[2]);
        let mut device_a = device(&lane_a, &lane_b, 64);
        let mut device_b = device(&lane_b, &lane_a, 64);

        assert!(device_a.transmit(Instant::from_millis(0)).is_none());
        assert!(device_b.transmit(Instant::from_millis(0)).is_none());

        let (rx_a, tx_a) = device_a.receive(Instant::from_millis(0)).unwrap();
        assert_eq!(phy::RxToken::consume(rx_a, |bytes| bytes[0]), 2);
        phy::TxToken::consume(tx_a, 1, |buffer| buffer[0] = 3);

        let (rx_b, tx_b) = device_b.receive(Instant::from_millis(0)).unwrap();
        assert_eq!(phy::RxToken::consume(rx_b, |bytes| bytes[0]), 1);
        drop(tx_b);

        assert_eq!(lane_a.borrow().pool.len(), 1);
        assert_eq!(lane_b.borrow().pool.len(), 2);
    }

    #[test]
    fn lane_stats_reports_reserved_packet_memory() {
        let lane = Lane::new(1500, 3);
        let stats = lane.stats();
        let payload_bytes = lane.pool.iter().map(|packet| packet.buf.capacity()).sum();
        let packet_slot_bytes =
            (lane.queue.capacity() + lane.pool.capacity()) * core::mem::size_of::<Packet>();

        assert_eq!(stats.reserved_payload_bytes, payload_bytes);
        assert_eq!(stats.reserved_packet_slot_bytes, packet_slot_bytes);
        assert_eq!(
            stats.reserved_total_bytes(),
            stats.reserved_payload_bytes + stats.reserved_packet_slot_bytes
        );
        assert_eq!(stats.tx_backpressure, 0);
        assert_eq!(stats.rx_backpressure, 0);
    }

    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    #[test]
    fn multi_tcp_memory_report_keeps_active_and_teardown_boundaries() {
        let before = AllocSnap {
            alloc_bytes: 1_000,
            alloc_count: 10,
            free_bytes: 400,
            rss: 8_192,
            ctxsw_voluntary: 0,
            ctxsw_nonvoluntary: 0,
            cpu_ns: 0,
            tsc: 0,
        };
        let after = AllocSnap {
            alloc_bytes: 1_600,
            alloc_count: 14,
            free_bytes: 850,
            rss: 12_288,
            ctxsw_voluntary: 0,
            ctxsw_nonvoluntary: 0,
            cpu_ns: 0,
            tsc: 0,
        };

        let report = MultiTcpMemoryReport::from_snapshots(before, after, 65_536, 0);

        assert_eq!(report.rss_start, 8_192);
        assert_eq!(report.rss_end, 12_288);
        assert_eq!(report.bytes_allocated, 600);
        assert_eq!(report.bytes_freed, 450);
        assert_eq!(report.net_heap_delta, 150);
        assert_eq!(report.allocation_count, 4);
        assert_eq!(report.pool_active, 65_536);
        assert_eq!(report.pool_after_teardown, 0);
    }

    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    fn assert_multi_tcp_worker_panic_propagates(fail_before_ready: bool) {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        let (completed_tx, completed_rx) = mpsc::channel();
        let supervisor = thread::spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let mut workers = MultiTcpWorkers::<()>::spawn(2, move |worker_id, mut phases| {
                    if worker_id == 0 && fail_before_ready {
                        panic!("injected multi_tcp setup failure");
                    }
                    if !phases.ready() {
                        return;
                    }
                    if worker_id == 0 && !fail_before_ready {
                        panic!("injected multi_tcp work failure");
                    }
                    if worker_id != 0 && !fail_before_ready {
                        thread::sleep(Duration::from_millis(50));
                    }
                    let _ = phases.finished(());
                });
                workers.wait_ready();
                workers.start();
                let _ = workers.wait_finished();
                workers.release_and_join();
            }));
            let _ = completed_tx.send(outcome);
        });

        let outcome = completed_rx
            .recv_timeout(Duration::from_millis(500))
            .expect("multi_tcp coordinator did not propagate worker failure");
        supervisor.join().unwrap();
        let panic = outcome.expect_err("injected worker panic was not resumed");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert!(
            message.is_some_and(|message| message.starts_with("injected multi_tcp")),
            "unexpected panic payload: {message:?}"
        );
    }

    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    #[test]
    fn multi_tcp_coordinator_propagates_setup_panic_without_deadlock() {
        assert_multi_tcp_worker_panic_propagates(true);
    }

    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    #[test]
    fn multi_tcp_coordinator_propagates_work_panic_without_deadlock() {
        assert_multi_tcp_worker_panic_propagates(false);
    }
}

/// A `Device` that sends to one queue and receives from another. Two of these
/// (with the queues swapped) form a paired link between two `Interface`s.
struct PairedDevice {
    tx: LaneRc,
    rx: LaneRc,
    mtu: usize,
    /// If true, the device advertises checksum offload so smoltcp skips
    /// IPv4/UDP/TCP checksum emit+verify (mimicking a hardware NIC, or
    /// e.g. an iOS NEPacketTunnelFlow where the OS already verified them).
    offload_checksums: bool,
    /// Bytes pushed through this device's TX path (i.e., what we emitted).
    tx_bytes: u64,
    tx_packets: u64,
}

impl PairedDevice {
    fn new(tx: LaneRc, rx: LaneRc, mtu: usize, offload_checksums: bool) -> Self {
        Self {
            tx,
            rx,
            mtu,
            offload_checksums,
            tx_bytes: 0,
            tx_packets: 0,
        }
    }
}

impl Device for PairedDevice {
    type RxToken<'a> = PairedRx<'a>;
    type TxToken<'a> = PairedTx<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.mtu;
        caps.checksum = if self.offload_checksums {
            ChecksumCapabilities::ignored()
        } else {
            ChecksumCapabilities::default()
        };
        caps
    }

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.rx.borrow().queue.is_empty() {
            return None;
        }

        let tx_packet = {
            let mut tx = self.tx.borrow_mut();
            match tx.try_take_packet() {
                Some(packet) => packet,
                None => {
                    tx.stats.rx_backpressure += 1;
                    return None;
                }
            }
        };
        let rx_packet = self
            .rx
            .borrow_mut()
            .queue
            .pop_front()
            .expect("RX queue changed while reserving paired TX packet");
        Some((
            PairedRx {
                pkt: Some(rx_packet),
                rx: &self.rx,
            },
            PairedTx {
                tx: &self.tx,
                packet: Some(tx_packet),
                mtu: self.mtu,
                tx_bytes: &mut self.tx_bytes,
                tx_packets: &mut self.tx_packets,
            },
        ))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        let packet = {
            let mut tx = self.tx.borrow_mut();
            if tx.pool.len() <= 1 {
                tx.stats.tx_backpressure += 1;
                return None;
            }
            tx.try_take_packet()
                .expect("standalone TX credit disappeared after availability check")
        };
        Some(PairedTx {
            tx: &self.tx,
            packet: Some(packet),
            mtu: self.mtu,
            tx_bytes: &mut self.tx_bytes,
            tx_packets: &mut self.tx_packets,
        })
    }
}

struct PairedRx<'a> {
    pkt: Option<Packet>,
    rx: &'a LaneRc,
}

impl<'a> phy::RxToken for PairedRx<'a> {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let result = {
            let packet = self.pkt.as_ref().unwrap();
            f(&packet.buf[..packet.len])
        };
        self.rx.borrow_mut().return_pkt(self.pkt.take().unwrap());
        result
    }
}

impl Drop for PairedRx<'_> {
    fn drop(&mut self) {
        if let Some(packet) = self.pkt.take() {
            self.rx.borrow_mut().return_pkt(packet);
        }
    }
}

struct PairedTx<'a> {
    tx: &'a LaneRc,
    packet: Option<Packet>,
    mtu: usize,
    tx_bytes: &'a mut u64,
    tx_packets: &'a mut u64,
}

impl<'a> phy::TxToken for PairedTx<'a> {
    fn consume<R, F>(mut self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(
            len <= self.mtu,
            "transmit length {len} exceeds MTU {}",
            self.mtu
        );
        let result = {
            let packet = self.packet.as_mut().unwrap();
            f(&mut packet.buf[..len])
        };
        let mut tx = self.tx.borrow_mut();
        let mut packet = self.packet.take().unwrap();
        packet.len = len;
        *self.tx_bytes += len as u64;
        *self.tx_packets += 1;
        tx.queue_pkt(packet);
        result
    }
}

impl Drop for PairedTx<'_> {
    fn drop(&mut self) {
        if let Some(packet) = self.packet.take() {
            self.tx.borrow_mut().return_pkt(packet);
        }
    }
}

struct Endpoint<'a> {
    iface: Interface,
    device: PairedDevice,
    sockets: SocketSet<'a>,
}

fn make_endpoint(
    addr: IpAddress,
    mtu: usize,
    tx: LaneRc,
    rx: LaneRc,
    offload_checksums: bool,
) -> Endpoint<'static> {
    let mut device = PairedDevice::new(tx, rx, mtu, offload_checksums);
    let mut config = InterfaceConfig::new(HardwareAddress::Ip);
    config.random_seed = 0xdead_beef;
    let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
    iface.update_ip_addrs(|ips| {
        ips.push(IpCidr::new(addr, 24)).unwrap();
    });
    Endpoint {
        iface,
        device,
        sockets: SocketSet::new(vec![]),
    }
}

/// Build a back-to-back server/client `Endpoint` pair joined by two
/// `Lane`s, with the server at `base.0.0.1` and the client at
/// `base.0.0.2`. The returned lane handles let callers report packet-pool
/// backpressure and fixed reservation size.
#[cfg_attr(not(feature = "socket-tcp-dynamic-buffer"), allow(dead_code))]
fn setup_paired_endpoints(
    base: u8,
    mtu: usize,
    queue_depth: usize,
    offload: bool,
) -> (Endpoint<'static>, Endpoint<'static>, LaneRc, LaneRc) {
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(mtu, queue_depth)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(mtu, queue_depth)));
    let server = make_endpoint(
        IpAddress::v4(base, 0, 0, 1),
        mtu,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let client = make_endpoint(
        IpAddress::v4(base, 0, 0, 2),
        mtu,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );
    (server, client, lane_a, lane_b)
}

fn add_tcp_socket(ep: &mut Endpoint<'static>, buf_size: usize) -> smoltcp::iface::SocketHandle {
    let rx = tcp::SocketBuffer::new(vec![0u8; buf_size]);
    let tx = tcp::SocketBuffer::new(vec![0u8; buf_size]);
    let socket = tcp::Socket::new(rx, tx);
    ep.sockets.add(socket)
}

/// Pool-backed dynamic-buffer variant. Buffers start at 0 and grow on
/// pressure up to `max_buf`, charged against the shared `pool`.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn add_tcp_socket_dyn(
    ep: &mut Endpoint<'static>,
    max_buf: u32,
    pool: &tcp::MemoryPool,
) -> smoltcp::iface::SocketHandle {
    let cfg = tcp::DynamicBufferConfig {
        rx_initial: 0,
        rx_max: max_buf,
        tx_initial: 0,
        tx_max: max_buf,
        grow_chunk: 8 * 1024,
    };
    let socket = tcp::Socket::new_dynamic(cfg, Some(pool.clone()));
    ep.sockets.add(socket)
}

/// Snapshot of the allocator counters + RSS at one instant. Take two and
/// `diff()` them to see what happened during a phase.
#[derive(Copy, Clone)]
struct AllocSnap {
    alloc_bytes: u64,
    alloc_count: u64,
    /// Live bytes = alloc_bytes - free_bytes, used to show net heap growth.
    free_bytes: u64,
    rss: u64,
    /// Voluntary context switches — process blocked or yielded.
    /// Hot-loop shapes should see this stay tiny.
    ctxsw_voluntary: u64,
    /// Involuntary context switches — preempted by the scheduler.
    /// Proportional to wall_time / scheduling_quantum × runnable_threads.
    ctxsw_nonvoluntary: u64,
    /// Calling-thread user CPU time, nanoseconds. Pairs with the TSC
    /// snapshot below to estimate the effective CPU frequency we ran
    /// at, which the `Report` then uses to convert cachegrind's
    /// instruction count into an IPC ratio.
    cpu_ns: u64,
    /// Time-stamp counter snapshot. On x86 this is rdtsc; on other
    /// archs it's zero (we just skip the IPC calculation).
    tsc: u64,
}

/// `CLOCK_THREAD_CPUTIME_ID` in nanoseconds. Caller thread's user CPU
/// time only. Returns 0 on unsupported platforms; we just skip the
/// IPC line in that case.
fn thread_cpu_ns() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0 {
            return (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64);
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// Read the x86 timestamp counter. Each tick is one core cycle (modulo
/// invariant-TSC behavior, which has been ubiquitous since ~2010).
/// Cheap (~20 cycles), so safe to call at run boundaries.
fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        unsafe { core::arch::x86_64::_rdtsc() }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

impl AllocSnap {
    fn now() -> Self {
        let (cv, cn) = ctxsw_counts();
        Self {
            alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
            alloc_count: ALLOC_COUNT.load(Ordering::Relaxed),
            free_bytes: FREE_BYTES.load(Ordering::Relaxed),
            rss: rss_bytes(),
            ctxsw_voluntary: cv,
            ctxsw_nonvoluntary: cn,
            cpu_ns: thread_cpu_ns(),
            tsc: read_tsc(),
        }
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn alloc_counters_with_rss(rss: u64) -> AllocSnap {
    AllocSnap {
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        alloc_count: ALLOC_COUNT.load(Ordering::Relaxed),
        free_bytes: FREE_BYTES.load(Ordering::Relaxed),
        rss,
        ctxsw_voluntary: 0,
        ctxsw_nonvoluntary: 0,
        cpu_ns: 0,
        tsc: 0,
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
struct MultiTcpMemoryReport {
    rss_start: u64,
    rss_end: u64,
    bytes_allocated: u64,
    bytes_freed: u64,
    net_heap_delta: i128,
    allocation_count: u64,
    pool_active: usize,
    pool_after_teardown: usize,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl MultiTcpMemoryReport {
    fn from_snapshots(
        before: AllocSnap,
        after: AllocSnap,
        pool_active: usize,
        pool_after_teardown: usize,
    ) -> Self {
        let bytes_allocated = after.alloc_bytes.saturating_sub(before.alloc_bytes);
        let bytes_freed = after.free_bytes.saturating_sub(before.free_bytes);
        Self {
            rss_start: before.rss,
            rss_end: after.rss,
            bytes_allocated,
            bytes_freed,
            net_heap_delta: bytes_allocated as i128 - bytes_freed as i128,
            allocation_count: after.alloc_count.saturating_sub(before.alloc_count),
            pool_active,
            pool_after_teardown,
        }
    }

    fn print(&self) {
        println!("  pool used active end:   {} KiB", self.pool_active / 1024);
        println!(
            "  pool used after teardown: {} KiB",
            self.pool_after_teardown / 1024
        );
        println!();
        println!("  steady-state allocations:");
        println!("    bytes allocated:       {}", self.bytes_allocated);
        println!("    bytes freed:           {}", self.bytes_freed);
        println!("    net heap delta:        {}", self.net_heap_delta);
        println!("    allocation count:      {}", self.allocation_count);
        println!();
        println!("  process memory:");
        println!(
            "    rss start:              {}  ({:.1} MiB)",
            self.rss_start,
            self.rss_start as f64 / (1024.0 * 1024.0)
        );
        println!(
            "    rss end:                {}  ({:.1} MiB)",
            self.rss_end,
            self.rss_end as f64 / (1024.0 * 1024.0)
        );
    }
}

/// Lay out a uniform metrics block so every shape prints the same shape of
/// data and comparisons across runs are unambiguous.
struct Report<'a> {
    name: &'a str,
    elapsed: f64,
    #[allow(dead_code)]
    app_bytes_sent: u64,
    app_bytes_recvd: u64,
    /// Total wire packets emitted by both peers.
    wire_packets: u64,
    /// Total wire bytes emitted by both peers (incl. headers).
    wire_bytes: u64,
    /// Latency histogram of poll cycles (one pump of both endpoints).
    poll_lat: Histo,
    /// Allocator state before and after the steady-state loop. The diff is
    /// the allocator load attributable to the loop body.
    alloc_before: AllocSnap,
    alloc_after: AllocSnap,
    /// Application-defined work-unit counter (rtts, packets, etc.) for the
    /// shape; printed verbatim with `unit_label`.
    work_units: u64,
    unit_label: &'a str,
}

impl<'a> Report<'a> {
    fn print(&self) {
        let bw_app = self.app_bytes_recvd as f64 * 8.0 / self.elapsed / 1e9;
        let bw_wire = self.wire_bytes as f64 * 8.0 / self.elapsed / 1e9;
        let mpps = self.wire_packets as f64 / self.elapsed / 1e6;
        let avg_pkt = if self.wire_packets == 0 {
            0.0
        } else {
            self.wire_bytes as f64 / self.wire_packets as f64
        };
        let ns_per_pkt = if self.wire_packets == 0 {
            0.0
        } else {
            self.elapsed * 1e9 / self.wire_packets as f64
        };
        let cyc_per_pkt = ns_per_pkt * REF_CPU_GHZ;
        let unit_rate = if self.elapsed == 0.0 {
            0.0
        } else {
            self.work_units as f64 / self.elapsed
        };

        let alloc_bytes = self.alloc_after.alloc_bytes - self.alloc_before.alloc_bytes;
        let alloc_count = self.alloc_after.alloc_count - self.alloc_before.alloc_count;
        let free_bytes = self.alloc_after.free_bytes - self.alloc_before.free_bytes;
        let net_heap = alloc_bytes as i64 - free_bytes as i64;
        let bytes_per_pkt = if self.wire_packets == 0 {
            0.0
        } else {
            alloc_bytes as f64 / self.wire_packets as f64
        };

        println!("\n========== {} ==========", self.name);
        println!("  elapsed:                {:.3} s", self.elapsed);
        println!(
            "  throughput (app):       {bw_app:>8.3} Gbps  ({:.1} MB/s)",
            self.app_bytes_recvd as f64 / self.elapsed / 1e6
        );
        println!(
            "  throughput (wire):      {bw_wire:>8.3} Gbps  ({:.1} MB/s)",
            self.wire_bytes as f64 / self.elapsed / 1e6
        );
        println!("  packet rate:            {mpps:>8.3} Mpps     (avg {avg_pkt:.1} bytes/pkt)");
        println!(
            "  per-packet:             {ns_per_pkt:>8.1} ns   (~{:.0} cycles @ {} GHz)",
            cyc_per_pkt, REF_CPU_GHZ
        );
        println!(
            "  work units:             {:>8} {}  ({:.3} M{}/s)",
            self.work_units,
            self.unit_label,
            unit_rate / 1e6,
            self.unit_label
        );
        println!(
            "  wire packets:           {:>8}   (use for IPC: cachegrind I refs / this = I/pkt)",
            self.wire_packets,
        );
        println!();
        println!("  poll-cycle latency (ns):");
        println!(
            "    min:    {:>6}   mean:   {:>6}   max:   {:>9}   samples: {}",
            self.poll_lat.min_ns,
            self.poll_lat.mean(),
            self.poll_lat.max_ns,
            self.poll_lat.samples
        );
        println!(
            "    p50:    {:>6}   p90:    {:>6}   p99:   {:>9}   p999:    {:>6}",
            self.poll_lat.percentile(0.50),
            self.poll_lat.percentile(0.90),
            self.poll_lat.percentile(0.99),
            self.poll_lat.percentile(0.999)
        );
        println!();
        println!("  steady-state allocations:");
        println!(
            "    bytes allocated:       {:>10}  ({:.3} bytes/packet)",
            alloc_bytes, bytes_per_pkt
        );
        println!("    bytes freed:           {:>10}", free_bytes);
        println!("    net heap delta:        {:>10}", net_heap);
        println!("    allocation count:      {:>10}", alloc_count);
        println!();
        println!("  process memory:");
        println!(
            "    rss start:             {:>10}  ({:.1} MiB)",
            self.alloc_before.rss,
            self.alloc_before.rss as f64 / (1024.0 * 1024.0)
        );
        println!(
            "    rss end:               {:>10}  ({:.1} MiB)",
            self.alloc_after.rss,
            self.alloc_after.rss as f64 / (1024.0 * 1024.0)
        );

        let cv = self.alloc_after.ctxsw_voluntary - self.alloc_before.ctxsw_voluntary;
        let cn = self.alloc_after.ctxsw_nonvoluntary - self.alloc_before.ctxsw_nonvoluntary;
        println!("  context switches:");
        println!("    voluntary:            {cv:>10}  (process yields / blocks)");
        println!("    nonvoluntary:         {cn:>10}  (scheduler preemption)");
        if self.elapsed > 0.0 {
            println!(
                "    rate:                  {:>10.1} cs/s total",
                (cv + cn) as f64 / self.elapsed
            );
        }

        // CPU-time + TSC: lets us compute an effective CPU frequency
        // (cycles/sec) and report cycles-per-packet alongside the
        // wall-clock ns/packet number. Cachegrind already has the
        // instruction count; combining the two yields IPC.
        let cpu_ns = self
            .alloc_after
            .cpu_ns
            .saturating_sub(self.alloc_before.cpu_ns);
        let tsc_d = self.alloc_after.tsc.saturating_sub(self.alloc_before.tsc);
        if cpu_ns > 0 && tsc_d > 0 {
            let eff_ghz = tsc_d as f64 / cpu_ns as f64;
            println!("  CPU:");
            println!(
                "    user time:            {:>10.3} s   ({:.3}% of wall)",
                cpu_ns as f64 / 1e9,
                (cpu_ns as f64 / 1e9) / self.elapsed * 100.0,
            );
            println!(
                "    TSC ticks:            {:>10}   (~{eff_ghz:.3} GHz effective)",
                tsc_d
            );
            if self.wire_packets > 0 {
                let cycles_per_pkt = tsc_d as f64 / self.wire_packets as f64;
                println!(
                    "    cycles/pkt:            {cycles_per_pkt:>9.1}  (use cachegrind I refs / this for IPC)"
                );
            }
        }
    }
}

fn shape_firehose(seconds: u64, offload: bool) {
    const BUF: usize = 256 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );

    let srv_h = add_tcp_socket(&mut server, BUF);
    let cli_h = add_tcp_socket(&mut client, BUF);

    // We want to measure smoltcp's per-packet CPU cost, not its delayed-ACK behaviour.
    // Suppressing delayed ACK + Nagle keeps the pipeline saturated.
    {
        let s = server.sockets.get_mut::<tcp::Socket>(srv_h);
        s.set_ack_delay(None);
        s.set_nagle_enabled(false);
        s.listen(1234).unwrap();
    }
    {
        let c = client.sockets.get_mut::<tcp::Socket>(cli_h);
        c.set_ack_delay(None);
        c.set_nagle_enabled(false);
    }
    client
        .sockets
        .get_mut::<tcp::Socket>(cli_h)
        .connect(
            client.iface.context(),
            (IpAddress::v4(10, 0, 0, 1), 1234),
            49152,
        )
        .unwrap();

    // Use wall clock for the virtual time so TCP timers (RTO, delayed ACK) behave realistically.
    let wall_origin = StdInstant::now();
    let now_smol = || Instant::from_micros(wall_origin.elapsed().as_micros() as i64);

    // Pump until ESTABLISHED.
    for _ in 0..1000 {
        let n = now_smol();
        server
            .iface
            .poll(n, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(n, &mut client.device, &mut client.sockets);
        if client.sockets.get::<tcp::Socket>(cli_h).may_send()
            && server.sockets.get::<tcp::Socket>(srv_h).may_recv()
        {
            break;
        }
    }

    let payload = vec![0x42u8; 64 * 1024];
    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let mut sent: u64 = 0;
    let mut recvd: u64 = 0;
    let mut idle_spins: u64 = 0;
    let mut sink = vec![0u8; 64 * 1024];
    let mut poll_lat = SampledTimer::new();
    let alloc_before = AllocSnap::now();

    let mut iters: u64 = 0;
    loop {
        if iters & 0xFF == 0 && StdInstant::now() >= deadline {
            break;
        }
        iters = iters.wrapping_add(1);
        let n = now_smol();

        // Client fills its send buffer.
        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        let mut sent_this_round = 0u64;
        while cs.can_send() {
            let cap = cs.send_capacity().min(payload.len());
            if cap == 0 {
                break;
            }
            let written = cs.send_slice(&payload[..cap]).unwrap_or(0);
            if written == 0 {
                break;
            }
            sent += written as u64;
            sent_this_round += written as u64;
        }

        let mut cli_state = smoltcp::iface::PollResult::None;
        let mut srv_state = smoltcp::iface::PollResult::None;
        poll_lat.measure(|| {
            cli_state = client
                .iface
                .poll(n, &mut client.device, &mut client.sockets);
            srv_state = server
                .iface
                .poll(n, &mut server.device, &mut server.sockets);
        });

        // Server drains its receive buffer.
        let ss = server.sockets.get_mut::<tcp::Socket>(srv_h);
        let mut recvd_this_round = 0u64;
        while ss.can_recv() {
            let r = ss.recv_slice(&mut sink).unwrap_or(0);
            if r == 0 {
                break;
            }
            recvd += r as u64;
            recvd_this_round += r as u64;
        }

        if sent_this_round == 0
            && recvd_this_round == 0
            && matches!(cli_state, smoltcp::iface::PollResult::None)
            && matches!(srv_state, smoltcp::iface::PollResult::None)
        {
            idle_spins += 1;
        }
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "firehose (TCP bulk, both peers smoltcp)",
        elapsed,
        app_bytes_sent: sent,
        app_bytes_recvd: recvd,
        wire_packets: client.device.tx_packets + server.device.tx_packets,
        wire_bytes: client.device.tx_bytes + server.device.tx_bytes,
        poll_lat: poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: idle_spins,
        unit_label: "idle-spins",
    }
    .print();
    print_lane_stats("firehose", collect_lane_stats(&[&lane_a, &lane_b]));
}

fn shape_small(seconds: u64, offload: bool) {
    // Force tiny segments by limiting the socket buffer; with a 1500 MTU the
    // client never fills more than a single small write at a time.
    const BUF: usize = 4 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );

    let srv_h = add_tcp_socket(&mut server, BUF);
    let cli_h = add_tcp_socket(&mut client, BUF);

    server
        .sockets
        .get_mut::<tcp::Socket>(srv_h)
        .listen(1234)
        .unwrap();
    client
        .sockets
        .get_mut::<tcp::Socket>(cli_h)
        .connect(
            client.iface.context(),
            (IpAddress::v4(10, 0, 0, 1), 1234),
            49152,
        )
        .unwrap();

    let mut t_ms: i64 = 0;
    for _ in 0..200 {
        let n = Instant::from_millis(t_ms);
        server
            .iface
            .poll(n, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(n, &mut client.device, &mut client.sockets);
        if client.sockets.get::<tcp::Socket>(cli_h).may_send() {
            break;
        }
        t_ms += 1;
    }

    let payload = [0x42u8; 64];
    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let mut sent: u64 = 0;
    let mut recvd: u64 = 0;
    let mut poll_lat = SampledTimer::new();
    let alloc_before = AllocSnap::now();
    let mut iters: u64 = 0;
    loop {
        if iters & 0xFF == 0 && StdInstant::now() >= deadline {
            break;
        }
        iters = iters.wrapping_add(1);
        let n = Instant::from_millis(t_ms);

        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        if cs.can_send()
            && let Ok(w) = cs.send_slice(&payload)
            && w > 0
        {
            sent += w as u64;
        }
        poll_lat.measure(|| {
            client
                .iface
                .poll(n, &mut client.device, &mut client.sockets);
            server
                .iface
                .poll(n, &mut server.device, &mut server.sockets);
        });

        let ss = server.sockets.get_mut::<tcp::Socket>(srv_h);
        if ss.can_recv() {
            let mut sink = [0u8; 64];
            if let Ok(r) = ss.recv_slice(&mut sink) {
                recvd += r as u64;
            }
        }
        t_ms += 1;
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "small (TCP 64B segments)",
        elapsed,
        app_bytes_sent: sent,
        app_bytes_recvd: recvd,
        wire_packets: client.device.tx_packets + server.device.tx_packets,
        wire_bytes: client.device.tx_bytes + server.device.tx_bytes,
        poll_lat: poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: recvd,
        unit_label: "bytes",
    }
    .print();
    print_lane_stats("small", collect_lane_stats(&[&lane_a, &lane_b]));
}

fn shape_pingpong(seconds: u64, offload: bool) {
    const BUF: usize = 16 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );

    let srv_h = add_tcp_socket(&mut server, BUF);
    let cli_h = add_tcp_socket(&mut client, BUF);

    server
        .sockets
        .get_mut::<tcp::Socket>(srv_h)
        .listen(1234)
        .unwrap();
    client
        .sockets
        .get_mut::<tcp::Socket>(cli_h)
        .connect(
            client.iface.context(),
            (IpAddress::v4(10, 0, 0, 1), 1234),
            49152,
        )
        .unwrap();

    let mut t_ms: i64 = 0;
    for _ in 0..200 {
        let n = Instant::from_millis(t_ms);
        server
            .iface
            .poll(n, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(n, &mut client.device, &mut client.sockets);
        if client.sockets.get::<tcp::Socket>(cli_h).may_send() {
            break;
        }
        t_ms += 1;
    }

    let msg = [0x55u8; 128];
    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let mut roundtrips: u64 = 0;
    let mut poll_lat = SampledTimer::new();
    let alloc_before = AllocSnap::now();
    let mut iters: u64 = 0;

    loop {
        if iters & 0xFF == 0 && StdInstant::now() >= deadline {
            break;
        }
        iters = iters.wrapping_add(1);
        let n = Instant::from_millis(t_ms);
        // Client sends one message.
        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        if cs.can_send() {
            let _ = cs.send_slice(&msg);
        }
        poll_lat.measure(|| {
            client
                .iface
                .poll(n, &mut client.device, &mut client.sockets);
            server
                .iface
                .poll(n, &mut server.device, &mut server.sockets);
        });

        // Server echoes.
        let ss = server.sockets.get_mut::<tcp::Socket>(srv_h);
        let mut sink = [0u8; 128];
        if ss.can_recv()
            && let Ok(r) = ss.recv_slice(&mut sink)
            && r > 0
            && ss.can_send()
        {
            let _ = ss.send_slice(&sink[..r]);
        }
        poll_lat.measure(|| {
            server
                .iface
                .poll(n, &mut server.device, &mut server.sockets);
            client
                .iface
                .poll(n, &mut client.device, &mut client.sockets);
        });

        // Client receives echo.
        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        if cs.can_recv()
            && let Ok(r) = cs.recv_slice(&mut sink)
            && r > 0
        {
            roundtrips += 1;
        }
        t_ms += 1;
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "pingpong (TCP 128B req/resp)",
        elapsed,
        app_bytes_sent: roundtrips * msg.len() as u64,
        app_bytes_recvd: roundtrips * msg.len() as u64,
        wire_packets: client.device.tx_packets + server.device.tx_packets,
        wire_bytes: client.device.tx_bytes + server.device.tx_bytes,
        poll_lat: poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: roundtrips,
        unit_label: "roundtrips",
    }
    .print();
    print_lane_stats("pingpong", collect_lane_stats(&[&lane_a, &lane_b]));
}

fn shape_udp_firehose(seconds: u64, offload: bool) {
    // Pure packet forwarding — no flow control, no cwnd. This is the closest
    // analogue to a packet tunnel forwarding fully-formed packets between peers.
    const PAYLOAD: usize = 1400;
    const META_SLOTS: usize = 256;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );

    let mk_buf = || -> (udp::PacketBuffer<'static>, udp::PacketBuffer<'static>) {
        let rx_meta = vec![udp::PacketMetadata::EMPTY; META_SLOTS];
        let rx_data = vec![0u8; PAYLOAD * META_SLOTS];
        let tx_meta = vec![udp::PacketMetadata::EMPTY; META_SLOTS];
        let tx_data = vec![0u8; PAYLOAD * META_SLOTS];
        (
            udp::PacketBuffer::new(rx_meta, rx_data),
            udp::PacketBuffer::new(tx_meta, tx_data),
        )
    };

    let (srv_rx, srv_tx) = mk_buf();
    let srv_h = server.sockets.add(udp::Socket::new(srv_rx, srv_tx));
    let (cli_rx, cli_tx) = mk_buf();
    let cli_h = client.sockets.add(udp::Socket::new(cli_rx, cli_tx));

    server
        .sockets
        .get_mut::<udp::Socket>(srv_h)
        .bind(2000)
        .unwrap();
    client
        .sockets
        .get_mut::<udp::Socket>(cli_h)
        .bind(2001)
        .unwrap();

    let dest_meta: udp::UdpMetadata = (IpAddress::v4(10, 0, 0, 1), 2000).into();
    let payload = vec![0xa5u8; PAYLOAD];

    // Advance the smoltcp virtual clock by 1 µs each loop iteration. This is
    // not wall-accurate but it is monotonic and avoids a vDSO `clock_gettime`
    // per iteration (which showed up at ~10% of profile when sampled per-poll).
    let mut t_us: i64 = 0;
    let mut iters: u64 = 0;
    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let mut sent: u64 = 0;
    let mut recvd: u64 = 0;
    let mut sink = [0u8; PAYLOAD];
    let mut poll_lat = SampledTimer::new();

    let alloc_before = AllocSnap::now();

    loop {
        if iters & 0xFF == 0 && StdInstant::now() >= deadline {
            break;
        }
        let n = Instant::from_micros(t_us);
        let cs = client.sockets.get_mut::<udp::Socket>(cli_h);
        while cs.can_send() && cs.send_slice(&payload, dest_meta).is_ok() {
            sent += PAYLOAD as u64;
        }
        poll_lat.measure(|| {
            client
                .iface
                .poll(n, &mut client.device, &mut client.sockets);
            server
                .iface
                .poll(n, &mut server.device, &mut server.sockets);
        });

        let ss = server.sockets.get_mut::<udp::Socket>(srv_h);
        while ss.can_recv() {
            match ss.recv_slice(&mut sink) {
                Ok((r, _)) => recvd += r as u64,
                Err(_) => break,
            }
        }
        t_us = t_us.wrapping_add(1);
        iters = iters.wrapping_add(1);
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "udp_firehose (1400B UDP)",
        elapsed,
        app_bytes_sent: sent,
        app_bytes_recvd: recvd,
        wire_packets: client.device.tx_packets + server.device.tx_packets,
        wire_bytes: client.device.tx_bytes + server.device.tx_bytes,
        poll_lat: poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: (recvd / PAYLOAD as u64),
        unit_label: "pkts-recvd",
    }
    .print();
    print_lane_stats("udp", collect_lane_stats(&[&lane_a, &lane_b]));
}

/// `n` concurrent TCP echo flows between two smoltcp endpoints. Each flow has
/// its own (src_port, dst_port) tuple so the stack treats them independently.
///
/// Verifies two properties:
///   * memory stays bounded (RSS trace + net heap delta)
///   * no flow is starved (Jain index + per-flow percentiles)
fn shape_many_tcp(seconds: u64, n: usize, offload: bool, mode: RunMode) {
    // Per-flow buffer sized small enough to keep total memory reasonable
    // even at N=1000: 1000 flows × 2 (rx+tx) × 4 KiB × 2 (server+client) ≈ 16 MiB.
    const BUF: usize = 4 * 1024;
    // Lane queue depth scales with N. The minimum has to be large enough
    // that a full round of egress packets never spills, otherwise
    // socket_egress short-circuits mid-walk and the late sockets in the
    // iteration order get systematically starved.
    let qd = (n * 16).clamp(1024, 16384);

    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );

    let mut srv_handles = Vec::with_capacity(n);
    let mut cli_handles = Vec::with_capacity(n);

    for i in 0..n {
        let h_srv = add_tcp_socket(&mut server, BUF);
        let h_cli = add_tcp_socket(&mut client, BUF);

        let dst_port: u16 = 10_000u16.wrapping_add(i as u16);
        let src_port: u16 = 30_000u16.wrapping_add(i as u16);

        {
            let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
            s.set_ack_delay(None);
            s.set_nagle_enabled(false);
            s.listen(dst_port).unwrap();
        }
        {
            let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
            c.set_ack_delay(None);
            c.set_nagle_enabled(false);
        }
        client
            .sockets
            .get_mut::<tcp::Socket>(h_cli)
            .connect(
                client.iface.context(),
                (IpAddress::v4(10, 0, 0, 1), dst_port),
                src_port,
            )
            .unwrap();

        srv_handles.push(h_srv);
        cli_handles.push(h_cli);
    }

    // Drive both stacks until every connection is ESTABLISHED. The single-flow
    // shapes use a fast (1-µs-per-iter) virtual clock, but that does NOT work
    // here: smoltcp's RTO is ≥1 s and the zero-window-probe timer needs to
    // actually fire when a flow ends up in a mutual-zero-window state, which
    // only happens with realistic virtual time. Drive the smoltcp clock from
    // the wall clock.
    let wall0 = StdInstant::now();
    let smol_now = || Instant::from_micros(wall0.elapsed().as_micros() as i64);
    let connect_deadline = StdInstant::now() + std::time::Duration::from_secs(seconds.min(5));
    loop {
        let now = smol_now();
        server
            .iface
            .poll(now, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(now, &mut client.device, &mut client.sockets);
        let all_ready = cli_handles
            .iter()
            .zip(srv_handles.iter())
            .all(|(&hc, &hs)| {
                client.sockets.get::<tcp::Socket>(hc).may_send()
                    && server.sockets.get::<tcp::Socket>(hs).may_recv()
            });
        if all_ready || StdInstant::now() >= connect_deadline {
            break;
        }
    }

    let established = cli_handles
        .iter()
        .zip(srv_handles.iter())
        .filter(|&(&hc, &hs)| {
            client.sockets.get::<tcp::Socket>(hc).may_send()
                && server.sockets.get::<tcp::Socket>(hs).may_recv()
        })
        .count();
    if established < n {
        eprintln!(
            "warning: only {established}/{n} flows established within {} s",
            seconds.min(5)
        );
    }

    let payload = vec![0x42u8; 256];
    let mut sink = vec![0u8; 256];
    let mut sent = vec![0u64; n];
    let mut recvd = vec![0u64; n];

    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let alloc_before = AllocSnap::now();
    let mut poll_lat = SampledTimer::new();
    let mut mem_trace = MemTrace::start();
    let mut iters: u64 = 0;

    loop {
        if iters & 0xFF == 0 && StdInstant::now() >= deadline {
            break;
        }
        let now = smol_now();

        // Client: try to push one chunk on every flow this iteration.
        for (i, &h) in cli_handles.iter().enumerate() {
            let cs = client.sockets.get_mut::<tcp::Socket>(h);
            if cs.can_send()
                && let Ok(w) = cs.send_slice(&payload)
            {
                sent[i] += w as u64;
            }
        }

        poll_lat.measure(|| {
            client
                .iface
                .poll(now, &mut client.device, &mut client.sockets);
            server
                .iface
                .poll(now, &mut server.device, &mut server.sockets);
        });

        // Server: drain RX completely, then echo as much as TX has room for.
        // Coupling drain to can_send (the previous shape) deadlocks: if
        // server.tx_buffer fills, we stop draining rx, the server's
        // advertised window collapses to 0, and the client backs off
        // entirely. So drain unconditionally; the echo just becomes lossy
        // when tx is full.
        for &h in &srv_handles {
            let ss = server.sockets.get_mut::<tcp::Socket>(h);
            while ss.can_recv() {
                match ss.recv_slice(&mut sink) {
                    Ok(r) if r > 0 => {
                        if ss.can_send() {
                            let _ = ss.send_slice(&sink[..r]);
                        }
                    }
                    _ => break,
                }
            }
        }

        poll_lat.measure(|| {
            server
                .iface
                .poll(now, &mut server.device, &mut server.sockets);
            client
                .iface
                .poll(now, &mut client.device, &mut client.sockets);
        });

        // Client: drain echo completely on every flow.
        for (i, &h) in cli_handles.iter().enumerate() {
            let cs = client.sockets.get_mut::<tcp::Socket>(h);
            while cs.can_recv() {
                match cs.recv_slice(&mut sink) {
                    Ok(r) if r > 0 => recvd[i] += r as u64,
                    _ => break,
                }
            }
        }

        iters = iters.wrapping_add(1);
        // ~4x/sec — cheap enough not to perturb throughput, dense enough to
        // see RSS trajectory.
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "many_tcp",
        elapsed,
        app_bytes_sent: sent.iter().sum(),
        app_bytes_recvd: recvd.iter().sum(),
        wire_packets: client.device.tx_packets + server.device.tx_packets,
        wire_bytes: client.device.tx_bytes + server.device.tx_bytes,
        poll_lat: poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: n as u64,
        unit_label: "flows",
    }
    .print();

    let sent_stats = Fairness::from(&sent);
    let recvd_stats = Fairness::from(&recvd);
    sent_stats.print("sent");
    recvd_stats.print("recvd");

    // If we detected a starved or zero-byte flow, dump its TCP socket state
    // side-by-side with a healthy flow (the max-throughput one). The
    // delta in send_queue/recv_queue at end-of-test usually points at the
    // root cause (RST, zero-window deadlock, sequence-arithmetic edge case).
    if recvd_stats.zero_flows > 0 || recvd_stats.starved > 0 {
        let dump = |label: &str, idx: usize| {
            let cs = client.sockets.get::<tcp::Socket>(cli_handles[idx]);
            let ss = server.sockets.get::<tcp::Socket>(srv_handles[idx]);
            println!(
                "  {label} flow #{idx:<4}  client.state={:?}/{:?}  server.state={:?}/{:?}",
                cs.state(),
                (cs.may_send(), cs.may_recv()),
                ss.state(),
                (ss.may_send(), ss.may_recv()),
            );
            println!(
                "                  client.send_q={:>5}  client.recv_q={:>5}  server.send_q={:>5}  server.recv_q={:>5}",
                cs.send_queue(),
                cs.recv_queue(),
                ss.send_queue(),
                ss.recv_queue(),
            );
            println!(
                "                  bytes sent={:>10}  bytes recvd={:>10}",
                sent[idx], recvd[idx]
            );
        };
        println!();
        println!("  flow-state diagnostic (compare starved vs healthy):");
        dump("starved", recvd_stats.min_flow);
        dump("healthy", recvd_stats.max_flow);
    }

    mem_trace.print();
    print_lane_stats("many_tcp", collect_lane_stats(&[&lane_a, &lane_b]));

    // Per-flow socket footprint estimate. Useful for sizing per-flow
    // budgets in downstream consumers that admit many concurrent flows.
    let tcp_socket_bytes = core::mem::size_of::<tcp::Socket>();
    let per_flow_bytes = tcp_socket_bytes + 2 * BUF;
    let total_bytes = 2 * n * per_flow_bytes; // both peers
    println!();
    println!("  socket-state footprint (without lane pool):");
    println!(
        "    per-flow:           {} bytes (Socket {} + 2 × {} KiB buf)",
        per_flow_bytes,
        tcp_socket_bytes,
        BUF / 1024,
    );
    println!(
        "    total (both peers): {} bytes  ({:.2} MiB)",
        total_bytes,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
}

/// Deterministic fairness variant for TCP flows. Each round gives every flow
/// one bounded client send and server drain opportunity, then rotates the
/// start index so flow 0 does not always go first.
fn shape_many_tcp_fair(seconds: u64, n: usize, offload: bool, mode: RunMode) {
    const BUF: usize = 4 * 1024;
    const PAYLOAD: usize = 256;
    let qd = (n * 16).clamp(1024, 16384);

    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );

    let mut srv_handles = Vec::with_capacity(n);
    let mut cli_handles = Vec::with_capacity(n);
    for i in 0..n {
        let h_srv = add_tcp_socket(&mut server, BUF);
        let h_cli = add_tcp_socket(&mut client, BUF);
        let dst_port: u16 = 10_000u16.wrapping_add(i as u16);
        let src_port: u16 = 30_000u16.wrapping_add(i as u16);

        {
            let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
            s.set_ack_delay(None);
            s.set_nagle_enabled(false);
            s.listen(dst_port).unwrap();
        }
        {
            let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
            c.set_ack_delay(None);
            c.set_nagle_enabled(false);
        }
        client
            .sockets
            .get_mut::<tcp::Socket>(h_cli)
            .connect(
                client.iface.context(),
                (IpAddress::v4(10, 0, 0, 1), dst_port),
                src_port,
            )
            .unwrap();

        srv_handles.push(h_srv);
        cli_handles.push(h_cli);
    }

    let wall0 = StdInstant::now();
    let smol_now = || Instant::from_micros(wall0.elapsed().as_micros() as i64);
    let connect_deadline = StdInstant::now() + std::time::Duration::from_secs(seconds.min(5));
    loop {
        let now = smol_now();
        server
            .iface
            .poll(now, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(now, &mut client.device, &mut client.sockets);
        let all_ready = cli_handles
            .iter()
            .zip(srv_handles.iter())
            .all(|(&hc, &hs)| {
                client.sockets.get::<tcp::Socket>(hc).may_send()
                    && server.sockets.get::<tcp::Socket>(hs).may_recv()
            });
        if all_ready || StdInstant::now() >= connect_deadline {
            break;
        }
    }

    let established = cli_handles
        .iter()
        .zip(srv_handles.iter())
        .filter(|&(&hc, &hs)| {
            client.sockets.get::<tcp::Socket>(hc).may_send()
                && server.sockets.get::<tcp::Socket>(hs).may_recv()
        })
        .count();
    if established < n {
        eprintln!(
            "warning: only {established}/{n} flows established within {} s",
            seconds.min(5)
        );
    }

    let payload = [0x42u8; PAYLOAD];
    let mut sink = [0u8; PAYLOAD];
    let mut sent = vec![0u64; n];
    let mut recvd = vec![0u64; n];

    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let alloc_before = AllocSnap::now();
    let mut poll_lat = SampledTimer::new();
    let mut mem_trace = MemTrace::start();
    let mut start_flow = 0usize;
    let mut rounds = 0u64;
    let poll_budget = n.saturating_mul(6).clamp(16, 1024);

    while StdInstant::now() < deadline {
        for offset in 0..n {
            let i = (start_flow + offset) % n;

            let cs = client.sockets.get_mut::<tcp::Socket>(cli_handles[i]);
            if sent[i].saturating_sub(recvd[i]) < PAYLOAD as u64
                && cs.can_send()
                && let Ok(w) = cs.send_slice(&payload)
            {
                sent[i] += w as u64;
            }

            for _ in 0..poll_budget {
                let now = smol_now();
                poll_lat.measure(|| {
                    client
                        .iface
                        .poll(now, &mut client.device, &mut client.sockets);
                    server
                        .iface
                        .poll(now, &mut server.device, &mut server.sockets);
                });
                if server.sockets.get::<tcp::Socket>(srv_handles[i]).can_recv() {
                    break;
                }
            }

            let ss = server.sockets.get_mut::<tcp::Socket>(srv_handles[i]);
            if ss.can_recv() {
                match ss.recv_slice(&mut sink) {
                    Ok(r) if r > 0 => recvd[i] += r as u64,
                    _ => {}
                }
            }

            // Return ACK/window updates to the sender before the next bounded
            // opportunity for this flow.
            let now = smol_now();
            poll_lat.measure(|| {
                server
                    .iface
                    .poll(now, &mut server.device, &mut server.sockets);
                client
                    .iface
                    .poll(now, &mut client.device, &mut client.sockets);
            });
        }

        start_flow = (start_flow + 1) % n;
        rounds = rounds.wrapping_add(1);
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "many_tcp_fair",
        elapsed,
        app_bytes_sent: sent.iter().sum(),
        app_bytes_recvd: recvd.iter().sum(),
        wire_packets: client.device.tx_packets + server.device.tx_packets,
        wire_bytes: client.device.tx_bytes + server.device.tx_bytes,
        poll_lat: poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: rounds,
        unit_label: "rounds",
    }
    .print();

    let sent_stats = Fairness::from(&sent);
    let recvd_stats = Fairness::from(&recvd);
    sent_stats.print("sent");
    recvd_stats.print("recvd");

    if recvd_stats.zero_flows > 0 || recvd_stats.starved > 0 {
        let dump = |label: &str, idx: usize| {
            let cs = client.sockets.get::<tcp::Socket>(cli_handles[idx]);
            let ss = server.sockets.get::<tcp::Socket>(srv_handles[idx]);
            println!(
                "  {label} flow #{idx:<4}  client.state={:?}/{:?}  server.state={:?}/{:?}",
                cs.state(),
                (cs.may_send(), cs.may_recv()),
                ss.state(),
                (ss.may_send(), ss.may_recv()),
            );
            println!(
                "                  client.send_q={:>5}  client.recv_q={:>5}  server.send_q={:>5}  server.recv_q={:>5}",
                cs.send_queue(),
                cs.recv_queue(),
                ss.send_queue(),
                ss.recv_queue(),
            );
            println!(
                "                  bytes sent={:>10}  bytes recvd={:>10}",
                sent[idx], recvd[idx]
            );
        };
        println!();
        println!("  flow-state diagnostic (compare starved vs healthy):");
        dump("starved", recvd_stats.min_flow);
        dump("healthy", recvd_stats.max_flow);
    }

    mem_trace.print();
    print_lane_stats("many_tcp_fair", collect_lane_stats(&[&lane_a, &lane_b]));

    let tcp_socket_bytes = core::mem::size_of::<tcp::Socket>();
    let per_flow_bytes = tcp_socket_bytes + 2 * BUF;
    let total_bytes = 2 * n * per_flow_bytes;
    println!();
    println!("  socket-state footprint (without lane pool):");
    println!(
        "    per-flow:           {} bytes (Socket {} + 2 x {} KiB buf)",
        per_flow_bytes,
        tcp_socket_bytes,
        BUF / 1024,
    );
    println!(
        "    total (both peers): {} bytes  ({:.2} MiB)",
        total_bytes,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
}

/// `n` concurrent UDP echo flows. Same metrics as `many_tcp`. UDP has no
/// flow control or cwnd so per-flow throughput is bounded only by the rate
/// at which the runner pumps bytes through.
fn shape_many_udp(seconds: u64, n: usize, offload: bool, mode: RunMode) {
    const PAYLOAD: usize = 256;
    // Per-flow UDP socket buffer: a small ring with ~32 metadata slots is
    // enough to keep the pipe full without ballooning memory.
    const META_SLOTS: usize = 32;
    let qd = (n * 4).clamp(256, 8192);

    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
        offload,
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
        offload,
    );

    let mk_udp = || -> (udp::PacketBuffer<'static>, udp::PacketBuffer<'static>) {
        let rx_meta = vec![udp::PacketMetadata::EMPTY; META_SLOTS];
        let rx_data = vec![0u8; PAYLOAD * META_SLOTS];
        let tx_meta = vec![udp::PacketMetadata::EMPTY; META_SLOTS];
        let tx_data = vec![0u8; PAYLOAD * META_SLOTS];
        (
            udp::PacketBuffer::new(rx_meta, rx_data),
            udp::PacketBuffer::new(tx_meta, tx_data),
        )
    };

    let mut srv_handles = Vec::with_capacity(n);
    let mut cli_handles = Vec::with_capacity(n);
    let mut dst_metas: Vec<udp::UdpMetadata> = Vec::with_capacity(n);

    for i in 0..n {
        let dst_port: u16 = 10_000u16.wrapping_add(i as u16);
        let src_port: u16 = 30_000u16.wrapping_add(i as u16);

        let (rx, tx) = mk_udp();
        let h_srv = server.sockets.add(udp::Socket::new(rx, tx));
        server
            .sockets
            .get_mut::<udp::Socket>(h_srv)
            .bind(dst_port)
            .unwrap();
        srv_handles.push(h_srv);

        let (rx, tx) = mk_udp();
        let h_cli = client.sockets.add(udp::Socket::new(rx, tx));
        client
            .sockets
            .get_mut::<udp::Socket>(h_cli)
            .bind(src_port)
            .unwrap();
        cli_handles.push(h_cli);

        dst_metas.push((IpAddress::v4(10, 0, 0, 1), dst_port).into());
    }

    let payload = vec![0xa5u8; PAYLOAD];
    let mut sink = vec![0u8; PAYLOAD];
    let mut sent = vec![0u64; n];
    let mut recvd = vec![0u64; n];

    // Wall-clock-driven virtual time so smoltcp's retry/timeout state
    // machine behaves realistically even at modest iter rates.
    let wall0 = StdInstant::now();
    let smol_now = || Instant::from_micros(wall0.elapsed().as_micros() as i64);

    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let alloc_before = AllocSnap::now();
    let mut poll_lat = SampledTimer::new();
    let mut mem_trace = MemTrace::start();
    let mut iters: u64 = 0;

    loop {
        if iters & 0xFF == 0 && StdInstant::now() >= deadline {
            break;
        }
        let now = smol_now();

        // Push on every flow.
        for (i, &h) in cli_handles.iter().enumerate() {
            let cs = client.sockets.get_mut::<udp::Socket>(h);
            if cs.can_send() && cs.send_slice(&payload, dst_metas[i]).is_ok() {
                sent[i] += PAYLOAD as u64;
            }
        }

        poll_lat.measure(|| {
            client
                .iface
                .poll(now, &mut client.device, &mut client.sockets);
            server
                .iface
                .poll(now, &mut server.device, &mut server.sockets);
        });

        // Drain every server flow.
        for (i, &h) in srv_handles.iter().enumerate() {
            let ss = server.sockets.get_mut::<udp::Socket>(h);
            while ss.can_recv() {
                match ss.recv_slice(&mut sink) {
                    Ok((r, _)) => recvd[i] += r as u64,
                    Err(_) => break,
                }
            }
        }

        iters = iters.wrapping_add(1);
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "many_udp",
        elapsed,
        app_bytes_sent: sent.iter().sum(),
        app_bytes_recvd: recvd.iter().sum(),
        wire_packets: client.device.tx_packets + server.device.tx_packets,
        wire_bytes: client.device.tx_bytes + server.device.tx_bytes,
        poll_lat: poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: n as u64,
        unit_label: "flows",
    }
    .print();

    Fairness::from(&sent).print("sent");
    Fairness::from(&recvd).print("recvd");
    mem_trace.print();
    print_lane_stats("many_udp", collect_lane_stats(&[&lane_a, &lane_b]));

    let udp_socket_bytes = core::mem::size_of::<udp::Socket>();
    let per_flow_bytes = udp_socket_bytes + 2 * (META_SLOTS * PAYLOAD) + 2 * META_SLOTS * 24; // approx
    let total_bytes = 2 * n * per_flow_bytes;
    println!();
    println!("  socket-state footprint (without lane pool):");
    println!(
        "    per-flow approx:    {} bytes (Socket {} + 2 × {} pkt × {} B)",
        per_flow_bytes, udp_socket_bytes, META_SLOTS, PAYLOAD,
    );
    println!(
        "    total (both peers): {} bytes  ({:.2} MiB)",
        total_bytes,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
type MultiTcpPanic = Box<dyn std::any::Any + Send + 'static>;

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[derive(Clone, Copy, Eq, PartialEq)]
enum MultiTcpWorkerState {
    Setup,
    Running,
    Released,
    Cancelled,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
type MultiTcpWorkerGate =
    std::sync::Arc<(std::sync::Mutex<MultiTcpWorkerState>, std::sync::Condvar)>;

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn set_multi_tcp_worker_state(gate: &MultiTcpWorkerGate, state: MultiTcpWorkerState) {
    let (current, changed) = &**gate;
    *current.lock().unwrap_or_else(|error| error.into_inner()) = state;
    changed.notify_all();
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
enum MultiTcpWorkerEvent<R> {
    Ready(usize),
    Finished(usize, R),
    Failed(usize, MultiTcpPanic),
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
struct MultiTcpWorkerPhases<R> {
    worker_id: usize,
    gate: MultiTcpWorkerGate,
    events: std::sync::mpsc::Sender<MultiTcpWorkerEvent<R>>,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl<R> MultiTcpWorkerPhases<R> {
    fn ready(&mut self) -> bool {
        if self
            .events
            .send(MultiTcpWorkerEvent::Ready(self.worker_id))
            .is_err()
        {
            return false;
        }
        self.wait_while(MultiTcpWorkerState::Setup, MultiTcpWorkerState::Running)
    }

    fn finished(&mut self, result: R) -> bool {
        if self
            .events
            .send(MultiTcpWorkerEvent::Finished(self.worker_id, result))
            .is_err()
        {
            return false;
        }
        self.wait_while(MultiTcpWorkerState::Running, MultiTcpWorkerState::Released)
    }

    fn wait_while(&self, waiting: MultiTcpWorkerState, proceed: MultiTcpWorkerState) -> bool {
        let (state, changed) = &*self.gate;
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        while *state == waiting {
            state = changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        *state == proceed
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
struct MultiTcpWorkers<R> {
    worker_count: usize,
    gate: MultiTcpWorkerGate,
    events: std::sync::mpsc::Receiver<MultiTcpWorkerEvent<R>>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl<R> MultiTcpWorkers<R> {
    fn spawn<F>(worker_count: usize, worker: F) -> Self
    where
        R: Send + 'static,
        F: Fn(usize, MultiTcpWorkerPhases<R>) + Send + Sync + 'static,
    {
        let gate = std::sync::Arc::new((
            std::sync::Mutex::new(MultiTcpWorkerState::Setup),
            std::sync::Condvar::new(),
        ));
        let (event_tx, events) = std::sync::mpsc::channel();
        let worker = std::sync::Arc::new(worker);
        let mut handles = Vec::with_capacity(worker_count);

        for worker_id in 0..worker_count {
            let worker_gate = gate.clone();
            let events = event_tx.clone();
            let worker = worker.clone();
            let spawn = std::thread::Builder::new()
                .name(format!("multi-tcp-{worker_id}"))
                .spawn(move || {
                    let phases = MultiTcpWorkerPhases {
                        worker_id,
                        gate: worker_gate,
                        events: events.clone(),
                    };
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        worker(worker_id, phases);
                    }));
                    if let Err(panic) = outcome {
                        let _ = events.send(MultiTcpWorkerEvent::Failed(worker_id, panic));
                    }
                });

            match spawn {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    set_multi_tcp_worker_state(&gate, MultiTcpWorkerState::Cancelled);
                    for handle in handles {
                        let _ = handle.join();
                    }
                    panic!("failed to spawn multi_tcp worker {worker_id}: {error}");
                }
            }
        }
        drop(event_tx);

        Self {
            worker_count,
            gate,
            events,
            handles,
        }
    }

    fn wait_ready(&mut self) {
        let mut ready = vec![false; self.worker_count];
        let mut ready_count = 0;
        while ready_count < self.worker_count {
            let event = match self.events.recv() {
                Ok(event) => event,
                Err(_) => self.abort_with_message("worker event channel closed before ready"),
            };
            match event {
                MultiTcpWorkerEvent::Ready(worker_id)
                    if worker_id < self.worker_count && !ready[worker_id] =>
                {
                    ready[worker_id] = true;
                    ready_count += 1;
                }
                MultiTcpWorkerEvent::Ready(worker_id) => self.abort_with_message(&format!(
                    "invalid or duplicate ready event from worker {worker_id}"
                )),
                MultiTcpWorkerEvent::Finished(worker_id, _) => self.abort_with_message(&format!(
                    "worker {worker_id} finished before the steady phase"
                )),
                MultiTcpWorkerEvent::Failed(worker_id, panic) => {
                    self.abort_worker_panic(worker_id, panic)
                }
            }
        }
    }

    fn start(&self) {
        set_multi_tcp_worker_state(&self.gate, MultiTcpWorkerState::Running);
    }

    fn wait_finished(&mut self) -> Vec<R> {
        let mut results: Vec<Option<R>> = std::iter::repeat_with(|| None)
            .take(self.worker_count)
            .collect();
        let mut finished_count = 0;
        while finished_count < self.worker_count {
            let event = match self.events.recv() {
                Ok(event) => event,
                Err(_) => self.abort_with_message("worker event channel closed before finish"),
            };
            match event {
                MultiTcpWorkerEvent::Finished(worker_id, result)
                    if worker_id < self.worker_count && results[worker_id].is_none() =>
                {
                    results[worker_id] = Some(result);
                    finished_count += 1;
                }
                MultiTcpWorkerEvent::Finished(worker_id, _) => self.abort_with_message(&format!(
                    "invalid or duplicate finish event from worker {worker_id}"
                )),
                MultiTcpWorkerEvent::Ready(worker_id) => self
                    .abort_with_message(&format!("duplicate ready event from worker {worker_id}")),
                MultiTcpWorkerEvent::Failed(worker_id, panic) => {
                    self.abort_worker_panic(worker_id, panic)
                }
            }
        }
        results.into_iter().map(Option::unwrap).collect()
    }

    fn release_and_join(mut self) {
        set_multi_tcp_worker_state(&self.gate, MultiTcpWorkerState::Released);
        let join_panic = self.join_all();
        if let Some((_, panic)) = self.take_worker_panic() {
            std::panic::resume_unwind(panic);
        }
        if let Some(panic) = join_panic {
            std::panic::resume_unwind(panic);
        }
    }

    fn abort_worker_panic(&mut self, _worker_id: usize, panic: MultiTcpPanic) -> ! {
        self.cancel();
        let _ = self.join_all();
        std::panic::resume_unwind(panic);
    }

    fn abort_with_message(&mut self, message: &str) -> ! {
        self.cancel();
        let join_panic = self.join_all();
        if let Some((_, panic)) = self.take_worker_panic() {
            std::panic::resume_unwind(panic);
        }
        if let Some(panic) = join_panic {
            std::panic::resume_unwind(panic);
        }
        panic!("{message}");
    }

    fn cancel(&self) {
        set_multi_tcp_worker_state(&self.gate, MultiTcpWorkerState::Cancelled);
    }

    fn join_all(&mut self) -> Option<MultiTcpPanic> {
        let mut first_panic = None;
        for handle in self.handles.drain(..) {
            if let Err(panic) = handle.join()
                && first_panic.is_none()
            {
                first_panic = Some(panic);
            }
        }
        first_panic
    }

    fn take_worker_panic(&self) -> Option<(usize, MultiTcpPanic)> {
        self.events.try_iter().find_map(|event| match event {
            MultiTcpWorkerEvent::Failed(worker_id, panic) => Some((worker_id, panic)),
            _ => None,
        })
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl<R> Drop for MultiTcpWorkers<R> {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            self.cancel();
            let _ = self.join_all();
        }
    }
}

/// Multi-Interface pool-contention shape. Spawns `n_threads` threads, each
/// owning its own server/client `Interface` pair and `flows_per_thread`
/// TCP echo flows, all sharing a single [`tcp::MemoryPool`]. Measures the
/// aggregate throughput scaling and serves as a regression gate against
/// pool-counter cache-line / CAS-retry contention.
///
/// Each thread runs the same workload as `many_tcp` but with sockets
/// created via `new_dynamic` so the pool is exercised. Threads start
/// together only after every worker reports setup complete.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[derive(Clone, Copy)]
enum MultiTcpWorkload {
    Echo,
    Sink,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl MultiTcpWorkload {
    fn shape_name(self) -> &'static str {
        match self {
            Self::Echo => "multi_tcp",
            Self::Sink => "multi_tcp_sink",
        }
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_multi_tcp(seconds: u64, n_threads: usize, flows_per_thread: usize, offload: bool) {
    shape_multi_tcp_impl(
        seconds,
        n_threads,
        flows_per_thread,
        offload,
        MultiTcpWorkload::Echo,
    );
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_multi_tcp_sink(seconds: u64, n_threads: usize, flows_per_thread: usize, offload: bool) {
    shape_multi_tcp_impl(
        seconds,
        n_threads,
        flows_per_thread,
        offload,
        MultiTcpWorkload::Sink,
    );
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_multi_tcp_impl(
    seconds: u64,
    n_threads: usize,
    flows_per_thread: usize,
    offload: bool,
    workload: MultiTcpWorkload,
) {
    use std::time::Instant as StdInstant;

    const MAX_BUF: u32 = 32 * 1024;
    const PAYLOAD: usize = 1024;
    let total_flows = n_threads * flows_per_thread;
    // One full rx+tx budget per logical flow. The paired endpoints share the
    // pool, so this keeps the usual one-active-direction workload clear of
    // growth refusal while still exercising allocator contention.
    let pool_bytes: usize = total_flows * 2 * MAX_BUF as usize;
    let pool = tcp::MemoryPool::new(pool_bytes);

    let (vol_before, nvol_before) = ctxsw_counts();
    let worker_pool = pool.clone();
    let mut workers = MultiTcpWorkers::spawn(n_threads, move |tid, mut phases| {
        let pool = worker_pool.clone();
        {
            let qd = (flows_per_thread * 16).clamp(1024, 16384);
            // Distinct base address per thread so server/client tuples don't
            // clash if anything inspects them (they won't — lanes are isolated).
            let base = 10 + tid as u8;
            let (mut server, mut client, lane_a, lane_b) =
                setup_paired_endpoints(base, 1500, qd, offload);

            let mut srv_handles = Vec::with_capacity(flows_per_thread);
            let mut cli_handles = Vec::with_capacity(flows_per_thread);
            for i in 0..flows_per_thread {
                let h_srv = add_tcp_socket_dyn(&mut server, MAX_BUF, &pool);
                let h_cli = add_tcp_socket_dyn(&mut client, MAX_BUF, &pool);
                let dst_port: u16 = 10_000u16.wrapping_add(i as u16);
                let src_port: u16 = 30_000u16.wrapping_add(i as u16);
                {
                    let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
                    s.set_ack_delay(None);
                    s.set_nagle_enabled(false);
                    s.listen(dst_port).unwrap();
                }
                {
                    let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
                    c.set_ack_delay(None);
                    c.set_nagle_enabled(false);
                }
                client
                    .sockets
                    .get_mut::<tcp::Socket>(h_cli)
                    .connect(
                        client.iface.context(),
                        (IpAddress::v4(base, 0, 0, 1), dst_port),
                        src_port,
                    )
                    .unwrap();
                srv_handles.push(h_srv);
                cli_handles.push(h_cli);
            }

            // Drive until ESTABLISHED on every flow.
            let smol_now = |w0: StdInstant| Instant::from_micros(w0.elapsed().as_micros() as i64);
            let w0 = StdInstant::now();
            let connect_deadline = w0 + std::time::Duration::from_secs(seconds.min(5));
            loop {
                let now = smol_now(w0);
                server
                    .iface
                    .poll(now, &mut server.device, &mut server.sockets);
                client
                    .iface
                    .poll(now, &mut client.device, &mut client.sockets);
                let all_ready = cli_handles
                    .iter()
                    .zip(srv_handles.iter())
                    .all(|(&hc, &hs)| {
                        client.sockets.get::<tcp::Socket>(hc).may_send()
                            && server.sockets.get::<tcp::Socket>(hs).may_recv()
                    });
                if all_ready || StdInstant::now() >= connect_deadline {
                    break;
                }
            }

            // Setup and start coordination stays outside the timed traffic loop.
            if !phases.ready() {
                return;
            }
            let steady_start = StdInstant::now();

            let mut sent: u64 = 0;
            let mut recvd: u64 = 0;
            let payload = vec![0xa5u8; PAYLOAD];
            let mut sink = vec![0u8; PAYLOAD];
            let deadline = steady_start + std::time::Duration::from_secs(seconds);
            match workload {
                MultiTcpWorkload::Echo => {
                    while StdInstant::now() < deadline {
                        let now = smol_now(w0);
                        for &h in &cli_handles {
                            let s = client.sockets.get_mut::<tcp::Socket>(h);
                            if s.can_send()
                                && let Ok(n) = s.send_slice(&payload)
                            {
                                sent += n as u64;
                            }
                        }
                        client
                            .iface
                            .poll(now, &mut client.device, &mut client.sockets);
                        server
                            .iface
                            .poll(now, &mut server.device, &mut server.sockets);
                        for &h in &srv_handles {
                            let s = server.sockets.get_mut::<tcp::Socket>(h);
                            while s.can_recv() {
                                match s.recv_slice(&mut sink) {
                                    Ok(r) if r > 0 => {
                                        recvd += r as u64;
                                        if s.can_send() {
                                            let _ = s.send_slice(&sink[..r]);
                                        }
                                    }
                                    _ => break,
                                }
                            }
                        }
                        server
                            .iface
                            .poll(now, &mut server.device, &mut server.sockets);
                        client
                            .iface
                            .poll(now, &mut client.device, &mut client.sockets);
                        for &h in &cli_handles {
                            let s = client.sockets.get_mut::<tcp::Socket>(h);
                            while s.can_recv() {
                                match s.recv_slice(&mut sink) {
                                    Ok(r) if r > 0 => {}
                                    _ => break,
                                }
                            }
                        }
                    }
                }
                MultiTcpWorkload::Sink => {
                    while StdInstant::now() < deadline {
                        let now = smol_now(w0);
                        for &h in &cli_handles {
                            let s = client.sockets.get_mut::<tcp::Socket>(h);
                            if s.can_send() {
                                let wrote = s
                                    .send(|buf| {
                                        let n = buf.len().min(PAYLOAD);
                                        buf[..n].fill(0xa5);
                                        (n, n)
                                    })
                                    .unwrap_or(0);
                                sent += wrote as u64;
                            }
                        }
                        client
                            .iface
                            .poll(now, &mut client.device, &mut client.sockets);
                        server
                            .iface
                            .poll(now, &mut server.device, &mut server.sockets);
                        for &h in &srv_handles {
                            let s = server.sockets.get_mut::<tcp::Socket>(h);
                            while s.can_recv() {
                                match s.recv(|buf| {
                                    let n = buf.len();
                                    (n, n)
                                }) {
                                    Ok(r) if r > 0 => recvd += r as u64,
                                    _ => break,
                                }
                            }
                        }
                        server
                            .iface
                            .poll(now, &mut server.device, &mut server.sockets);
                        client
                            .iface
                            .poll(now, &mut client.device, &mut client.sockets);
                    }
                }
            }
            let elapsed_us = steady_start.elapsed().as_micros() as u64;
            let lane_stats = collect_lane_stats(&[&lane_a, &lane_b]);
            // Keep every worker's sockets alive until the main thread samples
            // the active-end memory and pool boundaries.
            let _ = phases.finished((sent, recvd, elapsed_us, lane_stats));
        }
    });

    // Wait until every worker has connected its sockets, then bracket only the
    // steady phase. Workers remain blocked before start and after finish.
    workers.wait_ready();
    let rss_start = rss_bytes();
    let alloc_before = alloc_counters_with_rss(rss_start);
    workers.start();
    let results = workers.wait_finished();
    let mut alloc_after = alloc_counters_with_rss(0);
    let pool_active = pool.used();
    alloc_after.rss = rss_bytes();
    workers.release_and_join();
    let pool_after_teardown = pool.used();
    let memory_report = MultiTcpMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        pool_active,
        pool_after_teardown,
    );
    let total_elapsed_us = results
        .iter()
        .map(|(_, _, elapsed_us, _)| *elapsed_us)
        .max()
        .unwrap_or(1)
        .max(1);
    let total_secs = total_elapsed_us as f64 / 1_000_000.0;
    let (vol_after, nvol_after) = ctxsw_counts();
    let vol_delta = vol_after - vol_before;
    let nvol_delta = nvol_after - nvol_before;
    let agg_sent: u64 = results.iter().map(|(s, _, _, _)| s).sum();
    let agg_recvd: u64 = results.iter().map(|(_, r, _, _)| r).sum();
    let agg_gbps = (agg_recvd as f64 * 8.0) / total_secs / 1e9;
    let per_thread_gbps: Vec<f64> = results
        .iter()
        .map(|(_, r, elapsed_us, _)| {
            let elapsed_us = (*elapsed_us).max(1);
            (*r as f64 * 8.0) / (elapsed_us as f64 / 1_000_000.0) / 1e9
        })
        .collect();
    let mut lane_stats = LaneStats::default();
    for (_, _, _, stats) in &results {
        lane_stats.merge(*stats);
    }
    let min = per_thread_gbps
        .iter()
        .cloned()
        .fold(f64::INFINITY, f64::min);
    let max = per_thread_gbps.iter().cloned().fold(0f64, f64::max);
    let mean = agg_gbps / n_threads as f64;
    let cv = if mean > 0.0 {
        let variance: f64 = per_thread_gbps
            .iter()
            .map(|v| (v - mean).powi(2))
            .sum::<f64>()
            / per_thread_gbps.len() as f64;
        variance.sqrt() / mean
    } else {
        0.0
    };
    // Jain's fairness across threads.
    let sum: f64 = per_thread_gbps.iter().sum();
    let sum_sq: f64 = per_thread_gbps.iter().map(|v| v * v).sum();
    let jain = if sum_sq > 0.0 {
        (sum * sum) / (n_threads as f64 * sum_sq)
    } else {
        0.0
    };

    let shape_name = workload.shape_name();
    println!("\n========== shape: {shape_name} ==========");
    println!(
        "  threads:                {n_threads}   flows/thread: {flows_per_thread}   total flows: {total_flows}"
    );
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    memory_report.print();
    println!("  elapsed:                {:.3}s", total_secs);
    println!(
        "  aggregate app sent:     {:.3} GB    ({:.3} Gbps)",
        agg_sent as f64 / 1e9,
        (agg_sent as f64 * 8.0) / total_secs / 1e9
    );
    println!(
        "  aggregate app recvd:    {:.3} GB    ({:.3} Gbps)",
        agg_recvd as f64 / 1e9,
        agg_gbps,
    );
    println!("  per-thread throughput:");
    for (i, gbps) in per_thread_gbps.iter().enumerate() {
        println!("    t{i:>2}: {gbps:>7.3} Gbps");
    }
    println!(
        "  min/max/mean:           {min:.3} / {max:.3} / {mean:.3} Gbps   CV: {cv:.4}   Jain: {jain:.4}"
    );
    let verdict = if jain >= 0.95 {
        "FAIR (pool contention bounded)"
    } else {
        "UNFAIR (pool contention or scheduling)"
    };
    println!("  verdict: {verdict}");
    println!(
        "  context switches:       {vol_delta} voluntary, {nvol_delta} nonvoluntary  ({:.0} cs/thread/s)",
        (vol_delta + nvol_delta) as f64 / n_threads as f64 / total_secs
    );
    let cas_retries = pool.cas_retries();
    println!(
        "  pool CAS retries:       {cas_retries}  ({:.1} retries/thread/s)",
        cas_retries as f64 / n_threads as f64 / total_secs
    );
    print_lane_stats(shape_name, lane_stats);
}

/// Connection-churn shape. Repeatedly opens and tears down TCP flows at
/// `target_conn_per_sec`, each exchanging one short payload before close.
/// Exercises the release path (`set_state(Closed)`, `reset()`, `Drop`)
/// under load; verifies that pool refunds keep up with admissions and
/// the connection cap doesn't drift.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_churn(seconds: u64, target_conn_per_sec: usize, offload: bool, mode: RunMode) {
    use std::time::Instant as StdInstant;
    const MAX_BUF: u32 = 32 * 1024;
    const SLOTS: usize = 256;
    const PAYLOAD: usize = 128;

    let qd = (SLOTS * 16).clamp(1024, 16384);
    let pool_bytes: usize = SLOTS * 2 * MAX_BUF as usize;
    let pool = tcp::MemoryPool::new(pool_bytes);

    let (mut server, mut client, lane_a, lane_b) = setup_paired_endpoints(10, 1500, qd, offload);

    // Pre-allocate a ring of socket handles. Each "churn slot" is a pair
    // we cycle through; once a pair is fully torn down we recycle the slot.
    let mut slots: Vec<(
        smoltcp::iface::SocketHandle,
        smoltcp::iface::SocketHandle,
        u16,
    )> = Vec::with_capacity(SLOTS);
    for i in 0..SLOTS {
        let h_srv = add_tcp_socket_dyn(&mut server, MAX_BUF, &pool);
        let h_cli = add_tcp_socket_dyn(&mut client, MAX_BUF, &pool);
        slots.push((h_srv, h_cli, i as u16));
    }

    let alloc_before = AllocSnap::now();
    let start = StdInstant::now();
    let mut mem_trace = MemTrace::start();
    let smol_now = |w0: StdInstant| Instant::from_micros(w0.elapsed().as_micros() as i64);

    let mut next_slot = 0usize;
    let mut opened: u64 = 0;
    let mut closed: u64 = 0;
    let mut bytes_xferred: u64 = 0;
    let payload = vec![0xc5u8; PAYLOAD];
    let mut scratch = vec![0u8; PAYLOAD];
    let interval_us = if target_conn_per_sec > 0 {
        1_000_000 / target_conn_per_sec as u64
    } else {
        100
    };
    let mut next_open_us: u64 = 0;
    let deadline = start + std::time::Duration::from_secs(seconds);

    while StdInstant::now() < deadline {
        let elapsed_us = start.elapsed().as_micros() as u64;

        // Time to open another connection? Walk forward through slots
        // until we've either caught up to the schedule or exhausted free
        // slots; recycled slots become available as soon as both halves
        // are Closed (abort path avoids TIME_WAIT — the workload we want
        // here is admission-and-release rate, not a graceful shutdown
        // microbench).
        while elapsed_us >= next_open_us {
            let slot = next_slot % SLOTS;
            let (h_srv, h_cli, base_port) = slots[slot];
            let cs = client.sockets.get_mut::<tcp::Socket>(h_cli);
            let ss = server.sockets.get_mut::<tcp::Socket>(h_srv);
            if matches!(cs.state(), tcp::State::Closed)
                && matches!(ss.state(), tcp::State::Closed | tcp::State::Listen)
            {
                let dst_port: u16 = 10_000u16.wrapping_add(base_port);
                let src_port: u16 = 30_000u16.wrapping_add(opened as u16);
                ss.set_ack_delay(None);
                ss.set_nagle_enabled(false);
                let _ = ss.listen(dst_port);
                cs.set_ack_delay(None);
                cs.set_nagle_enabled(false);
                let _ = cs.connect(
                    client.iface.context(),
                    (IpAddress::v4(10, 0, 0, 1), dst_port),
                    src_port,
                );
                opened += 1;
                next_open_us += interval_us;
            }
            next_slot += 1;
            if next_slot.is_multiple_of(SLOTS) {
                break; // one full sweep per outer iteration max
            }
        }

        let now = smol_now(start);

        for &(_h_srv, h_cli, _) in &slots {
            let cs = client.sockets.get_mut::<tcp::Socket>(h_cli);
            if cs.can_send() {
                let _ = cs.send_slice(&payload);
            }
        }
        client
            .iface
            .poll(now, &mut client.device, &mut client.sockets);
        server
            .iface
            .poll(now, &mut server.device, &mut server.sockets);

        // Server: drain. After receiving payload, abort the connection
        // (skips TIME_WAIT so the slot recycles immediately). Client
        // sees the RST and transitions to Closed on its next poll.
        for &(h_srv, _h_cli, _) in &slots {
            let ss = server.sockets.get_mut::<tcp::Socket>(h_srv);
            if ss.can_recv()
                && let Ok(n) = ss.recv_slice(&mut scratch)
                && n > 0
            {
                bytes_xferred += n as u64;
                ss.abort();
                closed += 1;
            }
        }
        server
            .iface
            .poll(now, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(now, &mut client.device, &mut client.sockets);
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let alloc_after = AllocSnap::now();
    let conn_rate = opened as f64 / elapsed;
    let close_rate = closed as f64 / elapsed;
    let alloc_bytes = alloc_after.alloc_bytes - alloc_before.alloc_bytes;
    let free_bytes = alloc_after.free_bytes - alloc_before.free_bytes;
    let alloc_count = alloc_after.alloc_count - alloc_before.alloc_count;

    // Two pool readings with different roles. At the deadline, slots can
    // legitimately hold charge: connections mid-lifecycle, plus sockets
    // whose peer-side abort left undrained rx — which stays readable (and
    // charged) until the slot recycles, per the `may_recv`-after-Closed
    // contract. That value is a bounded diagnostic. The *leak gate* is the
    // post-teardown reading: dropping the sockets must refund every byte.
    let pool_at_deadline = pool.used();
    drop(server);
    drop(client);
    let pool_after_teardown = pool.used();

    println!("\n========== shape: churn ==========");
    println!("  target rate:            {} conn/s", target_conn_per_sec);
    println!("  slot ring size:         {SLOTS}");
    println!("  elapsed:                {elapsed:.3}s");
    println!("  opened:                 {opened}   ({conn_rate:.1} conn/s)");
    println!("  closed:                 {closed}   ({close_rate:.1} conn/s)");
    println!("  app bytes xfer:         {bytes_xferred}");
    println!(
        "  pool used at deadline:  {} KiB  (in-flight + retained rx; bounded)",
        pool_at_deadline / 1024
    );
    println!(
        "  pool used (end):        {} KiB  (after teardown; leak gate, expect 0)",
        pool_after_teardown / 1024
    );
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    println!("  RSS start:              {} KiB", alloc_before.rss / 1024);
    println!("  RSS end:                {} KiB", alloc_after.rss / 1024);
    println!("  bytes allocated:        {alloc_bytes}");
    println!("  bytes freed:            {free_bytes}");
    println!(
        "  net heap delta:         {}",
        alloc_bytes as i64 - free_bytes as i64
    );
    println!("  allocation count:       {alloc_count}");
    mem_trace.print();
    print_lane_stats("churn", collect_lane_stats(&[&lane_a, &lane_b]));
}

/// Mixed idle + active shape. Creates `n_idle` TCP sockets that never see
/// data and `n_active` TCP sockets that run a steady-state echo workload.
/// All share one [`tcp::MemoryPool`]. The point is to verify that lazy
/// allocation keeps idle-flow memory at ~0 while active flows still hit
/// full throughput.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_idle_hot(seconds: u64, n_idle: usize, n_active: usize, offload: bool, mode: RunMode) {
    use std::time::Instant as StdInstant;

    const MAX_BUF: u32 = 32 * 1024;
    const PAYLOAD: usize = 1024;
    let total = n_idle + n_active;
    let qd = (total * 16).clamp(1024, 16384);
    let active_socket_count = n_active * 2; // client + server
    let expected_steady_bytes = active_socket_count * 2 * MAX_BUF as usize; // rx + tx per socket
    let pool_bytes: usize = expected_steady_bytes
        .saturating_add(2 * MAX_BUF as usize)
        .max(2 * MAX_BUF as usize);
    let pool = tcp::MemoryPool::new(pool_bytes);

    let (mut server, mut client, lane_a, lane_b) = setup_paired_endpoints(10, 1500, qd, offload);

    // Active flows: open & connect.
    let mut srv_active: Vec<smoltcp::iface::SocketHandle> = Vec::with_capacity(n_active);
    let mut cli_active: Vec<smoltcp::iface::SocketHandle> = Vec::with_capacity(n_active);
    for i in 0..n_active {
        let h_srv = add_tcp_socket_dyn(&mut server, MAX_BUF, &pool);
        let h_cli = add_tcp_socket_dyn(&mut client, MAX_BUF, &pool);
        let dst_port: u16 = 10_000u16.wrapping_add(i as u16);
        let src_port: u16 = 30_000u16.wrapping_add(i as u16);
        {
            let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
            s.set_ack_delay(None);
            s.set_nagle_enabled(false);
            s.listen(dst_port).unwrap();
        }
        {
            let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
            c.set_ack_delay(None);
            c.set_nagle_enabled(false);
        }
        client
            .sockets
            .get_mut::<tcp::Socket>(h_cli)
            .connect(
                client.iface.context(),
                (IpAddress::v4(10, 0, 0, 1), dst_port),
                src_port,
            )
            .unwrap();
        srv_active.push(h_srv);
        cli_active.push(h_cli);
    }

    // Idle flows: create sockets, do not connect — they sit in Closed state
    // and exercise the dyn-buffer footprint that idle sockets pay for.
    for _ in 0..n_idle {
        let _ = add_tcp_socket_dyn(&mut server, MAX_BUF, &pool);
        let _ = add_tcp_socket_dyn(&mut client, MAX_BUF, &pool);
    }

    let rss_after_create = rss_bytes();
    let pool_after_create = pool.used();

    let clock_start = StdInstant::now();
    let smol_now = |w0: StdInstant| Instant::from_micros(w0.elapsed().as_micros() as i64);

    // Establish active flows.
    let connect_deadline = clock_start + std::time::Duration::from_secs(seconds.min(5));
    loop {
        let now = smol_now(clock_start);
        server
            .iface
            .poll(now, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(now, &mut client.device, &mut client.sockets);
        let ready = cli_active.iter().zip(srv_active.iter()).all(|(&hc, &hs)| {
            client.sockets.get::<tcp::Socket>(hc).may_send()
                && server.sockets.get::<tcp::Socket>(hs).may_recv()
        });
        if ready || StdInstant::now() >= connect_deadline {
            if !ready && n_active > 0 {
                let est = cli_active
                    .iter()
                    .zip(srv_active.iter())
                    .filter(|&(&hc, &hs)| {
                        matches!(
                            client.sockets.get::<tcp::Socket>(hc).state(),
                            tcp::State::Established
                        ) && matches!(
                            server.sockets.get::<tcp::Socket>(hs).state(),
                            tcp::State::Established
                        )
                    })
                    .count();
                eprintln!(
                    "warning: only {est}/{n_active} idle_hot flows established within {} s",
                    seconds.min(5)
                );
            }
            break;
        }
    }

    // Steady-state echo on active flows only.
    let mut sent: u64 = 0;
    let mut recvd: u64 = 0;
    let payload = vec![0xa5u8; PAYLOAD];
    let mut sink = vec![0u8; PAYLOAD];
    let steady_start = StdInstant::now();
    let deadline = steady_start + std::time::Duration::from_secs(seconds);
    let alloc_before = AllocSnap::now();
    let mut mem_trace = MemTrace::start();
    while StdInstant::now() < deadline {
        let now = smol_now(clock_start);
        for &h in &cli_active {
            let s = client.sockets.get_mut::<tcp::Socket>(h);
            if s.can_send()
                && let Ok(n) = s.send_slice(&payload)
            {
                sent += n as u64;
            }
        }
        client
            .iface
            .poll(now, &mut client.device, &mut client.sockets);
        server
            .iface
            .poll(now, &mut server.device, &mut server.sockets);
        for &h in &srv_active {
            let s = server.sockets.get_mut::<tcp::Socket>(h);
            while s.can_recv() {
                match s.recv_slice(&mut sink) {
                    Ok(r) if r > 0 => {
                        recvd += r as u64;
                        if s.can_send() {
                            let _ = s.send_slice(&sink[..r]);
                        }
                    }
                    _ => break,
                }
            }
        }
        server
            .iface
            .poll(now, &mut server.device, &mut server.sockets);
        client
            .iface
            .poll(now, &mut client.device, &mut client.sockets);
        for &h in &cli_active {
            let s = client.sockets.get_mut::<tcp::Socket>(h);
            while s.can_recv() {
                if s.recv_slice(&mut sink).map(|r| r > 0).unwrap_or(false) {
                    continue;
                }
                break;
            }
        }
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let elapsed = steady_start.elapsed().as_secs_f64();
    let mut alloc_after = AllocSnap::now();
    let pool_steady = pool.used();
    let rss_end = rss_bytes();
    alloc_after.rss = rss_end;
    let lane_stats = collect_lane_stats(&[&lane_a, &lane_b]);
    drop(server);
    drop(client);
    let pool_after_teardown = pool.used();
    let memory_report = MultiTcpMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        pool_steady,
        pool_after_teardown,
    );
    let harness_reserved = lane_stats.reserved_total_bytes() as u64;
    let rss_after_create_less_harness = rss_after_create.saturating_sub(harness_reserved);
    let rss_end_less_harness = rss_end.saturating_sub(harness_reserved);
    let gbps = (recvd as f64 * 8.0) / elapsed / 1e9;

    println!("\n========== shape: idle_hot ==========");
    println!("  idle flows:             {n_idle}");
    println!("  active flows:           {n_active}");
    println!(
        "  per-flow max budget:    {} KiB (rx) + {} KiB (tx)",
        MAX_BUF / 1024,
        MAX_BUF / 1024,
    );
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    println!("  RSS post-create:        {} KiB", rss_after_create / 1024);
    println!(
        "  RSS post-create less harness: {} KiB  (excludes harness packet pool)",
        rss_after_create_less_harness / 1024
    );
    println!(
        "  pool used post-create:  {} KiB  (expect ~0)",
        pool_after_create / 1024
    );
    println!("  elapsed:                {elapsed:.3}s");
    println!("  app sent / recvd:       {} / {}", sent, recvd);
    println!("  active throughput:      {gbps:.3} Gbps");
    memory_report.print();
    println!(
        "  RSS end less harness:   {} KiB  (excludes harness packet pool)",
        rss_end_less_harness / 1024
    );
    println!(
        "  expected: idle pool charge ~= 0 KiB; steady upper bound is {} KiB (active client/server sockets x rx/tx max)",
        expected_steady_bytes / 1024
    );
    mem_trace.print();
    print_lane_stats("idle_hot", lane_stats);
}

fn print_socket_sizes() {
    use core::mem::size_of;
    use smoltcp::socket;
    use smoltcp::storage::*;
    println!("\n========== smoltcp footprint (bytes) ==========");
    println!(
        "  TCP socket:             {:>6}",
        size_of::<socket::tcp::Socket>()
    );
    println!(
        "  UDP socket:             {:>6}",
        size_of::<socket::udp::Socket>()
    );
    #[cfg(feature = "socket-icmp")]
    println!(
        "  ICMP socket:            {:>6}",
        size_of::<socket::icmp::Socket>()
    );
    #[cfg(feature = "socket-raw")]
    println!(
        "  Raw socket:             {:>6}",
        size_of::<socket::raw::Socket>()
    );
    println!(
        "  RingBuffer<u8>:         {:>6}",
        size_of::<RingBuffer<u8>>()
    );
    println!("  Assembler:              {:>6}", size_of::<Assembler>());
    println!(
        "  IpRepr / TcpRepr:       {:>3} / {:>3}",
        size_of::<smoltcp::wire::IpRepr>(),
        size_of::<smoltcp::wire::TcpRepr>()
    );
}

const USAGE: &str = "\
Usage:
  profile_loopback [--mode bench|trace] <shape> <seconds> [offload]
  profile_loopback [--mode bench|trace] many_tcp|many_tcp_fair|many_udp <seconds> <flows> [offload]
  profile_loopback [--mode bench|trace] multi_tcp|multi_tcp_sink <seconds> <threads> <flows-per-thread> [offload]
  profile_loopback [--mode bench|trace] churn <seconds> <rate> [offload]
  profile_loopback [--mode bench|trace] idle_hot <seconds> <idle> <active> [offload]

Shapes without extra parameters: udp, firehose, pingpong, small, all
Dynamic shapes require --features socket-tcp-dynamic-buffer.
The optional final offload value is exactly one of: offload, 1, true.";

fn main() -> ExitCode {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::builder()
        .file_name("dhat-heap.json")
        .build();
    let config = match parse_args(env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };

    println!(
        "config: mode={} | {} checksums ({}{})",
        config.mode.label(),
        if config.offload_checksums {
            "device-offloaded"
        } else {
            "full software"
        },
        if config.offload_checksums {
            "mimics a NIC or iOS NEPacketTunnelFlow"
        } else {
            "worst case"
        },
        match config.shape.flow_count() {
            Some(n) => format!(", {n} flows"),
            None => String::new(),
        }
    );
    print_socket_sizes();

    let seconds = config.seconds.get();
    let offload = config.offload_checksums;
    let mode = config.mode;
    match config.shape {
        TrafficShape::Firehose => shape_firehose(seconds, offload),
        TrafficShape::PingPong => shape_pingpong(seconds, offload),
        TrafficShape::Small => shape_small(seconds, offload),
        TrafficShape::Udp => shape_udp_firehose(seconds, offload),
        TrafficShape::All => {
            shape_udp_firehose(seconds, offload);
            shape_small(seconds, offload);
            shape_pingpong(seconds, offload);
        }
        TrafficShape::ManyTcp { flows } => shape_many_tcp(seconds, flows.get(), offload, mode),
        TrafficShape::ManyTcpFair { flows } => {
            shape_many_tcp_fair(seconds, flows.get(), offload, mode)
        }
        TrafficShape::ManyUdp { flows } => shape_many_udp(seconds, flows.get(), offload, mode),
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::MultiTcp {
            threads,
            flows_per_thread,
        } => {
            shape_multi_tcp(seconds, threads.get(), flows_per_thread.get(), offload);
        }
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::MultiTcpSink {
            threads,
            flows_per_thread,
        } => {
            shape_multi_tcp_sink(seconds, threads.get(), flows_per_thread.get(), offload);
        }
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::Churn { rate } => {
            shape_churn(seconds, rate.get(), offload, mode);
        }
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::IdleHot { idle, active } => {
            shape_idle_hot(seconds, idle, active, offload, mode);
        }
    }

    ExitCode::SUCCESS
}
