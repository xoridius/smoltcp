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

    fn on_ack(&mut self, now: Instant, len: usize, in_flight: usize, rtt: &RttEstimator) {}

    /// Fired on each duplicate ack received, after `on_loss` has been called.
    fn on_dup_ack(&mut self, now: Instant, len: usize, in_flight: usize) {}

    /// Fired on a retransmission timeout.
    fn on_rto(&mut self, now: Instant, in_flight: usize) {}

    /// Fired after an inferred loss via three duplicate acks.
    fn on_loss(&mut self, now: Instant, in_flight: usize) {}

    fn pre_transmit(&mut self, now: Instant) {}

    fn post_transmit(&mut self, now: Instant, len: usize) {}

    /// Set the maximum segment size.
    fn set_mss(&mut self, mss: usize) {}
}

#[cfg(any(test, feature = "socket-tcp-reno", feature = "socket-tcp-cubic"))]
#[inline]
pub(super) fn initial_window(mss: u32) -> u32 {
    mss.saturating_mul(10)
        .min(mss.saturating_mul(2).max(14_600))
}

#[inline]
pub(super) fn window_to_usize(window: u32) -> usize {
    usize::try_from(window).unwrap_or(usize::MAX)
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

    /// Reset the selected controller for a new TCP control block.
    pub fn reset(&mut self) {
        match self {
            AnyController::None(_) => *self = AnyController::None(no_control::NoControl),
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(_) => *self = AnyController::Reno(reno::Reno::new()),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(_) => *self = AnyController::Cubic(cubic::Cubic::new()),
        }
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

    /// Whether this controller actively manages the send window. `false`
    /// for `NoControl`, where the consumer has opted out of bandwidth
    /// management entirely — loss recovery may then use opportunistic
    /// redundancy, since nothing else bounds the pipe.
    #[inline]
    pub fn manages_window(&self) -> bool {
        !matches!(self, AnyController::None(_))
    }

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
    pub fn on_ack(&mut self, now: Instant, len: usize, in_flight: usize, rtt: &RttEstimator) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.on_ack(now, len, in_flight, rtt),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_ack(now, len, in_flight, rtt),
        }
    }

    #[inline]
    pub fn on_dup_ack(&mut self, now: Instant, len: usize, in_flight: usize) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.on_dup_ack(now, len, in_flight),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_dup_ack(now, len, in_flight),
        }
    }

    #[inline]
    pub fn on_rto(&mut self, now: Instant, in_flight: usize) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.on_rto(now, in_flight),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_rto(now, in_flight),
        }
    }

    #[inline]
    pub fn on_loss(&mut self, now: Instant, in_flight: usize) {
        match self {
            AnyController::None(_) => {}
            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(r) => r.on_loss(now, in_flight),
            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(c) => c.on_loss(now, in_flight),
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn initial_window_matches_rfc_6928_and_saturates() {
        for (mss, expected) in [
            (0, 0),
            (48, 480),
            (536, 5_360),
            (1_460, 14_600),
            (8_960, 17_920),
            (u32::MAX, u32::MAX),
        ] {
            assert_eq!(initial_window(mss), expected, "MSS {mss}");
        }
    }

    #[test]
    fn managed_window_conversion_saturates_to_usize() {
        #[cfg(target_pointer_width = "16")]
        assert_eq!(window_to_usize(u32::MAX), usize::MAX);

        #[cfg(not(target_pointer_width = "16"))]
        assert_eq!(window_to_usize(u32::MAX), u32::MAX as usize);
    }

    #[test]
    fn no_control_reset_preserves_variant() {
        let mut controller = AnyController::None(no_control::NoControl);

        controller.reset();

        assert!(matches!(controller, AnyController::None(_)));
        assert_eq!(controller.window(), usize::MAX);
    }
}
