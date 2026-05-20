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
//! * `rx_buffer.capacity()` is monotonically non-decreasing during a
//!   connection's lifetime. We only shrink (release) when the socket
//!   transitions into the `Closed` state — by which point any peer
//!   already-in-flight bytes will arrive at a buffer that's about to drop
//!   them anyway.
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

#[derive(Debug)]
struct MemoryPoolInner {
    budget: usize,
    used: AtomicUsize,
}

impl MemoryPool {
    /// Create a new pool with the given total byte budget.
    ///
    /// `budget` is the maximum sum of `rx_buffer.capacity() + tx_buffer.capacity()`
    /// across all sockets that share this pool.
    pub fn new(budget: usize) -> Self {
        Self {
            inner: Arc::new(MemoryPoolInner {
                budget,
                used: AtomicUsize::new(0),
            }),
        }
    }

    /// Total byte budget configured for the pool.
    pub fn budget(&self) -> usize {
        self.inner.budget
    }

    /// Bytes currently charged against the pool by all sockets.
    pub fn used(&self) -> usize {
        self.inner.used.load(Ordering::Acquire)
    }

    /// Bytes still available for further growth (snapshot).
    pub fn available(&self) -> usize {
        self.budget().saturating_sub(self.used())
    }

    /// Attempt to charge `bytes` against the budget.
    ///
    /// Returns `true` on success. On contention, retries via CAS until either
    /// the charge succeeds or the budget is exceeded.
    pub(crate) fn try_charge(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let mut current = self.inner.used.load(Ordering::Acquire);
        loop {
            let next = match current.checked_add(bytes) {
                Some(n) if n <= self.inner.budget => n,
                _ => return false,
            };
            match self.inner.used.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => current = now,
            }
        }
    }

    /// Release `bytes` back to the budget.
    pub(crate) fn refund(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.inner.used.fetch_sub(bytes, Ordering::AcqRel);
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
    pub rx_max: u32,
    pub tx_max: u32,
    pub grow_chunk: u32,
    /// Bytes currently charged to `pool` for rx + tx buffers. Tracked so
    /// reset() and Drop refund exactly what was charged regardless of any
    /// future shrinks.
    pub charged: u32,
    pub pool: Option<MemoryPool>,
}

impl DynBufState {
    pub(super) fn new(cfg: &DynamicBufferConfig, pool: Option<MemoryPool>) -> Self {
        Self {
            rx_max: cfg.rx_max,
            tx_max: cfg.tx_max,
            grow_chunk: cfg.grow_chunk.max(1),
            charged: 0,
            pool,
        }
    }

    /// Try to charge `bytes` against the pool, if any. Returns true if the
    /// reservation succeeded (or no pool is attached).
    pub(super) fn charge(&mut self, bytes: u32) -> bool {
        if bytes == 0 {
            return true;
        }
        if let Some(pool) = &self.pool
            && !pool.try_charge(bytes as usize)
        {
            return false;
        }
        self.charged = self.charged.saturating_add(bytes);
        true
    }

    /// Refund all of this socket's outstanding charge to the pool.
    pub(super) fn refund_all(&mut self) {
        if let Some(pool) = &self.pool
            && self.charged > 0
        {
            pool.refund(self.charged as usize);
        }
        self.charged = 0;
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
