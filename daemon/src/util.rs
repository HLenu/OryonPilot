//! Small helpers: monotonic clock, EWMA, statistics, ISO-8601 formatting.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Wall clock in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Process-relative monotonic milliseconds. Used for dwell/cooldown maths so that
/// a wall-clock jump (NTP sync, timezone change) cannot break the state machine.
pub fn mono_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as u64
}

/// ISO-8601 UTC timestamp (`2026-10-30T12:00:00Z`) from epoch milliseconds.
pub fn iso_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Exponentially weighted moving average with an explicit "not primed yet" state.
#[derive(Debug, Clone)]
pub struct Ewma {
    alpha: f64,
    value: Option<f64>,
}

impl Ewma {
    pub fn new(alpha: f64) -> Self {
        Self {
            alpha: alpha.clamp(0.001, 1.0),
            value: None,
        }
    }

    pub fn push(&mut self, sample: f64) -> f64 {
        let v = match self.value {
            Some(prev) => prev + self.alpha * (sample - prev),
            None => sample,
        };
        self.value = Some(v);
        v
    }

    pub fn value(&self) -> Option<f64> {
        self.value
    }

    pub fn reset(&mut self) {
        self.value = None;
    }
}

pub fn clampf(v: f64, lo: f64, hi: f64) -> f64 {
    if v.is_nan() || v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

pub fn percent(part: f64, total: f64) -> f64 {
    if total <= 0.0 {
        0.0
    } else {
        (part / total * 100.0).clamp(0.0, 100.0)
    }
}

/// `Some(v)` if the first whitespace separated token parses as f64.
pub fn first_f64(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse::<f64>().ok()
}

pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
    sorted[idx]
}

pub fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// Shannon entropy (bits) of a probability distribution, used by the predictor to
/// express how undecided the next-app guess is.
pub fn entropy(p: &[f64]) -> f64 {
    let mut h = 0.0;
    for &x in p {
        if x > 1e-9 {
            h -= x * x.log2();
        }
    }
    h.max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_epoch_is_1970() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso_known_value() {
        // 2026-10-30T12:34:56Z
        assert_eq!(iso_utc(1_793_363_696_000), "2026-10-30T12:34:56Z");
    }

    #[test]
    fn ewma_primes_then_smooths() {
        let mut e = Ewma::new(0.5);
        assert_eq!(e.push(10.0), 10.0);
        assert_eq!(e.push(20.0), 15.0);
        assert_eq!(e.push(20.0), 17.5);
    }

    #[test]
    fn percentile_picks_expected_index() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(percentile(&v, 0.5), 3.0);
        assert_eq!(percentile(&v, 1.0), 5.0);
    }

    #[test]
    fn entropy_of_certain_is_zero() {
        assert!(entropy(&[1.0]).abs() < 1e-9);
        assert!((entropy(&[0.5, 0.5]) - 1.0).abs() < 1e-9);
    }
}
