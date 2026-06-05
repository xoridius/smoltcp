//! Pool-backed, lazy, resizable TCP socket buffers.
//!
//! See `socket::tcp::Socket::new_dynamic` for the public entry point.
//!
//! ## Design (Linux/XNU-inspired)
//!
//! - A [`MemoryPool`] is a process-wide byte budget shared across many TCP
//!   sockets. It is the smoltcp analogue of Linux's `tcp_mem` global
//!   accounting (`memory_allocated` against `sysctl_mem[0..2]`) or XNU's
//!   `proto_memacct_*` against the mbuf zone limits.
//!
//! - Each socket carries a per-flow `[initial, max]` quota — the smoltcp
//!   analogue of Linux's `tcp_rmem`/`tcp_wmem` and XNU's
//!   `tcp_recvspace`/`tcp_autorcvbuf_max`. Buffers start at `initial`, grow on
//!   pressure up to `max`, and are released back to the pool on
//!   close/reset/drop. This matches XNU's observation that `sbreserve` does
//!   no allocation at the call site; it only sets a high-water mark.
//!
//! - The pool tracks bytes outstanding with an `AtomicUsize`. Growth requests
//!   atomically check-then-commit against the budget; on exhaustion, growth
//!   refuses and the socket's advertised receive window stays small (or
//!   collapses to zero), creating natural sender backpressure. Already-
//!   accepted payload is never dropped, because we only grow *up to* the cap
//!   that the wire layer is already willing to absorb.
//!
//! ## Correctness invariants
//!
//! * The socket's RFC 1323 window-scale factor is fixed at SYN time using
//!   the per-flow **max**, not the current capacity. Once a connection is
//!   negotiated, the scale cannot change, so it must accommodate any future
//!   growth.
//!
//! * `rx_buffer.capacity()` is monotonically non-decreasing while a live peer
//!   can still deliver bytes into the receive half. We release only after the
//!   state machine no longer needs queued bytes to compute sequence numbers or
//!   acknowledgments: failed passive handshakes returning to `Listen`, aborts
//!   after the RST is emitted, final `LastAck`, terminal `TimeWait`, reset, and
//!   drop.
//!
//! * The advertised window is always `capacity() - len()`; the pool only
//!   gates *growing* capacity, never the in-flight `len()`. The wire layer
//!   never advertises more than the buffer can absorb.

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::sync::Arc;

/// A shared, bounded memory budget for TCP socket buffers.
///
/// Clone freely — all clones refer to the same atomic accounting. The pool is
/// not Drop-aware; a leaked `Socket` will keep its reservation charged until
/// the pool itself is dropped.
#[derive(Debug, Clone)]
pub struct MemoryPool {
    inner: Arc<MemoryPoolInner>,
}

/// Cache-line-aligned to keep `used` off the same line as the `Arc`
/// strong/weak counts that precede it. Without this, every `Arc::clone`
/// or `Drop` touches the cache line that `used` lives on — false-sharing-
/// adjacent (it's true sharing of the line, false sharing of the field)
/// and visible as cross-core LLC traffic on multi-Interface pool sharing.
///
/// Linux's per-CPU `mibs[]` arrays use the same idiom (`____cacheline_aligned`).
#[repr(align(64))]
#[derive(Debug)]
struct MemoryPoolInner {
    /// Immutable for the pool's lifetime; placed first so cold readers
    /// only touch this cache line.
    budget: usize,
    /// Pre-computed `budget * 3 / 4`. The pressure-throttle gate in
    /// `under_pressure` checks `used >= pressure_threshold`, replacing
    /// two saturating multiplies on the hot path with a single load
    /// and compare.
    pressure_threshold: usize,
    /// Hot atomic counter. `Relaxed` everywhere: we maintain a single
    /// totally-ordered count, and no other memory writes are gated on
    /// the counter's value (each Interface owns its own buffers; the
    /// pool only tracks an aggregate sum). This matches Linux's
    /// `proto_memory_allocated` atomic, which uses `atomic_long_add/sub`
    /// with no explicit barriers.
    used: AtomicUsize,
    /// Diagnostic: total CAS retries observed in `try_charge`. Each
    /// failure of the CAS-weak loop bumps this once. Uncontended
    /// single-thread use sees 0 retries; multi-Interface deployments
    /// see retries proportional to inter-thread grow simultaneity.
    /// Bounded growth of this counter under load is the
    /// "bounded CAS retry rate" gate the multi-Interface bench checks.
    cas_retries: AtomicUsize,
}

