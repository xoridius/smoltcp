use crate::{socket::tcp::RttEstimator, time::Instant};

use super::{Controller, initial_window, window_to_usize};

// Constants for the Cubic congestion control algorithm.
// See RFC 9438.
const BETA_CUBIC: f64 = 0.7;
const C: f64 = 0.4;
// RFC 9438 §4.3: α_cubic = 3(1-β)/(1+β). ~0.5294 for β=0.7.
const ALPHA_CUBIC: f64 = 3.0 * (1.0 - BETA_CUBIC) / (1.0 + BETA_CUBIC);

const DEFAULT_MSS: u32 = 1024;

#[inline]
fn window_to_u32(window: usize) -> u32 {
    window.min(u32::MAX as usize) as u32
}

#[inline]
fn f64_to_window(window: f64) -> u32 {
    if window <= 0.0 {
        0
    } else if window >= u32::MAX as f64 {
        u32::MAX
    } else {
        window as u32
    }
}

#[inline]
fn beta_window(in_flight: usize, mss: u32) -> u32 {
    f64_to_window(in_flight as f64 * BETA_CUBIC).max(mss.saturating_mul(2))
}

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Cubic {
    k: f64,     // cubic curve offset in seconds; depends only on w_max and mss
    w_est: f64, // RFC 9438 §4.3 reno-friendly window, integrated per ACK
    recovery_start: Option<Instant>,
    idle_start: Option<Instant>, // RFC 9438 §4.2: when in-flight last hit 0

    w_max: u32, // window size prior to loss
    cwnd: u32,
    mss: u32,
    ssthresh: u32,
    rwnd: u32,
    cwnd_prior: u32, // cwnd at the most recent congestion event; gates α_cubic

    in_fast_recovery: bool,
    // Set on RTO, cleared when new data is ACKed. While set, further RTOs
    // are retransmissions of the same segment and must not reduce ssthresh
    // again (RFC 5681 section 3.1).
    in_rto_recovery: bool,
}

impl Cubic {
    pub fn new() -> Cubic {
        let mut cubic = Cubic {
            k: 0.0,
            w_est: (DEFAULT_MSS * 2) as f64,
            recovery_start: None,
            idle_start: None,

            w_max: DEFAULT_MSS * 2,
            cwnd: DEFAULT_MSS * 2,
            mss: DEFAULT_MSS,
            rwnd: 64 * DEFAULT_MSS,
            ssthresh: u32::MAX,
            cwnd_prior: DEFAULT_MSS * 2,

            in_fast_recovery: false,
            in_rto_recovery: false,
        };
        cubic.recompute_k();
        cubic
    }

    // K = cbrt(w_max * (1 - beta) / C) ^ 1/3
    fn recompute_k(&mut self) {
        let c_as_bytes = C * self.mss as f64;
        let k3 = (self.w_max as f64) * (1.0 - BETA_CUBIC) / c_as_bytes;
        self.k = cube_root(k3);
    }

    // RFC 9438 §4.2: subtract the most recent idle period from t by sliding
    // recovery_start forward by the idle duration.
    fn absorb_idle(&mut self, now: Instant) {
        if let (Some(idle), Some(start)) = (self.idle_start, self.recovery_start)
            && now >= idle
        {
            self.recovery_start = Some(start + (now - idle));
        }
        self.idle_start = None;
    }
}

#[inline]
fn cube(value: f64) -> f64 {
    value * value * value
}

#[inline]
fn cubic_increment(target: usize, cwnd: usize, segment: usize) -> usize {
    if cwnd == 0 {
        return 0;
    }

    let delta = target.saturating_sub(cwnd);
    let increment = (delta as u128 * segment as u128) / cwnd as u128;
    increment.min(usize::MAX as u128) as usize
}

impl Controller for Cubic {
    fn window(&self) -> usize {
        window_to_usize(self.cwnd)
    }

