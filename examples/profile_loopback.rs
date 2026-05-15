//! In-process smoltcp throughput / latency profiler.
//!
//! Wires two `Interface`s back-to-back through a pair of in-memory packet
//! queues (no tun/tap, no syscalls per packet), then drives different
//! traffic shapes through them and reports throughput, packet rate, and
//! cycles/byte. Designed to be runnable under `perf record`, `valgrind`,
//! or `heaptrack` with zero external dependencies.
//!
//! Usage:
//!   cargo run --release --example profile_loopback -- [shape] [seconds]
//!
//! Shapes:
//!   firehose   - one-way TCP bulk transfer (default)
//!   pingpong   - request/response ping-pong of small messages
//!   small      - many small (64B) TCP segments, measures per-packet overhead
//!
//! Recommended profiling recipes:
//!   perf stat -e cycles,instructions,cache-misses,branch-misses -- \
//!     cargo run --release --example profile_loopback -- firehose 5
//!   perf record -F 999 --call-graph dwarf -- \
//!     cargo run --release --example profile_loopback -- firehose 5
//!   perf report
//!   heaptrack cargo run --release --example profile_loopback -- firehose 2

use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::rc::Rc;
use std::time::Instant as StdInstant;

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
    /// Bytes pushed through this device's TX path (i.e., what we emitted).
    tx_bytes: u64,
    tx_packets: u64,
}

