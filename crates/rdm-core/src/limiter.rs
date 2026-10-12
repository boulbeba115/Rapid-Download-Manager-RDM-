//! Token-bucket rate limiter shared by every connection it governs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

struct Bucket {
    /// May go negative: a caller that overdraws sleeps off its own debt, so
    /// concurrent callers queue up behind each other without a wait list.
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    /// Bytes per second; 0 means unlimited.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

impl RateLimiter {
    pub fn new(rate: u64) -> Self {
        Self { rate: AtomicU64::new(rate), bucket: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }) }
    }

    pub fn unlimited() -> Self {
        Self::new(0)
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn set_rate(&self, rate: u64) {
        let old = self.rate.swap(rate, Ordering::Relaxed);
        if old != rate {
            // Forget accumulated credit/debt so the new rate applies immediately.
            let mut b = self.bucket.lock();
            b.tokens = 0.0;
            b.last = Instant::now();
        }
    }

    /// Account for `n` bytes, sleeping as long as needed to stay under the rate.
    pub async fn acquire(&self, n: usize) {
        let rate = self.rate.load(Ordering::Relaxed);
        if rate == 0 || n == 0 {
            return;
        }
        let wait = {
            let mut b = self.bucket.lock();
            let now = Instant::now();
            let rate_f = rate as f64;
            // Up to 100 ms of burst keeps throughput smooth without overshooting.
            let burst = rate_f * 0.1;
            b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate_f).min(burst);
            b.last = now;
            b.tokens -= n as f64;
            if b.tokens < 0.0 {
                Duration::from_secs_f64(-b.tokens / rate_f)
            } else {
                Duration::ZERO
            }
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn holds_rate_across_tasks() {
        let limiter = Arc::new(RateLimiter::new(400_000));
        let start = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let l = limiter.clone();
            tasks.push(tokio::spawn(async move {
                for _ in 0..25 {
                    l.acquire(4_000).await;
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        // 800 kB at 400 kB/s ≈ 2 s (minus the initial burst allowance).
        let secs = start.elapsed().as_secs_f64();
        assert!((1.7..2.6).contains(&secs), "took {secs}s");
    }

    #[tokio::test]
    async fn unlimited_is_instant() {
        let limiter = RateLimiter::unlimited();
        let start = Instant::now();
        for _ in 0..1000 {
            limiter.acquire(1 << 20).await;
        }
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
