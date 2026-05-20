//! In-process smoltcp throughput / latency profiler.
//!
//! Wires two `Interface`s back-to-back through a pair of in-memory packet
//! queues (no tun/tap, no syscalls per packet), then drives different
//! traffic shapes through them and reports a comprehensive metrics block:
//!
//!   * throughput (Gbps app, Gbps wire, Mpps)
//!   * per-poll latency: mean / p50 / p90 / p99 / max
//!   * allocation count + bytes allocated (instrumented allocator)
//!   * RSS peak (from /proc/self/status)
//!   * smoltcp Socket footprint of relevant sockets
//!   * `cycles_estimate` per packet from a 2.4 GHz reference
//!
//! Designed to run under `perf record`, `valgrind --tool=massif`, or
//! `heaptrack` with no external setup.
//!
//! Usage:
//!   cargo run --release --example profile_loopback -- [shape] [seconds] [opts...]
//!
//! Shapes:
//!   udp           - 1400B UDP packet forwarding (default; tunnel analogue)
//!   small         - many small TCP segments, measures per-packet overhead
//!   pingpong      - 128B request/response, latency-bound
//!   firehose      - one-way TCP bulk transfer (cwnd-limited)
//!   many_tcp      - N concurrent TCP echo flows; verifies per-flow fairness +
//!                   memory growth bounds. Usage:
//!                     profile_loopback many_tcp 5 200 [offload]
//!   many_udp      - N concurrent UDP flows; same fairness + memory metrics.
//!                     profile_loopback many_udp 5 200 [offload]
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
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant as StdInstant;

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

/// Read VmRSS (kB) from /proc/self/status, returning bytes. Returns 0 on
/// non-Linux platforms.
fn rss_bytes() -> u64 {
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

/// Read voluntary + involuntary context-switch counts from
/// /proc/self/status. Returns `(voluntary, nonvoluntary)`.
/// Voluntary = process blocked / yielded (rare in our spin loops);
/// nonvoluntary = preempted by the scheduler. Multi-thread shapes
/// expect a small voluntary count and a nonvoluntary count proportional
/// to N_threads × wall_time / time_slice.
fn ctxsw_counts() -> (u64, u64) {
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
        if self.samples == 0 {
            0
        } else {
            self.sum_ns / self.samples
        }
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
        println!("    {:>8}   {:>10}   {:>10}", "t_ms", "rss_bytes", "alloc_delta");
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

use smoltcp::iface::{Config, Interface, SocketSet};
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

/// One direction of the paired link.
///
/// `queue` holds packets in flight (FIFO). `pool` holds empty buffers we
/// rotate through, so steady-state runs do zero allocations.
struct Lane {
    queue: VecDeque<Packet>,
    pool: Vec<Packet>,
}

impl Lane {
    fn new(mtu: usize, depth: usize) -> Self {
        let mut pool = Vec::with_capacity(depth);
        for _ in 0..depth {
            pool.push(Packet::with_capacity(mtu));
        }
        Self {
            queue: VecDeque::with_capacity(depth),
            pool,
        }
    }

    /// Borrow an empty buffer (allocating only if the pool runs dry).
    fn take_pkt(&mut self, mtu: usize) -> Packet {
        self.pool.pop().unwrap_or_else(|| Packet::with_capacity(mtu))
    }

    fn return_pkt(&mut self, mut pkt: Packet) {
        pkt.len = 0;
        self.pool.push(pkt);
    }
}

type LaneRc = Rc<RefCell<Lane>>;

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
        let pkt = self.rx.borrow_mut().queue.pop_front()?;
        Some((
            PairedRx {
                pkt: Some(pkt),
                rx: &self.rx,
            },
            PairedTx {
                tx: &self.tx,
                mtu: self.mtu,
                tx_bytes: &mut self.tx_bytes,
                tx_packets: &mut self.tx_packets,
            },
        ))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        Some(PairedTx {
            tx: &self.tx,
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
        let pkt = self.pkt.take().unwrap();
        let r = f(&pkt.buf[..pkt.len]);
        self.rx.borrow_mut().return_pkt(pkt);
        r
    }
}

struct PairedTx<'a> {
    tx: &'a LaneRc,
    mtu: usize,
    tx_bytes: &'a mut u64,
    tx_packets: &'a mut u64,
}

impl<'a> phy::TxToken for PairedTx<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut pkt = self.tx.borrow_mut().take_pkt(self.mtu);
        // Grow on-demand only if a caller asks for more than MTU (shouldn't happen).
        if pkt.buf.len() < len {
            pkt.buf.resize(len, 0);
        }
        let r = f(&mut pkt.buf[..len]);
        pkt.len = len;
        *self.tx_bytes += len as u64;
        *self.tx_packets += 1;
        self.tx.borrow_mut().queue.push_back(pkt);
        r
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
    let mut config = Config::new(HardwareAddress::Ip);
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
        println!(
            "  packet rate:            {mpps:>8.3} Mpps     (avg {avg_pkt:.1} bytes/pkt)"
        );
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
        let cpu_ns = self.alloc_after.cpu_ns.saturating_sub(self.alloc_before.cpu_ns);
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
        server.iface.poll(n, &mut server.device, &mut server.sockets);
        client.iface.poll(n, &mut client.device, &mut client.sockets);
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

        if sent_this_round == 0 && recvd_this_round == 0
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
}

