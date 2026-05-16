use crate::time::Instant;

use super::RttEstimator;

pub(super) mod no_control;

#[cfg(feature = "socket-tcp-cubic")]
pub(super) mod cubic;

#[cfg(feature = "socket-tcp-reno")]
pub(super) mod reno;

#[allow(unused_variables, dead_code)]
pub(super) trait Controller {
    /// Returns the number of bytes that can be sent.
    fn window(&self) -> usize;

    /// Set the remote window size.
    fn set_remote_window(&mut self, remote_window: usize) {}

    fn on_ack(&mut self, now: Instant, len: usize, rtt: &RttEstimator) {}

    fn on_retransmit(&mut self, now: Instant) {}

    fn on_duplicate_ack(&mut self, now: Instant) {}

    fn pre_transmit(&mut self, now: Instant) {}

    fn post_transmit(&mut self, now: Instant, len: usize) {}

    /// Set the maximum segment size.
    fn set_mss(&mut self, mss: usize) {}
}

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(super) enum AnyController {
    None(no_control::NoControl),

    #[cfg(feature = "socket-tcp-reno")]
    Reno(reno::Reno),

    #[cfg(feature = "socket-tcp-cubic")]
    Cubic(cubic::Cubic),
}

// Unused parameters in the `None` arm of every wrapper below — the
// real controllers behind cfg-gated arms do use them.
#[allow(unused_variables)]
impl AnyController {
    /// Create a new congestion controller.
    /// `AnyController::new()` selects the best congestion controller based on the features.
    ///
    /// - If `socket-tcp-cubic` feature is enabled, it will use `Cubic`.
    /// - If `socket-tcp-reno` feature is enabled, it will use `Reno`.
    /// - If both `socket-tcp-cubic` and `socket-tcp-reno` features are enabled, it will use `Cubic`.
    ///    - `Cubic` is more efficient regarding throughput.
    ///    - `Reno` is more conservative and is suitable for low-power devices.
    /// - If no congestion controller is available, it will use `NoControl`.
    ///
    /// Users can also select a congestion controller manually by [`super::Socket::set_congestion_control()`]
    /// method at run-time.
    #[allow(unreachable_code)]
    #[inline]
    pub fn new() -> Self {
        #[cfg(feature = "socket-tcp-cubic")]
        {
            return AnyController::Cubic(cubic::Cubic::new());
        }

        #[cfg(feature = "socket-tcp-reno")]
        {
            return AnyController::Reno(reno::Reno::new());
        }

        AnyController::None(no_control::NoControl)
    }

    // Static-dispatch wrappers for the hot-path `Controller` methods.
    //
    // Routing through `&dyn Controller` forced every call (e.g. `window()`
    // inside `seq_to_transmit`, which runs per packet) to indirect through a
    // v-table. With direct match-on-self the optimizer monomorphizes the call
    // and, in the common single-variant builds, eliminates the match outright.
    //
    // Parameters are unused in the `None` arm when no real controller is
    // compiled in, hence the impl-level `unused_variables` allow.

    #[inline]
    pub fn window(&self) -> usize {
        match self {
            AnyController::None(_) => usize::MAX,
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.window(),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.window(),
        }
    }

    #[inline]
    pub fn set_remote_window(&mut self, remote_window: usize) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.set_remote_window(remote_window),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.set_remote_window(remote_window),
        }
    }

    #[inline]
    pub fn on_ack(&mut self, now: Instant, len: usize, rtt: &RttEstimator) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.on_ack(now, len, rtt),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_ack(now, len, rtt),
        }
    }

    #[inline]
    pub fn on_retransmit(&mut self, now: Instant) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.on_retransmit(now),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_retransmit(now),
        }
    }

    #[inline]
    pub fn on_duplicate_ack(&mut self, now: Instant) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.on_duplicate_ack(now),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_duplicate_ack(now),
        }
    }

    #[inline]
    pub fn pre_transmit(&mut self, now: Instant) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.pre_transmit(now),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.pre_transmit(now),
        }
    }

    #[inline]
    pub fn post_transmit(&mut self, now: Instant, len: usize) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.post_transmit(now, len),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.post_transmit(now, len),
        }
    }

    #[inline]
    pub fn set_mss(&mut self, mss: usize) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.set_mss(mss),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.set_mss(mss),
        }
    }
}
