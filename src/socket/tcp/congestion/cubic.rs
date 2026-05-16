use crate::time::Instant;

use super::Controller;

// Constants for the Cubic congestion control algorithm.
// See RFC 8312.
const BETA_CUBIC: f64 = 0.7;
const C: f64 = 0.4;

/// RFC 8312 Cubic congestion controller.
///
/// All window-size fields are u32 (1 GiB cap from RFC 1323 / window scale 14),
/// halving the struct footprint on 64-bit targets vs usize-typed fields.
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Cubic {
    cwnd: u32,     // Congestion window
    min_cwnd: u32, // The minimum size of congestion window
    w_max: u32,    // Window size just before congestion
    rwnd: u32,     // Remote window
    ssthresh: u32,
    recovery_start: Option<Instant>,
    last_update: Instant,
}

impl Cubic {
    pub fn new() -> Cubic {
        Cubic {
            cwnd: 1024 * 2,
            min_cwnd: 1024 * 2,
            w_max: 1024 * 2,
            recovery_start: None,
            rwnd: 64 * 1024,
            last_update: Instant::from_millis(0),
            ssthresh: u32::MAX,
        }
    }
}

impl Controller for Cubic {
    fn window(&self) -> usize {
        self.cwnd as usize
    }

    fn on_retransmit(&mut self, now: Instant) {
        self.w_max = self.cwnd;
        self.ssthresh = self.cwnd >> 1;
        self.recovery_start = Some(now);
    }

    fn on_duplicate_ack(&mut self, now: Instant) {
        self.w_max = self.cwnd;
        self.ssthresh = self.cwnd >> 1;
        self.recovery_start = Some(now);
    }

    fn set_remote_window(&mut self, remote_window: usize) {
        // Track the peer's currently advertised window, including shrinks
        // (RFC 793 allows the receiver to reduce its window). Clamp to u32
        // since RFC 1323 caps the effective window at 2^30 anyway.
        self.rwnd = remote_window.min(u32::MAX as usize) as u32;
    }

    fn on_ack(&mut self, _now: Instant, len: usize, _rtt: &crate::socket::tcp::RttEstimator) {
        // Slow start.
        if self.cwnd < self.ssthresh {
            let len = len.min(u32::MAX as usize) as u32;
            self.cwnd = self
                .cwnd
                .saturating_add(len)
                .min(self.rwnd)
                .max(self.min_cwnd);
        }
    }

    fn pre_transmit(&mut self, now: Instant) {
        let Some(recovery_start) = self.recovery_start else {
            self.recovery_start = Some(now);
            return;
        };

        let now_millis = now.total_millis();

        // If the last update was less than 100ms ago, don't update the congestion window.
        if self.last_update > recovery_start && now_millis - self.last_update.total_millis() < 100 {
            return;
        }

        // Elapsed time since the start of the recovery phase.
        let t = now_millis - recovery_start.total_millis();
        if t < 0 {
            return;
        }

        // K = (w_max * (1 - beta) / C)^(1/3)
        let k3 = ((self.w_max as f64) * (1.0 - BETA_CUBIC)) / C;
        let k = if let Some(k) = cube_root(k3) {
            k
        } else {
            return;
        };

        // cwnd = C(T - K)^3 + w_max
        let s = t as f64 / 1000.0 - k;
        let s = s * s * s;
        let cwnd = C * s + self.w_max as f64;

        self.last_update = now;

        // `f64 as u32` is saturating since Rust 1.45 (NaN -> 0, negative -> 0,
        // overflow -> u32::MAX); the explicit clamp documents the bounds.
        let cwnd = cwnd.clamp(0.0, u32::MAX as f64) as u32;
        self.cwnd = cwnd.max(self.min_cwnd).min(self.rwnd);
    }

    fn set_mss(&mut self, mss: usize) {
        let mss = mss.min(u32::MAX as usize) as u32;
        self.min_cwnd = mss;
        // RFC 6928 IW = min(10*MSS, max(2*MSS, 14600)). Called when the peer's
        // MSS is learned (on SYN) so we open at this size before any data
        // segments. mss fits in 16 bits so 10*mss never overflows u32.
        self.cwnd = self
            .cwnd
            .max((10 * mss).min((2 * mss).max(14_600)));
    }
}

#[inline]
fn abs(a: f64) -> f64 {
    if a < 0.0 { -a } else { a }
}

/// Calculate cube root by using the Newton-Raphson method.
fn cube_root(a: f64) -> Option<f64> {
    if a <= 0.0 {
        return None;
    }

    let (tolerance, init) = if a < 1_000.0 {
        (1.0, 8.879040017426005) // cube_root(700.0)
    } else if a < 1_000_000.0 {
        (5.0, 88.79040017426004) // cube_root(700_000.0)
    } else if a < 1_000_000_000.0 {
        (50.0, 887.9040017426004) // cube_root(700_000_000.0)
    } else if a < 1_000_000_000_000.0 {
        (500.0, 8879.040017426003) // cube_root(700_000_000_000.0)
    } else if a < 1_000_000_000_000_000.0 {
        (5000.0, 88790.40017426001) // cube_root(700_000_000_000.0)
    } else {
        (50000.0, 887904.0017426) // cube_root(700_000_000_000_000.0)
    };

    let mut x = init; // initial value
    let mut n = 20; // The maximum iteration
    loop {
        let next_x = (2.0 * x + a / (x * x)) / 3.0;
        if abs(next_x - x) < tolerance {
            return Some(next_x);
        }
        x = next_x;

        if n == 0 {
            return Some(next_x);
        }

        n -= 1;
    }
}