    fn on_ack(&mut self, now: Instant, len: usize, in_flight: usize, rtt: &RttEstimator) {
        let segment = len.min(self.mss as usize);

        self.absorb_idle(now);

        if in_flight == 0 {
            self.idle_start = Some(now);
        }

        // RFC 9438 only acts on ACKs of new data. The socket also notifies us
        // of accepted segments that acknowledge nothing (window updates, data
        // segments from the remote): those must not exit fast recovery nor
        // grow the window.
        if len == 0 {
            return;
        }

        // New data was ACKed: a timer-based loss episode, if any, is over.
        self.in_rto_recovery = false;

        // First new-data-ack exits fast recovery and deflates `cwnd`
        if self.in_fast_recovery {
            self.in_fast_recovery = false;
            self.cwnd = self.ssthresh;
            self.w_est = self.cwnd as f64;
            return;
        } else if self.cwnd < self.ssthresh {
            // Slow start: increase `cwnd` by 1 MSS per ACK.
            self.cwnd = self
                .cwnd
                .saturating_add(window_to_u32(segment))
                .min(self.rwnd)
                .max(self.mss);
            return;
        }

        // ca: RFC 9438 §4.2 and §4.3: Calculate W_cubic and W_est. Use whichever grows faster.
        let recovery_start = match self.recovery_start {
            Some(t) => t,
            None => {
                // RFC 9438 §4.8: set W_max = cwnd and K = 0 at start of CA
                self.w_max = self.cwnd;
                self.k = 0.0;
                self.w_est = self.cwnd as f64;
                self.recovery_start = Some(now);
                now
            }
        };

        // Elapsed time since the start of the recovery phase, in microseconds so the
        // cubic curve still advances between ACKs on sub-millisecond-RTT links.
        let t = now.total_micros() - recovery_start.total_micros();
        if t < 0 {
            return;
        }

        // RFC 9438 §4.3: use cubic function to get suggested cwnd.
        // W_cubic(t) = C(t - K)^3 + w_max, evaluated at the current time t.
        let c_as_bytes = C * self.mss as f64;
        let w_cubic = c_as_bytes * cube(t as f64 / 1_000_000.0 - self.k) + self.w_max as f64;

        // RFC 9438 §4.3: advance our reno-like suggested cwnd.
        // When cwnd exceeds prior cwnd, change α_cubic to match Reno's AIMD.
        let w_est = {
            let alpha = if self.w_est >= self.cwnd_prior as f64 {
                1.0
            } else {
                ALPHA_CUBIC
            };

            self.w_est += alpha * self.mss as f64 * segment as f64 / self.cwnd as f64;
            self.w_est
        };

        // RFC 9438 §4.3: use the suggested window that grows fastest.
        if w_cubic < w_est {
            self.cwnd = f64_to_window(w_est).min(self.rwnd).max(self.mss);
            return;
        }

        // RFC 9438 §4.2: the congestion window target is W_cubic one RTT into the future.
        let w_cubic_target = {
            // srtt is in millis so floor at 1ms to ensure sub-ms RTTs don't ruin the lookahead.
            let srtt = (rtt.smoothed_rtt() as u64 * 1000).max(1000);

            let t_ahead = (t as f64 + srtt as f64) / 1_000_000.0;
            let raw = c_as_bytes * cube(t_ahead - self.k) + self.w_max as f64;
            raw.min(1.5 * self.cwnd as f64) // clamp to avoid increasing faster than slow-start would
        };

        // TODO: clamps to 0 on small w_cubic_target (i.e. close to plateau)
        // add additional counter (linux `cwnd_cnt`?) to track "lost" bytes
        let increment =
            cubic_increment(w_cubic_target as usize, window_to_usize(self.cwnd), segment);
        self.cwnd = self
            .cwnd
            .saturating_add(window_to_u32(increment))
            .min(self.rwnd)
            .max(self.mss);
    }

    fn on_dup_ack(&mut self, _now: Instant, len: usize, _in_flight: usize) {
        if self.in_fast_recovery {
            self.cwnd = self
                .cwnd
                .saturating_add(window_to_u32(len))
                .min(self.rwnd)
                .max(self.mss);
        }
    }