impl MemoryPool {
    /// Create a new pool with the given total byte budget.
    ///
    /// `budget` is the maximum sum of `rx_buffer.capacity() + tx_buffer.capacity()`
    /// across all sockets that share this pool.
    pub fn new(budget: usize) -> Self {
        // 75 % threshold. Saturating math is overkill for any realistic
        // budget; `usize::MAX / 4 * 3` is still ~13 EiB. Use plain * / 4
        // (no overflow path), but keep saturating semantics for the
        // pathological-input case so we never panic on a debug build.
        let pressure_threshold = (budget / 4).saturating_mul(3);
        Self {
            inner: Arc::new(MemoryPoolInner {
                budget,
                pressure_threshold,
                used: AtomicUsize::new(0),
                cas_retries: AtomicUsize::new(0),
            }),
        }
    }

    /// Total CAS retries observed in `try_charge` across this pool's
    /// lifetime. Use as a diagnostic for multi-Interface contention:
    /// uncontended single-thread sees zero; under realistic
    /// many-Interface load it should grow much slower than the
    /// `(charge_calls × threads)` upper bound.
    pub fn cas_retries(&self) -> usize {
        self.inner.cas_retries.load(Ordering::Relaxed)
    }

    /// Total byte budget configured for the pool.
    pub fn budget(&self) -> usize {
        self.inner.budget
    }

    /// Bytes currently charged against the pool by all sockets.
    pub fn used(&self) -> usize {
        self.inner.used.load(Ordering::Relaxed)
    }

    /// Bytes still available for further growth (snapshot).
    pub fn available(&self) -> usize {
        self.budget().saturating_sub(self.used())
    }

    /// Attempt to charge `bytes` against the budget.
    ///
    /// Returns `true` on success. On contention, retries via CAS until either
    /// the charge succeeds or the budget is exceeded. Relaxed ordering: we
    /// maintain a single counter with no other memory writes gated on its
    /// value — atomic-RMW on x86 already implies the ordering we need, and
    /// on ARM this drops the dmb/ldar/stlr fences we were paying for nothing.
    pub(crate) fn try_charge(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let mut current = self.inner.used.load(Ordering::Relaxed);
        let mut retries: usize = 0;
        loop {
            let next = match current.checked_add(bytes) {
                Some(n) if n <= self.inner.budget => n,
                _ => {
                    if retries > 0 {
                        self.inner.cas_retries.fetch_add(retries, Ordering::Relaxed);
                    }
                    return false;
                }
            };
            match self.inner.used.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if retries > 0 {
                        self.inner.cas_retries.fetch_add(retries, Ordering::Relaxed);
                    }
                    return true;
                }
                Err(now) => {
                    retries += 1;
                    current = now;
                }
            }
        }
    }

    /// Release `bytes` back to the budget. Relaxed ordering for the same
    /// reason as `try_charge` — counter consistency is provided by the
    /// atomic RMW itself.
    pub(crate) fn refund(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.inner.used.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Whether the pool is past its growth-throttle threshold (~75% used).
    ///
    /// Above this point, individual sockets back off from geometric growth
    /// to linear growth, leaving headroom for other sockets to claim. This
    /// is the smoltcp analogue of Linux's `tcp_under_memory_pressure(sk)`
    /// gate, which suppresses receive-window autotuning when
    /// `memory_allocated > sysctl_mem[1]` (the middle of the three-tier
    /// `tcp_mem` budget). Threshold pre-computed at construction so the
    /// hot path is one atomic load and one compare.
    pub(crate) fn under_pressure(&self) -> bool {
        self.inner.used.load(Ordering::Relaxed) >= self.inner.pressure_threshold
    }
}

/// Per-flow configuration for a dynamic-buffer TCP socket.
///
/// `initial` is allocated up-front (may be 0 for fully-lazy sockets);
/// `max` is the cap that the receive-window scale is sized for and that
/// the buffers may grow to. `grow_chunk` controls how much capacity is
/// added at each growth step.
///
/// Sensible defaults via [`Default`] match a small per-flow footprint:
/// `initial = 0`, `max = 64 KiB`, `grow_chunk = 4 KiB`. Override
/// individual fields as needed.
#[derive(Debug, Clone, Copy)]
pub struct DynamicBufferConfig {
    /// Initial receive-buffer capacity in bytes. May be 0.
    pub rx_initial: u32,
    /// Maximum receive-buffer capacity in bytes. Must be ≤ `1 << 30`.
    pub rx_max: u32,
    /// Initial transmit-buffer capacity in bytes. May be 0.
    pub tx_initial: u32,
    /// Maximum transmit-buffer capacity in bytes.
    pub tx_max: u32,
    /// Chunk size for each growth step. Clamped to `[1, max]`.
    pub grow_chunk: u32,
}

