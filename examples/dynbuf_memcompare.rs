//! Compare per-flow memory cost: legacy fixed-buffer Socket vs.
//! pool-backed `new_dynamic` Socket. Allocates N sockets in a selected mode
//! and reports the steady-state process-memory delta.
//!
//!   cargo run --release --example dynbuf_memcompare \
//!     --features socket-tcp-dynamic-buffer -- legacy <N>
//!   cargo run --release --example dynbuf_memcompare \
//!     --features socket-tcp-dynamic-buffer -- dynamic <N>
//!
//! The legacy column shows the floor cost a smoltcp consumer pays today
//! per admitted flow. The dynamic column shows the cost when buffers are
//! kept lazy — the iOS / NetworkExtension use-case where many idle flows
//! coexist with a small number of actively-buffered ones.

use std::env;
use std::num::NonZeroUsize;

use smoltcp::socket::tcp::{self, DynamicBufferConfig, MemoryPool, SocketBuffer};

mod process_memory;
use process_memory::{
    process_memory_bytes, process_memory_label, process_memory_sample, signed_delta,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Legacy,
    Dynamic,
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "legacy" => Some(Self::Legacy),
            "dynamic" => Some(Self::Dynamic),
            _ => None,
        }
    }
}

fn usage(error: &str) -> ! {
    eprintln!("{error}\nusage: dynbuf_memcompare <legacy|dynamic> <N>");
    std::process::exit(2);
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<(Mode, NonZeroUsize), String> {
    let mut args = args.into_iter();
    let mode_name = args
        .next()
        .ok_or_else(|| "missing mode, expected legacy|dynamic".to_owned())?;
    let mode = Mode::parse(&mode_name)
        .ok_or_else(|| format!("invalid mode '{mode_name}', expected legacy|dynamic"))?;

    let count = args
        .next()
        .ok_or_else(|| "missing socket count N".to_owned())?;
    let count = count
        .parse::<usize>()
        .map_err(|_| format!("invalid socket count '{count}': expected a positive integer"))?;
    let count =
        NonZeroUsize::new(count).ok_or_else(|| "socket count N must be non-zero".to_owned())?;

    if let Some(trailing) = args.next() {
        return Err(format!("unexpected trailing argument '{trailing}'"));
    }
    Ok((mode, count))
}

fn per_flow_kib(delta_bytes: i128, n: usize) -> f64 {
    delta_bytes as f64 / 1024.0 / n as f64
}

fn print_lifetime_peak(stage: &str, n: usize, peak: Option<u64>) {
    let metric = process_memory_label();
    match peak {
        Some(peak) => {
            println!("process after {stage} N={n}: {metric} lifetime peak: {peak} bytes")
        }
        None => println!("process after {stage} N={n}: {metric} lifetime peak: unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, parse_args};

    fn args(values: &[&str]) -> Result<(Mode, std::num::NonZeroUsize), String> {
        parse_args(values.iter().map(|value| (*value).to_owned()))
    }

    #[test]
    fn parse_args_accepts_explicit_mode_and_positive_count() {
        assert_eq!(
            args(&["legacy", "1"]),
            Ok((Mode::Legacy, std::num::NonZeroUsize::new(1).unwrap()))
        );
        assert_eq!(
            args(&["dynamic", "1000"]),
            Ok((Mode::Dynamic, std::num::NonZeroUsize::new(1000).unwrap()))
        );
    }

    #[test]
    fn parse_args_rejects_unsupported_forms() {
        for input in [
            &[][..],
            &["1000"][..],
            &["both", "1000"][..],
            &["legacy"][..],
            &["legacy", "0"][..],
            &["dynamic", "many"][..],
            &["legacy", "1", "extra"][..],
        ] {
            assert!(args(input).is_err(), "input {input:?} should be rejected");
        }
    }
}

fn run_legacy(n: usize) {
    const RX: usize = 32 * 1024;
    const TX: usize = 32 * 1024;

    let start = process_memory_sample();
    let mut legacy: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        let rx = SocketBuffer::new(vec![0u8; RX]);
        let tx = SocketBuffer::new(vec![0u8; TX]);
        legacy.push(tcp::Socket::new(rx, tx));
    }
    std::hint::black_box(&legacy);
    let end = process_memory_sample();
    let cost = signed_delta(end.current_bytes, start.current_bytes);
    let per_flow = per_flow_kib(cost, n);
    let cost_kib = cost as f64 / 1024.0;
    let metric = process_memory_label();
    println!(
        "legacy fixed-buffer N={n}: {metric} delta {cost_kib:>8.1} KiB \
         ({per_flow:>6.2} KiB / flow) ({cost:+} bytes)"
    );
    print_lifetime_peak("legacy fixed-buffer", n, end.lifetime_peak_bytes);
}

fn run_dynamic(n: usize) {
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
    let start = process_memory_sample();
    let mut dynamic: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        dynamic.push(tcp::Socket::new_dynamic(cfg, Some(pool.clone())));
    }
    std::hint::black_box(&dynamic);
    let end = process_memory_sample();
    let cost = signed_delta(end.current_bytes, start.current_bytes);
    let per_flow = per_flow_kib(cost, n);
    let cost_kib = cost as f64 / 1024.0;
    let metric = process_memory_label();
    println!(
        "dynamic idle N={n}: {metric} delta      {cost_kib:>8.1} KiB \
         ({per_flow:>6.2} KiB / flow) ({cost:+} bytes)"
    );
    print_lifetime_peak("dynamic idle", n, end.lifetime_peak_bytes);
    println!(
        "pool charged after N idle sockets:   {:>8} KiB  (expect 0)",
        pool.used() / 1024
    );
    drop(dynamic);
    println!(
        "pool charged after Drop:             {:>8} KiB  (expect 0)",
        pool.used() / 1024
    );
}

fn main() {
    let (mode, n) = parse_args(env::args().skip(1)).unwrap_or_else(|error| usage(&error));
    let n = n.get();
    let base = process_memory_bytes() / 1024;
    let metric = process_memory_label();
    println!("baseline {metric}:                   {base:>8} KiB");

    match mode {
        Mode::Legacy => run_legacy(n),
        Mode::Dynamic => run_dynamic(n),
    }
}
