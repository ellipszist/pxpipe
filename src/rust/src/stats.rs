//! Running session totals — surfaced via GET /proxy-stats and per-request log line.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Stats {
    pub requests: AtomicU64,
    pub compressed_requests: AtomicU64,
    /// Effective tokens we actually paid for (sum of input + cache_create*1.25 + cache_read*0.10).
    /// Stored as bits of f64.
    effective_actual_bits: AtomicU64,
    /// What we *would* have paid uncompressed (conservative estimate).
    effective_baseline_bits: AtomicU64,
}

impl Stats {
    pub fn record(&self, actual: f64, baseline: f64, compressed: bool) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        if compressed {
            self.compressed_requests.fetch_add(1, Ordering::Relaxed);
        }
        self.add_f64(&self.effective_actual_bits, actual);
        self.add_f64(&self.effective_baseline_bits, baseline);
    }

    pub fn snapshot(&self) -> Snapshot {
        let actual = self.read_f64(&self.effective_actual_bits);
        let baseline = self.read_f64(&self.effective_baseline_bits);
        let saved = (baseline - actual).max(0.0);
        let pct = if baseline > 0.0 { saved / baseline * 100.0 } else { 0.0 };
        Snapshot {
            requests: self.requests.load(Ordering::Relaxed),
            compressed_requests: self.compressed_requests.load(Ordering::Relaxed),
            effective_input_actual: actual,
            effective_input_baseline_est: baseline,
            saved_effective_tokens: saved,
            saved_pct: pct,
            // Opus 4.7 input price: $15 / M tokens
            saved_usd_opus47: saved * 15.0 / 1_000_000.0,
        }
    }

    fn add_f64(&self, atomic: &AtomicU64, delta: f64) {
        let mut cur = atomic.load(Ordering::Relaxed);
        loop {
            let new_val = f64::from_bits(cur) + delta;
            match atomic.compare_exchange_weak(cur, new_val.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    fn read_f64(&self, atomic: &AtomicU64) -> f64 {
        f64::from_bits(atomic.load(Ordering::Relaxed))
    }
}

#[derive(serde::Serialize)]
pub struct Snapshot {
    pub requests: u64,
    pub compressed_requests: u64,
    pub effective_input_actual: f64,
    pub effective_input_baseline_est: f64,
    pub saved_effective_tokens: f64,
    pub saved_pct: f64,
    pub saved_usd_opus47: f64,
}
