//! In-process smoltcp throughput / latency profiler.
//!
//! Wires two `Interface`s back-to-back through a pair of in-memory packet
//! queues (no tun/tap, no syscalls per packet), then drives different
//! traffic shapes through them and reports a comprehensive metrics block:
//!
//!   * throughput (Gbps app, Gbps wire, Mpps)
//!   * per-poll latency: mean / p50 / p90 / p99 / max
//!   * allocation count + bytes allocated (instrumented allocator)
//!   * process memory (Linux RSS; Apple physical footprint)
//!   * smoltcp Socket footprint of relevant sockets
//!   * `cycles_estimate` per packet from a 2.4 GHz reference
//!
//! Designed to run under `perf`, Massif, or Heaptrack without external setup.
//! Traffic commands live in the iOS gate manifests under `ci/`; see
//! `./ci.sh help` for verification entry points.

#[cfg(not(feature = "dhat-heap"))]
use std::alloc::System;
use std::alloc::{GlobalAlloc, Layout};
use std::collections::VecDeque;
use std::env;
use std::num::{NonZeroU64, NonZeroUsize};
use std::process::ExitCode;
use std::str::FromStr;
#[cfg(feature = "socket-tcp-dynamic-buffer")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant as StdInstant};

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[path = "profile_loopback/multi_tcp_workers.rs"]
mod multi_tcp_workers;
mod process_memory;
#[cfg(feature = "socket-tcp-dynamic-buffer")]
use multi_tcp_workers::MultiTcpWorkers;
use process_memory::{
    ProcessMemorySample, process_memory_bytes, process_memory_label, process_memory_sample,
    signed_delta,
};

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
    RstUnreadRx {
        flows: NonZeroUsize,
    },
    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    PoolPressure {
        flows: NonZeroUsize,
    },
    #[cfg(feature = "socket-tcp-dynamic-buffer")]
    MixedTcpUdp {
        tcp_flows: NonZeroUsize,
        udp_flows: NonZeroUsize,
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
    let mode = match args.peek().map(String::as_str) {
        Some("--mode") => {
            args.next();
            args.next()
                .ok_or_else(|| "missing value for --mode, expected bench|trace".to_owned())?
                .parse()?
        }
        Some(arg) if arg.starts_with("--mode=") => {
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
        return Err(MODE_POSITION_ERROR.to_owned());
    }
    if shape_name.starts_with('-') {
        return Err(format!("unknown option '{shape_name}'"));
    }

    let seconds = next_nonzero_u64(&mut args, "seconds")?;

    let shape = match shape_name.as_str() {
        "udp" => TrafficShape::Udp,
        "firehose" => TrafficShape::Firehose,
        "pingpong" => TrafficShape::PingPong,
        "small" => TrafficShape::Small,
        "all" => TrafficShape::All,
        "many_tcp" => TrafficShape::ManyTcp {
            flows: next_nonzero_usize(&mut args, "flows")?,
        },
        "many_tcp_fair" => TrafficShape::ManyTcpFair {
            flows: next_nonzero_usize(&mut args, "flows")?,
        },
        "many_udp" => TrafficShape::ManyUdp {
            flows: next_nonzero_usize(&mut args, "flows")?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "multi_tcp" => TrafficShape::MultiTcp {
            threads: next_nonzero_usize(&mut args, "threads")?,
            flows_per_thread: next_nonzero_usize(&mut args, "flows per thread")?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "multi_tcp_sink" => TrafficShape::MultiTcpSink {
            threads: next_nonzero_usize(&mut args, "threads")?,
            flows_per_thread: next_nonzero_usize(&mut args, "flows per thread")?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "churn" => TrafficShape::Churn {
            rate: next_nonzero_usize(&mut args, "rate")?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "rst_unread_rx" => TrafficShape::RstUnreadRx {
            flows: next_nonzero_usize(&mut args, "flows")?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "pool_pressure" => TrafficShape::PoolPressure {
            flows: next_nonzero_usize(&mut args, "flows")?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "mixed_tcp_udp" => TrafficShape::MixedTcpUdp {
            tcp_flows: next_nonzero_usize(&mut args, "TCP flows")?,
            udp_flows: next_nonzero_usize(&mut args, "UDP flows")?,
        },
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "idle_hot" => {
            let idle = next_usize(&mut args, "idle flows")?;
            let active = next_usize(&mut args, "active flows")?;
            if idle == 0 && active == 0 {
                return Err("idle_hot requires at least one idle or active flow".to_owned());
            }
            TrafficShape::IdleHot { idle, active }
        }
        #[cfg(not(feature = "socket-tcp-dynamic-buffer"))]
        "multi_tcp" | "multi_tcp_sink" | "churn" | "rst_unread_rx" | "pool_pressure"
        | "mixed_tcp_udp" | "idle_hot" => {
            return Err(format!(
                "traffic shape '{shape_name}' requires feature 'socket-tcp-dynamic-buffer'"
            ));
        }
        other => return Err(format!("unknown traffic shape '{other}'")),
    };

    let offload_checksums = parse_offload(&mut args)?;
    Ok(Config {
        mode,
        seconds,
        shape,
        offload_checksums,
    })
}

fn next_value(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    let value = args.next().ok_or_else(|| format!("missing {name}"))?;
    if is_mode_option(&value) {
        return Err(MODE_POSITION_ERROR.to_owned());
    }
    if value.starts_with('-') {
        return Err(format!("unknown option '{value}'"));
    }
    Ok(value)
}

fn next_nonzero_u64(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<NonZeroU64, String> {
    let value = next_value(args, name)?;
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("invalid {name} '{value}': expected a non-zero integer"))?;
    NonZeroU64::new(parsed).ok_or_else(|| format!("{name} must be non-zero"))
}

fn next_nonzero_usize(
    args: &mut impl Iterator<Item = String>,
    name: &str,
) -> Result<NonZeroUsize, String> {
    let value = next_value(args, name)?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid {name} '{value}': expected a non-zero integer"))?;
    NonZeroUsize::new(parsed).ok_or_else(|| format!("{name} must be non-zero"))
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn next_usize(args: &mut impl Iterator<Item = String>, name: &str) -> Result<usize, String> {
    let value = next_value(args, name)?;
    value
        .parse()
        .map_err(|_| format!("invalid {name} '{value}': expected a non-negative integer"))
}

fn parse_offload(args: &mut impl Iterator<Item = String>) -> Result<bool, String> {
    let Some(value) = args.next() else {
        return Ok(false);
    };
    if is_mode_option(&value) {
        return Err(MODE_POSITION_ERROR.to_owned());
    }
    if value.starts_with('-') {
        return Err(format!("unknown option '{value}'"));
    }
    if value != "offload" {
        return Err(format!("invalid offload value '{value}': expected offload"));
    }
    if let Some(trailing) = args.next() {
        if is_mode_option(&trailing) {
            return Err(MODE_POSITION_ERROR.to_owned());
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

const MODE_POSITION_ERROR: &str =
    "--mode must appear before the traffic shape and may be specified only once";

const SERVER_PORT_BASE: u16 = 10_000;
const CLIENT_PORT_BASE: u16 = 30_000;
const MAX_UNIQUE_FLOWS: usize = (u16::MAX - CLIENT_PORT_BASE) as usize + 1;
#[cfg(feature = "socket-tcp-dynamic-buffer")]
const MAX_UNIQUE_WORKERS: usize = u16::MAX as usize + 1;

fn checked_run_duration(shape: &str, seconds: u64) -> Result<Duration, String> {
    let duration = Duration::from_secs(seconds);
    StdInstant::now()
        .checked_add(duration)
        .ok_or_else(|| format!("{shape}: duration exceeds the platform timer range"))?;
    Ok(duration)
}

fn checked_deadline(
    shape: &str,
    start: StdInstant,
    duration: Duration,
) -> Result<StdInstant, String> {
    start
        .checked_add(duration)
        .ok_or_else(|| format!("{shape}: deadline exceeds the platform timer range"))
}

fn validate_unique_flow_count(shape: &str, flows: usize) -> Result<(), String> {
    if flows > MAX_UNIQUE_FLOWS {
        return Err(format!(
            "{shape}: {flows} flows exceed the {MAX_UNIQUE_FLOWS} unique non-zero port pairs"
        ));
    }
    Ok(())
}

fn flow_ports(shape: &str, index: usize) -> Result<(u16, u16), String> {
    let offset = u16::try_from(index)
        .map_err(|_| format!("{shape}: flow index {index} exceeds the port space"))?;
    let server = SERVER_PORT_BASE
        .checked_add(offset)
        .ok_or_else(|| format!("{shape}: flow index {index} exhausted server ports"))?;
    let client = CLIENT_PORT_BASE
        .checked_add(offset)
        .ok_or_else(|| format!("{shape}: flow index {index} exhausted client ports"))?;
    Ok((server, client))
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn validate_unique_worker_count(shape: &str, workers: usize) -> Result<(), String> {
    if workers > MAX_UNIQUE_WORKERS {
        return Err(format!(
            "{shape}: {workers} workers exceed the {MAX_UNIQUE_WORKERS} unique address prefixes"
        ));
    }
    Ok(())
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn worker_subnet(shape: &str, worker: usize) -> Result<[u8; 3], String> {
    let subnet = u16::try_from(worker)
        .map_err(|_| format!("{shape}: worker index {worker} exceeds the address space"))?;
    Ok([10, (subnet >> 8) as u8, subnet as u8])
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn churn_interval_us(rate: usize) -> Result<u64, String> {
    const TIMER_TICKS_PER_SECOND: u64 = 1_000_000;
    let rate = u64::try_from(rate)
        .map_err(|_| format!("churn: rate {rate} exceeds the timer counter range"))?;
    let interval = TIMER_TICKS_PER_SECOND
        .checked_div(rate)
        .and_then(NonZeroU64::new)
        .ok_or_else(|| {
            format!("churn: rate {rate} exceeds the {TIMER_TICKS_PER_SECOND} Hz timer resolution")
        })?;
    Ok(interval.get())
}

/// Tracks successful requested allocation bytes. Byte counters use relaxed
/// atomics; dynamic phase ownership uses acquire/release, with begin and finish
/// externally bracketed while workload workers are quiescent.
struct CountingAlloc;
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static ALLOC_COUNT: AtomicU64 = AtomicU64::new(0);
static FREE_BYTES: AtomicU64 = AtomicU64::new(0);

#[cfg(not(feature = "dhat-heap"))]
static ALLOCATOR_BACKEND: System = System;
#[cfg(feature = "dhat-heap")]
static ALLOCATOR_BACKEND: dhat::Alloc = dhat::Alloc;

#[cfg(feature = "socket-tcp-dynamic-buffer")]
struct AllocatorTelemetry {
    live: AtomicU64,
    phase_peak: AtomicU64,
    phase_active: AtomicBool,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl AllocatorTelemetry {
    const fn new() -> Self {
        Self {
            live: AtomicU64::new(0),
            phase_peak: AtomicU64::new(0),
            phase_active: AtomicBool::new(false),
        }
    }

    fn record_alloc(&self, bytes: u64) {
        let live = self
            .live
            .fetch_add(bytes, Ordering::Relaxed)
            .wrapping_add(bytes);
        if self.phase_active.load(Ordering::Acquire) {
            self.phase_peak.fetch_max(live, Ordering::Relaxed);
        }
    }

    fn record_dealloc(&self, bytes: u64) {
        self.live.fetch_sub(bytes, Ordering::Relaxed);
    }

    fn begin(&self) -> Result<AllocatorPhase<'_>, &'static str> {
        self.phase_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "allocator phase is already active")?;
        let live_start = self.live.load(Ordering::Relaxed);
        self.phase_peak.store(live_start, Ordering::Relaxed);
        Ok(AllocatorPhase {
            telemetry: self,
            live_start,
            active: true,
        })
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
static ALLOCATOR_TELEMETRY: AllocatorTelemetry = AllocatorTelemetry::new();

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[derive(Debug, Eq, PartialEq)]
struct AllocatorPeak {
    live_start: u64,
    live_end: u64,
    live_peak: u64,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
struct AllocatorPhase<'a> {
    telemetry: &'a AllocatorTelemetry,
    live_start: u64,
    active: bool,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl AllocatorPhase<'_> {
    fn finish(mut self) -> AllocatorPeak {
        self.telemetry.phase_active.store(false, Ordering::Release);
        self.active = false;
        AllocatorPeak {
            live_start: self.live_start,
            live_end: self.telemetry.live.load(Ordering::Relaxed),
            live_peak: self.telemetry.phase_peak.load(Ordering::Relaxed),
        }
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl Drop for AllocatorPhase<'_> {
    fn drop(&mut self) {
        if self.active {
            self.telemetry.phase_active.store(false, Ordering::Release);
        }
    }
}

unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { ALLOCATOR_BACKEND.alloc(layout) };
        if !ptr.is_null() {
            let bytes = layout.size() as u64;
            ALLOC_BYTES.fetch_add(bytes, Ordering::Relaxed);
            ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "socket-tcp-dynamic-buffer")]
            ALLOCATOR_TELEMETRY.record_alloc(bytes);
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let bytes = layout.size() as u64;
        FREE_BYTES.fetch_add(bytes, Ordering::Relaxed);
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        ALLOCATOR_TELEMETRY.record_dealloc(bytes);
        unsafe { ALLOCATOR_BACKEND.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

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
            for (j, &count) in row.iter().enumerate() {
                cum += count;
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
    jain: f64,
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
        // Jain's fairness index.
        let sum_sq: f64 = per_flow.iter().map(|&x| (x as f64).powi(2)).sum();
        let jain = if sum_sq > 0.0 {
            let s = total as f64;
            (s * s) / (n as f64 * sum_sq)
        } else {
            0.0
        };
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
            jain,
            starved,
            zero_flows,
        }
    }

    fn print(&self, label: &str) {
        println!();
        println!("  per-flow {label} (bytes):");
        println!(
            "    flows: {:>5}     total: {:>14}     mean: {:>12.1}",
            self.n, self.total, self.mean
        );
        println!(
            "    min:   {:>14} (flow #{:<5})  max: {:>14} (flow #{})",
            self.min, self.min_flow, self.max, self.max_flow
        );
        println!("    Jain:  {:>14.4}", self.jain);
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

fn validate_tcp_transfer(
    shape: &str,
    client_established: bool,
    server_established: bool,
    sent: u64,
    received: u64,
) -> Result<(), String> {
    if !client_established {
        return Err(format!("{shape}: client TCP side did not establish"));
    }
    if !server_established {
        return Err(format!("{shape}: server TCP side did not establish"));
    }
    if sent == 0 {
        return Err(format!("{shape}: sent no application bytes"));
    }
    if received == 0 {
        return Err(format!("{shape}: received no application bytes"));
    }
    Ok(())
}

fn validate_pingpong(
    client_established: bool,
    server_established: bool,
    roundtrips: u64,
) -> Result<(), String> {
    if !client_established {
        return Err("pingpong: client TCP side did not establish".to_owned());
    }
    if !server_established {
        return Err("pingpong: server TCP side did not establish".to_owned());
    }
    if roundtrips == 0 {
        return Err("pingpong: completed no roundtrips".to_owned());
    }
    Ok(())
}

fn validate_udp_bindings(
    shape: &str,
    server_bound: bool,
    client_bound: bool,
) -> Result<(), String> {
    if !server_bound {
        return Err(format!("{shape}: server UDP socket did not bind"));
    }
    if !client_bound {
        return Err(format!("{shape}: client UDP socket did not bind"));
    }
    Ok(())
}

fn validate_udp_transfer(
    shape: &str,
    server_bound: bool,
    client_bound: bool,
    sent: u64,
    received: u64,
) -> Result<(), String> {
    validate_udp_bindings(shape, server_bound, client_bound)?;
    if sent == 0 {
        return Err(format!("{shape}: sent no application bytes"));
    }
    if received == 0 {
        return Err(format!("{shape}: received no application bytes"));
    }
    Ok(())
}

fn validate_flow_stats(shape: &str, stats: &Fairness) -> Result<(), String> {
    if stats.total == 0 {
        return Err(format!("{shape}: received no application bytes"));
    }
    if stats.zero_flows != 0 {
        return Err(format!(
            "{shape}: {} flow(s) received zero bytes",
            stats.zero_flows
        ));
    }
    if stats.starved != 0 {
        return Err(format!(
            "{shape}: {} flow(s) received less than 10% of the mean",
            stats.starved
        ));
    }
    Ok(())
}

fn validate_established_flows(
    shape: &str,
    established: usize,
    expected: usize,
    received: &Fairness,
) -> Result<(), String> {
    if established != expected {
        return Err(format!(
            "{shape}: only {established}/{expected} flows established"
        ));
    }
    validate_flow_stats(shape, received)
}

fn validate_fairness(shape: &str, fairness: &Fairness) -> Result<(), String> {
    if fairness.jain < 0.95 {
        return Err(format!(
            "{shape}: Jain fairness {:.4} is below 0.9500",
            fairness.jain
        ));
    }
    Ok(())
}

/// Periodic process-memory trace for leak diagnosis. Landing RSS gates compare
/// fresh-process matched samples instead of this short-run heuristic.
struct MemTrace {
    samples: Vec<(u64, u64, u64)>, // (ms_since_start, memory_bytes, alloc_bytes_delta)
    start_wall: StdInstant,
    start_alloc: u64,
}

impl MemTrace {
    fn start(mode: RunMode) -> Self {
        Self {
            samples: if mode.sample_memory() {
                Vec::with_capacity(64)
            } else {
                Vec::new()
            },
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
            let memory = process_memory_bytes();
            let alloc_now = ALLOC_BYTES.load(Ordering::Relaxed);
            let sample = (elapsed, memory, alloc_now - self.start_alloc);
            if self.samples.len() < self.samples.capacity() {
                self.samples.push(sample);
            } else if let Some(last) = self.samples.last_mut() {
                *last = sample;
            }
        }
    }
    fn print(&self) {
        if self.samples.is_empty() {
            return;
        }
        println!();
        println!("  memory trace (snapshot every ~250 ms):");
        println!(
            "    {:>8}   {:>22}   {:>10}",
            "t_ms",
            process_memory_label(),
            "alloc_delta"
        );
        for (t, memory, alloc) in &self.samples {
            println!("    {t:>8}   {memory:>22}   {alloc:>10}");
        }
        // Flag a large late-run rise for follow-up profiling.
        let mut memory_sorted: Vec<u64> = self.samples.iter().map(|s| s.1).collect();
        memory_sorted.sort_unstable();
        let median = memory_sorted[memory_sorted.len() / 2];
        let last = self.samples.last().unwrap().1;
        let verdict = if last as f64 > 1.5 * median as f64 {
            "GROWTH (possible leak)"
        } else {
            "bounded"
        };
        let metric = process_memory_label();
        println!("    {metric} verdict: {verdict} (last={last}, median={median})");
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

use smoltcp::iface::{Config as InterfaceConfig, Interface, SocketHandle, SocketSet};
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
#[path = "profile_loopback/tests.rs"]
mod tests;

#[derive(Clone, Copy)]
enum LinkEndpoint {
    A,
    B,
}

#[derive(Default)]
struct DeviceStats {
    tx_bytes: u64,
    tx_packets: u64,
}

/// Two unidirectional lanes forming a back-to-back link.
struct PairedLink {
    // Keep queue state out of the large shape-runner stack frames.
    a_to_b: Box<Lane>,
    b_to_a: Box<Lane>,
    mtu: usize,
    offload_checksums: bool,
}

impl PairedLink {
    fn new(mtu: usize, depth: usize, offload_checksums: bool) -> Self {
        Self {
            a_to_b: Box::new(Lane::new(mtu, depth)),
            b_to_a: Box::new(Lane::new(mtu, depth)),
            mtu,
            offload_checksums,
        }
    }

    fn device<'a>(
        &'a mut self,
        endpoint: LinkEndpoint,
        stats: &'a mut DeviceStats,
    ) -> PairedDevice<'a> {
        let (tx, rx) = match endpoint {
            LinkEndpoint::A => (&mut self.a_to_b, &mut self.b_to_a),
            LinkEndpoint::B => (&mut self.b_to_a, &mut self.a_to_b),
        };
        PairedDevice {
            tx,
            rx,
            mtu: self.mtu,
            offload_checksums: self.offload_checksums,
            stats,
        }
    }

    fn stats(&self) -> LaneStats {
        let mut stats = self.a_to_b.stats();
        stats.merge(self.b_to_a.stats());
        stats
    }
}

/// A short-lived `Device` view of one endpoint on a [`PairedLink`].
struct PairedDevice<'a> {
    tx: &'a mut Lane,
    rx: &'a mut Lane,
    mtu: usize,
    /// If true, the device advertises checksum offload so smoltcp skips
    /// IPv4/UDP/TCP checksum emit+verify, mimicking a hardware NIC.
    offload_checksums: bool,
    stats: &'a mut DeviceStats,
}

impl Device for PairedDevice<'_> {
    type RxToken<'a>
        = PairedRx<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = PairedTx<'a>
    where
        Self: 'a;

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
        if self.rx.queue.is_empty() {
            return None;
        }

        if self.tx.pool.is_empty() {
            self.tx.stats.rx_backpressure += 1;
            return None;
        }
        let rx_packet = self
            .rx
            .queue
            .pop_front()
            .expect("RX queue changed after paired TX availability check");
        Some((
            PairedRx {
                pkt: Some(rx_packet),
                rx: self.rx,
            },
            PairedTx {
                tx: self.tx,
                mtu: self.mtu,
                stats: self.stats,
            },
        ))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        if self.tx.pool.len() <= 1 {
            self.tx.stats.tx_backpressure += 1;
            return None;
        }
        Some(PairedTx {
            tx: self.tx,
            mtu: self.mtu,
            stats: self.stats,
        })
    }
}

struct PairedRx<'a> {
    pkt: Option<Packet>,
    rx: &'a mut Lane,
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
        self.rx.return_pkt(self.pkt.take().unwrap());
        result
    }
}

impl Drop for PairedRx<'_> {
    fn drop(&mut self) {
        if let Some(packet) = self.pkt.take() {
            refund_packet(self.rx, packet);
        }
    }
}

struct PairedTx<'a> {
    tx: &'a mut Lane,
    mtu: usize,
    stats: &'a mut DeviceStats,
}

struct CheckedOutPacket<'a> {
    tx: &'a mut Lane,
    packet: Option<Packet>,
}

impl CheckedOutPacket<'_> {
    fn payload_mut(&mut self, len: usize) -> &mut [u8] {
        &mut self.packet.as_mut().unwrap().buf[..len]
    }

    fn commit(mut self, len: usize, stats: &mut DeviceStats) {
        let mut packet = self.packet.take().unwrap();
        packet.len = len;
        stats.tx_bytes += len as u64;
        stats.tx_packets += 1;
        self.tx.queue_pkt(packet);
    }
}

#[cold]
fn refund_packet(lane: &mut Lane, packet: Packet) {
    lane.return_pkt(packet);
}

impl Drop for CheckedOutPacket<'_> {
    fn drop(&mut self) {
        if let Some(packet) = self.packet.take() {
            refund_packet(self.tx, packet);
        }
    }
}

impl<'a> phy::TxToken for PairedTx<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(
            len <= self.mtu,
            "transmit length {len} exceeds MTU {}",
            self.mtu
        );
        let packet = self
            .tx
            .try_take_packet()
            .expect("TX credit disappeared after token construction");
        let mut packet = CheckedOutPacket {
            tx: self.tx,
            packet: Some(packet),
        };
        let result = f(packet.payload_mut(len));
        packet.commit(len, self.stats);
        result
    }
}

struct Endpoint<'a> {
    iface: Interface,
    sockets: SocketSet<'a>,
    link_endpoint: LinkEndpoint,
    device_stats: DeviceStats,
}

struct PairedEndpoints {
    server: Endpoint<'static>,
    client: Endpoint<'static>,
    link: PairedLink,
}

impl Endpoint<'_> {
    fn poll(&mut self, now: Instant, link: &mut PairedLink) -> smoltcp::iface::PollResult {
        let mut device = link.device(self.link_endpoint, &mut self.device_stats);
        self.iface.poll(now, &mut device, &mut self.sockets)
    }
}

fn make_endpoint(
    addr: IpAddress,
    link: &mut PairedLink,
    link_endpoint: LinkEndpoint,
) -> Endpoint<'static> {
    let mut device_stats = DeviceStats::default();
    let mut device = link.device(link_endpoint, &mut device_stats);
    let mut config = InterfaceConfig::new(HardwareAddress::Ip);
    config.random_seed = 0xdead_beef;
    let mut iface = Interface::new(config, &mut device, Instant::from_millis(0));
    iface.update_ip_addrs(|ips| {
        ips.push(IpCidr::new(addr, 24)).unwrap();
    });
    Endpoint {
        iface,
        sockets: SocketSet::new(vec![]),
        link_endpoint,
        device_stats,
    }
}

/// Build a back-to-back server/client `Endpoint` pair joined by two
/// `Lane`s, with the server at `subnet.1` and the client at `subnet.2`. The
/// returned link lets callers poll either endpoint and report packet-pool
/// backpressure and fixed reservation size.
#[cfg_attr(not(feature = "socket-tcp-dynamic-buffer"), allow(dead_code))]
fn setup_paired_endpoints(
    subnet: [u8; 3],
    mtu: usize,
    queue_depth: usize,
    offload: bool,
) -> PairedEndpoints {
    let mut link = PairedLink::new(mtu, queue_depth, offload);
    let server = make_endpoint(
        IpAddress::v4(subnet[0], subnet[1], subnet[2], 1),
        &mut link,
        LinkEndpoint::A,
    );
    let client = make_endpoint(
        IpAddress::v4(subnet[0], subnet[1], subnet[2], 2),
        &mut link,
        LinkEndpoint::B,
    );
    PairedEndpoints {
        server,
        client,
        link,
    }
}

fn add_tcp_socket(ep: &mut Endpoint<'static>, buf_size: usize) -> smoltcp::iface::SocketHandle {
    let rx = tcp::SocketBuffer::new(vec![0u8; buf_size]);
    let tx = tcp::SocketBuffer::new(vec![0u8; buf_size]);
    let socket = tcp::Socket::new(rx, tx);
    ep.sockets.add(socket)
}

fn establish_tcp_flows(
    server: &mut Endpoint<'_>,
    client: &mut Endpoint<'_>,
    link: &mut PairedLink,
    server_handles: &[SocketHandle],
    client_handles: &[SocketHandle],
    wall_origin: StdInstant,
    deadline: StdInstant,
) -> usize {
    loop {
        let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
        server.poll(now, link);
        client.poll(now, link);
        let established = client_handles
            .iter()
            .zip(server_handles)
            .filter(|&(&client_handle, &server_handle)| {
                matches!(
                    client.sockets.get::<tcp::Socket>(client_handle).state(),
                    tcp::State::Established
                ) && matches!(
                    server.sockets.get::<tcp::Socket>(server_handle).state(),
                    tcp::State::Established
                )
            })
            .count();
        if established == client_handles.len() || StdInstant::now() >= deadline {
            return established;
        }
    }
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

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn add_dynamic_tcp_flows(
    shape: &str,
    server: &mut Endpoint<'static>,
    client: &mut Endpoint<'static>,
    flows: usize,
    max_buf: u32,
    pool: &tcp::MemoryPool,
) -> Result<(Vec<SocketHandle>, Vec<SocketHandle>), String> {
    validate_unique_flow_count(shape, flows)?;
    let mut server_handles = Vec::with_capacity(flows);
    let mut client_handles = Vec::with_capacity(flows);
    for flow in 0..flows {
        let server_handle = add_tcp_socket_dyn(server, max_buf, pool);
        let client_handle = add_tcp_socket_dyn(client, max_buf, pool);
        let (server_port, client_port) = flow_ports(shape, flow)?;

        let socket = server.sockets.get_mut::<tcp::Socket>(server_handle);
        socket.set_ack_delay(None);
        socket.set_nagle_enabled(false);
        socket
            .listen(server_port)
            .map_err(|error| format!("{shape}: flow {flow} listen failed: {error:?}"))?;

        let socket = client.sockets.get_mut::<tcp::Socket>(client_handle);
        socket.set_ack_delay(None);
        socket.set_nagle_enabled(false);
        socket
            .connect(
                client.iface.context(),
                (IpAddress::v4(10, 0, 0, 1), server_port),
                client_port,
            )
            .map_err(|error| format!("{shape}: flow {flow} connect failed: {error:?}"))?;

        server_handles.push(server_handle);
        client_handles.push(client_handle);
    }
    Ok((server_handles, client_handles))
}

/// Snapshot of allocator counters and process memory at one instant.
#[derive(Copy, Clone)]
struct AllocSnap {
    alloc_bytes: u64,
    alloc_count: u64,
    /// Live bytes = alloc_bytes - free_bytes, used to show net heap growth.
    free_bytes: u64,
    process_memory: ProcessMemorySample,
    /// Voluntary context switches — process blocked or yielded.
    /// Hot-loop shapes should see this stay tiny.
    ctxsw_voluntary: u64,
    /// Involuntary context switches — preempted by the scheduler.
    /// Proportional to wall_time / scheduling_quantum × runnable_threads.
    ctxsw_nonvoluntary: u64,
    /// Calling-thread CPU time, nanoseconds.
    cpu_ns: u64,
}

/// `CLOCK_THREAD_CPUTIME_ID` in nanoseconds. Returns zero when unsupported.
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

impl AllocSnap {
    fn now() -> Self {
        let (cv, cn) = ctxsw_counts();
        Self {
            alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
            alloc_count: ALLOC_COUNT.load(Ordering::Relaxed),
            free_bytes: FREE_BYTES.load(Ordering::Relaxed),
            process_memory: process_memory_sample(),
            ctxsw_voluntary: cv,
            ctxsw_nonvoluntary: cn,
            cpu_ns: thread_cpu_ns(),
        }
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn alloc_counters_with_memory(process_memory: ProcessMemorySample) -> AllocSnap {
    AllocSnap {
        alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
        alloc_count: ALLOC_COUNT.load(Ordering::Relaxed),
        free_bytes: FREE_BYTES.load(Ordering::Relaxed),
        process_memory,
        ctxsw_voluntary: 0,
        ctxsw_nonvoluntary: 0,
        cpu_ns: 0,
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[derive(Clone, Copy)]
struct PoolUsage {
    start: usize,
    end: usize,
    budget: usize,
    after_teardown: usize,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
struct DynamicMemoryReport {
    process_memory_start: u64,
    process_memory_end: u64,
    process_memory_lifetime_peak: Option<u64>,
    bytes_allocated: u64,
    bytes_freed: u64,
    net_heap_delta: i128,
    allocation_count: u64,
    allocator_live_start: u64,
    allocator_live_end: u64,
    allocator_peak_live: u64,
    allocator_peak_growth: u64,
    allocator_peak_bound: u64,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
impl DynamicMemoryReport {
    fn from_snapshots(
        before: AllocSnap,
        after: AllocSnap,
        allocator: AllocatorPeak,
        pool: PoolUsage,
    ) -> Result<Self, String> {
        let allocator_peak_bound = pool
            .budget
            .checked_mul(2)
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or_else(|| "dynamic pool transient bound overflowed".to_owned())?;
        let allocator_peak_growth = allocator
            .live_peak
            .checked_sub(allocator.live_start)
            .ok_or_else(|| "allocator peak was below phase start".to_owned())?;
        if allocator.live_peak < allocator.live_end {
            return Err("allocator peak was below phase end".to_owned());
        }
        if allocator_peak_growth > allocator_peak_bound {
            return Err(format!(
                "allocator peak growth {allocator_peak_growth} exceeded transient bound {allocator_peak_bound}"
            ));
        }
        if pool.start > pool.budget {
            return Err(format!(
                "starting pool use {} exceeded budget {}",
                pool.start, pool.budget
            ));
        }
        if pool.end > pool.budget {
            return Err(format!(
                "active pool use {} exceeded budget {}",
                pool.end, pool.budget
            ));
        }
        if pool.after_teardown != 0 {
            return Err(format!(
                "pool use after teardown was {}, expected 0",
                pool.after_teardown
            ));
        }
        let bytes_allocated = after.alloc_bytes.saturating_sub(before.alloc_bytes);
        let bytes_freed = after.free_bytes.saturating_sub(before.free_bytes);
        Ok(Self {
            process_memory_start: before.process_memory.current_bytes,
            process_memory_end: after.process_memory.current_bytes,
            process_memory_lifetime_peak: after.process_memory.lifetime_peak_bytes,
            bytes_allocated,
            bytes_freed,
            net_heap_delta: signed_delta(bytes_allocated, bytes_freed),
            allocation_count: after.alloc_count.saturating_sub(before.alloc_count),
            allocator_live_start: allocator.live_start,
            allocator_live_end: allocator.live_end,
            allocator_peak_live: allocator.live_peak,
            allocator_peak_growth,
            allocator_peak_bound,
        })
    }

    fn print(&self) {
        println!("  phase allocations:");
        println!("    bytes allocated:       {}", self.bytes_allocated);
        println!("    bytes freed:           {}", self.bytes_freed);
        println!("    net heap delta:        {}", self.net_heap_delta);
        println!("    allocation count:      {}", self.allocation_count);
        println!("    requested live start:  {}", self.allocator_live_start);
        println!("    requested live end:    {}", self.allocator_live_end);
        println!("    requested live peak:   {}", self.allocator_peak_live);
        println!("    requested peak growth: {}", self.allocator_peak_growth);
        println!("    requested peak bound:  {}", self.allocator_peak_bound);
        println!();
        println!("  process memory:");
        let metric = process_memory_label();
        let delta = signed_delta(self.process_memory_end, self.process_memory_start);
        println!(
            "    {metric} start:         {}  ({:.1} MiB)",
            self.process_memory_start,
            self.process_memory_start as f64 / (1024.0 * 1024.0)
        );
        println!(
            "    {metric} end:           {}  ({:.1} MiB)",
            self.process_memory_end,
            self.process_memory_end as f64 / (1024.0 * 1024.0)
        );
        println!(
            "    {metric} delta:         {delta:+}  ({:+.1} MiB)",
            delta as f64 / (1024.0 * 1024.0)
        );
        match self.process_memory_lifetime_peak {
            Some(peak) => println!(
                "    {metric} lifetime peak: {peak}  ({:.1} MiB)",
                peak as f64 / (1024.0 * 1024.0)
            ),
            None => println!("    {metric} lifetime peak: unavailable"),
        }
    }
}

/// Lay out a uniform metrics block so every shape prints the same shape of
/// data and comparisons across runs are unambiguous.
struct Report<'a> {
    name: &'a str,
    elapsed: f64,
    app_bytes_recvd: u64,
    /// Total wire packets emitted by both peers.
    wire_packets: u64,
    /// Total wire bytes emitted by both peers (incl. headers).
    wire_bytes: u64,
    /// Latency histogram of poll cycles (one pump of both endpoints).
    poll_lat: &'a Histo,
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
        let net_heap = signed_delta(alloc_bytes, free_bytes);
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
            "  per-packet:             {ns_per_pkt:>8.1} ns   (~{:.0} reference cycles @ {} GHz)",
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
            "  wire packets:           {:>8}   (cachegrind I refs / this = instructions/pkt)",
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
        let metric = process_memory_label();
        let memory_delta = signed_delta(
            self.alloc_after.process_memory.current_bytes,
            self.alloc_before.process_memory.current_bytes,
        );
        println!(
            "    {metric} start:        {:>10}  ({:.1} MiB)",
            self.alloc_before.process_memory.current_bytes,
            self.alloc_before.process_memory.current_bytes as f64 / (1024.0 * 1024.0)
        );
        println!(
            "    {metric} end:          {:>10}  ({:.1} MiB)",
            self.alloc_after.process_memory.current_bytes,
            self.alloc_after.process_memory.current_bytes as f64 / (1024.0 * 1024.0)
        );
        println!(
            "    {metric} delta:        {memory_delta:>+10}  ({:+.1} MiB)",
            memory_delta as f64 / (1024.0 * 1024.0)
        );
        match self.alloc_after.process_memory.lifetime_peak_bytes {
            Some(peak) => println!(
                "    {metric} lifetime peak: {peak:>10}  ({:.1} MiB)",
                peak as f64 / (1024.0 * 1024.0)
            ),
            None => println!("    {metric} lifetime peak: unavailable"),
        }

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

        let cpu_ns = self
            .alloc_after
            .cpu_ns
            .saturating_sub(self.alloc_before.cpu_ns);
        if cpu_ns > 0 {
            println!("  CPU:");
            println!(
                "    thread time:          {:>10.3} s   ({:.3}% of wall)",
                cpu_ns as f64 / 1e9,
                (cpu_ns as f64 / 1e9) / self.elapsed * 100.0,
            );
        }
    }
}

// Keep the large workload frame out of the dispatcher.
#[inline(never)]
fn shape_firehose(seconds: u64, offload: bool) -> Result<(), String> {
    const BUF: usize = 256 * 1024;
    let duration = checked_run_duration("firehose", seconds)?;
    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, 256, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

    let srv_h = add_tcp_socket(server, BUF);
    let cli_h = add_tcp_socket(client, BUF);

    // We want to measure smoltcp's per-packet CPU cost, not its delayed-ACK behaviour.
    // Suppressing delayed ACK + Nagle keeps the pipeline saturated.
    {
        let s = server.sockets.get_mut::<tcp::Socket>(srv_h);
        s.set_ack_delay(None);
        s.set_nagle_enabled(false);
        let _ = s.listen(1234);
    }
    {
        let c = client.sockets.get_mut::<tcp::Socket>(cli_h);
        c.set_ack_delay(None);
        c.set_nagle_enabled(false);
    }
    let _ = client.sockets.get_mut::<tcp::Socket>(cli_h).connect(
        client.iface.context(),
        (IpAddress::v4(10, 0, 0, 1), 1234),
        49152,
    );

    // Use wall clock for the virtual time so TCP timers (RTO, delayed ACK) behave realistically.
    let wall_origin = StdInstant::now();
    let now_smol = || Instant::from_micros(wall_origin.elapsed().as_micros() as i64);

    // Pump until ESTABLISHED.
    for _ in 0..1000 {
        let n = now_smol();
        server.poll(n, link);
        client.poll(n, link);
        if client.sockets.get::<tcp::Socket>(cli_h).may_send()
            && server.sockets.get::<tcp::Socket>(srv_h).may_recv()
        {
            break;
        }
    }
    let client_established = matches!(
        client.sockets.get::<tcp::Socket>(cli_h).state(),
        tcp::State::Established
    );
    let server_established = matches!(
        server.sockets.get::<tcp::Socket>(srv_h).state(),
        tcp::State::Established
    );

    let payload = vec![0x42u8; 64 * 1024];
    let deadline = checked_deadline("firehose", StdInstant::now(), duration)?;
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
            cli_state = client.poll(n, link);
            srv_state = server.poll(n, link);
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
        app_bytes_recvd: recvd,
        wire_packets: client.device_stats.tx_packets + server.device_stats.tx_packets,
        wire_bytes: client.device_stats.tx_bytes + server.device_stats.tx_bytes,
        poll_lat: &poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: idle_spins,
        unit_label: "idle-spins",
    }
    .print();
    print_lane_stats("firehose", link.stats());
    validate_tcp_transfer(
        "firehose",
        client_established,
        server_established,
        sent,
        recvd,
    )
}

fn shape_small(seconds: u64, offload: bool) -> Result<(), String> {
    // Force tiny segments by limiting the socket buffer; with a 1500 MTU the
    // client never fills more than a single small write at a time.
    const BUF: usize = 4 * 1024;
    let duration = checked_run_duration("small", seconds)?;
    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, 256, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

    let srv_h = add_tcp_socket(server, BUF);
    let cli_h = add_tcp_socket(client, BUF);

    client
        .sockets
        .get_mut::<tcp::Socket>(cli_h)
        .set_nagle_enabled(false);

    let _ = server.sockets.get_mut::<tcp::Socket>(srv_h).listen(1234);
    let _ = client.sockets.get_mut::<tcp::Socket>(cli_h).connect(
        client.iface.context(),
        (IpAddress::v4(10, 0, 0, 1), 1234),
        49152,
    );

    let mut t_ms: i64 = 0;
    for _ in 0..200 {
        let n = Instant::from_millis(t_ms);
        server.poll(n, link);
        client.poll(n, link);
        if matches!(
            client.sockets.get::<tcp::Socket>(cli_h).state(),
            tcp::State::Established
        ) && matches!(
            server.sockets.get::<tcp::Socket>(srv_h).state(),
            tcp::State::Established
        ) {
            break;
        }
        t_ms += 1;
    }
    let client_established = matches!(
        client.sockets.get::<tcp::Socket>(cli_h).state(),
        tcp::State::Established
    );
    let server_established = matches!(
        server.sockets.get::<tcp::Socket>(srv_h).state(),
        tcp::State::Established
    );

    let payload = [0x42u8; 64];
    let deadline = checked_deadline("small", StdInstant::now(), duration)?;
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
            client.poll(n, link);
            server.poll(n, link);
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
        app_bytes_recvd: recvd,
        wire_packets: client.device_stats.tx_packets + server.device_stats.tx_packets,
        wire_bytes: client.device_stats.tx_bytes + server.device_stats.tx_bytes,
        poll_lat: &poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: recvd,
        unit_label: "bytes",
    }
    .print();
    print_lane_stats("small", link.stats());
    validate_tcp_transfer("small", client_established, server_established, sent, recvd)
}

fn shape_pingpong(seconds: u64, offload: bool) -> Result<(), String> {
    const BUF: usize = 16 * 1024;
    let duration = checked_run_duration("pingpong", seconds)?;
    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, 256, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

    let srv_h = add_tcp_socket(server, BUF);
    let cli_h = add_tcp_socket(client, BUF);

    let _ = server.sockets.get_mut::<tcp::Socket>(srv_h).listen(1234);
    let _ = client.sockets.get_mut::<tcp::Socket>(cli_h).connect(
        client.iface.context(),
        (IpAddress::v4(10, 0, 0, 1), 1234),
        49152,
    );

    let mut t_ms: i64 = 0;
    for _ in 0..200 {
        let n = Instant::from_millis(t_ms);
        server.poll(n, link);
        client.poll(n, link);
        if matches!(
            client.sockets.get::<tcp::Socket>(cli_h).state(),
            tcp::State::Established
        ) && matches!(
            server.sockets.get::<tcp::Socket>(srv_h).state(),
            tcp::State::Established
        ) {
            break;
        }
        t_ms += 1;
    }
    let client_established = matches!(
        client.sockets.get::<tcp::Socket>(cli_h).state(),
        tcp::State::Established
    );
    let server_established = matches!(
        server.sockets.get::<tcp::Socket>(srv_h).state(),
        tcp::State::Established
    );

    let msg = [0x55u8; 128];
    let deadline = checked_deadline("pingpong", StdInstant::now(), duration)?;
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
            client.poll(n, link);
            server.poll(n, link);
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
            server.poll(n, link);
            client.poll(n, link);
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
        app_bytes_recvd: roundtrips * msg.len() as u64,
        wire_packets: client.device_stats.tx_packets + server.device_stats.tx_packets,
        wire_bytes: client.device_stats.tx_bytes + server.device_stats.tx_bytes,
        poll_lat: &poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: roundtrips,
        unit_label: "roundtrips",
    }
    .print();
    print_lane_stats("pingpong", link.stats());
    validate_pingpong(client_established, server_established, roundtrips)
}

fn shape_udp_firehose(seconds: u64, offload: bool) -> Result<(), String> {
    // Pure packet forwarding — no flow control, no cwnd. This is the closest
    // analogue to a packet tunnel forwarding fully-formed packets between peers.
    const PAYLOAD: usize = 1400;
    const META_SLOTS: usize = 256;
    let duration = checked_run_duration("udp", seconds)?;
    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, 256, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

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

    let server_bound = server
        .sockets
        .get_mut::<udp::Socket>(srv_h)
        .bind(2000)
        .is_ok();
    let client_bound = client
        .sockets
        .get_mut::<udp::Socket>(cli_h)
        .bind(2001)
        .is_ok();

    let dest_meta: udp::UdpMetadata = (IpAddress::v4(10, 0, 0, 1), 2000).into();
    let payload = vec![0xa5u8; PAYLOAD];

    // Advance the smoltcp virtual clock by 1 µs each loop iteration. This is
    // not wall-accurate but it is monotonic and avoids a vDSO `clock_gettime`
    // per iteration (which showed up at ~10% of profile when sampled per-poll).
    let mut t_us: i64 = 0;
    let mut iters: u64 = 0;
    let deadline = checked_deadline("udp", StdInstant::now(), duration)?;
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
            client.poll(n, link);
            server.poll(n, link);
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
        app_bytes_recvd: recvd,
        wire_packets: client.device_stats.tx_packets + server.device_stats.tx_packets,
        wire_bytes: client.device_stats.tx_bytes + server.device_stats.tx_bytes,
        poll_lat: &poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: (recvd / PAYLOAD as u64),
        unit_label: "pkts-recvd",
    }
    .print();
    print_lane_stats("udp", link.stats());
    validate_udp_transfer("udp", server_bound, client_bound, sent, recvd)
}

fn packet_queue_depth(shape: &str, flows: usize) -> Result<usize, String> {
    flows
        .checked_mul(16)
        .map(|depth| depth.clamp(1024, 16384))
        .ok_or_else(|| format!("{shape}: packet queue size overflowed"))
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn idle_hot_queue_depth(active_flows: usize) -> Result<usize, String> {
    active_flows
        .checked_mul(16)
        .map(|depth| {
            if active_flows == 0 {
                2
            } else {
                depth.clamp(64, 16384)
            }
        })
        .ok_or_else(|| "idle_hot: packet queue size overflowed".to_owned())
}

struct TcpResources {
    queue_depth: usize,
    socket_bytes: usize,
    per_flow_bytes: usize,
    total_bytes: usize,
}

fn checked_many_tcp_resources(
    shape: &str,
    flows: usize,
    buffer_bytes: usize,
) -> Result<TcpResources, String> {
    let queue_depth = packet_queue_depth(shape, flows)?;
    let socket_bytes = core::mem::size_of::<tcp::Socket>();
    let per_flow_bytes = socket_bytes
        .checked_add(2 * buffer_bytes)
        .ok_or_else(|| format!("{shape}: per-flow socket footprint overflowed"))?;
    let total_bytes = flows
        .checked_mul(2)
        .and_then(|sockets| sockets.checked_mul(per_flow_bytes))
        .ok_or_else(|| format!("{shape}: total socket footprint overflowed"))?;
    Ok(TcpResources {
        queue_depth,
        socket_bytes,
        per_flow_bytes,
        total_bytes,
    })
}

/// `n` concurrent TCP echo flows between two smoltcp endpoints. Each flow has
/// its own (src_port, dst_port) tuple so the stack treats them independently.
///
/// Verifies two properties:
///   * memory stays bounded (process-memory trace + net heap delta)
///   * no flow is starved (Jain index + per-flow bounds)
#[inline(never)] // Keep the large workload frame out of the dispatcher.
fn shape_many_tcp(seconds: u64, n: usize, offload: bool, mode: RunMode) -> Result<(), String> {
    // Per-flow buffer sized small enough to keep total memory reasonable
    // even at N=1000: 1000 flows × 2 (rx+tx) × 4 KiB × 2 (server+client) ≈ 16 MiB.
    const BUF: usize = 4 * 1024;
    // Lane queue depth scales with N. The minimum has to be large enough
    // that a full round of egress packets never spills, otherwise
    // socket_egress short-circuits mid-walk and the late sockets in the
    // iteration order get systematically starved.
    let duration = checked_run_duration("many_tcp", seconds)?;
    validate_unique_flow_count("many_tcp", n)?;
    let TcpResources {
        queue_depth,
        socket_bytes,
        per_flow_bytes,
        total_bytes,
    } = checked_many_tcp_resources("many_tcp", n, BUF)?;

    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, queue_depth, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

    let mut srv_handles = Vec::with_capacity(n);
    let mut cli_handles = Vec::with_capacity(n);

    for i in 0..n {
        let h_srv = add_tcp_socket(server, BUF);
        let h_cli = add_tcp_socket(client, BUF);

        let (dst_port, src_port) = flow_ports("many_tcp", i)?;

        {
            let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
            s.set_ack_delay(None);
            s.set_nagle_enabled(false);
            let _ = s.listen(dst_port);
        }
        {
            let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
            c.set_ack_delay(None);
            c.set_nagle_enabled(false);
        }
        let _ = client.sockets.get_mut::<tcp::Socket>(h_cli).connect(
            client.iface.context(),
            (IpAddress::v4(10, 0, 0, 1), dst_port),
            src_port,
        );

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
    let connect_deadline = checked_deadline(
        "many_tcp",
        StdInstant::now(),
        Duration::from_secs(seconds.min(5)),
    )?;
    let established = establish_tcp_flows(
        server,
        client,
        link,
        &srv_handles,
        &cli_handles,
        wall0,
        connect_deadline,
    );
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

    let deadline = checked_deadline("many_tcp", StdInstant::now(), duration)?;
    let start = StdInstant::now();
    let alloc_before = AllocSnap::now();
    let mut poll_lat = SampledTimer::new();
    let mut mem_trace = MemTrace::start(mode);
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
            client.poll(now, link);
            server.poll(now, link);
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
            server.poll(now, link);
            client.poll(now, link);
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
        // see the process-memory trajectory.
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let alloc_after = AllocSnap::now();
    let elapsed = start.elapsed().as_secs_f64();

    Report {
        name: "many_tcp",
        elapsed,
        app_bytes_recvd: recvd.iter().sum(),
        wire_packets: client.device_stats.tx_packets + server.device_stats.tx_packets,
        wire_bytes: client.device_stats.tx_bytes + server.device_stats.tx_bytes,
        poll_lat: &poll_lat.histo,
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
    print_lane_stats("many_tcp", link.stats());

    // Per-flow socket footprint estimate. Useful for sizing per-flow
    // budgets in downstream consumers that admit many concurrent flows.
    println!();
    println!("  socket-state footprint (without lane pool):");
    println!(
        "    per-flow:           {} bytes (Socket {} + 2 × {} KiB buf)",
        per_flow_bytes,
        socket_bytes,
        BUF / 1024,
    );
    println!(
        "    total (both peers): {} bytes  ({:.2} MiB)",
        total_bytes,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
    validate_established_flows("many_tcp", established, n, &recvd_stats)
}

/// Deterministic fairness variant for TCP flows. Each round gives every flow
/// one bounded client send and server drain opportunity, then rotates the
/// start index so flow 0 does not always go first.
#[inline(never)] // Keep the large workload frame out of the dispatcher.
fn shape_many_tcp_fair(seconds: u64, n: usize, offload: bool, mode: RunMode) -> Result<(), String> {
    const BUF: usize = 4 * 1024;
    const PAYLOAD: usize = 256;
    let duration = checked_run_duration("many_tcp_fair", seconds)?;
    validate_unique_flow_count("many_tcp_fair", n)?;
    let TcpResources {
        queue_depth,
        socket_bytes,
        per_flow_bytes,
        total_bytes,
    } = checked_many_tcp_resources("many_tcp_fair", n, BUF)?;

    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, queue_depth, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

    let mut srv_handles = Vec::with_capacity(n);
    let mut cli_handles = Vec::with_capacity(n);
    for i in 0..n {
        let h_srv = add_tcp_socket(server, BUF);
        let h_cli = add_tcp_socket(client, BUF);
        let (dst_port, src_port) = flow_ports("many_tcp_fair", i)?;

        {
            let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
            s.set_ack_delay(None);
            s.set_nagle_enabled(false);
            let _ = s.listen(dst_port);
        }
        {
            let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
            c.set_ack_delay(None);
            c.set_nagle_enabled(false);
        }
        let _ = client.sockets.get_mut::<tcp::Socket>(h_cli).connect(
            client.iface.context(),
            (IpAddress::v4(10, 0, 0, 1), dst_port),
            src_port,
        );

        srv_handles.push(h_srv);
        cli_handles.push(h_cli);
    }

    let wall0 = StdInstant::now();
    let smol_now = || Instant::from_micros(wall0.elapsed().as_micros() as i64);
    let connect_deadline = checked_deadline(
        "many_tcp_fair",
        StdInstant::now(),
        Duration::from_secs(seconds.min(5)),
    )?;
    let established = establish_tcp_flows(
        server,
        client,
        link,
        &srv_handles,
        &cli_handles,
        wall0,
        connect_deadline,
    );
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

    let deadline = checked_deadline("many_tcp_fair", StdInstant::now(), duration)?;
    let start = StdInstant::now();
    let alloc_before = AllocSnap::now();
    let mut poll_lat = SampledTimer::new();
    let mut mem_trace = MemTrace::start(mode);
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
                    client.poll(now, link);
                    server.poll(now, link);
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
                server.poll(now, link);
                client.poll(now, link);
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
        app_bytes_recvd: recvd.iter().sum(),
        wire_packets: client.device_stats.tx_packets + server.device_stats.tx_packets,
        wire_bytes: client.device_stats.tx_bytes + server.device_stats.tx_bytes,
        poll_lat: &poll_lat.histo,
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
    print_lane_stats("many_tcp_fair", link.stats());

    println!();
    println!("  socket-state footprint (without lane pool):");
    println!(
        "    per-flow:           {} bytes (Socket {} + 2 x {} KiB buf)",
        per_flow_bytes,
        socket_bytes,
        BUF / 1024,
    );
    println!(
        "    total (both peers): {} bytes  ({:.2} MiB)",
        total_bytes,
        total_bytes as f64 / (1024.0 * 1024.0)
    );
    validate_established_flows("many_tcp_fair", established, n, &recvd_stats)?;
    validate_fairness("many_tcp_fair", &recvd_stats)
}

/// `n` concurrent UDP echo flows. Same metrics as `many_tcp`. UDP has no
/// flow control or cwnd so per-flow throughput is bounded only by the rate
/// at which the runner pumps bytes through.
#[inline(never)] // Keep the large workload frame out of the dispatcher.
fn shape_many_udp(seconds: u64, n: usize, offload: bool, mode: RunMode) -> Result<(), String> {
    const PAYLOAD: usize = 256;
    // Per-flow UDP socket buffer: a small ring with ~32 metadata slots is
    // enough to keep the pipe full without ballooning memory.
    const META_SLOTS: usize = 32;
    let duration = checked_run_duration("many_udp", seconds)?;
    validate_unique_flow_count("many_udp", n)?;
    let qd = n
        .checked_mul(4)
        .ok_or_else(|| "many_udp: packet queue size overflowed".to_owned())?
        .clamp(256, 8192);
    let udp_socket_bytes = core::mem::size_of::<udp::Socket>();
    let per_flow_bytes = udp_socket_bytes
        .checked_add(2 * (META_SLOTS * PAYLOAD))
        .and_then(|bytes| bytes.checked_add(2 * META_SLOTS * 24))
        .ok_or_else(|| "many_udp: per-flow socket footprint overflowed".to_owned())?;
    let total_bytes = n
        .checked_mul(2)
        .and_then(|sockets| sockets.checked_mul(per_flow_bytes))
        .ok_or_else(|| "many_udp: total socket footprint overflowed".to_owned())?;

    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, qd, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

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
    let mut server_bound = true;
    let mut client_bound = true;

    for i in 0..n {
        let (dst_port, src_port) = flow_ports("many_udp", i)?;

        let (rx, tx) = mk_udp();
        let h_srv = server.sockets.add(udp::Socket::new(rx, tx));
        server_bound &= server
            .sockets
            .get_mut::<udp::Socket>(h_srv)
            .bind(dst_port)
            .is_ok();
        srv_handles.push(h_srv);

        let (rx, tx) = mk_udp();
        let h_cli = client.sockets.add(udp::Socket::new(rx, tx));
        client_bound &= client
            .sockets
            .get_mut::<udp::Socket>(h_cli)
            .bind(src_port)
            .is_ok();
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

    let deadline = checked_deadline("many_udp", StdInstant::now(), duration)?;
    let start = StdInstant::now();
    let alloc_before = AllocSnap::now();
    let mut poll_lat = SampledTimer::new();
    let mut mem_trace = MemTrace::start(mode);
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
            client.poll(now, link);
            server.poll(now, link);
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
        app_bytes_recvd: recvd.iter().sum(),
        wire_packets: client.device_stats.tx_packets + server.device_stats.tx_packets,
        wire_bytes: client.device_stats.tx_bytes + server.device_stats.tx_bytes,
        poll_lat: &poll_lat.histo,
        alloc_before,
        alloc_after,
        work_units: n as u64,
        unit_label: "flows",
    }
    .print();

    Fairness::from(&sent).print("sent");
    let recvd_stats = Fairness::from(&recvd);
    recvd_stats.print("recvd");
    mem_trace.print();
    print_lane_stats("many_udp", link.stats());

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
    validate_udp_bindings("many_udp", server_bound, client_bound)?;
    validate_flow_stats("many_udp", &recvd_stats)?;
    validate_fairness("many_udp", &recvd_stats)
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
#[derive(Clone, Copy)]
struct MultiTcpWorkerStats {
    established: usize,
    expected_flows: usize,
    sent: u64,
    received: u64,
    elapsed_us: u64,
    lane_stats: LaneStats,
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn validate_nonzero_counters(shape: &str, counters: &[(&str, u64)]) -> Result<(), String> {
    for &(name, value) in counters {
        if value == 0 {
            return Err(format!("{shape}: {name} was zero"));
        }
    }
    Ok(())
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn validate_churn_rate(
    target_per_second: usize,
    elapsed_seconds: f64,
    opened: u64,
    closed: u64,
) -> Result<(), String> {
    const MIN_RATIO: f64 = 0.95;

    if closed > opened {
        return Err(format!(
            "churn: closed {closed} connections after opening {opened}"
        ));
    }
    let minimum = target_per_second as f64 * elapsed_seconds * MIN_RATIO;
    if opened as f64 >= minimum && closed as f64 >= minimum {
        Ok(())
    } else {
        Err(format!(
            "churn: opened {opened} and closed {closed}; expected at least {minimum:.0} of each"
        ))
    }
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn validate_pool_growth(shape: &str, start: usize, end: usize) -> Result<(), String> {
    if end <= start {
        return Err(format!(
            "{shape}: active pool use did not grow ({start} -> {end})"
        ));
    }
    Ok(())
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn validate_rst_unread_rx(
    flows: usize,
    closed: usize,
    queued: u64,
    drained: u64,
    retained_pool: usize,
    pool_after_drain: usize,
    pool_after_teardown: usize,
) -> Result<(), String> {
    if closed != flows {
        return Err(format!("rst_unread_rx: reset {closed}/{flows} flows"));
    }
    if queued == 0 || drained != queued {
        return Err(format!(
            "rst_unread_rx: queued {queued} bytes, drained {drained}"
        ));
    }
    if retained_pool == 0 {
        return Err("rst_unread_rx: unread data retained no pool charge".to_owned());
    }
    if pool_after_drain != 0 || pool_after_teardown != 0 {
        return Err(format!(
            "rst_unread_rx: pool after drain/teardown was {pool_after_drain}/{pool_after_teardown}"
        ));
    }
    Ok(())
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn fairness_after_prefill(received: &[u64], prefill_per_flow: u64) -> Fairness {
    let progress: Vec<_> = received
        .iter()
        .map(|bytes| bytes.saturating_sub(prefill_per_flow))
        .collect();
    Fairness::from(&progress)
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn validate_pool_pressure(
    established: usize,
    pool_used: usize,
    pool_budget: usize,
    pending_bytes: usize,
    received: &Fairness,
    pool_after_teardown: usize,
) -> Result<(), String> {
    let flows = received.n;
    if established != flows {
        return Err(format!(
            "pool_pressure: established {established}/{flows} flows"
        ));
    }
    if pool_used != pool_budget {
        return Err(format!(
            "pool_pressure: used {pool_used} of {pool_budget} pool bytes"
        ));
    }
    if pending_bytes == 0 {
        return Err("pool_pressure: saturation applied no sender backpressure".to_owned());
    }
    if pool_after_teardown != 0 {
        return Err(format!(
            "pool_pressure: pool after teardown was {pool_after_teardown}"
        ));
    }
    validate_flow_stats("pool_pressure", received)?;
    validate_fairness("pool_pressure", received)
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn validate_multi_tcp_workers(
    shape: &str,
    workers: &[Result<MultiTcpWorkerStats, String>],
) -> Result<(), String> {
    let mut sent = 0u64;
    let mut received = Vec::with_capacity(workers.len());
    for (worker_id, result) in workers.iter().enumerate() {
        let stats = match result {
            Ok(stats) => stats,
            Err(error) => return Err(format!("{shape}: worker {worker_id}: {error}")),
        };
        if stats.established != stats.expected_flows {
            return Err(format!(
                "{shape}: worker {worker_id} established {}/{} flows",
                stats.established, stats.expected_flows
            ));
        }
        sent += stats.sent;
        received.push(stats.received);
    }

    let received = Fairness::from(&received);
    validate_nonzero_counters(shape, &[("aggregate sent bytes", sent)])?;
    validate_flow_stats(shape, &received)?;
    validate_fairness(shape, &received)
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_multi_tcp(
    seconds: u64,
    n_threads: usize,
    flows_per_thread: usize,
    offload: bool,
) -> Result<(), String> {
    shape_multi_tcp_impl(
        seconds,
        n_threads,
        flows_per_thread,
        offload,
        MultiTcpWorkload::Echo,
    )
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_multi_tcp_sink(
    seconds: u64,
    n_threads: usize,
    flows_per_thread: usize,
    offload: bool,
) -> Result<(), String> {
    shape_multi_tcp_impl(
        seconds,
        n_threads,
        flows_per_thread,
        offload,
        MultiTcpWorkload::Sink,
    )
}

#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_multi_tcp_impl(
    seconds: u64,
    n_threads: usize,
    flows_per_thread: usize,
    offload: bool,
    workload: MultiTcpWorkload,
) -> Result<(), String> {
    const MAX_BUF: u32 = 32 * 1024;
    const PAYLOAD: usize = 1024;
    let shape_name = workload.shape_name();
    let duration = checked_run_duration(shape_name, seconds)?;
    validate_unique_worker_count(shape_name, n_threads)?;
    validate_unique_flow_count(shape_name, flows_per_thread)?;
    let total_flows = n_threads
        .checked_mul(flows_per_thread)
        .ok_or_else(|| format!("{shape_name}: total flow count overflowed"))?;
    // One full rx+tx budget per logical flow. The paired endpoints share the
    // pool, so this keeps the usual one-active-direction workload clear of
    // growth refusal while still exercising allocator contention.
    let pool_bytes = total_flows
        .checked_mul(2)
        .and_then(|sockets| sockets.checked_mul(MAX_BUF as usize))
        .ok_or_else(|| format!("{shape_name}: pool budget overflowed"))?;
    let qd = packet_queue_depth(shape_name, flows_per_thread)?;
    let pool = tcp::MemoryPool::new(pool_bytes);

    let (vol_before, nvol_before) = ctxsw_counts();
    let worker_pool = pool.clone();
    let mut workers = MultiTcpWorkers::spawn(n_threads, move |tid, mut phases| {
        let pool = worker_pool.clone();
        {
            // Distinct base address per thread so server/client tuples don't
            // clash if anything inspects them (they won't — lanes are isolated).
            let subnet = match worker_subnet(shape_name, tid) {
                Ok(subnet) => subnet,
                Err(error) => {
                    let _ = phases.ready(Err(error));
                    return;
                }
            };
            let mut endpoints = setup_paired_endpoints(subnet, 1500, qd, offload);
            let PairedEndpoints {
                server,
                client,
                link,
            } = &mut endpoints;

            let mut srv_handles = Vec::with_capacity(flows_per_thread);
            let mut cli_handles = Vec::with_capacity(flows_per_thread);
            let mut setup_error = None;
            for i in 0..flows_per_thread {
                if i & 0xff == 0 && phases.is_cancelled() {
                    break;
                }
                let h_srv = add_tcp_socket_dyn(server, MAX_BUF, &pool);
                let h_cli = add_tcp_socket_dyn(client, MAX_BUF, &pool);
                let (dst_port, src_port) = match flow_ports(shape_name, i) {
                    Ok(ports) => ports,
                    Err(error) => {
                        setup_error.get_or_insert(error);
                        break;
                    }
                };
                {
                    let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
                    s.set_ack_delay(None);
                    s.set_nagle_enabled(false);
                    if let Err(error) = s.listen(dst_port) {
                        setup_error.get_or_insert_with(|| {
                            format!("listen failed for flow {i}: {error:?}")
                        });
                    }
                }
                {
                    let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
                    c.set_ack_delay(None);
                    c.set_nagle_enabled(false);
                }
                if let Err(error) = client.sockets.get_mut::<tcp::Socket>(h_cli).connect(
                    client.iface.context(),
                    (IpAddress::v4(subnet[0], subnet[1], subnet[2], 1), dst_port),
                    src_port,
                ) {
                    setup_error
                        .get_or_insert_with(|| format!("connect failed for flow {i}: {error:?}"));
                }
                srv_handles.push(h_srv);
                cli_handles.push(h_cli);
                if setup_error.is_some() {
                    break;
                }
            }

            // Drive until ESTABLISHED on every flow.
            let smol_now = |w0: StdInstant| Instant::from_micros(w0.elapsed().as_micros() as i64);
            let w0 = StdInstant::now();
            let connect_deadline =
                match checked_deadline(shape_name, w0, Duration::from_secs(seconds.min(5))) {
                    Ok(deadline) => Some(deadline),
                    Err(error) => {
                        setup_error.get_or_insert(error);
                        None
                    }
                };
            if let Some(connect_deadline) = connect_deadline {
                loop {
                    let now = smol_now(w0);
                    server.poll(now, link);
                    client.poll(now, link);
                    let all_ready = cli_handles
                        .iter()
                        .zip(srv_handles.iter())
                        .all(|(&hc, &hs)| {
                            matches!(
                                client.sockets.get::<tcp::Socket>(hc).state(),
                                tcp::State::Established
                            ) && matches!(
                                server.sockets.get::<tcp::Socket>(hs).state(),
                                tcp::State::Established
                            )
                        });
                    if all_ready || phases.is_cancelled() || StdInstant::now() >= connect_deadline {
                        break;
                    }
                }
            }
            let established = cli_handles
                .iter()
                .zip(srv_handles.iter())
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
            if established != flows_per_thread {
                setup_error.get_or_insert_with(|| {
                    format!("established {established}/{flows_per_thread} flows")
                });
            }

            let payload = vec![0xa5u8; PAYLOAD];
            let mut sink = vec![0u8; PAYLOAD];

            // Setup and start coordination stays outside the timed traffic loop.
            let setup = match setup_error {
                Some(error) => Err(error),
                None => Ok(()),
            };
            if !phases.ready(setup) {
                return;
            }
            let steady_start = StdInstant::now();

            let mut sent: u64 = 0;
            let mut recvd: u64 = 0;
            let deadline = match checked_deadline(shape_name, steady_start, duration) {
                Ok(deadline) => deadline,
                Err(error) => {
                    let _ = phases.finished(Err(error));
                    return;
                }
            };
            let mut iterations = 0u64;
            match workload {
                MultiTcpWorkload::Echo => {
                    while StdInstant::now() < deadline {
                        if iterations & 0xff == 0 && phases.is_cancelled() {
                            break;
                        }
                        iterations = iterations.wrapping_add(1);
                        let now = smol_now(w0);
                        for &h in &cli_handles {
                            let s = client.sockets.get_mut::<tcp::Socket>(h);
                            if s.can_send()
                                && let Ok(n) = s.send_slice(&payload)
                            {
                                sent += n as u64;
                            }
                        }
                        client.poll(now, link);
                        server.poll(now, link);
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
                        server.poll(now, link);
                        client.poll(now, link);
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
                        if iterations & 0xff == 0 && phases.is_cancelled() {
                            break;
                        }
                        iterations = iterations.wrapping_add(1);
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
                        client.poll(now, link);
                        server.poll(now, link);
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
                        server.poll(now, link);
                        client.poll(now, link);
                    }
                }
            }
            let elapsed_us = steady_start.elapsed().as_micros() as u64;
            let lane_stats = link.stats();
            // Keep every worker's sockets alive until the main thread samples
            // the active-end memory and pool boundaries.
            let _ = phases.finished(Ok(MultiTcpWorkerStats {
                established,
                expected_flows: flows_per_thread,
                sent,
                received: recvd,
                elapsed_us,
                lane_stats,
            }));
        }
    })
    .map_err(|error| format!("{shape_name}: {error}"))?;

    // Wait until every worker has connected its sockets, then bracket only the
    // steady phase. Workers remain blocked before start and after finish.
    if let Err(error) = workers.wait_ready() {
        let pool_after_teardown = pool.used();
        if pool_after_teardown != 0 {
            return Err(format!(
                "{shape_name}: {error}; pool use after setup teardown was {pool_after_teardown}, expected 0"
            ));
        }
        return Err(format!("{shape_name}: {error}"));
    }
    let mut result_slots = std::iter::repeat_with(|| None)
        .take(n_threads)
        .collect::<Vec<_>>();
    let pool_used_start = pool.used();
    let memory_start = process_memory_sample();
    let alloc_before = alloc_counters_with_memory(memory_start);
    let allocator_phase = ALLOCATOR_TELEMETRY
        .begin()
        .map_err(|error| format!("{shape_name}: {error}"))?;
    workers.start();
    workers.wait_finished(&mut result_slots);
    let allocator_peak = allocator_phase.finish();
    let memory_end = process_memory_sample();
    let alloc_after = alloc_counters_with_memory(memory_end);
    let pool_used_end = pool.used();
    workers.release_and_join();
    let pool_after_teardown = pool.used();
    let memory_report = DynamicMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        allocator_peak,
        PoolUsage {
            start: pool_used_start,
            end: pool_used_end,
            budget: pool_bytes,
            after_teardown: pool_after_teardown,
        },
    )
    .map_err(|error| format!("{shape_name}: {error}"))?;
    let results = result_slots
        .into_iter()
        .map(Option::unwrap)
        .collect::<Vec<_>>();
    let total_elapsed_us = results
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .map(|stats| stats.elapsed_us)
        .max()
        .unwrap_or(1)
        .max(1);
    let total_secs = total_elapsed_us as f64 / 1_000_000.0;
    let (vol_after, nvol_after) = ctxsw_counts();
    let vol_delta = vol_after - vol_before;
    let nvol_delta = nvol_after - nvol_before;
    let agg_sent: u64 = results
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .map(|stats| stats.sent)
        .sum();
    let agg_recvd: u64 = results
        .iter()
        .filter_map(|result| result.as_ref().ok())
        .map(|stats| stats.received)
        .sum();
    let agg_gbps = (agg_recvd as f64 * 8.0) / total_secs / 1e9;
    let per_thread_gbps: Vec<f64> = results
        .iter()
        .map(|result| match result {
            Ok(stats) => {
                let elapsed_us = stats.elapsed_us.max(1);
                (stats.received as f64 * 8.0) / (elapsed_us as f64 / 1_000_000.0) / 1e9
            }
            Err(_) => 0.0,
        })
        .collect();
    let mut lane_stats = LaneStats::default();
    for stats in results.iter().filter_map(|result| result.as_ref().ok()) {
        lane_stats.merge(stats.lane_stats);
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

    println!("\n========== shape: {shape_name} ==========");
    println!(
        "  threads:                {n_threads}   flows/thread: {flows_per_thread}   total flows: {total_flows}"
    );
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    println!("  pool used active start: {} KiB", pool_used_start / 1024);
    println!("  pool used active end:   {} KiB", pool_used_end / 1024);
    println!(
        "  pool used after teardown: {} KiB",
        pool_after_teardown / 1024
    );
    println!();
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
    for (i, (gbps, result)) in per_thread_gbps.iter().zip(results.iter()).enumerate() {
        match result {
            Ok(_) => println!("    t{i:>2}: {gbps:>7.3} Gbps"),
            Err(error) => println!("    t{i:>2}: ERROR ({error})"),
        }
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
    validate_pool_growth(shape_name, pool_used_start, pool_used_end)?;
    validate_multi_tcp_workers(shape_name, &results)
}

/// Retain unread bytes across a reset, hold them resident, then drain them
/// through the public receive API and require an exact pool refund.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[inline(never)]
fn shape_rst_unread_rx(
    seconds: u64,
    flows: usize,
    offload: bool,
    mode: RunMode,
) -> Result<(), String> {
    const MAX_BUF: u32 = 32 * 1024;
    const PAYLOAD: usize = 1024;

    let duration = checked_run_duration("rst_unread_rx", seconds)?;
    let queue_depth = packet_queue_depth("rst_unread_rx", flows)?;
    let pool_bytes = flows
        .checked_mul(4 * MAX_BUF as usize)
        .ok_or_else(|| "rst_unread_rx: pool budget overflowed".to_owned())?;
    let pool = tcp::MemoryPool::new(pool_bytes);
    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, queue_depth, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;
    let (server_handles, client_handles) =
        add_dynamic_tcp_flows("rst_unread_rx", server, client, flows, MAX_BUF, &pool)?;

    let wall_origin = StdInstant::now();
    let connect_deadline = checked_deadline(
        "rst_unread_rx",
        wall_origin,
        Duration::from_secs(seconds.min(5)),
    )?;
    let established = establish_tcp_flows(
        server,
        client,
        link,
        &server_handles,
        &client_handles,
        wall_origin,
        connect_deadline,
    );
    if established != flows {
        return Err(format!(
            "rst_unread_rx: established {established}/{flows} flows"
        ));
    }
    let payload = [0x5a; PAYLOAD];
    for (flow, &handle) in server_handles.iter().enumerate() {
        let sent = server
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .send_slice(&payload)
            .map_err(|error| format!("rst_unread_rx: flow {flow} send failed: {error:?}"))?;
        if sent != PAYLOAD {
            return Err(format!(
                "rst_unread_rx: flow {flow} queued {sent}/{PAYLOAD} bytes"
            ));
        }
    }

    let receive_deadline = checked_deadline(
        "rst_unread_rx",
        StdInstant::now(),
        Duration::from_secs(seconds.min(5)),
    )?;
    loop {
        let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
        server.poll(now, link);
        client.poll(now, link);
        if client_handles
            .iter()
            .all(|&handle| client.sockets.get::<tcp::Socket>(handle).recv_queue() == PAYLOAD)
        {
            break;
        }
        if StdInstant::now() >= receive_deadline {
            return Err("rst_unread_rx: payload delivery timed out".to_owned());
        }
    }
    let queued = client_handles
        .iter()
        .map(|&handle| client.sockets.get::<tcp::Socket>(handle).recv_queue() as u64)
        .sum();

    for &handle in &server_handles {
        server.sockets.get_mut::<tcp::Socket>(handle).abort();
    }
    let reset_deadline = checked_deadline(
        "rst_unread_rx",
        StdInstant::now(),
        Duration::from_secs(seconds.min(5)),
    )?;
    let closed = loop {
        let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
        server.poll(now, link);
        client.poll(now, link);
        let closed = client_handles
            .iter()
            .filter(|&&handle| {
                client.sockets.get::<tcp::Socket>(handle).state() == tcp::State::Closed
            })
            .count();
        if closed == flows || StdInstant::now() >= reset_deadline {
            break closed;
        }
    };

    let retained_pool = pool.used();
    let retained_capacity: usize = client_handles
        .iter()
        .map(|&handle| client.sockets.get::<tcp::Socket>(handle).recv_capacity())
        .sum();
    if retained_pool != retained_capacity {
        return Err(format!(
            "rst_unread_rx: retained pool {retained_pool} != receive capacity {retained_capacity}"
        ));
    }

    let mut mem_trace = MemTrace::start(mode);
    let memory_start = process_memory_sample();
    let alloc_before = alloc_counters_with_memory(memory_start);
    let allocator_phase = ALLOCATOR_TELEMETRY
        .begin()
        .map_err(|error| format!("rst_unread_rx: {error}"))?;
    let hold_start = StdInstant::now();
    let hold_deadline = checked_deadline("rst_unread_rx", hold_start, duration)?;
    while StdInstant::now() < hold_deadline {
        let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
        server.poll(now, link);
        client.poll(now, link);
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let elapsed = hold_start.elapsed().as_secs_f64();
    let allocator_peak = allocator_phase.finish();
    let memory_end = process_memory_sample();
    let alloc_after = alloc_counters_with_memory(memory_end);
    let pool_after_hold = pool.used();

    let mut drained = 0u64;
    let mut scratch = [0u8; PAYLOAD];
    for (flow, &handle) in client_handles.iter().enumerate() {
        let socket = client.sockets.get_mut::<tcp::Socket>(handle);
        while socket.can_recv() {
            let read = socket
                .recv_slice(&mut scratch)
                .map_err(|error| format!("rst_unread_rx: flow {flow} drain failed: {error:?}"))?;
            if read == 0 {
                break;
            }
            if !scratch[..read].iter().all(|&byte| byte == 0x5a) {
                return Err(format!("rst_unread_rx: flow {flow} payload mismatch"));
            }
            drained += read as u64;
        }
    }
    let pool_after_drain = pool.used();
    let lane_stats = link.stats();
    drop(endpoints);
    let pool_after_teardown = pool.used();
    let memory_report = DynamicMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        allocator_peak,
        PoolUsage {
            start: retained_pool,
            end: pool_after_hold,
            budget: pool_bytes,
            after_teardown: pool_after_teardown,
        },
    )
    .map_err(|error| format!("rst_unread_rx: {error}"))?;

    println!("\n========== shape: rst_unread_rx ==========");
    println!("  flows:                  {flows}");
    println!("  established / reset:    {established} / {closed}");
    println!("  queued / drained bytes: {queued} / {drained}");
    println!("  elapsed retained:       {elapsed:.3}s");
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    println!("  pool used active start: {} KiB", retained_pool / 1024);
    println!("  pool used active end:   {} KiB", pool_after_hold / 1024);
    println!("  pool used after drain:  {} KiB", pool_after_drain / 1024);
    println!(
        "  pool used after teardown: {} KiB",
        pool_after_teardown / 1024
    );
    memory_report.print();
    mem_trace.print();
    print_lane_stats("rst_unread_rx", lane_stats);
    validate_rst_unread_rx(
        flows,
        closed,
        queued,
        drained,
        retained_pool,
        pool_after_drain,
        pool_after_teardown,
    )
}

/// Fill a shared pool exactly, observe sender backpressure, then require fair
/// progress while all buffers remain within the fixed budget.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[inline(never)]
fn shape_pool_pressure(
    seconds: u64,
    flows: usize,
    offload: bool,
    mode: RunMode,
) -> Result<(), String> {
    const MAX_BUF: u32 = 32 * 1024;
    const CHUNK: usize = 8 * 1024;

    let duration = checked_run_duration("pool_pressure", seconds)?;
    let queue_depth = packet_queue_depth("pool_pressure", flows)?;
    let pool_bytes = flows
        .checked_mul(3 * CHUNK)
        .ok_or_else(|| "pool_pressure: pool budget overflowed".to_owned())?;
    let established_bytes = flows
        .checked_mul(2 * CHUNK)
        .ok_or_else(|| "pool_pressure: establishment budget overflowed".to_owned())?;
    let pool = tcp::MemoryPool::new(pool_bytes);
    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, queue_depth, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;
    let (server_handles, client_handles) =
        add_dynamic_tcp_flows("pool_pressure", server, client, flows, MAX_BUF, &pool)?;

    let wall_origin = StdInstant::now();
    let connect_deadline = checked_deadline(
        "pool_pressure",
        wall_origin,
        Duration::from_secs(seconds.min(5)),
    )?;
    let established = establish_tcp_flows(
        server,
        client,
        link,
        &server_handles,
        &client_handles,
        wall_origin,
        connect_deadline,
    );
    if pool.used() != established_bytes {
        return Err(format!(
            "pool_pressure: establishment used {} of {established_bytes} expected bytes",
            pool.used()
        ));
    }

    let payload = [0xa5; CHUNK];
    for (flow, &handle) in client_handles.iter().enumerate() {
        let sent = client
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .send_slice(&payload)
            .map_err(|error| format!("pool_pressure: flow {flow} send failed: {error:?}"))?;
        if sent != CHUNK {
            return Err(format!(
                "pool_pressure: flow {flow} queued {sent}/{CHUNK} initial bytes"
            ));
        }
    }
    let fill_deadline = checked_deadline(
        "pool_pressure",
        StdInstant::now(),
        Duration::from_secs(seconds.min(5)),
    )?;
    loop {
        let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
        client.poll(now, link);
        server.poll(now, link);
        client.poll(now, link);
        let full = server_handles
            .iter()
            .all(|&handle| server.sockets.get::<tcp::Socket>(handle).recv_queue() == CHUNK);
        let acked = client_handles
            .iter()
            .all(|&handle| client.sockets.get::<tcp::Socket>(handle).send_queue() == 0);
        if full && acked {
            break;
        }
        if StdInstant::now() >= fill_deadline {
            return Err("pool_pressure: initial saturation timed out".to_owned());
        }
    }

    for (flow, &handle) in client_handles.iter().enumerate() {
        let sent = client
            .sockets
            .get_mut::<tcp::Socket>(handle)
            .send_slice(&payload)
            .map_err(|error| format!("pool_pressure: flow {flow} retry failed: {error:?}"))?;
        if sent != CHUNK {
            return Err(format!(
                "pool_pressure: flow {flow} queued {sent}/{CHUNK} pending bytes"
            ));
        }
    }
    let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
    client.poll(now, link);
    server.poll(now, link);
    client.poll(now, link);
    let saturated_pool = pool.used();
    let pending_bytes: usize = client_handles
        .iter()
        .map(|&handle| client.sockets.get::<tcp::Socket>(handle).send_queue())
        .sum();

    let mut received = vec![0u64; flows];
    let mut scratch = [0u8; CHUNK];
    let mut mem_trace = MemTrace::start(mode);
    let memory_start = process_memory_sample();
    let alloc_before = alloc_counters_with_memory(memory_start);
    let allocator_phase = ALLOCATOR_TELEMETRY
        .begin()
        .map_err(|error| format!("pool_pressure: {error}"))?;
    let start = StdInstant::now();
    let deadline = checked_deadline("pool_pressure", start, duration)?;
    while StdInstant::now() < deadline {
        for (flow, &handle) in server_handles.iter().enumerate() {
            let socket = server.sockets.get_mut::<tcp::Socket>(handle);
            while socket.can_recv() {
                match socket.recv_slice(&mut scratch) {
                    Ok(read) if read > 0 => received[flow] += read as u64,
                    _ => break,
                }
            }
        }
        let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
        server.poll(now, link);
        client.poll(now, link);
        for &handle in &client_handles {
            let socket = client.sockets.get_mut::<tcp::Socket>(handle);
            if socket.send_queue() < CHUNK && socket.can_send() {
                let _ = socket.send_slice(&payload);
            }
        }
        client.poll(now, link);
        server.poll(now, link);
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let allocator_peak = allocator_phase.finish();
    let memory_end = process_memory_sample();
    let alloc_after = alloc_counters_with_memory(memory_end);
    let pool_end = pool.used();
    let lane_stats = link.stats();
    drop(endpoints);
    let pool_after_teardown = pool.used();
    let memory_report = DynamicMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        allocator_peak,
        PoolUsage {
            start: saturated_pool,
            end: pool_end,
            budget: pool_bytes,
            after_teardown: pool_after_teardown,
        },
    )
    .map_err(|error| format!("pool_pressure: {error}"))?;
    let fairness = fairness_after_prefill(&received, CHUNK as u64);
    let total = fairness.total;

    println!("\n========== shape: pool_pressure ==========");
    println!("  flows:                  {flows}");
    println!("  established:            {established}");
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    println!("  pool used active start: {} KiB", saturated_pool / 1024);
    println!("  pool used active end:   {} KiB", pool_end / 1024);
    println!("  pending at saturation:  {pending_bytes} bytes");
    println!("  elapsed:                {elapsed:.3}s");
    println!("  app received:           {total} bytes");
    println!(
        "  active throughput:      {:.3} Gbps",
        total as f64 * 8.0 / elapsed / 1e9
    );
    println!(
        "  pool used after teardown: {} KiB",
        pool_after_teardown / 1024
    );
    fairness.print("received under pressure");
    memory_report.print();
    mem_trace.print();
    print_lane_stats("pool_pressure", lane_stats);
    validate_pool_pressure(
        established,
        saturated_pool,
        pool_bytes,
        pending_bytes,
        &fairness,
        pool_after_teardown,
    )
}

/// Concurrent one-way dynamic TCP and buffered UDP on the same interfaces,
/// link, allocator, and process-memory budget.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
#[inline(never)]
fn shape_mixed_tcp_udp(
    seconds: u64,
    tcp_flows: usize,
    udp_flows: usize,
    offload: bool,
    mode: RunMode,
) -> Result<(), String> {
    const MAX_BUF: u32 = 32 * 1024;
    const TCP_PAYLOAD: usize = 512;
    const UDP_PAYLOAD: usize = 256;
    const UDP_SLOTS: usize = 8;

    let duration = checked_run_duration("mixed_tcp_udp", seconds)?;
    validate_unique_flow_count("mixed_tcp_udp", tcp_flows.max(udp_flows))?;
    let total_flows = tcp_flows
        .checked_add(udp_flows)
        .ok_or_else(|| "mixed_tcp_udp: total flow count overflowed".to_owned())?;
    let queue_depth = packet_queue_depth("mixed_tcp_udp", total_flows)?;
    let pool_bytes = tcp_flows
        .checked_mul(4 * MAX_BUF as usize)
        .ok_or_else(|| "mixed_tcp_udp: pool budget overflowed".to_owned())?;
    let pool = tcp::MemoryPool::new(pool_bytes);
    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, queue_depth, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;
    let (server_tcp, client_tcp) =
        add_dynamic_tcp_flows("mixed_tcp_udp", server, client, tcp_flows, MAX_BUF, &pool)?;

    let make_udp = || {
        let rx_meta = vec![udp::PacketMetadata::EMPTY; UDP_SLOTS];
        let tx_meta = vec![udp::PacketMetadata::EMPTY; UDP_SLOTS];
        let rx_data = vec![0; UDP_SLOTS * UDP_PAYLOAD];
        let tx_data = vec![0; UDP_SLOTS * UDP_PAYLOAD];
        (
            udp::PacketBuffer::new(rx_meta, rx_data),
            udp::PacketBuffer::new(tx_meta, tx_data),
        )
    };
    let mut server_udp = Vec::with_capacity(udp_flows);
    let mut client_udp = Vec::with_capacity(udp_flows);
    let mut destinations = Vec::with_capacity(udp_flows);
    let mut server_bound = true;
    let mut client_bound = true;
    for flow in 0..udp_flows {
        let (server_port, client_port) = flow_ports("mixed_tcp_udp", flow)?;
        let (rx, tx) = make_udp();
        let server_handle = server.sockets.add(udp::Socket::new(rx, tx));
        server_bound &= server
            .sockets
            .get_mut::<udp::Socket>(server_handle)
            .bind(server_port)
            .is_ok();
        let (rx, tx) = make_udp();
        let client_handle = client.sockets.add(udp::Socket::new(rx, tx));
        client_bound &= client
            .sockets
            .get_mut::<udp::Socket>(client_handle)
            .bind(client_port)
            .is_ok();
        server_udp.push(server_handle);
        client_udp.push(client_handle);
        destinations.push(udp::UdpMetadata::from((
            IpAddress::v4(10, 0, 0, 1),
            server_port,
        )));
    }

    let wall_origin = StdInstant::now();
    let connect_deadline = checked_deadline(
        "mixed_tcp_udp",
        wall_origin,
        Duration::from_secs(seconds.min(5)),
    )?;
    let established = establish_tcp_flows(
        server,
        client,
        link,
        &server_tcp,
        &client_tcp,
        wall_origin,
        connect_deadline,
    );

    let tcp_payload = [0x6d; TCP_PAYLOAD];
    let udp_payload = [0x75; UDP_PAYLOAD];
    let mut tcp_scratch = [0; TCP_PAYLOAD];
    let mut udp_scratch = [0; UDP_PAYLOAD];
    let mut tcp_received = vec![0u64; tcp_flows];
    let mut udp_received = vec![0u64; udp_flows];
    let mut mem_trace = MemTrace::start(mode);
    let pool_start = pool.used();
    let memory_start = process_memory_sample();
    let alloc_before = alloc_counters_with_memory(memory_start);
    let allocator_phase = ALLOCATOR_TELEMETRY
        .begin()
        .map_err(|error| format!("mixed_tcp_udp: {error}"))?;
    let start = StdInstant::now();
    let deadline = checked_deadline("mixed_tcp_udp", start, duration)?;
    while StdInstant::now() < deadline {
        for &handle in &client_tcp {
            let socket = client.sockets.get_mut::<tcp::Socket>(handle);
            if socket.can_send() {
                let _ = socket.send_slice(&tcp_payload);
            }
        }
        for (flow, &handle) in client_udp.iter().enumerate() {
            let socket = client.sockets.get_mut::<udp::Socket>(handle);
            if socket.can_send() {
                let _ = socket.send_slice(&udp_payload, destinations[flow]);
            }
        }

        let now = Instant::from_micros(wall_origin.elapsed().as_micros() as i64);
        client.poll(now, link);
        server.poll(now, link);
        for (flow, &handle) in server_tcp.iter().enumerate() {
            let socket = server.sockets.get_mut::<tcp::Socket>(handle);
            while socket.can_recv() {
                match socket.recv_slice(&mut tcp_scratch) {
                    Ok(read) if read > 0 => tcp_received[flow] += read as u64,
                    _ => break,
                }
            }
        }
        for (flow, &handle) in server_udp.iter().enumerate() {
            let socket = server.sockets.get_mut::<udp::Socket>(handle);
            while socket.can_recv() {
                match socket.recv_slice(&mut udp_scratch) {
                    Ok((read, _)) => udp_received[flow] += read as u64,
                    Err(_) => break,
                }
            }
        }
        server.poll(now, link);
        client.poll(now, link);
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let allocator_peak = allocator_phase.finish();
    let memory_end = process_memory_sample();
    let alloc_after = alloc_counters_with_memory(memory_end);
    let pool_end = pool.used();
    let lane_stats = link.stats();
    drop(endpoints);
    let pool_after_teardown = pool.used();
    let memory_report = DynamicMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        allocator_peak,
        PoolUsage {
            start: pool_start,
            end: pool_end,
            budget: pool_bytes,
            after_teardown: pool_after_teardown,
        },
    )
    .map_err(|error| format!("mixed_tcp_udp: {error}"))?;
    let tcp_fairness = Fairness::from(&tcp_received);
    let udp_fairness = Fairness::from(&udp_received);
    let total = tcp_fairness.total + udp_fairness.total;

    println!("\n========== shape: mixed_tcp_udp ==========");
    println!("  TCP / UDP flows:        {tcp_flows} / {udp_flows}");
    println!("  TCP established:        {established}");
    println!("  elapsed:                {elapsed:.3}s");
    println!(
        "  TCP / UDP received:     {} / {} bytes",
        tcp_fairness.total, udp_fairness.total
    );
    println!(
        "  active throughput:      {:.3} Gbps",
        total as f64 * 8.0 / elapsed / 1e9
    );
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    println!("  pool used active start: {} KiB", pool_start / 1024);
    println!("  pool used active end:   {} KiB", pool_end / 1024);
    println!(
        "  pool used after teardown: {} KiB",
        pool_after_teardown / 1024
    );
    tcp_fairness.print("TCP received");
    udp_fairness.print("UDP received");
    memory_report.print();
    mem_trace.print();
    print_lane_stats("mixed_tcp_udp", lane_stats);

    validate_established_flows("mixed_tcp_udp TCP", established, tcp_flows, &tcp_fairness)?;
    validate_fairness("mixed_tcp_udp TCP", &tcp_fairness)?;
    validate_udp_bindings("mixed_tcp_udp UDP", server_bound, client_bound)?;
    validate_flow_stats("mixed_tcp_udp UDP", &udp_fairness)?;
    validate_fairness("mixed_tcp_udp UDP", &udp_fairness)?;
    validate_pool_growth("mixed_tcp_udp", pool_start, pool_end)
}

/// Connection-churn shape. Repeatedly opens and tears down TCP flows at
/// `target_conn_per_sec`, each exchanging one short payload before close.
/// Exercises the release path (`set_state(Closed)`, `reset()`, `Drop`)
/// under load; verifies that pool refunds keep up with admissions and
/// the connection cap doesn't drift.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_churn(
    seconds: u64,
    target_conn_per_sec: usize,
    offload: bool,
    mode: RunMode,
) -> Result<(), String> {
    const MAX_BUF: u32 = 32 * 1024;
    const SLOTS: usize = 256;
    const PAYLOAD: usize = 128;

    let duration = checked_run_duration("churn", seconds)?;
    let interval_us = churn_interval_us(target_conn_per_sec)?;
    let qd = (SLOTS * 16).clamp(1024, 16384);
    let pool_bytes: usize = SLOTS * 2 * MAX_BUF as usize;
    let pool = tcp::MemoryPool::new(pool_bytes);

    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, qd, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

    // Pre-allocate a ring of socket handles. Each "churn slot" is a pair
    // we cycle through; once a pair is fully torn down we recycle the slot.
    let mut slots: Vec<(
        smoltcp::iface::SocketHandle,
        smoltcp::iface::SocketHandle,
        u16,
    )> = Vec::with_capacity(SLOTS);
    for i in 0..SLOTS {
        let h_srv = add_tcp_socket_dyn(server, MAX_BUF, &pool);
        let h_cli = add_tcp_socket_dyn(client, MAX_BUF, &pool);
        slots.push((h_srv, h_cli, i as u16));
    }

    let smol_now = |w0: StdInstant| Instant::from_micros(w0.elapsed().as_micros() as i64);

    let mut next_slot = 0usize;
    let mut opened: u64 = 0;
    let mut closed: u64 = 0;
    let mut bytes_xferred: u64 = 0;
    let mut setup_error = None;
    let payload = vec![0xc5u8; PAYLOAD];
    let mut scratch = vec![0u8; PAYLOAD];
    let mut mem_trace = MemTrace::start(mode);
    let pool_used_start = pool.used();
    let memory_start = process_memory_sample();
    let alloc_before = alloc_counters_with_memory(memory_start);
    let allocator_phase = ALLOCATOR_TELEMETRY
        .begin()
        .map_err(|error| format!("churn: {error}"))?;
    let start = StdInstant::now();
    let mut next_open_us: u64 = 0;
    let deadline = checked_deadline("churn", start, duration)?;

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
                let (dst_port, src_port) = flow_ports("churn", base_port as usize)?;
                ss.set_ack_delay(None);
                ss.set_nagle_enabled(false);
                let listen_ok = match ss.listen(dst_port) {
                    Ok(()) => true,
                    Err(error) => {
                        setup_error.get_or_insert_with(|| {
                            format!("listen failed for slot {slot}: {error:?}")
                        });
                        false
                    }
                };
                cs.set_ack_delay(None);
                cs.set_nagle_enabled(false);
                let connect_ok = if listen_ok {
                    match cs.connect(
                        client.iface.context(),
                        (IpAddress::v4(10, 0, 0, 1), dst_port),
                        src_port,
                    ) {
                        Ok(()) => true,
                        Err(error) => {
                            setup_error.get_or_insert_with(|| {
                                format!("connect failed for slot {slot}: {error:?}")
                            });
                            false
                        }
                    }
                } else {
                    false
                };
                if listen_ok && connect_ok {
                    opened += 1;
                } else {
                    ss.abort();
                    cs.abort();
                }
                next_open_us = next_open_us
                    .checked_add(interval_us)
                    .ok_or_else(|| "churn: connection schedule overflowed".to_owned())?;
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
        client.poll(now, link);
        server.poll(now, link);

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
        server.poll(now, link);
        client.poll(now, link);
        if mode.sample_memory() {
            mem_trace.maybe_sample(250);
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    let allocator_peak = allocator_phase.finish();
    let memory_end = process_memory_sample();
    let alloc_after = alloc_counters_with_memory(memory_end);
    let conn_rate = opened as f64 / elapsed;
    let close_rate = closed as f64 / elapsed;

    // Two pool readings with different roles. At the deadline, slots can
    // legitimately hold charge: connections mid-lifecycle, plus sockets
    // whose peer-side abort left undrained rx — which stays readable (and
    // charged) until the slot recycles, per the `may_recv`-after-Closed
    // contract. That value is a bounded diagnostic. The *leak gate* is the
    // post-teardown reading: dropping the sockets must refund every byte.
    let pool_at_deadline = pool.used();
    let lane_stats = link.stats();
    drop(endpoints);
    let pool_after_teardown = pool.used();
    let memory_report = DynamicMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        allocator_peak,
        PoolUsage {
            start: pool_used_start,
            end: pool_at_deadline,
            budget: pool_bytes,
            after_teardown: pool_after_teardown,
        },
    )
    .map_err(|error| format!("churn: {error}"))?;

    println!("\n========== shape: churn ==========");
    println!("  target rate:            {} conn/s", target_conn_per_sec);
    println!("  slot ring size:         {SLOTS}");
    println!("  elapsed:                {elapsed:.3}s");
    println!("  opened:                 {opened}   ({conn_rate:.1} conn/s)");
    println!("  closed:                 {closed}   ({close_rate:.1} conn/s)");
    println!("  app bytes xfer:         {bytes_xferred}");
    println!("  pool used active start: {} KiB", pool_used_start / 1024);
    println!(
        "  pool used at deadline:  {} KiB  (in-flight + retained rx; bounded)",
        pool_at_deadline / 1024
    );
    println!(
        "  pool used (end):        {} KiB  (after teardown; leak gate, expect 0)",
        pool_after_teardown / 1024
    );
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    memory_report.print();
    mem_trace.print();
    print_lane_stats("churn", lane_stats);
    if let Some(error) = setup_error {
        return Err(format!("churn: {error}"));
    }
    validate_nonzero_counters("churn", &[("transferred bytes", bytes_xferred)])?;
    validate_churn_rate(target_conn_per_sec, elapsed, opened, closed)
}

/// Mixed idle + active shape. Creates `n_idle` TCP sockets that never see
/// data and `n_active` TCP sockets that run a steady-state echo workload.
/// All share one [`tcp::MemoryPool`]. The point is to verify that lazy
/// allocation keeps idle-flow memory at ~0 while active flows carry traffic.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_idle_hot(
    seconds: u64,
    n_idle: usize,
    n_active: usize,
    offload: bool,
    mode: RunMode,
) -> Result<(), String> {
    const MAX_BUF: u32 = 32 * 1024;
    const PAYLOAD: usize = 1024;
    let duration = checked_run_duration("idle_hot", seconds)?;
    let total = n_idle
        .checked_add(n_active)
        .ok_or_else(|| "idle_hot: total flow count overflowed".to_owned())?;
    validate_unique_flow_count("idle_hot", total)?;
    let qd = idle_hot_queue_depth(n_active)?;
    let active_socket_count = n_active
        .checked_mul(2)
        .ok_or_else(|| "idle_hot: active socket count overflowed".to_owned())?;
    let expected_steady_bytes = active_socket_count
        .checked_mul(2)
        .and_then(|buffers| buffers.checked_mul(MAX_BUF as usize))
        .ok_or_else(|| "idle_hot: steady pool budget overflowed".to_owned())?;
    let pool_bytes: usize = expected_steady_bytes
        .checked_add(2 * MAX_BUF as usize)
        .ok_or_else(|| "idle_hot: pool budget overflowed".to_owned())?
        .max(2 * MAX_BUF as usize);
    let pool = tcp::MemoryPool::new(pool_bytes);

    let mut endpoints = setup_paired_endpoints([10, 0, 0], 1500, qd, offload);
    let PairedEndpoints {
        server,
        client,
        link,
    } = &mut endpoints;

    // Active flows: open & connect.
    let mut srv_active: Vec<smoltcp::iface::SocketHandle> = Vec::with_capacity(n_active);
    let mut cli_active: Vec<smoltcp::iface::SocketHandle> = Vec::with_capacity(n_active);
    let mut setup_error = None;
    for i in 0..n_active {
        let h_srv = add_tcp_socket_dyn(server, MAX_BUF, &pool);
        let h_cli = add_tcp_socket_dyn(client, MAX_BUF, &pool);
        let (dst_port, src_port) = flow_ports("idle_hot", i)?;
        {
            let s = server.sockets.get_mut::<tcp::Socket>(h_srv);
            s.set_ack_delay(None);
            s.set_nagle_enabled(false);
            if let Err(error) = s.listen(dst_port) {
                setup_error.get_or_insert_with(|| format!("listen failed for flow {i}: {error:?}"));
            }
        }
        {
            let c = client.sockets.get_mut::<tcp::Socket>(h_cli);
            c.set_ack_delay(None);
            c.set_nagle_enabled(false);
        }
        if let Err(error) = client.sockets.get_mut::<tcp::Socket>(h_cli).connect(
            client.iface.context(),
            (IpAddress::v4(10, 0, 0, 1), dst_port),
            src_port,
        ) {
            setup_error.get_or_insert_with(|| format!("connect failed for flow {i}: {error:?}"));
        }
        srv_active.push(h_srv);
        cli_active.push(h_cli);
    }

    // Idle flows: create sockets, do not connect — they sit in Closed state
    // and exercise the dyn-buffer footprint that idle sockets pay for.
    for _ in 0..n_idle {
        let _ = add_tcp_socket_dyn(server, MAX_BUF, &pool);
        let _ = add_tcp_socket_dyn(client, MAX_BUF, &pool);
    }

    let memory_after_create = process_memory_bytes();
    let pool_after_create = pool.used();

    let clock_start = StdInstant::now();
    let smol_now = |w0: StdInstant| Instant::from_micros(w0.elapsed().as_micros() as i64);

    // Establish active flows.
    let connect_deadline =
        checked_deadline("idle_hot", clock_start, Duration::from_secs(seconds.min(5)))?;
    loop {
        let now = smol_now(clock_start);
        server.poll(now, link);
        client.poll(now, link);
        let ready = cli_active.iter().zip(srv_active.iter()).all(|(&hc, &hs)| {
            matches!(
                client.sockets.get::<tcp::Socket>(hc).state(),
                tcp::State::Established
            ) && matches!(
                server.sockets.get::<tcp::Socket>(hs).state(),
                tcp::State::Established
            )
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
    let established = cli_active
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

    // Steady-state echo on active flows only.
    let mut sent: u64 = 0;
    let mut received_per_flow = vec![0u64; n_active];
    let payload = vec![0xa5u8; PAYLOAD];
    let mut sink = vec![0u8; PAYLOAD];
    let mut mem_trace = MemTrace::start(mode);
    let pool_used_start = pool.used();
    let memory_start = process_memory_sample();
    let alloc_before = alloc_counters_with_memory(memory_start);
    let allocator_phase = ALLOCATOR_TELEMETRY
        .begin()
        .map_err(|error| format!("idle_hot: {error}"))?;
    let steady_start = StdInstant::now();
    let deadline = checked_deadline("idle_hot", steady_start, duration)?;
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
        client.poll(now, link);
        server.poll(now, link);
        for (flow, &h) in srv_active.iter().enumerate() {
            let s = server.sockets.get_mut::<tcp::Socket>(h);
            while s.can_recv() {
                match s.recv_slice(&mut sink) {
                    Ok(r) if r > 0 => {
                        received_per_flow[flow] += r as u64;
                        if s.can_send() {
                            let _ = s.send_slice(&sink[..r]);
                        }
                    }
                    _ => break,
                }
            }
        }
        server.poll(now, link);
        client.poll(now, link);
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
    let allocator_peak = allocator_phase.finish();
    let memory_end = process_memory_sample();
    let alloc_after = alloc_counters_with_memory(memory_end);
    let pool_steady = pool.used();
    let lane_stats = link.stats();
    drop(endpoints);
    let pool_after_teardown = pool.used();
    let memory_report = DynamicMemoryReport::from_snapshots(
        alloc_before,
        alloc_after,
        allocator_peak,
        PoolUsage {
            start: pool_used_start,
            end: pool_steady,
            budget: pool_bytes,
            after_teardown: pool_after_teardown,
        },
    )
    .map_err(|error| format!("idle_hot: {error}"))?;
    let received = Fairness::from(&received_per_flow);
    let recvd = received.total;
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
    let metric = process_memory_label();
    println!(
        "  {metric} post-create:   {} KiB",
        memory_after_create / 1024
    );
    println!(
        "  pool used post-create:  {} KiB  (expect ~0)",
        pool_after_create / 1024
    );
    println!("  elapsed:                {elapsed:.3}s");
    println!("  app sent / recvd:       {} / {}", sent, recvd);
    println!("  active throughput:      {gbps:.3} Gbps");
    println!("  pool used active start: {} KiB", pool_used_start / 1024);
    println!("  pool used active end:   {} KiB", pool_steady / 1024);
    println!(
        "  pool used after teardown: {} KiB",
        pool_after_teardown / 1024
    );
    println!();
    memory_report.print();
    println!(
        "  expected: idle pool charge ~= 0 KiB; steady upper bound is {} KiB (active client/server sockets x rx/tx max)",
        expected_steady_bytes / 1024
    );
    if n_active > 0 {
        received.print("active received");
    }
    mem_trace.print();
    print_lane_stats("idle_hot", lane_stats);
    if let Some(error) = setup_error {
        return Err(format!("idle_hot: {error}"));
    }
    if established != n_active {
        return Err(format!(
            "idle_hot: only {established}/{n_active} active flows established"
        ));
    }
    if n_active > 0 {
        validate_nonzero_counters(
            "idle_hot",
            &[("sent bytes", sent), ("received bytes", recvd)],
        )?;
        validate_flow_stats("idle_hot", &received)?;
    }
    if pool_after_create != 0 {
        return Err(format!(
            "idle_hot: post-create pool use was {pool_after_create}, expected 0"
        ));
    }
    if n_active == 0 && (pool_used_start != 0 || pool_steady != 0) {
        return Err(format!(
            "idle_hot: idle-only pool use was {pool_used_start} -> {pool_steady}, expected 0 -> 0"
        ));
    }
    if n_active > 0 {
        validate_pool_growth("idle_hot", pool_used_start, pool_steady)?;
        if pool_steady > expected_steady_bytes {
            return Err(format!(
                "idle_hot: steady pool use {pool_steady} exceeded active socket maximum {expected_steady_bytes}"
            ));
        }
    }
    Ok(())
}

fn run_config(config: Config) -> Result<(), String> {
    println!(
        "config: mode={} | {} checksums ({}{})",
        config.mode.label(),
        if config.offload_checksums {
            "device-offloaded"
        } else {
            "full software"
        },
        if config.offload_checksums {
            "mimics a hardware NIC"
        } else {
            "worst case"
        },
        match config.shape.flow_count() {
            Some(n) => format!(", {n} flows"),
            None => String::new(),
        }
    );

    let seconds = config.seconds.get();
    let offload = config.offload_checksums;
    let mode = config.mode;
    match config.shape {
        TrafficShape::Firehose => shape_firehose(seconds, offload),
        TrafficShape::PingPong => shape_pingpong(seconds, offload),
        TrafficShape::Small => shape_small(seconds, offload),
        TrafficShape::Udp => shape_udp_firehose(seconds, offload),
        TrafficShape::All => {
            let udp = shape_udp_firehose(seconds, offload);
            let small = shape_small(seconds, offload);
            let pingpong = shape_pingpong(seconds, offload);
            udp.and(small).and(pingpong)
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
        } => shape_multi_tcp(seconds, threads.get(), flows_per_thread.get(), offload),
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::MultiTcpSink {
            threads,
            flows_per_thread,
        } => shape_multi_tcp_sink(seconds, threads.get(), flows_per_thread.get(), offload),
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::Churn { rate } => shape_churn(seconds, rate.get(), offload, mode),
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::RstUnreadRx { flows } => {
            shape_rst_unread_rx(seconds, flows.get(), offload, mode)
        }
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::PoolPressure { flows } => {
            shape_pool_pressure(seconds, flows.get(), offload, mode)
        }
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::MixedTcpUdp {
            tcp_flows,
            udp_flows,
        } => shape_mixed_tcp_udp(seconds, tcp_flows.get(), udp_flows.get(), offload, mode),
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        TrafficShape::IdleHot { idle, active } => {
            shape_idle_hot(seconds, idle, active, offload, mode)
        }
    }
}

fn main() -> ExitCode {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::builder()
        .file_name("dhat-heap.json")
        .build();
    let config = match parse_args(env::args().skip(1)) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };

    match run_config(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