impl PairedDevice {
    fn new(tx: LaneRc, rx: LaneRc, mtu: usize) -> Self {
        Self {
            tx,
            rx,
            mtu,
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
        caps.checksum = ChecksumCapabilities::default();
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

fn make_endpoint(addr: IpAddress, mtu: usize, tx: LaneRc, rx: LaneRc) -> Endpoint<'static> {
    let mut device = PairedDevice::new(tx, rx, mtu);
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

fn shape_firehose(seconds: u64) {
    const BUF: usize = 256 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(
        IpAddress::v4(10, 0, 0, 1),
        1500,
        lane_a.clone(),
        lane_b.clone(),
    );
    let mut client = make_endpoint(
        IpAddress::v4(10, 0, 0, 2),
        1500,
        lane_b.clone(),
        lane_a.clone(),
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

    while StdInstant::now() < deadline {
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

        let cli_state = client
            .iface
            .poll(n, &mut client.device, &mut client.sockets);
        let srv_state = server
            .iface
            .poll(n, &mut server.device, &mut server.sockets);

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
    let elapsed = start.elapsed().as_secs_f64();

    let bytes = recvd as f64;
    let gbps = bytes * 8.0 / elapsed / 1e9;
    let total_pkts = client.device.tx_packets + server.device.tx_packets;
    let total_bytes = client.device.tx_bytes + server.device.tx_bytes;
    let mpps = total_pkts as f64 / elapsed / 1e6;
    let avg_pkt = total_bytes as f64 / total_pkts.max(1) as f64;
    println!("== firehose ==");
    println!("  elapsed:           {elapsed:.3} s");
    println!("  app bytes sent:    {sent}");
    println!("  app bytes recvd:   {recvd}");
    println!("  throughput:        {gbps:.3} Gbps  ({:.1} MB/s)", bytes / elapsed / 1e6);
    println!("  packets emitted:   {total_pkts}");
    println!("  packet rate:       {mpps:.3} Mpps");
    println!("  avg packet size:   {avg_pkt:.1} bytes");
    println!("  idle spins:        {idle_spins}");
    let _ = run_for; // suppress dead_code on this helper, kept for future shapes
}

fn shape_small(seconds: u64) {
    // Force tiny segments by limiting the socket buffer; with a 1500 MTU the
    // client never fills more than a single small write at a time.
    const BUF: usize = 4 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(IpAddress::v4(10, 0, 0, 1), 1500, lane_a.clone(), lane_b.clone());
    let mut client = make_endpoint(IpAddress::v4(10, 0, 0, 2), 1500, lane_b.clone(), lane_a.clone());

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
    while StdInstant::now() < deadline {
        let n = Instant::from_millis(t_ms);

        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        if cs.can_send() {
            if let Ok(w) = cs.send_slice(&payload) {
                if w > 0 {
                    sent += w as u64;
                }
            }
        }
        client.iface.poll(n, &mut client.device, &mut client.sockets);
        server.iface.poll(n, &mut server.device, &mut server.sockets);

        let ss = server.sockets.get_mut::<tcp::Socket>(srv_h);
        if ss.can_recv() {
            let mut sink = [0u8; 64];
            if let Ok(r) = ss.recv_slice(&mut sink) {
                recvd += r as u64;
            }
        }
        t_ms += 1;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let pkts = client.device.tx_packets + server.device.tx_packets;
    let bytes_wire = client.device.tx_bytes + server.device.tx_bytes;
    println!("== small ==");
    println!("  elapsed:           {elapsed:.3} s");
    println!("  app bytes sent:    {sent}  (recvd {recvd})");
    println!("  packets emitted:   {pkts}  ({:.3} Mpps)", pkts as f64 / elapsed / 1e6);
    println!("  wire bytes:        {bytes_wire}  ({:.2} MB/s)", bytes_wire as f64 / elapsed / 1e6);
    println!("  avg packet size:   {:.1} bytes", bytes_wire as f64 / pkts.max(1) as f64);
    println!("  ns/packet:         {:.1}", elapsed * 1e9 / pkts.max(1) as f64);
}

fn shape_pingpong(seconds: u64) {
    const BUF: usize = 16 * 1024;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(IpAddress::v4(10, 0, 0, 1), 1500, lane_a.clone(), lane_b.clone());
    let mut client = make_endpoint(IpAddress::v4(10, 0, 0, 2), 1500, lane_b.clone(), lane_a.clone());

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

    while StdInstant::now() < deadline {
        let n = Instant::from_millis(t_ms);
        // Client sends one message.
        let cs = client.sockets.get_mut::<tcp::Socket>(cli_h);
        if cs.can_send() {
            let _ = cs.send_slice(&msg);
        }
        client.iface.poll(n, &mut client.device, &mut client.sockets);
        server.iface.poll(n, &mut server.device, &mut server.sockets);

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
        server.iface.poll(n, &mut server.device, &mut server.sockets);
        client.iface.poll(n, &mut client.device, &mut client.sockets);

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
    let elapsed = start.elapsed().as_secs_f64();
    println!("== pingpong ==");
    println!("  elapsed:           {elapsed:.3} s");
    println!("  roundtrips:        {roundtrips}");
    println!("  rate:              {:.3} M-rtt/s", roundtrips as f64 / elapsed / 1e6);
    println!("  avg latency:       {:.1} ns/rtt", elapsed * 1e9 / roundtrips.max(1) as f64);
}

fn shape_udp_firehose(seconds: u64) {
    // Pure packet forwarding — no flow control, no cwnd. This is the closest
    // analogue to a tunnel forwarding fully-formed packets between two peers
    // (which is what tunnel-lib-rust wraps smoltcp for).
    const PAYLOAD: usize = 1400;
    const META_SLOTS: usize = 256;
    let lane_a: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));
    let lane_b: LaneRc = Rc::new(RefCell::new(Lane::new(1500, 256)));

    let mut server = make_endpoint(IpAddress::v4(10, 0, 0, 1), 1500, lane_a.clone(), lane_b.clone());
    let mut client = make_endpoint(IpAddress::v4(10, 0, 0, 2), 1500, lane_b.clone(), lane_a.clone());

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

    let wall_origin = StdInstant::now();
    let now_smol = || Instant::from_micros(wall_origin.elapsed().as_micros() as i64);

    let deadline = StdInstant::now() + std::time::Duration::from_secs(seconds);
    let start = StdInstant::now();
    let mut sent: u64 = 0;
    let mut recvd: u64 = 0;
    let mut sink = [0u8; PAYLOAD];

    while StdInstant::now() < deadline {
        let n = now_smol();
        // Try to enqueue as many packets as the client tx buffer holds.
        let cs = client.sockets.get_mut::<udp::Socket>(cli_h);
        while cs.can_send() && cs.send_slice(&payload, dest_meta).is_ok() {
            sent += PAYLOAD as u64;
        }
        client.iface.poll(n, &mut client.device, &mut client.sockets);
        server.iface.poll(n, &mut server.device, &mut server.sockets);

        let ss = server.sockets.get_mut::<udp::Socket>(srv_h);
        while ss.can_recv() {
            match ss.recv_slice(&mut sink) {
                Ok((r, _)) => recvd += r as u64,
                Err(_) => break,
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let total_pkts = client.device.tx_packets + server.device.tx_packets;
    let total_bytes = client.device.tx_bytes + server.device.tx_bytes;
    println!("== udp_firehose ==");
    println!("  elapsed:           {elapsed:.3} s");
    println!("  app bytes sent:    {sent}");
    println!("  app bytes recvd:   {recvd}");
    println!("  throughput:        {:.3} Gbps  ({:.1} MB/s)", recvd as f64 * 8.0 / elapsed / 1e9, recvd as f64 / elapsed / 1e6);
    println!("  packets emitted:   {total_pkts}  ({:.3} Mpps)", total_pkts as f64 / elapsed / 1e6);
    println!("  wire bytes:        {total_bytes}");
    println!("  avg packet size:   {:.1} bytes", total_bytes as f64 / total_pkts.max(1) as f64);
    println!("  ns/packet:         {:.1}", elapsed * 1e9 / total_pkts.max(1) as f64);
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let shape = args.get(1).map(String::as_str).unwrap_or("firehose");
    let seconds: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    match shape {
        "firehose" => shape_firehose(seconds),
        "pingpong" => shape_pingpong(seconds),
        "small" => shape_small(seconds),
        "udp" => shape_udp_firehose(seconds),
        _ => {
            eprintln!("unknown shape '{shape}'. expected firehose|pingpong|small|udp");
            std::process::exit(2);
        }
    }
}
