//! Compare per-flow memory cost: legacy fixed-buffer Socket vs.
//! pool-backed `new_dynamic` Socket. Allocates N sockets in each mode and
//! reports the steady-state RSS delta.
//!
//!   cargo run --release --example dynbuf_memcompare \
//!     --features socket-tcp-dynamic-buffer -- <N>
//!
//! The legacy column shows the floor cost a smoltcp consumer pays today
//! per admitted flow. The dynamic column shows the cost when buffers are
//! kept lazy — the iOS / NetworkExtension use-case where many idle flows
//! coexist with a small number of actively-buffered ones.

use std::env;
use std::fs;

use smoltcp::socket::tcp::{self, DynamicBufferConfig, MemoryPool, SocketBuffer};

fn rss_kib() -> usize {
    // /proc/self/status's VmRSS line: "VmRSS:\t      1234 kB"
    let s = fs::read_to_string("/proc/self/status").unwrap();
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib: usize = rest
                .split_whitespace()
                .next()
                .unwrap()
                .parse()
                .unwrap();
            return kib;
        }
    }
    0
}

fn main() {
    let n: usize = env::args()
        .nth(1)
        .as_deref()
        .unwrap_or("1000")
        .parse()
        .expect("usage: dynbuf_memcompare <N>");

    // Match the per-flow budget the iOS/userspace-VPN consumer ships
    // today: 32 KiB rx + 32 KiB tx = 64 KiB worst-case per admitted flow.
    const RX: usize = 32 * 1024;
    const TX: usize = 32 * 1024;

    // Baseline RSS before any socket allocation.
    let base = rss_kib();
    println!("baseline RSS:                        {base:>8} KiB");

    // === Legacy: fixed buffers ===
    let legacy_start = rss_kib();
    let mut legacy: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        let rx = SocketBuffer::new(vec![0u8; RX]);
        let tx = SocketBuffer::new(vec![0u8; TX]);
        legacy.push(tcp::Socket::new(rx, tx));
    }
    let legacy_end = rss_kib();
    let legacy_cost = legacy_end - legacy_start;
    let per_legacy_kb = legacy_cost as f64 / n as f64;
    println!(
        "legacy fixed-buffer N={n}: RSS Δ          {legacy_cost:>8} KiB \
         ({per_legacy_kb:>6.2} KiB / flow)"
    );

    drop(legacy);
    let post_legacy = rss_kib();
    println!("after dropping legacy sockets:       {post_legacy:>8} KiB");

    // === Dynamic: lazy, pool-backed, rx_initial = tx_initial = 0 ===
    // Pool sized for an iOS-realistic 24 MiB TCP payload budget.
    let pool = MemoryPool::new(24 * 1024 * 1024);
    let cfg = DynamicBufferConfig {
        rx_initial: 0,
        rx_max: RX as u32,
        tx_initial: 0,
        tx_max: TX as u32,
        grow_chunk: 4 * 1024,
    };
    let dyn_start = rss_kib();
    let mut dynamic: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        dynamic.push(tcp::Socket::new_dynamic(cfg, Some(pool.clone())));
    }
    let dyn_end = rss_kib();
    let dyn_cost = dyn_end - dyn_start;
    let per_dyn_kb = dyn_cost as f64 / n as f64;
    println!(
        "dynamic idle  N={n}: RSS Δ          {dyn_cost:>8} KiB \
         ({per_dyn_kb:>6.2} KiB / flow)"
    );
    println!(
        "pool charged after N idle sockets:   {:>8} KiB  (expect 0)",
        pool.used() / 1024
    );

    let ratio = if dyn_cost > 0 {
        legacy_cost as f64 / dyn_cost as f64
    } else {
        f64::INFINITY
    };
    println!(
        "savings:                              {:>6.1}× ({:>5.1} KiB → {:>5.1} KiB / flow)",
        ratio, per_legacy_kb, per_dyn_kb
    );

    drop(dynamic);
    println!("pool charged after Drop:             {:>8} KiB  (expect 0)", pool.used() / 1024);
}
