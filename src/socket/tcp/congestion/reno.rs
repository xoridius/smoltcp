use crate::{socket::tcp::RttEstimator, time::Instant};

use super::Controller;

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Reno {
    cwnd: usize,
    min_cwnd: usize,
    ssthresh: usize,
    rwnd: usize,
}

impl Reno {
    pub fn new() -> Self {
        Reno {
            cwnd: 1024 * 2,
            min_cwnd: 1024 * 2,
            ssthresh: usize::MAX,
            rwnd: 64 * 1024,
        }
    }
}

impl Controller for Reno {
    fn window(&self) -> usize {
        self.cwnd
    }

    fn on_ack(&mut self, _now: Instant, len: usize, _rtt: &RttEstimator) {
        let len = if self.cwnd < self.ssthresh {
            // Slow start.
            len
        } else {
            self.ssthresh = self.cwnd;
            self.min_cwnd
        };

        self.cwnd = self
            .cwnd
            .saturating_add(len)
            .min(self.rwnd)
            .max(self.min_cwnd);
    }

    fn on_duplicate_ack(&mut self, _now: Instant) {
        self.ssthresh = (self.cwnd >> 1).max(self.min_cwnd);
    }

    fn on_retransmit(&mut self, _now: Instant) {
        self.cwnd = (self.cwnd >> 1).max(self.min_cwnd);
    }

    fn set_mss(&mut self, mss: usize) {
        self.min_cwnd = mss;
        // RFC 6928 IW: min(10*MSS, max(2*MSS, 14600)). set_mss is called when the
        // peer's MSS is learned (on SYN), before any data segments are sent, so
        // raise the initial window then.
        let iw = (10 * mss).min((2 * mss).max(14_600));
        if self.cwnd < iw {
            self.cwnd = iw;
        }
    }

    fn set_remote_window(&mut self, remote_window: usize) {
        self.rwnd = remote_window;
    }
}

#[cfg(test)]
mod test {
    use crate::time::Instant;

    use super::*;

    #[test]
    fn test_reno() {
        let remote_window = 64 * 1024;
        let now = Instant::from_millis(0);

        for i in 0..10 {
            for j in 0..9 {
                let mut reno = Reno::new();
                reno.set_mss(1480);

                // Set remote window.
                reno.set_remote_window(remote_window);

                reno.on_ack(now, 4096, &RttEstimator::default());

                let mut n = i;
                for _ in 0..j {
                    n *= i;
                }

                if i & 1 == 0 {
                    reno.on_retransmit(now);
                } else {
                    reno.on_duplicate_ack(now);
                }

                let elapsed = Instant::from_millis(1000);
                reno.on_ack(elapsed, n, &RttEstimator::default());

                let cwnd = reno.window();
                println!("Reno: elapsed = {}, cwnd = {}", elapsed, cwnd);

                assert!(cwnd >= reno.min_cwnd);
                assert!(reno.window() <= remote_window);
            }
        }
    }

    #[test]
    fn reno_min_cwnd() {
        let remote_window = 64 * 1024;
        let now = Instant::from_millis(0);

        let mut reno = Reno::new();
        reno.set_remote_window(remote_window);

        for _ in 0..100 {
            reno.on_retransmit(now);
            assert!(reno.window() >= reno.min_cwnd);
        }
    }

    #[test]
    fn reno_set_rwnd() {
        let mut reno = Reno::new();
        reno.set_remote_window(64 * 1024 * 1024);

        println!("{reno:?}");
    }

    // RFC 6928: IW = min(10*MSS, max(2*MSS, 14600)).
    #[test]
    fn reno_iw10_on_set_mss() {
        let mut reno = Reno::new();
        reno.set_remote_window(64 * 1024);
        reno.set_mss(1460);
        assert_eq!(reno.window(), 14_600);

        let mut reno = Reno::new();
        reno.set_remote_window(64 * 1024);
        reno.set_mss(536);
        assert_eq!(reno.window(), 5_360);

        let mut reno = Reno::new();
        reno.set_remote_window(128 * 1024);
        reno.set_mss(8960);
        assert_eq!(reno.window(), 17_920);
    }

    // set_remote_window must track the current advertised window, including shrinks.
    #[test]
    fn reno_rwnd_can_shrink() {
        let mut reno = Reno::new();
        reno.set_remote_window(64 * 1024);
        reno.set_remote_window(4 * 1024);
        reno.set_mss(1460);
        reno.on_ack(Instant::from_millis(0), 100_000, &RttEstimator::default());
        assert!(reno.window() <= 4 * 1024);
    }
}