fn shape_small(seconds: u64, offload: bool) {
    // Force tiny segments by limiting the socket buffer; with a 1500 MTU the
    // client never fills more than a single small write at a time.
    const BUF: usize = 4 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(IpAddress::v4(10, 0, 0, 1), 1500, lane_a.clone(), lane_b.clone(), offload);
    let mut client = make_endpoint(IpAddress::v4(10, 0, 0, 2), 1500, lane_b.clone(), lane_a.clone(), offload);

    let srv_h = add_tcp_socket(&mut server, BUF);
    let cli_h = add_tcp_socket(&mut client, BUF);

    server.sockets.get_mut::<tcp::Socket>(srv_h).listen(1234).unwrap();
    client
        .sockets
        .get_mut::<tcp::Socket>(cli_h)
        .connect(client.iface.context(), (IpAddress::v4(10, 0, 0, 1), 1234), 49152)
        .unwrap();

    let mut t_ms: i64 = 0;
    for _ in 0..200 {
        let n = Instant::from_millis(t_ms);
        server.iface.poll(n, &mut server.device, &mut server.sockets);
        client.iface.poll(n, &mut client.device, &mut client.sockets);
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
                && w > 0 {
                    sent += w as u64;
                }
        poll_lat.measure(|| {
            client.iface.poll(n, &mut client.device, &mut client.sockets);
            server.iface.poll(n, &mut server.device, &mut server.sockets);
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
}

fn shape_pingpong(seconds: u64, offload: bool) {
    const BUF: usize = 16 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(IpAddress::v4(10, 0, 0, 1), 1500, lane_a.clone(), lane_b.clone(), offload);
    let mut client = make_endpoint(IpAddress::v4(10, 0, 0, 2), 1500, lane_b.clone(), lane_a.clone(), offload);

    let srv_h = add_tcp_socket(&mut server, BUF);
    let cli_h = add_tcp_socket(&mut client, BUF);

    server.sockets.get_mut::<tcp::Socket>(srv_h).listen(1234).unwrap();
    client
        .sockets
        .get_mut::<tcp::Socket>(cli_h)
        .connect(client.iface.context(), (IpAddress::v4(10, 0, 0, 1), 1234), 49152)
        .unwrap();

    let mut t_ms: i64 = 0;
    for _ in 0..200 {
        let n = Instant::from_millis(t_ms);
        server.iface.poll(n, &mut server.device, &mut server.sockets);
        client.iface.poll(n, &mut client.device, &mut client.sockets);
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
            client.iface.poll(n, &mut client.device, &mut client.sockets);
            server.iface.poll(n, &mut server.device, &mut server.sockets);
        });

        // Server echoes.
        let ss = server.sockets.get_mut::<tcp::Socket>(srv_h);
        let mut sink = [0u8; 128];
        if ss.can_recv()
            && let Ok(r) = ss.recv_slice(&mut sink)
                && r > 0 && ss.can_send() {
                    let _ = ss.send_slice(&sink[..r]);
                }
        poll_lat.measure(|| {
            server.iface.poll(n, &mut server.device, &mut server.sockets);
            client.iface.poll(n, &mut client.device, &mut client.sockets);
        });

        // Client receives echo.
        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        if cs.can_recv()
            && let Ok(r) = cs.recv_slice(&mut sink)
                && r > 0 {
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
}

fn shape_udp_firehose(seconds: u64, offload: bool) {
    // Pure packet forwarding — no flow control, no cwnd. This is the closest
    // analogue to a tunnel forwarding fully-formed packets between two peers
    // (which is what tunnel-lib-rust wraps smoltcp for).
    const PAYLOAD: usize = 1400;
    const META_SLOTS: usize = 256;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(IpAddress::v4(10, 0, 0, 1), 1500, lane_a.clone(), lane_b.clone(), offload);
    let mut client = make_endpoint(IpAddress::v4(10, 0, 0, 2), 1500, lane_b.clone(), lane_a.clone(), offload);

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

    server.sockets.get_mut::<udp::Socket>(srv_h).bind(2000).unwrap();
    client.sockets.get_mut::<udp::Socket>(cli_h).bind(2001).unwrap();

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
            client.iface.poll(n, &mut client.device, &mut client.sockets);
            server.iface.poll(n, &mut server.device, &mut server.sockets);
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
}

/// `n` concurrent TCP echo flows between two smoltcp endpoints. Each flow has
/// its own (src_port, dst_port) tuple so the stack treats them independently.
///
/// Verifies two properties:
///   * memory stays bounded (RSS trace + net heap delta)
///   * no flow is starved (Jain index + per-flow percentiles)
fn shape_many_tcp(seconds: u64, n: usize, offload: bool) {
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
        server.iface.poll(now, &mut server.device, &mut server.sockets);
        client.iface.poll(now, &mut client.device, &mut client.sockets);
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
                && let Ok(w) = cs.send_slice(&payload) {
                    sent[i] += w as u64;
                }
        }

        poll_lat.measure(|| {
            client.iface.poll(now, &mut client.device, &mut client.sockets);
            server.iface.poll(now, &mut server.device, &mut server.sockets);
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
            server.iface.poll(now, &mut server.device, &mut server.sockets);
            client.iface.poll(now, &mut client.device, &mut client.sockets);
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
        mem_trace.maybe_sample(250);
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

/// `n` concurrent UDP echo flows. Same metrics as `many_tcp`. UDP has no
/// flow control or cwnd so per-flow throughput is bounded only by the rate
/// at which the runner pumps bytes through.
fn shape_many_udp(seconds: u64, n: usize, offload: bool) {
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
            client.iface.poll(now, &mut client.device, &mut client.sockets);
            server.iface.poll(now, &mut server.device, &mut server.sockets);
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
        mem_trace.maybe_sample(250);
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

/// Multi-Interface pool-contention shape. Spawns `n_threads` threads, each
/// owning its own server/client `Interface` pair and `flows_per_thread`
/// TCP echo flows, all sharing a single [`tcp::MemoryPool`]. Measures the
/// aggregate throughput scaling and serves as a regression gate against
/// pool-counter cache-line / CAS-retry contention.
///
/// Each thread runs the same workload as `many_tcp` but with sockets
/// created via `new_dynamic` so the pool is exercised. Threads start
/// in lockstep via a barrier so the contention window aligns.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_multi_tcp(seconds: u64, n_threads: usize, flows_per_thread: usize, offload: bool) {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Instant as StdInstant;

    const MAX_BUF: u32 = 32 * 1024;
    const PAYLOAD: usize = 1024;
    let total_flows = n_threads * flows_per_thread;
    // Pool sized to hold every flow's full max — exercises CAS contention
    // without forcing refusal (which would obscure the perf signal).
    let pool_bytes: usize = total_flows * 2 * MAX_BUF as usize;
    let pool = tcp::MemoryPool::new(pool_bytes);

    let barrier = Arc::new(Barrier::new(n_threads));
    let (vol_before, nvol_before) = ctxsw_counts();
    let start_wall = StdInstant::now();

    let mut handles: Vec<thread::JoinHandle<(u64, u64, u64)>> = Vec::with_capacity(n_threads);
    for tid in 0..n_threads {
        let pool = pool.clone();
        let barrier = barrier.clone();
        handles.push(thread::spawn(move || {
            let qd = (flows_per_thread * 16).clamp(1024, 16384);
            let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
            let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
            // Use distinct addresses per thread so server/client tuples don't
            // clash if anything inspects them (they won't — lanes are isolated).
            let base = 10 + tid as u8;
            let mut server = make_endpoint(
                IpAddress::v4(base, 0, 0, 1),
                1500,
                lane_a.clone(),
                lane_b.clone(),
                offload,
            );
            let mut client = make_endpoint(
                IpAddress::v4(base, 0, 0, 2),
                1500,
                lane_b.clone(),
                lane_a.clone(),
                offload,
            );

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
                server.iface.poll(now, &mut server.device, &mut server.sockets);
                client.iface.poll(now, &mut client.device, &mut client.sockets);
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

            // Synchronize the steady-state start across threads.
            barrier.wait();

            let mut sent: u64 = 0;
            let mut recvd: u64 = 0;
            let payload = vec![0xa5u8; PAYLOAD];
            let mut sink = vec![0u8; PAYLOAD];
            let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
            while StdInstant::now() < deadline {
                let now = smol_now(w0);
                // Client: send chunks. Don't gate on can_send — for dynamic
                // buffers, tx_buffer starts at 0 capacity and grows inside
                // send_impl; can_send() returns false until the first grow.
                for &h in &cli_handles {
                    let s = client.sockets.get_mut::<tcp::Socket>(h);
                    if s.may_send()
                        && let Ok(n) = s.send_slice(&payload)
                    {
                        sent += n as u64;
                    }
                }
                // Push client TX onto wire, let server receive.
                client.iface.poll(now, &mut client.device, &mut client.sockets);
                server.iface.poll(now, &mut server.device, &mut server.sockets);
                // Server: drain and echo so the client's tx_buffer frees up
                // (otherwise it stalls at the dynamic-buffer cap).
                for &h in &srv_handles {
                    let s = server.sockets.get_mut::<tcp::Socket>(h);
                    while s.can_recv() {
                        match s.recv_slice(&mut sink) {
                            Ok(r) if r > 0 => {
                                recvd += r as u64;
                                if s.may_send() {
                                    let _ = s.send_slice(&sink[..r]);
                                }
                            }
                            _ => break,
                        }
                    }
                }
                // Push server ACKs / echo back to client.
                server.iface.poll(now, &mut server.device, &mut server.sockets);
                client.iface.poll(now, &mut client.device, &mut client.sockets);
                // Drain client RX (we don't count it; the recvd above
                // measures the server-side bytes which is the workload signal).
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
            let elapsed_us = w0.elapsed().as_micros() as u64;
            (sent, recvd, elapsed_us)
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let total_secs = start_wall.elapsed().as_secs_f64();
    let (vol_after, nvol_after) = ctxsw_counts();
    let vol_delta = vol_after - vol_before;
    let nvol_delta = nvol_after - nvol_before;
    let agg_sent: u64 = results.iter().map(|(s, _, _)| s).sum();
    let agg_recvd: u64 = results.iter().map(|(_, r, _)| r).sum();
    let agg_gbps = (agg_recvd as f64 * 8.0) / total_secs / 1e9;
    let per_thread_gbps: Vec<f64> = results
        .iter()
        .map(|(_, r, _)| (*r as f64 * 8.0) / total_secs / 1e9)
        .collect();
    let min = per_thread_gbps.iter().cloned().fold(f64::INFINITY, f64::min);
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

    println!("\n========== shape: multi_tcp ==========");
    println!(
        "  threads:                {n_threads}   flows/thread: {flows_per_thread}   total flows: {total_flows}"
    );
    println!(
        "  pool budget:            {} KiB    pool used (end): {} KiB",
        pool_bytes / 1024,
        pool.used() / 1024,
    );
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
}

/// Connection-churn shape. Repeatedly opens and tears down TCP flows at
/// `target_conn_per_sec`, each exchanging one short payload before close.
/// Exercises the release path (`set_state(Closed)`, `reset()`, `Drop`)
/// under load; verifies that pool refunds keep up with admissions and
/// the connection cap doesn't drift.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_churn(seconds: u64, target_conn_per_sec: usize, offload: bool) {
    use std::time::Instant as StdInstant;
    const MAX_BUF: u32 = 32 * 1024;
    const SLOTS: usize = 256;
    const PAYLOAD: usize = 128;

    let qd = (SLOTS * 16).clamp(1024, 16384);
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
    let pool_bytes: usize = SLOTS * 2 * MAX_BUF as usize;
    let pool = tcp::MemoryPool::new(pool_bytes);

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

    // Pre-allocate a ring of socket handles. Each "churn slot" is a pair
    // we cycle through; once a pair is fully torn down we recycle the slot.
    let mut slots: Vec<(smoltcp::iface::SocketHandle, smoltcp::iface::SocketHandle, u16)> =
        Vec::with_capacity(SLOTS);
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

        // Client: push payload on every Established flow (no can_send gate
        // — dynamic tx buffer grows on first send_impl call).
        for &(_h_srv, h_cli, _) in &slots {
            let cs = client.sockets.get_mut::<tcp::Socket>(h_cli);
            if cs.may_send() {
                let _ = cs.send_slice(&payload);
            }
        }
        client.iface.poll(now, &mut client.device, &mut client.sockets);
        server.iface.poll(now, &mut server.device, &mut server.sockets);

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
        server.iface.poll(now, &mut server.device, &mut server.sockets);
        client.iface.poll(now, &mut client.device, &mut client.sockets);
        mem_trace.maybe_sample(250);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let alloc_after = AllocSnap::now();
    let conn_rate = opened as f64 / elapsed;
    let close_rate = closed as f64 / elapsed;
    let alloc_bytes = alloc_after.alloc_bytes - alloc_before.alloc_bytes;
    let free_bytes = alloc_after.free_bytes - alloc_before.free_bytes;
    let alloc_count = alloc_after.alloc_count - alloc_before.alloc_count;

    println!("\n========== shape: churn ==========");
    println!("  target rate:            {} conn/s", target_conn_per_sec);
    println!("  slot ring size:         {SLOTS}");
    println!("  elapsed:                {elapsed:.3}s");
    println!("  opened:                 {opened}   ({conn_rate:.1} conn/s)");
    println!("  closed:                 {closed}   ({close_rate:.1} conn/s)");
    println!("  app bytes xfer:         {bytes_xferred}");
    println!("  pool used (end):        {} KiB", pool.used() / 1024);
    println!("  pool budget:            {} KiB", pool_bytes / 1024);
    println!("  bytes allocated:        {alloc_bytes}");
    println!("  bytes freed:            {free_bytes}");
    println!("  net heap delta:         {}", alloc_bytes as i64 - free_bytes as i64);
    println!("  allocation count:       {alloc_count}");
    mem_trace.print();
}

/// Mixed idle + active shape. Creates `n_idle` TCP sockets that never see
/// data and `n_active` TCP sockets that run a steady-state echo workload.
/// All share one [`tcp::MemoryPool`]. The point is to verify that lazy
/// allocation keeps idle-flow memory at ~0 while active flows still hit
/// full throughput.
#[cfg(feature = "socket-tcp-dynamic-buffer")]
fn shape_idle_hot(seconds: u64, n_idle: usize, n_active: usize, offload: bool) {
    use std::time::Instant as StdInstant;

    const MAX_BUF: u32 = 32 * 1024;
    const PAYLOAD: usize = 1024;
    let total = n_idle + n_active;
    let qd = (total * 16).clamp(1024, 16384);
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, qd)));
    let pool_bytes: usize = (n_active * 2 + 1) * MAX_BUF as usize * 2;
    let pool = tcp::MemoryPool::new(pool_bytes);

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

    let alloc_before = AllocSnap::now();
    let start = StdInstant::now();
    let mut mem_trace = MemTrace::start();
    let smol_now = |w0: StdInstant| Instant::from_micros(w0.elapsed().as_micros() as i64);

    // Establish active flows.
    let connect_deadline = start + std::time::Duration::from_secs(seconds.min(5));
    loop {
        let now = smol_now(start);
        server.iface.poll(now, &mut server.device, &mut server.sockets);
        client.iface.poll(now, &mut client.device, &mut client.sockets);
        let ready = cli_active
            .iter()
            .zip(srv_active.iter())
            .all(|(&hc, &hs)| {
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
    let deadline = start + std::time::Duration::from_secs(seconds);
    while StdInstant::now() < deadline {
        let now = smol_now(start);
        for &h in &cli_active {
            let s = client.sockets.get_mut::<tcp::Socket>(h);
            if s.may_send()
                && let Ok(n) = s.send_slice(&payload)
            {
                sent += n as u64;
            }
        }
        client.iface.poll(now, &mut client.device, &mut client.sockets);
        server.iface.poll(now, &mut server.device, &mut server.sockets);
        for &h in &srv_active {
            let s = server.sockets.get_mut::<tcp::Socket>(h);
            while s.can_recv() {
                match s.recv_slice(&mut sink) {
                    Ok(r) if r > 0 => {
                        recvd += r as u64;
                        if s.may_send() {
                            let _ = s.send_slice(&sink[..r]);
                        }
                    }
                    _ => break,
                }
            }
        }
        server.iface.poll(now, &mut server.device, &mut server.sockets);
        client.iface.poll(now, &mut client.device, &mut client.sockets);
        for &h in &cli_active {
            let s = client.sockets.get_mut::<tcp::Socket>(h);
            while s.can_recv() {
                if s.recv_slice(&mut sink).map(|r| r > 0).unwrap_or(false) {
                    continue;
                }
                break;
            }
        }
        mem_trace.maybe_sample(250);
    }
    let elapsed = start.elapsed().as_secs_f64();
    let alloc_after = AllocSnap::now();
    let pool_steady = pool.used();
    let rss_end = rss_bytes();
    let gbps = (recvd as f64 * 8.0) / elapsed / 1e9;

    println!("\n========== shape: idle_hot ==========");
    println!("  idle flows:             {n_idle}");
    println!("  active flows:           {n_active}");
    println!(
        "  per-flow max budget:    {} KiB (rx) + {} KiB (tx)",
        MAX_BUF / 1024,
        MAX_BUF / 1024,
    );
    println!(
        "  pool budget:            {} KiB",
        pool_bytes / 1024
    );
    println!("  RSS post-create:        {} KiB", rss_after_create / 1024);
    println!("  pool used post-create:  {} KiB  (expect ~0)", pool_after_create / 1024);
    println!("  elapsed:                {elapsed:.3}s");
    println!("  app sent / recvd:       {} / {}", sent, recvd);
    println!("  active throughput:      {gbps:.3} Gbps");
    println!("  pool used steady:       {} KiB", pool_steady / 1024);
    println!("  RSS end:                {} KiB", rss_end / 1024);
    let alloc_bytes = alloc_after.alloc_bytes - alloc_before.alloc_bytes;
    let free_bytes = alloc_after.free_bytes - alloc_before.free_bytes;
    let alloc_count = alloc_after.alloc_count - alloc_before.alloc_count;
    println!("  bytes allocated:        {alloc_bytes}");
    println!("  net heap delta:         {}", alloc_bytes as i64 - free_bytes as i64);
    println!("  allocation count:       {alloc_count}");
    println!(
        "  expected: idle pool charge ≈ 0 KiB; steady should equal {} KiB (active flows × per-flow grown peak)",
        (n_active * 2 * MAX_BUF as usize) / 1024
    );
    mem_trace.print();
}

fn print_socket_sizes() {
    use core::mem::size_of;
    use smoltcp::socket;
    use smoltcp::storage::*;
    println!("\n========== smoltcp footprint (bytes) ==========");
    println!("  TCP socket:             {:>6}", size_of::<socket::tcp::Socket>());
    println!("  UDP socket:             {:>6}", size_of::<socket::udp::Socket>());
    println!("  ICMP socket:            {:>6}", size_of::<socket::icmp::Socket>());
    println!("  Raw socket:             {:>6}", size_of::<socket::raw::Socket>());
    println!("  RingBuffer<u8>:         {:>6}", size_of::<RingBuffer<u8>>());
    println!("  Assembler:              {:>6}", size_of::<Assembler>());
    println!(
        "  IpRepr / TcpRepr:       {:>3} / {:>3}",
        size_of::<smoltcp::wire::IpRepr>(),
        size_of::<smoltcp::wire::TcpRepr>()
    );
}

fn main() {
    // Args:
    //   <shape> [seconds] [offload]                     for single-flow shapes
    //   many_tcp|many_udp [seconds] [n_flows] [offload] for many-flow shapes
    //
    //   offload: "offload" | "1" | "true" -> Device advertises checksum
    //            offload (mimics a hardware NIC or iOS NEPacketTunnelFlow).
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::builder().file_name("dhat-heap.json").build();
    let args: Vec<String> = env::args().collect();
    let shape = args.get(1).map(String::as_str).unwrap_or("all");
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);

    let is_offload = |s: Option<&str>| matches!(s, Some("offload") | Some("1") | Some("true"));

    // Many-flow shapes interpret args[3] as the flow count and args[4] as the
    // offload flag; single-flow shapes interpret args[3] directly as offload.
    // Pool-aware shapes (multi_tcp, churn, idle_hot) use args[3]/args[4]
    // for their own knobs; offload is interpreted positionally per shape.
    let (n_flows, offload) = if shape.starts_with("many_") {
        let n: usize = args
            .get(3)
            .and_then(|s| s.parse().ok())
            .unwrap_or(100)
            .max(1);
        (Some(n), is_offload(args.get(4).map(String::as_str)))
    } else if matches!(shape, "multi_tcp" | "churn" | "idle_hot") {
        // For these shapes, offload is the 5th arg position.
        (None, is_offload(args.get(5).map(String::as_str)))
    } else {
        (None, is_offload(args.get(3).map(String::as_str)))
    };

    println!(
        "config: {} checksums ({}{})",
        if offload {
            "device-offloaded"
        } else {
            "full software"
        },
        if offload {
            "mimics a NIC or iOS NEPacketTunnelFlow"
        } else {
            "worst case"
        },
        match n_flows {
            Some(n) => format!(", {n} flows"),
            None => String::new(),
        }
    );
    print_socket_sizes();

    match shape {
        "firehose" => shape_firehose(seconds, offload),
        "pingpong" => shape_pingpong(seconds, offload),
        "small" => shape_small(seconds, offload),
        "udp" => shape_udp_firehose(seconds, offload),
        "all" => {
            shape_udp_firehose(seconds, offload);
            shape_small(seconds, offload);
            shape_pingpong(seconds, offload);
        }
        "many_tcp" => shape_many_tcp(seconds, n_flows.unwrap(), offload),
        "many_udp" => shape_many_udp(seconds, n_flows.unwrap(), offload),
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "multi_tcp" => {
            let n_threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(2).max(1);
            let flows: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(50).max(1);
            shape_multi_tcp(seconds, n_threads, flows, offload);
        }
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "churn" => {
            let rate: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(500).max(1);
            shape_churn(seconds, rate, offload);
        }
        #[cfg(feature = "socket-tcp-dynamic-buffer")]
        "idle_hot" => {
            let n_idle: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(500).max(0);
            let n_active: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(10).max(1);
            shape_idle_hot(seconds, n_idle, n_active, offload);
        }
        _ => {
            eprintln!(
                "unknown shape '{shape}'. expected udp|small|pingpong|firehose|all|many_tcp|many_udp\
                 |multi_tcp|churn|idle_hot (last three need --features socket-tcp-dynamic-buffer)"
            );
            std::process::exit(2);
        }
    }
}
