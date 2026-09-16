//! Maps the producer's clock domain onto the consumer host's.
//!
//! Producer timestamps are monotonic in host A's clock and cross the link
//! untouched. The ingress half runs a four-timestamp ping and pong once a
//! second on the control connection, keeps the samples with the smallest
//! round trips, and fits offset plus drift over a sliding window. Republished
//! frames carry `ts_local = ts_remote - (remote - local)`.
//!
//! Precision through userspace SSH hops on a LAN is expected in the 0.1 to
//! 0.5 ms range; nothing here claims better.

use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Pinger send time (local clock).
    pub t1: u64,
    /// Peer receive time (remote clock).
    pub t2: u64,
    /// Peer send time (remote clock).
    pub t3: u64,
    /// Pinger receive time (local clock).
    pub t4: u64,
}

impl Sample {
    /// Round trip excluding the peer's processing time.
    #[must_use]
    pub fn rtt_ns(&self) -> u64 {
        self.t4.saturating_sub(self.t1).saturating_sub(self.t3.saturating_sub(self.t2))
    }

    /// `remote - local` at the midpoint of the exchange, assuming symmetric paths.
    #[must_use]
    pub fn remote_minus_local_ns(&self) -> i64 {
        let forward = i128::from(self.t2) - i128::from(self.t1);
        let backward = i128::from(self.t3) - i128::from(self.t4);
        i64::try_from((forward + backward) / 2).unwrap_or(i64::MAX)
    }

    #[must_use]
    pub fn local_midpoint_ns(&self) -> u64 {
        self.t1 + (self.t4.saturating_sub(self.t1)) / 2
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Estimate {
    /// `remote - local` in nanoseconds at `reference_local_ns`.
    pub offset_ns: i64,
    /// Parts per billion by which the remote clock runs fast relative to local.
    pub drift_ppb: i64,
    pub reference_local_ns: u64,
    pub rtt_min_ns: u64,
    pub samples: usize,
}

impl Estimate {
    /// Converts a remote timestamp to the local clock.
    #[must_use]
    pub fn local_from_remote(&self, remote_ns: u64) -> u64 {
        let base = i128::from(remote_ns) - i128::from(self.offset_ns);
        // drift is expressed against local time; solve approximately with the
        // uncorrected value, which is well within precision at ppb scales.
        let elapsed = base - i128::from(self.reference_local_ns);
        let correction = elapsed * i128::from(self.drift_ppb) / 1_000_000_000;
        u64::try_from(base - correction).unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
pub struct ClockEstimator {
    window: usize,
    samples: VecDeque<Sample>,
}

impl Default for ClockEstimator {
    fn default() -> Self {
        Self::new(64)
    }
}

impl ClockEstimator {
    #[must_use]
    pub fn new(window: usize) -> Self {
        Self {
            window: window.max(1),
            samples: VecDeque::new(),
        }
    }

    pub fn add(&mut self, sample: Sample) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    /// The current estimate, or `None` before the first sample. With fewer than
    /// three good samples the offset is the minimum-RTT sample's and drift is
    /// zero; otherwise offset and drift come from a least-squares fit over the
    /// samples whose round trip is within 1.5x of the minimum plus 100 us.
    #[must_use]
    pub fn estimate(&self) -> Option<Estimate> {
        let rtt_min = self.samples.iter().map(Sample::rtt_ns).min()?;
        let good: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|s| s.rtt_ns() <= rtt_min + rtt_min / 2 + 100_000)
            .collect();
        let best = good.iter().min_by_key(|s| s.rtt_ns()).copied()?;
        let reference = best.local_midpoint_ns();
        if good.len() < 3 {
            return Some(Estimate {
                offset_ns: best.remote_minus_local_ns(),
                drift_ppb: 0,
                reference_local_ns: reference,
                rtt_min_ns: rtt_min,
                samples: good.len(),
            });
        }
        // least squares of offset against local time, relative to the reference
        let n = good.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for s in &good {
            let x = (i128::from(s.local_midpoint_ns()) - i128::from(reference)) as f64;
            let y = s.remote_minus_local_ns() as f64;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let denom = n * sxx - sx * sx;
        let (slope, intercept) = if denom.abs() < 1e-9 {
            (0.0, sy / n)
        } else {
            let slope = (n * sxy - sx * sy) / denom;
            (slope, (sy - slope * sx) / n)
        };
        Some(Estimate {
            offset_ns: intercept.round() as i64,
            drift_ppb: (slope * 1e9).round() as i64,
            reference_local_ns: reference,
            rtt_min_ns: rtt_min,
            samples: good.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exchange(local_send: u64, remote_offset: i64, rtt: u64, remote_hold: u64) -> Sample {
        let one_way = rtt / 2;
        let t2 = (i128::from(local_send + one_way) + i128::from(remote_offset)) as u64;
        let t3 = t2 + remote_hold;
        Sample {
            t1: local_send,
            t2,
            t3,
            t4: local_send + one_way + remote_hold + one_way,
        }
    }

    #[test]
    fn single_sample_gives_offset_and_rtt() {
        let mut c = ClockEstimator::default();
        c.add(exchange(1_000_000_000, 5_000_000, 400_000, 50_000));
        let e = c.estimate().unwrap();
        assert_eq!(e.offset_ns, 5_000_000);
        assert_eq!(e.rtt_min_ns, 400_000);
        assert_eq!(e.drift_ppb, 0);
        assert_eq!(e.local_from_remote(1_005_000_000 + 200_000), 1_000_200_000);
    }

    #[test]
    fn fit_recovers_drift_and_ignores_slow_samples() {
        let mut c = ClockEstimator::default();
        // remote runs 50 ppb fast: offset grows 50 ns per second
        for i in 0..20u64 {
            let t = 1_000_000_000 + i * 1_000_000_000;
            let offset = 7_000_000 + (i as i64) * 50;
            let rtt = if i == 7 { 20_000_000 } else { 300_000 };
            c.add(exchange(t, offset, rtt, 10_000));
        }
        let e = c.estimate().unwrap();
        assert_eq!(e.samples, 19, "the slow sample is excluded");
        assert!((e.drift_ppb - 50).abs() <= 2, "drift {}", e.drift_ppb);
        // offset at the reference sample (the first, minimum-RTT one) is close to 7 ms
        assert!((e.offset_ns - 7_000_000).abs() < 200, "offset {}", e.offset_ns);
    }

    #[test]
    fn window_bounds_memory() {
        let mut c = ClockEstimator::new(4);
        for i in 0..10 {
            c.add(exchange(i * 1_000_000, 0, 100, 0));
        }
        assert_eq!(c.samples.len(), 4);
    }
}
