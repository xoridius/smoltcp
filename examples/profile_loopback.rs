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
//!   cargo run --release --example profile_loopback -- [shape] [seconds]
//!
//! Shapes:
//!   udp        - 1400B UDP packet forwarding (default; closest tunnel analogue)
//!   small      - many small TCP segments, measures per-packet overhead
//!   pingpong   - request/response ping-pong of 128B messages, latency-bound
//!   firehose   - one-way TCP bulk transfer (cwnd-limited by both peers being smoltcp)
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

#[global_allocator]
static A: CountingAlloc = CountingAlloc;

/// Read VmRSS (kB) from /proc/self/status, returning bytes. Returns 0 on
/// non-Linux platforms.
fn rss_bytes() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib: u64 = rest
                .trim()
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            return kib * 1024;
        }
    }
    0
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

/// Snapshot of the allocator counters + RSS at one instant. Take two and
/// `diff()` them to see what happened during a phase.
#[derive(Copy, Clone)]
struct AllocSnap {
    alloc_bytes: u64,
    alloc_count: u64,
    /// Live bytes = alloc_bytes - free_bytes, used to show net heap growth.
    free_bytes: u64,
    rss: u64,
}

impl AllocSnap {
    fn now() -> Self {
        Self {
            alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
            alloc_count: ALLOC_COUNT.load(Ordering::Relaxed),
            free_bytes: FREE_BYTES.load(Ordering::Relaxed),
            rss: rss_bytes(),
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
    }
}

/// Drive both endpoints until both sides indicate no more state changes,
/// advancing the virtual clock in 1ms steps when idle.
fn run_for(server: &mut Endpoint<'static>, client: &mut Endpoint<'static>, until: StdInstant) {
    let mut t_ms: i64 = 0;
    while StdInstant::now() < until {
        let now = Instant::from_millis(t_ms);
        let _ = server.iface.poll(now, &mut server.device, &mut server.sockets);
        let _ = client.iface.poll(now, &mut client.device, &mut client.sockets);
        t_ms = t_ms.wrapping_add(1);
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
    let _ = run_for; // suppress dead_code on this helper, kept for future shapes
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
        if cs.can_send() {
            if let Ok(w) = cs.send_slice(&payload) {
                if w > 0 {
                    sent += w as u64;
                }
            }
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
        if ss.can_recv() {
            if let Ok(r) = ss.recv_slice(&mut sink) {
                if r > 0 && ss.can_send() {
                    let _ = ss.send_slice(&sink[..r]);
                }
            }
        }
        poll_lat.measure(|| {
            server.iface.poll(n, &mut server.device, &mut server.sockets);
            client.iface.poll(n, &mut client.device, &mut client.sockets);
        });

        // Client receives echo.
        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        if cs.can_recv() {
            if let Ok(r) = cs.recv_slice(&mut sink) {
                if r > 0 {
                    roundtrips += 1;
                }
            }
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
    // Args: <shape|all> [seconds] [offload]
    //   shape:   udp | small | pingpong | firehose | all
    //   offload: "offload" | "1" | "true" -> Device advertises checksum
    //            offload, so smoltcp skips IP/TCP/UDP csums (mimics a
    //            hardware NIC or iOS NEPacketTunnelFlow).
    let args: Vec<String> = env::args().collect();
    let shape = args.get(1).map(String::as_str).unwrap_or("all");
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let offload = matches!(
        args.get(3).map(String::as_str),
        Some("offload") | Some("1") | Some("true")
    );

    let cfg_line = if offload {
        "config: checksum offload ENABLED (device-verified, like a NIC or NEPacketTunnelFlow)"
    } else {
        "config: full software checksums on both peers (worst case)"
    };
    println!("{cfg_line}");
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
        _ => {
            eprintln!("unknown shape '{shape}'. expected udp|small|pingpong|firehose|all");
            std::process::exit(2);
        }
    }
}