    fn post_transmit(&mut self, now: Instant, _len: usize) {
        self.absorb_idle(now);
    }

    fn on_loss(&mut self, now: Instant, in_flight: usize) {
        self.idle_start = None;
        // Only cut window size on first entrance to fast recovery.
        if !self.in_fast_recovery {
            // RFC 9438 §4.3: remember the cwnd at this congestion event so W_est can
            // detect when it has recovered and switch α_cubic to 1.
            self.cwnd_prior = self.cwnd;

            // TODO: Make this optional?
            // RFC recommends (SHOULD) disabling if only a single CUBIC flow is on a network.
            //
            // RFC 9483.4.7: Fast Convergence
            // If loss happened at a smaller cwnd than before, it indicates a new flow.
            // Reduce the cubic plateau more than usual to create headroom.
            self.w_max = if self.cwnd < self.w_max {
                f64_to_window((self.cwnd as f64) * (1.0 + BETA_CUBIC) / 2.0)
            } else {
                self.cwnd
            };

            self.ssthresh = beta_window(in_flight, self.mss);
            self.cwnd = self
                .ssthresh
                .min(self.rwnd)
                .saturating_add(self.mss.saturating_mul(3));

            self.recovery_start = Some(now);
            self.in_fast_recovery = true;
            self.recompute_k();
        }
    }

    fn on_rto(&mut self, _now: Instant, in_flight: usize) {
        // RFC 5681: when the retransmission timer fires for a segment that has
        // already been retransmitted by the timer (no new data was ACKed since
        // the previous RTO), ssthresh is held constant.
        if !self.in_rto_recovery {
            self.ssthresh = beta_window(in_flight, self.mss);
            self.in_rto_recovery = true;
        }

        self.cwnd = self.mss;
        self.cwnd_prior = window_to_u32(in_flight);

        // RFC 9438 §4.8: defer W_max and K reset to the start of the next CA stage.
        self.recovery_start = None;
        self.in_fast_recovery = false;
        self.idle_start = None;
    }

    fn set_mss(&mut self, mss: usize) {
        let mss = window_to_u32(mss);
        self.mss = mss;
        // Fork delta (FORK.md §16): open the window at RFC 6928 IW10 —
        // min(10*MSS, max(2*MSS, 14600)) — when the peer's MSS is learned on
        // the SYN, rather than upstream's flat 2*MSS. Faster first-RTT ramp for
        // short flows.
        let initial_window = initial_window(mss);
        self.cwnd = initial_window;
        self.w_max = initial_window;
        self.w_est = initial_window as f64;
        self.cwnd_prior = initial_window;
        self.recompute_k();
    }

    fn set_remote_window(&mut self, remote_window: usize) {
        let remote_window = window_to_u32(remote_window);
        if self.rwnd < remote_window {
            self.rwnd = remote_window;
        }
    }
}

