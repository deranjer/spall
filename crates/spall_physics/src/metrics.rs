//! Small percentile / summary helpers for the feasibility measurements. No
//! external stats dependency: the samples are short and CPU-cheap.

use std::time::Duration;

/// A collected set of duration samples with percentile access.
#[derive(Debug, Default, Clone)]
pub struct DurationSamples {
    micros: Vec<f64>,
}

impl DurationSamples {
    /// Empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one sample.
    pub fn push(&mut self, d: Duration) {
        self.micros.push(d.as_secs_f64() * 1.0e6);
    }

    /// Number of samples.
    pub fn len(&self) -> usize {
        self.micros.len()
    }

    /// Whether no samples were recorded.
    pub fn is_empty(&self) -> bool {
        self.micros.is_empty()
    }

    /// Nearest-rank percentile in microseconds (`p` in `0.0..=1.0`). `None`
    /// when empty.
    pub fn percentile_us(&self, p: f64) -> Option<f64> {
        if self.micros.is_empty() {
            return None;
        }
        let mut sorted = self.micros.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let rank = (p.clamp(0.0, 1.0) * (sorted.len() as f64 - 1.0)).round() as usize;
        Some(sorted[rank])
    }

    /// Mean in microseconds, or `None` when empty.
    pub fn mean_us(&self) -> Option<f64> {
        if self.micros.is_empty() {
            return None;
        }
        Some(self.micros.iter().sum::<f64>() / self.micros.len() as f64)
    }

    /// Maximum sample in microseconds, or `None` when empty.
    pub fn max_us(&self) -> Option<f64> {
        self.micros
            .iter()
            .copied()
            .fold(None, |m, v| Some(m.map_or(v, |m: f64| m.max(v))))
    }

    /// `(p50, p95, p99, max, mean)` in microseconds. Zeros when empty.
    pub fn summary_us(&self) -> PercentileSummary {
        PercentileSummary {
            samples: self.len(),
            p50_us: self.percentile_us(0.50).unwrap_or(0.0),
            p95_us: self.percentile_us(0.95).unwrap_or(0.0),
            p99_us: self.percentile_us(0.99).unwrap_or(0.0),
            max_us: self.max_us().unwrap_or(0.0),
            mean_us: self.mean_us().unwrap_or(0.0),
        }
    }
}

/// Plain percentile summary, ready to serialise into a report line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PercentileSummary {
    /// Sample count.
    pub samples: usize,
    /// 50th percentile, µs.
    pub p50_us: f64,
    /// 95th percentile, µs.
    pub p95_us: f64,
    /// 99th percentile, µs.
    pub p99_us: f64,
    /// Maximum, µs.
    pub max_us: f64,
    /// Mean, µs.
    pub mean_us: f64,
}

impl PercentileSummary {
    /// Renders as a compact JSON object (no serde dependency for one shape).
    pub fn to_json(&self) -> String {
        format!(
            "{{\"samples\":{},\"p50_us\":{:.3},\"p95_us\":{:.3},\"p99_us\":{:.3},\"max_us\":{:.3},\"mean_us\":{:.3}}}",
            self.samples, self.p50_us, self.p95_us, self.p99_us, self.max_us, self.mean_us
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut s = DurationSamples::new();
        for us in 1..=100 {
            s.push(Duration::from_micros(us));
        }
        let sum = s.summary_us();
        assert_eq!(sum.samples, 100);
        assert!((sum.p50_us - 50.0).abs() <= 1.0);
        assert!((sum.p95_us - 95.0).abs() <= 1.0);
        assert!((sum.p99_us - 99.0).abs() <= 1.0);
        assert!((sum.max_us - 100.0).abs() < 1e-9);
    }

    #[test]
    fn empty_summary_is_zero() {
        let s = DurationSamples::new();
        assert!(s.is_empty());
        assert_eq!(s.summary_us().p95_us, 0.0);
        assert_eq!(s.percentile_us(0.5), None);
    }
}
