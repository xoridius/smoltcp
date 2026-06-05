//! Compare per-flow memory cost: legacy fixed-buffer Socket vs.
//! pool-backed `new_dynamic` Socket. Allocates N sockets in a selected mode
//! and reports the steady-state RSS delta.
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
//! Use separate `legacy` and `dynamic` process runs as RSS evidence. `both`
//! is only a convenient smoke check because allocator state from the first
//! phase can affect the second phase's process RSS.

use std::env;
#[cfg(target_os = "linux")]
use std::fs;

use smoltcp::socket::tcp::{self, DynamicBufferConfig, MemoryPool, SocketBuffer};

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

/// Read process resident memory in bytes.
fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let s = fs::read_to_string("/proc/self/status").unwrap_or_default();
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

fn rss_kib() -> u64 {
    rss_bytes() / 1024
}

fn per_flow_kib(delta_kib: u64, n: usize) -> f64 {
    delta_kib as f64 / n.max(1) as f64
}

fn run_legacy(n: usize) -> (u64, f64) {
    const RX: usize = 32 * 1024;
    const TX: usize = 32 * 1024;

    let start = rss_kib();
    let mut legacy: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        let rx = SocketBuffer::new(vec![0u8; RX]);
        let tx = SocketBuffer::new(vec![0u8; TX]);
        legacy.push(tcp::Socket::new(rx, tx));
    }
    std::hint::black_box(&legacy);
    let end = rss_kib();
    let cost = end.saturating_sub(start);
    let per_flow = per_flow_kib(cost, n);
    println!(
        "legacy fixed-buffer N={n}: RSS Δ    {cost:>8} KiB \
         ({per_flow:>6.2} KiB / flow)"
    );
    (cost, per_flow)
}

fn run_dynamic(n: usize) -> (u64, f64) {
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
    let start = rss_kib();
    let mut dynamic: Vec<tcp::Socket<'static>> = Vec::with_capacity(n);
    for _ in 0..n {
        dynamic.push(tcp::Socket::new_dynamic(cfg, Some(pool.clone())));
    }
    std::hint::black_box(&dynamic);
    let end = rss_kib();
    let cost = end.saturating_sub(start);
    let per_flow = per_flow_kib(cost, n);
    println!(
        "dynamic idle N={n}: RSS Δ           {cost:>8} KiB \
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
    let base = rss_kib();
    println!("baseline RSS:                        {base:>8} KiB");

    match mode {
        Mode::Legacy => {
            let _ = run_legacy(n);
        }
        Mode::Dynamic => {
            let _ = run_dynamic(n);
        }
        Mode::Both => {
            println!(
                "mode=both is a smoke check only; use separate legacy/dynamic runs as RSS evidence"
            );
            let (legacy_cost, per_legacy_kib) = run_legacy(n);
            let post_legacy = rss_kib();
            println!("after dropping legacy sockets:       {post_legacy:>8} KiB");
            let (dyn_cost, per_dyn_kib) = run_dynamic(n);
            let ratio = if dyn_cost > 0 {
                legacy_cost as f64 / dyn_cost as f64
            } else {
                f64::INFINITY
            };
            println!(
                "savings:                              {:>6.1}x ({:>5.1} KiB -> {:>5.1} KiB / flow)",
                ratio, per_legacy_kib, per_dyn_kib
            );
        }
    }
}