/// Efficient cube root using f64 bit tricks and Newton-Raphson.
///
/// a = mantissa * 2^e
///
/// cbrt(a) = cbrt(mantissa * 2^e)
/// cbrt(a) = cbrt(mantissa) * cbrt(2^e)
/// -> cbrt(2^e) = 2^(e/3)
///     -> e = 3q + r
///   -> 2^(e/3) = 2^(3q + r)
///   -> 2^(e/3) = 2^q * 2^(r/3)
/// cbrt(a) = cbrt(mantissa) * 2^q * 2^(r/3)
///
/// Floats are constructed from a mantissa and expontnet component.
/// Cbrt of a float can be achieved by cbrt of these two components.
///
/// The mantissa is always between 1 and 2, putting the cbrt between 1
/// and 1.2599. The slope of the curve between those two points is
/// almost a straight line, so a linear interpolatoin between those
/// two points gives an error less than 2%.
///
///
/// The exponent `e` of `2^e` has two parts, tt's quotient and remainder: `e = 3q + r`.
/// Which means cbrt `2^e` is cbrt `2^3q * 2^r` which becomes `2^q * 2^(r/3)`.
///
/// The remainder operation means `r` can only be 0, 1, or 2, so we can calaculate
/// `2^(r/3)` ahead of time.
///
/// Multiplying everything gets us a pretty close answer to the true cbrt.
/// `cbrt(a) = cbrt(mantissa) * 2^q * 2^(r/3)`
///
/// One or two Newton-Raphson iterations reduce any error enough not to matter.
fn cube_root(a: f64) -> f64 {
    if !(a >= f64::MIN_POSITIVE && a.is_finite()) {
        return 0.0;
    }

    const POW2_REM_OVER_3: [f64; 3] = [1.0, 1.2599210498948732, 1.5874010519681994];

    // decompose a into IEEE-754 components
    let bits = a.to_bits();

    // extract mantissa, get rough cbrt using linear interpolation
    let m = f64::from_bits((bits & 0x000F_FFFF_FFFF_FFFF) | 0x3FF0_0000_0000_0000);
    let cbrt_m = m * 0.2599 + 0.7401;

    // extract exponent, break into quotient and remainder
    // e = 3q + r where r ∈ {0, 1, 2}
    let e = (((bits >> 52) & 0x7FF) as i64) - 1023;
    let q = e.div_euclid(3);
    let r = e.rem_euclid(3) as usize;

    // calculate 2^q efficiently by constructing f64 from bits
    let pow2q = f64::from_bits(((q + 1023) as u64) << 52);

    // add cbrt mantissa and other component back in:
    // cbrt mantiassa * 2^q * 2^(r/3)
    let x = cbrt_m * pow2q * POW2_REM_OVER_3[r];

    // a single iteration should bring us close enough
    (2.0 * x + a / (x * x)) / 3.0
}

#[cfg(test)]
mod test {
    use crate::socket::tcp::congestion::AnyController;
    use crate::{socket::tcp::RttEstimator, time::Instant};

    use super::*;

    const MSS: usize = 1024;

    fn ack(cubic: &mut Cubic, len: usize, now: Instant) {
        cubic.on_ack(now, len, cubic.window().saturating_sub(MSS), &rtte())
    }

    fn rtte() -> RttEstimator {
        RttEstimator::default()
    }

    fn stored(window: usize) -> u32 {
        window_to_u32(window)
    }