impl Default for DynamicBufferConfig {
    fn default() -> Self {
        Self {
            rx_initial: 0,
            rx_max: 64 * 1024,
            tx_initial: 0,
            tx_max: 64 * 1024,
            grow_chunk: 4 * 1024,
        }
    }
}

impl DynamicBufferConfig {
    /// Convenience for a symmetric `max` on both directions.
    pub const fn symmetric(initial: u32, max: u32, grow_chunk: u32) -> Self {
        Self {
            rx_initial: initial,
            rx_max: max,
            tx_initial: initial,
            tx_max: max,
            grow_chunk,
        }
    }
}

/// Per-socket dynamic-buffer state.
///
/// Held boxed in `Socket` so the legacy fixed-buffer path pays only one
/// `Option<Box<_>>` (8 bytes) when the feature is enabled, and nothing when
/// it is disabled.
#[derive(Debug)]
pub(super) struct DynBufState {
    pub rx_initial: u32,
    pub tx_initial: u32,
    pub rx_max: u32,
    pub tx_max: u32,
    pub grow_chunk: u32,
    /// Bytes currently charged to `pool` for rx + tx buffers. Tracked so
    /// reset() and Drop refund exactly what was charged regardless of any
    /// future shrinks. `usize` (not `u32`) so that pool accounting stays
    /// correct even when rx_max + tx_max approach the RFC 1323 cap
    /// (2 × 1 GiB = 2 GiB > u32::MAX on 32-bit pointers — and unbounded
    /// on 64-bit if a caller passed pathological values before the
    /// per-direction clamp landed).
    pub charged: usize,
    pub pool: Option<MemoryPool>,
}

impl DynBufState {
    pub(super) fn new(cfg: &DynamicBufferConfig, pool: Option<MemoryPool>) -> Self {
        Self {
            rx_initial: cfg.rx_initial,
            tx_initial: cfg.tx_initial,
            rx_max: cfg.rx_max,
            tx_max: cfg.tx_max,
            grow_chunk: cfg.grow_chunk.max(1),
            charged: 0,
            pool,
        }
    }

    /// Try to charge `bytes` against the pool, if any. Returns true if the
    /// reservation succeeded (or no pool is attached).
    pub(super) fn charge(&mut self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        if let Some(pool) = &self.pool
            && !pool.try_charge(bytes)
        {
            return false;
        }
        // Saturating against `usize::MAX` is informational only: a 16 EiB
        // overflow on 64-bit isn't physically reachable. The point of using
        // `usize` over `u32` is that the saturating arm becomes unreachable
        // in practice rather than a known stick-at-u32-max bug.
        self.charged = self.charged.saturating_add(bytes);
        true
    }

    /// Refund `bytes` of this socket's outstanding charge to the pool,
    /// clamped to what is actually charged. Used both for full release
    /// (`refund(self.charged)` on close/reset/drop) and for the
    /// grow-failure rollback path (`refund(need)` when `try_grow`
    /// rejects the freshly-charged increment).
    pub(super) fn refund(&mut self, bytes: usize) {
        let bytes = bytes.min(self.charged);
        if bytes == 0 {
            return;
        }
        if let Some(pool) = &self.pool {
            pool.refund(bytes);
        }
        self.charged -= bytes;
    }
}

/// Refund the pool when the per-socket state is dropped — covers
/// `SocketSet::remove`, SocketSet teardown, and any other path where a
/// `Socket` is dropped without first going through `set_state(Closed)`
/// or `reset()`. Idempotent: explicit close paths already refund and
/// leave `charged == 0` here.
impl Drop for DynBufState {
    fn drop(&mut self) {
        self.refund(self.charged);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn pool_charge_and_refund() {
        let pool = MemoryPool::new(1024);
        assert_eq!(pool.budget(), 1024);
        assert_eq!(pool.used(), 0);
        assert_eq!(pool.available(), 1024);

        assert!(pool.try_charge(512));
        assert_eq!(pool.used(), 512);
        assert!(pool.try_charge(256));
        assert_eq!(pool.used(), 768);
        assert!(!pool.try_charge(512), "should refuse exceeding budget");
        assert_eq!(pool.used(), 768);
        pool.refund(256);
        assert_eq!(pool.used(), 512);
        pool.refund(512);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn pool_clone_shares_state() {
        let pool = MemoryPool::new(100);
        let other = pool.clone();
        assert!(pool.try_charge(50));
        assert_eq!(other.used(), 50);
        other.refund(50);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn pool_zero_charge_is_noop() {
        let pool = MemoryPool::new(0);
        assert!(pool.try_charge(0));
        assert_eq!(pool.used(), 0);
    }
}
