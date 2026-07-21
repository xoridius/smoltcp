//! Pool-backed, lazy, resizable TCP socket buffers.
//!
//! See `socket::tcp::Socket::new_dynamic` for the public entry point.
//!
//! ## Correctness invariants
//!
//! * Window scaling is fixed at SYN time from the receive maximum and capped
//!   at 14. Growth must reach one scale granule to escape a zero window.
//! * Pool pressure can refuse growth but never discards accepted payload or
//!   advertises beyond backing capacity.
//! * Receive capacity is not released while a peer can still deliver data;
//!   lifecycle code in `Socket` decides when release is safe.

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::SocketBuffer;

/// A shared, bounded memory budget for TCP socket buffers.
///
/// Clone freely — all clones refer to the same atomic accounting. The pool is
/// not Drop-aware; a leaked `Socket` will keep its reservation charged until
/// the pool itself is dropped.
#[derive(Debug, Clone)]
pub struct MemoryPool {
    inner: Arc<MemoryPoolInner>,
}

/// Cache alignment keeps the hot counter away from adjacent `Arc` counts.
#[repr(align(64))]
#[derive(Debug)]
struct MemoryPoolInner {
    budget: usize,
    /// Precomputed so the pressure gate is one load and comparison.
    pressure_threshold: usize,
    /// `Relaxed`: this is aggregate accounting and gates no other memory.
    used: AtomicUsize,
}