    #[test]
    fn congestion_avoidance_works() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);
        cubic.w_max = stored(MSS * 32);
        cubic.recompute_k();

        // Post-fast-recovery state: cwnd = ssthresh ≈ w_max * beta.
        cubic.cwnd = stored((MSS * 32 * 7) / 10);
        cubic.ssthresh = cubic.cwnd;
        cubic.recovery_start = Some(Instant::from_millis(0));

        // CA at small time intervals should grow by less than 1 MSS per ACK.
        for i in 1..10 {
            let initial_cwnd = cubic.window();
            ack(&mut cubic, MSS, Instant::from_millis(i));
            assert!(cubic.window() < initial_cwnd + MSS);
        }

        // CA approaches w_max as t approaches K, and exceeds it past K.
        let pre = cubic.window();
        for i in 0..60 {
            ack(&mut cubic, MSS, Instant::from_millis(i * 100));
        }
        assert!(cubic.window() >= cubic.w_max as usize);
        assert!(cubic.window() > pre);

        // RFC 9438 §4.2: the target is clamped to 1.5 * cwnd
        let pre = cubic.window();
        ack(&mut cubic, MSS, Instant::from_millis(100_000));
        assert!(cubic.window() <= pre + MSS);

        // CA should still cap at the receive window once enough ACKs accrue.
        for i in 0..200 {
            ack(&mut cubic, MSS, Instant::from_millis(100_000 + i * 100));
        }
        assert_eq!(cubic.window(), cubic.rwnd as usize);
    }

    #[test]
    fn fast_recovery_works() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);
        cubic.cwnd = stored(MSS * 32);

        // duplicate ACKs before fast recovery should do nothing
        let initial_cwnd = cubic.window();
        for _ in 0..3 {
            cubic.on_dup_ack(Instant::from_millis(0), MSS, initial_cwnd);
        }
        assert_eq!(cubic.window(), initial_cwnd);

        // we enter fast recovery upon minor loss (three duplicate ACKs).
        // ssthresh = flight_size * beta_cubic, cwnd = ssthresh + 3*MSS, recovery_start = now.
        // w_max = cwnd since the prior w_max (initial 2*MSS) is below cwnd.
        let in_flight = initial_cwnd / 2;
        let expected_ssthresh = (in_flight as f64 * BETA_CUBIC) as usize;
        cubic.on_loss(Instant::from_millis(0), in_flight);
        assert_eq!(cubic.ssthresh as usize, expected_ssthresh);
        assert_eq!(cubic.cwnd as usize, expected_ssthresh + 3 * MSS);
        assert_eq!(cubic.w_max as usize, initial_cwnd);
        assert!(cubic.in_fast_recovery);
        assert_eq!(cubic.recovery_start, Some(Instant::from_millis(0)));

        // in fast recovery, each dup-ACK should increase the cwnd by 1 MSS
        let initial_cwnd = cubic.window();
        for i in 0..3 {
            for _ in 0..3 {
                let initial_cwnd = cubic.window();
                cubic.on_dup_ack(Instant::from_millis(i), MSS, initial_cwnd);
                assert_eq!(cubic.window(), initial_cwnd + MSS);
            }

            // multiple loss events (trip-dup-ack) should not trigger additional fast recovery reductions
            let initial_cwnd = cubic.window();
            let initial_ssthresh = cubic.ssthresh;
            let initial_w_max = cubic.w_max;
            cubic.on_loss(Instant::from_millis(i), initial_cwnd);
            assert_eq!(cubic.window(), initial_cwnd);
            assert_eq!(cubic.ssthresh, initial_ssthresh);
            assert_eq!(cubic.w_max, initial_w_max);
        }
        assert_eq!(cubic.window(), initial_cwnd + MSS * 9);

        // a non-duplicate ACK exits fast recovery and deflates cwnd to ssthresh
        ack(&mut cubic, MSS, Instant::from_millis(10));
        assert_eq!(cubic.window(), cubic.ssthresh as usize);
        assert!(!cubic.in_fast_recovery);
    }

    #[test]
    fn zero_length_ack_does_not_exit_fast_recovery() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);
        cubic.cwnd = stored(MSS * 32);

        cubic.on_loss(Instant::from_millis(0), cubic.cwnd as usize);
        assert!(cubic.in_fast_recovery);

        let cwnd = cubic.window();
        let ssthresh = cubic.ssthresh;

        // Accepted segments that acknowledge no new data (window updates,
        // data segments from the remote) must not end fast recovery or
        // change the window.
        ack(&mut cubic, 0, Instant::from_millis(1));
        assert!(cubic.in_fast_recovery);
        assert_eq!(cubic.window(), cwnd);
        assert_eq!(cubic.ssthresh, ssthresh);

        // The first ACK of new data still exits and deflates.
        ack(&mut cubic, MSS, Instant::from_millis(2));
        assert!(!cubic.in_fast_recovery);
        assert_eq!(cubic.window(), ssthresh as usize);
    }

    #[test]
    fn repeated_rto_holds_ssthresh() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);
        cubic.cwnd = stored(MSS * 32);

        // First RTO reduces ssthresh based on the flight size.
        cubic.on_rto(Instant::from_millis(0), MSS * 32);
        let ssthresh = cubic.ssthresh;
        assert_eq!(ssthresh as usize, (32.0 * MSS as f64 * BETA_CUBIC) as usize);

        // Until new data is ACKed, further RTOs are retransmissions of the
        // same segment and must hold ssthresh constant instead of collapsing
        // it towards the minimum.
        cubic.on_rto(Instant::from_millis(1), MSS);
        assert_eq!(cubic.ssthresh, ssthresh);

        // Once new data is ACKed, the next RTO is a fresh loss detection
        // and reduces ssthresh again.
        ack(&mut cubic, MSS, Instant::from_millis(2));
        cubic.on_rto(Instant::from_millis(3), MSS * 4);
        assert_eq!(
            cubic.ssthresh as usize,
            (4.0 * MSS as f64 * BETA_CUBIC) as usize
        );
    }

    #[test]
    fn slow_start_works() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);
        cubic.cwnd = stored(MSS * 32);
        cubic.ssthresh = stored(MSS * 16);

        // we enter slow start upon major loss (an RTO)
        // window resets to MSS, ssthresh becomes a fraction of the inflight bytes,
        // recovery_start is cleared so any later CA uses a fresh epoch,
        // and w_max is preserved (RFC 9438 §4.8 defers it to the next CA stage).
        let w_max_before_rto = cubic.w_max;
        let inflight = cubic.window();
        cubic.on_rto(Instant::from_millis(0), inflight);
        assert_eq!(
            cubic.ssthresh as usize,
            (inflight as f64 * BETA_CUBIC) as usize
        );
        assert_eq!(cubic.window(), MSS);
        assert!(!cubic.in_fast_recovery);
        assert_eq!(cubic.recovery_start, None);
        assert_eq!(cubic.w_max, w_max_before_rto);

        // slow start grows by at most the MSS per ack
        let initial_cwnd = cubic.window();
        for i in 0..10 {
            let initial_cwnd = cubic.window();
            let now = Instant::from_millis(i);
            ack(&mut cubic, MSS * 2, now);
            assert_eq!(cubic.window(), initial_cwnd + MSS);
        }
        assert_eq!(cubic.window(), initial_cwnd + MSS * 10);

        // slow start uses the number of ACKed bytes if they're less than the MSS
        let initial_cwnd = cubic.window();
        for i in 0..10 {
            let initial_cwnd = cubic.window();
            let now = Instant::from_millis(10 + i);
            ack(&mut cubic, MSS / 2, now);
            assert_eq!(cubic.window(), initial_cwnd + MSS / 2);
        }
        assert_eq!(cubic.window(), initial_cwnd + MSS / 2 * 10);

        // slow start transitions to congestion avoidance at ssthresh
        let initial_cwnd = cubic.window();
        cubic.ssthresh = stored(initial_cwnd + MSS);
        ack(&mut cubic, MSS, Instant::from_millis(30));
        assert_eq!(cubic.window(), initial_cwnd + MSS);
        assert_eq!(cubic.ssthresh as usize, initial_cwnd + MSS);
    }

    #[test]
    fn progress_to_ca_via_rto() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);

        let mut time = 0;

        // slow start from default state
        let initial_cwnd = cubic.window();
        for _ in 0..30 {
            time += 1;
            ack(&mut cubic, MSS, Instant::from_millis(time));
        }
        assert_eq!(cubic.window(), initial_cwnd + MSS * 30);
        assert!(cubic.window() < cubic.ssthresh as usize);

        // rto: cwnd resets to MSS and sstresh reduces
        let rto_cwnd = cubic.window();
        cubic.on_rto(Instant::from_millis(time), rto_cwnd);
        assert_eq!(cubic.window(), MSS);
        assert_eq!(
            cubic.ssthresh as usize,
            (rto_cwnd as f64 * BETA_CUBIC) as usize
        );

        // slow start again until cwnd reaches new ssthresh
        while cubic.window() < cubic.ssthresh as usize {
            time += 1;
            let initial_cwnd = cubic.window();
            ack(&mut cubic, MSS, Instant::from_millis(time));
            assert_eq!(cubic.window(), initial_cwnd + MSS);
        }
        assert!(cubic.window() >= cubic.ssthresh as usize);
        assert!(cubic.window() < cubic.ssthresh as usize + MSS);

        // ca: first CA ACK starts a fresh epoch with W_max = cwnd and K = 0.
        time += 1;
        let cwnd_at_ca_entry = cubic.window();
        ack(&mut cubic, MSS, Instant::from_millis(time));
        assert_eq!(cubic.w_max as usize, cwnd_at_ca_entry);
        assert_eq!(cubic.k, 0.0);
        assert!(cubic.window() >= cwnd_at_ca_entry);
    }

    #[test]
    fn progress_to_ca_via_loss() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);

        let mut time = 0;

        // slow start from default state
        let initial_cwnd = cubic.window();
        for _ in 0..30 {
            time += 1;
            ack(&mut cubic, MSS, Instant::from_millis(time));
        }
        assert_eq!(cubic.window(), initial_cwnd + MSS * 30);
        assert!(cubic.window() < cubic.ssthresh as usize);

        // dup ACKs: ssthresh = cwnd * beta, cwnd = ssthresh + 3*MSS, recovery_start = now
        time += 1;
        let loss_cwnd = cubic.window();
        let expected_ssthresh = (loss_cwnd as f64 * BETA_CUBIC) as usize;
        cubic.on_loss(Instant::from_millis(time), loss_cwnd);
        assert_eq!(cubic.ssthresh as usize, expected_ssthresh);
        assert_eq!(cubic.window(), expected_ssthresh + 3 * MSS);
        assert!(cubic.in_fast_recovery);
        assert_eq!(cubic.recovery_start, Some(Instant::from_millis(time)));

        // inflate cwnd on each duplicate ACK
        for _ in 0..9 {
            time += 1;
            let initial_cwnd = cubic.window();
            cubic.on_dup_ack(Instant::from_millis(time), MSS, cubic.cwnd as usize);
            assert_eq!(cubic.window(), initial_cwnd + MSS);
        }

        // non-duplicate ACK deflates cwnd to ssthresh
        time += 1;
        ack(&mut cubic, MSS, Instant::from_millis(time));
        assert_eq!(cubic.window(), expected_ssthresh);
        assert!(!cubic.in_fast_recovery);

        // ca: subsequent ACKs follow the cubic curve
        time += 1;
        let initial_cwnd = cubic.window();
        ack(&mut cubic, MSS, Instant::from_millis(time));
        assert!(cubic.window() >= initial_cwnd);
    }

    #[test]
    fn fast_convergence_reduces_w_max() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);
        cubic.w_max = stored(MSS * 50);
        cubic.cwnd = stored(MSS * 30);

        // Loss while cwnd < w_max (a new competing flow) should pull w_max down.
        let w_max_prev = cubic.w_max;
        cubic.on_loss(Instant::from_millis(0), cubic.cwnd as usize);
        assert!(cubic.w_max < w_max_prev);
    }

    #[test]
    fn congestion_avoidance_large_window_does_not_overflow() {
        let mut cubic = Cubic::new();
        cubic.set_mss(MSS);
        cubic.cwnd = u32::MAX / 2;
        cubic.ssthresh = cubic.cwnd;
        cubic.rwnd = u32::MAX;
        cubic.w_max = cubic.cwnd;
        cubic.k = 0.0;
        cubic.w_est = 0.0;
        cubic.recovery_start = Some(Instant::from_millis(0));
        let initial_cwnd = cubic.window();

        cubic.on_ack(
            Instant::from_secs(100_000),
            MSS,
            cubic.cwnd.saturating_sub(stored(MSS)) as usize,
            &rtte(),
        );

        assert!(cubic.window() > initial_cwnd);
        assert!(cubic.window() <= cubic.rwnd as usize);
    }

    #[test]
    fn test_cube_root() {
        let mut max_err = 0.0;
        let mut max_err_at = 0;
        let mut max_err_expected = 0.0;
        let mut max_err_found = 0.0;

        for i in (1..1_000_000).step_by(99) {
            let found = cube_root(i as f64);
            let expected = (i as f64).cbrt();
            let err = (found - expected).abs() / expected;

            if err > max_err {
                max_err = err;
                max_err_at = i;
                max_err_found = found;
                max_err_expected = expected;
            }
        }

        assert!(
            max_err < 0.0005,
            "cube_root({max_err_at}) = {max_err_found}, expected ~{max_err_expected}, rel err {max_err:.3e}"
        );
    }

    // Fork delta (FORK.md §16): set_mss opens the window at RFC 6928 IW10
    // = min(10*MSS, max(2*MSS, 14600)).
    #[test]
    fn cubic_iw10_on_set_mss() {
        for (mss, expected) in [(48, 480), (536, 5_360), (1_460, 14_600)] {
            let mut cubic = Cubic::new();
            cubic.set_mss(mss);
            assert_eq!(cubic.window(), expected, "MSS {mss}");
        }
    }

    #[test]
    fn cubic_set_mss_reinitializes_iw_history() {
        let mut cubic = Cubic::new();
        cubic.cwnd = 100_000;
        cubic.w_max = 90_000;
        cubic.w_est = 80_000.0;
        cubic.cwnd_prior = 70_000;

        cubic.set_mss(536);

        assert_eq!(cubic.cwnd, 5_360);
        assert_eq!(cubic.w_max, 5_360);
        assert_eq!(cubic.w_est, 5_360.0);
        assert_eq!(cubic.cwnd_prior, 5_360);
        assert_eq!(
            cubic.k,
            cube_root(5_360.0 * (1.0 - BETA_CUBIC) / (C * 536.0))
        );
    }

    #[test]
    fn cubic_any_controller_reset_preserves_variant_and_clears_all_fields() {
        let mut controller = AnyController::Cubic(Cubic {
            k: 1.0,
            w_est: 2.0,
            recovery_start: Some(Instant::from_millis(3)),
            idle_start: Some(Instant::from_millis(4)),
            w_max: 5,
            cwnd: 6,
            mss: 7,
            ssthresh: 8,
            rwnd: 9,
            cwnd_prior: 10,
            in_fast_recovery: true,
            in_rto_recovery: true,
        });

        controller.reset();

        let fresh = Cubic::new();
        let cubic = match &controller {
            AnyController::Cubic(cubic) => cubic,
            _ => panic!("reset changed CUBIC variant"),
        };
        assert_eq!(cubic.k, fresh.k);
        assert_eq!(cubic.w_est, fresh.w_est);
        assert_eq!(cubic.recovery_start, fresh.recovery_start);
        assert_eq!(cubic.idle_start, fresh.idle_start);
        assert_eq!(cubic.w_max, fresh.w_max);
        assert_eq!(cubic.cwnd, fresh.cwnd);
        assert_eq!(cubic.mss, fresh.mss);
        assert_eq!(cubic.ssthresh, fresh.ssthresh);
        assert_eq!(cubic.rwnd, fresh.rwnd);
        assert_eq!(cubic.cwnd_prior, fresh.cwnd_prior);
        assert_eq!(cubic.in_fast_recovery, fresh.in_fast_recovery);
        assert_eq!(cubic.in_rto_recovery, fresh.in_rto_recovery);

        controller.set_mss(536);
        let cubic = match &controller {
            AnyController::Cubic(cubic) => cubic,
            _ => panic!("set_mss changed CUBIC variant"),
        };
        assert_eq!(cubic.cwnd, 5_360);
        assert_eq!(cubic.mss, 536);
        assert_eq!(cubic.w_max, 5_360);
        assert_eq!(cubic.w_est, 5_360.0);
        assert_eq!(cubic.cwnd_prior, 5_360);
        assert_eq!(cubic.ssthresh, u32::MAX);
        assert_eq!(cubic.rwnd, 64 * DEFAULT_MSS);
        assert_eq!(cubic.recovery_start, None);
        assert_eq!(cubic.idle_start, None);
        assert!(!cubic.in_fast_recovery);
        assert!(!cubic.in_rto_recovery);
        assert_eq!(
            cubic.k,
            cube_root(5_360.0 * (1.0 - BETA_CUBIC) / (C * 536.0))
        );
    }

    // The controller's rwnd is a grow-only high-water mark bounding cwnd; the
    // live receive window is enforced at the socket layer, so a shrink here must
    // not drag cwnd down.
    #[test]
    fn cubic_rwnd_is_grow_only() {
        let mut cubic = Cubic::new();
        cubic.set_remote_window(64 * 1024);
        cubic.set_remote_window(4 * 1024);
        assert_eq!(cubic.rwnd as usize, 64 * 1024);
    }
}
