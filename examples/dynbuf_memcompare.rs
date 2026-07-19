//! Compare per-flow memory cost: legacy fixed-buffer Socket vs.
//! pool-backed `new_dynamic` Socket. Allocates N sockets in a selected mode
//! and reports the steady-state process-memory delta.
//!
//!   cargo run --release --example dynbuf_memcompare \
//!     --features socket-tcp-dynamic-buffer -- legacy <N>
//!   cargo run --release --example dynbuf_memcompare \
//!     --features socket-tcp-dynamic-buffer -- dynamic <N>
//!   cargo run --release --example dynbuf_memcompare \
//!     --features socket-tcp-dynamic-buffer -- both <N>
//!
//! The legacy column shows the floor cost a smoltcp consumer pays today
//! per admitted flow. The dynamic column shows the cost when buffers are
//! kept lazy — the iOS / NetworkExtension use-case where many idle flows
//! coexist with a small number of actively-buffered ones.
//!
//! Use separate `legacy` and `dynamic` process runs as memory evidence. `both`
//! is only a convenient smoke check because allocator state from the first
//! phase can affect the second phase's process memory.

use std::env;

use smoltcp::socket::tcp::{self, DynamicBufferConfig, MemoryPool, SocketBuffer};

mod process_memory;
use process_memory::{process_memory_bytes, process_memory_label, signed_delta};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Legacy,
    Dynamic,
    Both,
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "legacy" => Some(Self::Legacy),
            "dynamic" => Some(Self::Dynamic),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: dynbuf_memcompare [legacy|dynamic|both] <N>");
    std::process::exit(2);
}

fn parse_args() -> (Mode, usize) {
    let mut args = env::args().skip(1);
    let first = args.next();
    match first.as_deref() {
        None => (Mode::Both, 1000),
        Some(value) => {
            if let Some(mode) = Mode::parse(value) {
                let n = args
                    .next()
                    .as_deref()
                    .unwrap_or("1000")
                    .parse()
                    .unwrap_or_else(|_| usage());
                (mode, n)
            } else {
                let n = value.parse().unwrap_or_else(|_| usage());
                (Mode::Both, n)
            }
        }
    }
}

fn per_flow_kib(delta_bytes: i128, n: usize) -> f64 {
    delta_bytes as f64 / 1024.0 / n.max(1) as f64
}

fn savings_ratio(legacy_delta: i128, dynamic_delta: i128) -> Option<f64> {
    if legacy_delta > 0 && dynamic_delta > 0 {
        Some(legacy_delta as f64 / dynamic_delta as f64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::savings_ratio;

    #[test]
    fn savings_ratio_requires_two_positive_deltas() {
        assert_eq!(savings_ratio(12, 3), Some(4.0));
        assert_eq!(savings_ratio(0, 3), None);
        assert_eq!(savings_ratio(-1, 3), None);
        assert_eq!(savings_ratio(12, 0), None);
        assert_eq!(savings_ratio(12, -1), None);
    }
}

fn run_legacy(n: usize) -> (i128, f64) {
    const RX: usize = 32 * 1024;
    const TX: usize = 32 * 1024;

    let start = process_memory_bytes();
    let mut legacy: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        let rx = SocketBuffer::new(vec![0u8; RX]);
        let tx = SocketBuffer::new(vec![0u8; TX]);
        legacy.push(tcp::Socket::new(rx, tx));
    }
    std::hint::black_box(&legacy);
    let end = process_memory_bytes();
    let cost = signed_delta(end, start);
    let per_flow = per_flow_kib(cost, n);
    let cost_kib = cost as f64 / 1024.0;
    let metric = process_memory_label();
    println!(
        "legacy fixed-buffer N={n}: {metric} delta {cost_kib:>8.1} KiB \
         ({per_flow:>6.2} KiB / flow)"
    );
    (cost, per_flow)
}

fn run_dynamic(n: usize) -> (i128, f64) {
    const RX: usize = 32 * 1024;
    const TX: usize = 32 * 1024;

    let pool = MemoryPool::new(24 * 1024 * 1024);
    let cfg = DynamicBufferConfig {
        rx_initial: 0,
        rx_max: RX as u32,
        tx_initial: 0,
        tx_max: TX as u32,
        grow_chunk: 4 * 1024,
    };
    let start = process_memory_bytes();
    let mut dynamic: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        dynamic.push(tcp::Socket::new_dynamic(cfg, Some(pool.clone())));
    }
    std::hint::black_box(&dynamic);
    let end = process_memory_bytes();
    let cost = signed_delta(end, start);
    let per_flow = per_flow_kib(cost, n);
    let cost_kib = cost as f64 / 1024.0;
    let metric = process_memory_label();
    println!(
        "dynamic idle N={n}: {metric} delta      {cost_kib:>8.1} KiB \
         ({per_flow:>6.2} KiB / flow)"
    );
    println!(
        "pool charged after N idle sockets:   {:>8} KiB  (expect 0)",
        pool.used() / 1024
    );
    drop(dynamic);
    println!(
        "pool charged after Drop:             {:>8} KiB  (expect 0)",
        pool.used() / 1024
    );
    (cost, per_flow)
}

fn main() {
    let (mode, n) = parse_args();
    let base = process_memory_bytes() / 1024;
    let metric = process_memory_label();
    println!("baseline {metric}:                   {base:>8} KiB");

    match mode {
        Mode::Legacy => {
            let _ = run_legacy(n);
        }
        Mode::Dynamic => {
            let _ = run_dynamic(n);
        }
        Mode::Both => {
            println!(
                "mode=both is a smoke check only; use separate legacy/dynamic runs as memory evidence"
            );
            let (legacy_cost, per_legacy_kib) = run_legacy(n);
            let post_legacy = process_memory_bytes() / 1024;
            println!("after dropping legacy sockets:       {post_legacy:>8} KiB");
            let (dyn_cost, per_dyn_kib) = run_dynamic(n);
            if let Some(ratio) = savings_ratio(legacy_cost, dyn_cost) {
                println!(
                    "savings ratio: {ratio:>6.1}x ({per_legacy_kib:>5.1} KiB -> {per_dyn_kib:>5.1} KiB / flow)"
                );
            } else {
                println!("savings ratio: unavailable");
            }
        }
    }
}