impl MemoryPool {
    /// Create a new pool with the given total byte budget.
    ///
    /// `budget` is the maximum sum of `rx_buffer.capacity() + tx_buffer.capacity()`
    /// across all sockets that share this pool.
    pub fn new(budget: usize) -> Self {
        let pressure_threshold = (budget / 4).saturating_mul(3);
        Self {
            inner: Arc::new(MemoryPoolInner {
                budget,
                pressure_threshold,
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
        self.inner.used.load(Ordering::Relaxed)
    }

    /// Bytes still available for further growth (snapshot).
    pub fn available(&self) -> usize {
        self.budget().saturating_sub(self.used())
    }

    /// Attempt to charge `bytes` against the budget.
    ///
    /// Returns `true` on success. Relaxed ordering is sufficient because this
    /// aggregate counter gates no buffer memory access.
    fn try_charge(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let mut current = self.inner.used.load(Ordering::Relaxed);
        loop {
            let next = match current.checked_add(bytes) {
                Some(n) if n <= self.inner.budget => n,
                _ => return false,
            };
            match self.inner.used.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(now) => current = now,
            }
        }
    }

    /// Release `bytes` back to the budget.
    fn refund(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        self.inner.used.fetch_sub(bytes, Ordering::Relaxed);
    }

    /// Whether the pool is past its growth-throttle threshold (~75% used).
    ///
    /// Above this point growth becomes linear, preserving shared headroom.
    fn under_pressure(&self) -> bool {
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
    /// Maximum receive-buffer capacity in bytes. Values above `1 << 30`
    /// (the RFC 7323 window cap) are clamped at construction.
    pub rx_max: u32,
    /// Initial transmit-buffer capacity in bytes. May be 0.
    pub tx_initial: u32,
    /// Maximum transmit-buffer capacity in bytes. Values above `1 << 30`
    /// are clamped at construction.
    pub tx_max: u32,
    /// Chunk size for each growth step. Values of 0 are treated as 1.
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

/// Owns a dynamic socket's capacity policy and pool accounting.
#[derive(Debug)]
pub(super) struct DynamicBufferState {
    rx_initial: u32,
    tx_initial: u32,
    rx_max: u32,
    tx_max: u32,
    grow_chunk: u32,
    /// `usize` keeps the combined two-direction charge exact on 32-bit targets.
    charged: usize,
    pool: Option<MemoryPool>,
}

impl DynamicBufferState {
    pub(super) fn new(config: DynamicBufferConfig, pool: Option<MemoryPool>) -> Self {
        const PER_DIRECTION_CAP: u32 = 1 << 30;
        let cap =
            |value: u32| usize::try_from(value.min(PER_DIRECTION_CAP)).unwrap_or(usize::MAX) as u32;
        let rx_max = cap(config.rx_max);
        let tx_max = cap(config.tx_max);

        Self {
            rx_initial: cap(config.rx_initial).min(rx_max),
            tx_initial: cap(config.tx_initial).min(tx_max),
            rx_max,
            tx_max,
            grow_chunk: cap(config.grow_chunk).max(1),
            charged: 0,
            pool,
        }
    }

    pub(super) fn rx_capacity_max(&self) -> usize {
        self.rx_max as usize
    }

    pub(super) fn tx_capacity_max(&self) -> usize {
        self.tx_max as usize
    }

    pub(super) fn should_grow_rx(
        &self,
        window: usize,
        remote_mss: usize,
        scale_granule: usize,
    ) -> bool {
        window
            < (self.grow_chunk as usize)
                .max(remote_mss)
                .max(scale_granule)
    }

    pub(super) fn should_grow_tx(&self, window: usize) -> bool {
        window < self.grow_chunk as usize
    }

    pub(super) fn can_grow_tx(&self, current_capacity: usize) -> bool {
        let Some(need) = self.growth_increment(current_capacity, self.tx_capacity_max()) else {
            return false;
        };
        self.pool
            .as_ref()
            .is_none_or(|pool| pool.available() >= need)
    }

    pub(super) fn allocate_initial(&mut self) -> (Vec<u8>, Vec<u8>) {
        self.allocate_initial_with(try_zeroed_socket_storage)
    }

    fn allocate_initial_with<F>(&mut self, mut allocate: F) -> (Vec<u8>, Vec<u8>)
    where
        F: FnMut(usize) -> Option<Vec<u8>>,
    {
        let rx_initial = self.rx_initial as usize;
        let tx_initial = self.tx_initial as usize;
        let Some(charge) = combined_initial_capacity(rx_initial, tx_initial) else {
            return (Vec::new(), Vec::new());
        };
        if !self.charge(charge) {
            return (Vec::new(), Vec::new());
        }

        let Some(rx_storage) = allocate(rx_initial) else {
            self.refund(charge);
            return (Vec::new(), Vec::new());
        };
        let Some(tx_storage) = allocate(tx_initial) else {
            drop(rx_storage);
            self.refund(charge);
            return (Vec::new(), Vec::new());
        };
        (rx_storage, tx_storage)
    }

    pub(super) fn restore_initial(
        &mut self,
        rx_buffer: &mut SocketBuffer<'_>,
        tx_buffer: &mut SocketBuffer<'_>,
    ) {
        let rx_initial = self.rx_initial as usize;
        let tx_initial = self.tx_initial as usize;
        let Some(charge) = combined_initial_capacity(rx_initial, tx_initial) else {
            return;
        };
        if charge == 0 || !self.charge(charge) {
            return;
        }

        let restored = (rx_initial == 0 || rx_buffer.try_grow(rx_initial))
            && (tx_initial == 0 || tx_buffer.try_grow(tx_initial));
        if !restored {
            rx_buffer.release_owned();
            tx_buffer.release_owned();
            self.refund(charge);
        }
    }

    pub(super) fn try_grow_rx(&mut self, buffer: &mut SocketBuffer<'_>) -> bool {
        self.try_grow_buffer(buffer, self.rx_capacity_max())
    }

    pub(super) fn try_grow_tx(&mut self, buffer: &mut SocketBuffer<'_>) -> bool {
        self.try_grow_buffer(buffer, self.tx_capacity_max())
    }

    pub(super) fn release(
        &mut self,
        rx_buffer: &mut SocketBuffer<'_>,
        tx_buffer: &mut SocketBuffer<'_>,
    ) {
        rx_buffer.release_owned();
        tx_buffer.release_owned();
        self.refund(self.charged);
    }

    pub(super) fn release_tx(&mut self, tx_buffer: &mut SocketBuffer<'_>) {
        let capacity = tx_buffer.capacity();
        tx_buffer.release_owned();
        self.refund(capacity);
    }

    fn try_grow_buffer(&mut self, buffer: &mut SocketBuffer<'_>, max: usize) -> bool {
        let current = buffer.capacity();
        let Some(need) = self.growth_increment(current, max) else {
            return false;
        };
        if !self.charge(need) {
            return false;
        }

        if !buffer.try_grow(current + need) {
            self.refund(need);
            return false;
        }
        true
    }

    fn growth_increment(&self, current: usize, max: usize) -> Option<usize> {
        let pressure = self.pool.as_ref().is_some_and(MemoryPool::under_pressure);
        next_capacity(current, self.grow_chunk as usize, max, pressure)
            .checked_sub(current)
            .filter(|increment| *increment > 0)
    }

    fn charge(&mut self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        if let Some(pool) = &self.pool
            && !pool.try_charge(bytes)
        {
            return false;
        }
        self.charged = self.charged.saturating_add(bytes);
        true
    }

    fn refund(&mut self, bytes: usize) {
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

fn next_capacity(current: usize, chunk: usize, max: usize, pressure: bool) -> usize {
    if current >= max {
        return current;
    }
    let target = if pressure {
        current.saturating_add(chunk)
    } else {
        current.saturating_add(chunk).max(current.saturating_mul(2))
    };
    target.min(max)
}

fn combined_initial_capacity(rx_initial: usize, tx_initial: usize) -> Option<usize> {
    rx_initial.checked_add(tx_initial)
}

fn try_zeroed_socket_storage(capacity: usize) -> Option<Vec<u8>> {
    let mut storage = Vec::new();
    if storage.try_reserve_exact(capacity).is_err() {
        return None;
    }
    storage.resize(capacity, 0);
    Some(storage)
}

/// Drop is the backstop for sockets removed without an explicit release.
impl Drop for DynamicBufferState {
    fn drop(&mut self) {
        self.refund(self.charged);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn config(initial: u32, max: u32, grow_chunk: u32) -> DynamicBufferConfig {
        DynamicBufferConfig::symmetric(initial, max, grow_chunk)
    }

    #[test]
    fn state_normalizes_configuration() {
        let state = DynamicBufferState::new(
            DynamicBufferConfig {
                rx_initial: 8192,
                rx_max: 4096,
                tx_initial: u32::MAX,
                tx_max: u32::MAX,
                grow_chunk: 0,
            },
            None,
        );

        assert_eq!(state.rx_initial, 4096);
        assert_eq!(state.rx_capacity_max(), 4096);
        assert_eq!(state.tx_initial, 1 << 30);
        assert_eq!(state.tx_capacity_max(), 1 << 30);
        assert_eq!(state.grow_chunk, 1);
    }

    #[test]
    fn initial_first_allocation_failure_short_circuits() {
        let pool = MemoryPool::new(6144);
        let mut state = DynamicBufferState::new(
            DynamicBufferConfig {
                rx_initial: 4096,
                rx_max: 8192,
                tx_initial: 2048,
                tx_max: 4096,
                grow_chunk: 4096,
            },
            Some(pool.clone()),
        );
        let mut calls = 0;

        let (rx_storage, tx_storage) = state.allocate_initial_with(|_| {
            calls += 1;
            None
        });

        assert_eq!(calls, 1);
        assert!(rx_storage.is_empty());
        assert!(tx_storage.is_empty());
        assert_eq!(state.charged, 0);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn initial_second_allocation_failure_rolls_back_reservation() {
        let pool = MemoryPool::new(6144);
        let mut state = DynamicBufferState::new(
            DynamicBufferConfig {
                rx_initial: 4096,
                rx_max: 8192,
                tx_initial: 2048,
                tx_max: 4096,
                grow_chunk: 4096,
            },
            Some(pool.clone()),
        );
        let mut calls = 0;

        let (rx_storage, tx_storage) = state.allocate_initial_with(|capacity| {
            calls += 1;
            (calls == 1).then(|| vec![0; capacity])
        });

        assert_eq!(calls, 2);
        assert!(rx_storage.is_empty());
        assert!(tx_storage.is_empty());
        assert_eq!(state.charged, 0);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn restoration_failure_rolls_back_reservation() {
        let pool = MemoryPool::new(6144);
        let mut state = DynamicBufferState::new(
            DynamicBufferConfig {
                rx_initial: 4096,
                rx_max: 8192,
                tx_initial: 2048,
                tx_max: 4096,
                grow_chunk: 4096,
            },
            Some(pool.clone()),
        );
        let mut tx_storage = [];
        let mut rx_buffer = SocketBuffer::new(Vec::<u8>::new());
        let mut tx_buffer = SocketBuffer::new(&mut tx_storage[..]);

        state.restore_initial(&mut rx_buffer, &mut tx_buffer);

        assert_eq!(rx_buffer.capacity(), 0);
        assert_eq!(tx_buffer.capacity(), 0);
        assert_eq!(state.charged, 0);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn growth_failure_rolls_back_increment() {
        let pool = MemoryPool::new(4096);
        let mut state = DynamicBufferState::new(config(0, 4096, 4096), Some(pool.clone()));
        let mut storage = [];
        let mut buffer = SocketBuffer::new(&mut storage[..]);

        assert!(!state.try_grow_rx(&mut buffer));
        assert_eq!(buffer.capacity(), 0);
        assert_eq!(state.charged, 0);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn next_capacity_math() {
        assert_eq!(next_capacity(0, 4096, 65536, false), 4096);
        assert_eq!(next_capacity(4096, 4096, 65536, false), 8192);
        assert_eq!(next_capacity(8192, 4096, 65536, false), 16384);
        assert_eq!(next_capacity(32768, 4096, 65536, false), 65536);
        assert_eq!(next_capacity(65536, 4096, 65536, false), 65536);
        assert_eq!(next_capacity(8192, 4096, 65536, true), 12288);
        assert_eq!(next_capacity(32768, 4096, 65536, true), 36864);
        assert_eq!(next_capacity(60000, 4096, 65536, true), 64096);
    }

    #[test]
    fn impossible_initial_allocation_is_fallible() {
        assert!(try_zeroed_socket_storage(usize::MAX).is_none());
    }

    #[test]
    fn accounting_supports_combined_direction_capacity() {
        let pool = MemoryPool::new(2 * 1024 * 1024 * 1024);
        let mut state = DynamicBufferState::new(config(0, 1 << 30, 8192), Some(pool.clone()));
        let charge = 2 * 768 * 1024 * 1024;

        assert!(state.charge(charge));
        assert_eq!(state.charged, charge);
        assert_eq!(pool.used(), charge);

        state.refund(charge);
        assert_eq!(state.charged, 0);
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn combined_initial_capacity_overflow_is_rejected() {
        assert_eq!(combined_initial_capacity(usize::MAX, 1), None);
    }

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