#[cfg(test)]
mod test {
    use crate::{socket::tcp::RttEstimator, time::Instant};

    use super::*;

    #[test]
    fn test_cubic() {
        let remote_window = 64 * 1024 * 1024;
        let now = Instant::from_millis(0);

        for i in 0..10 {
            for j in 0..9 {
                let mut cubic = Cubic::new();
                // Set remote window.
                cubic.set_remote_window(remote_window);

                cubic.set_mss(1480);

                if i & 1 == 0 {
                    cubic.on_retransmit(now);
                } else {
                    cubic.on_duplicate_ack(now);
                }

                cubic.pre_transmit(now);

                let mut n = i;
                for _ in 0..j {
                    n *= i;
                }

                let elapsed = Instant::from_millis(n);
                cubic.pre_transmit(elapsed);

                let cwnd = cubic.window();
                println!("Cubic: elapsed = {}, cwnd = {}", elapsed, cwnd);

                assert!(cwnd >= (cubic.min_cwnd as usize));
                assert!(cubic.window() <= remote_window);
            }
        }
    }

    #[test]
    fn cubic_time_inversion() {
        let mut cubic = Cubic::new();

        let t1 = Instant::from_micros(0);
        let t2 = Instant::from_micros(i64::MAX);

        cubic.on_retransmit(t2);
        cubic.pre_transmit(t1);

        let cwnd = cubic.window();
        println!("Cubic:time_inversion: cwnd: {}, cubic: {cubic:?}", cwnd);

        assert!(cwnd >= (cubic.min_cwnd as usize));
        assert!(cwnd <= (cubic.rwnd as usize));
    }

    #[test]
    fn cubic_long_elapsed_time() {
        let mut cubic = Cubic::new();

        let t1 = Instant::from_millis(0);
        let t2 = Instant::from_micros(i64::MAX);

        cubic.on_retransmit(t1);
        cubic.pre_transmit(t2);

        let cwnd = cubic.window();
        println!("Cubic:long_elapsed_time: cwnd: {}", cwnd);

        assert!(cwnd >= (cubic.min_cwnd as usize));
        assert!(cwnd <= (cubic.rwnd as usize));
    }

    #[test]
    fn cubic_last_update() {
        let mut cubic = Cubic::new();

        let t1 = Instant::from_millis(0);
        let t2 = Instant::from_millis(100);
        let t3 = Instant::from_millis(199);
        let t4 = Instant::from_millis(20000);

        cubic.on_retransmit(t1);

        cubic.pre_transmit(t2);
        let cwnd2 = cubic.window();

        cubic.pre_transmit(t3);
        let cwnd3 = cubic.window();

        cubic.pre_transmit(t4);
        let cwnd4 = cubic.window();

        println!(
            "Cubic:last_update: cwnd2: {}, cwnd3: {}, cwnd4: {}",
            cwnd2, cwnd3, cwnd4
        );

        assert_eq!(cwnd2, cwnd3);
        assert_ne!(cwnd2, cwnd4);
    }

    #[test]
    fn cubic_slow_start() {
        let mut cubic = Cubic::new();

        let t1 = Instant::from_micros(0);

        let cwnd = cubic.window();
        let ack_len = 1024;

        cubic.on_ack(t1, ack_len, &RttEstimator::default());

        assert!(cubic.window() > cwnd);

        for i in 1..1000 {
            let t2 = Instant::from_micros(i);
            cubic.on_ack(t2, ack_len * 100, &RttEstimator::default());
            assert!(cubic.window() <= (cubic.rwnd as usize));
        }

        let t3 = Instant::from_micros(2000);

        let cwnd = cubic.window();
        cubic.on_retransmit(t3);
        assert_eq!(cwnd >> 1, (cubic.ssthresh as usize));
    }

    #[test]
    fn cubic_pre_transmit() {
        let mut cubic = Cubic::new();
        cubic.pre_transmit(Instant::from_micros(2000));
    }

    #[test]
    fn test_cube_root() {
        for n in (1..1000000).step_by(99) {
            let a = n as f64;
            let a = a * a * a;
            let result = cube_root(a);
            println!("cube_root({a}) = {}", result.unwrap());
        }
    }

    #[test]
    #[should_panic]
    fn cube_root_zero() {
        cube_root(0.0).unwrap();
    }

    // RFC 6928: IW = min(10*MSS, max(2*MSS, 14600)).
    #[test]
    fn cubic_iw10_on_set_mss() {
        let mut cubic = Cubic::new();
        cubic.set_remote_window(64 * 1024);
        cubic.set_mss(1460);
        assert_eq!(cubic.window(), 14_600);
    }

    // set_remote_window must track the current advertised window, including shrinks.
    #[test]
    fn cubic_rwnd_can_shrink() {
        let mut cubic = Cubic::new();
        cubic.set_remote_window(64 * 1024);
        cubic.set_remote_window(4 * 1024);
        cubic.set_mss(1460);
        cubic.on_ack(
            Instant::from_millis(0),
            100_000,
            &RttEstimator::default(),
        );
        assert!(cubic.window() <= 4 * 1024);
    }
}
