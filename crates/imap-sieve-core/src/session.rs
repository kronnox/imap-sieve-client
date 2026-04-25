//! IMAP session manager: drives IDLE, handles reconnects, fires the processor.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct BackoffConfig {
    pub initial: Duration,
    pub max: Duration,
    pub jitter: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    cfg: BackoffConfig,
    attempt: u32,
}

impl Backoff {
    pub fn new(cfg: BackoffConfig) -> Self {
        Self { cfg, attempt: 0 }
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Returns the next delay and increments the attempt counter.
    pub fn next_delay(&mut self, rng: &mut impl rand::Rng) -> Duration {
        let exp = 2u32.saturating_pow(self.attempt) as u64;
        let base = self.cfg.initial.as_secs().saturating_mul(exp);
        let capped = base.min(self.cfg.max.as_secs());
        let jitter_factor = 1.0 + rng.gen_range(0.0..=self.cfg.jitter);
        let jittered = (capped as f64 * jitter_factor).min(self.cfg.max.as_secs() as f64);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_secs(jittered as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn first_delay_is_at_least_initial() {
        let cfg = BackoffConfig { initial: Duration::from_secs(5), max: Duration::from_secs(300), jitter: 0.5 };
        let mut b = Backoff::new(cfg);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let d = b.next_delay(&mut rng);
        assert!(d >= Duration::from_secs(5), "got {d:?}");
        assert!(d <= Duration::from_secs(8), "got {d:?}"); // 5 * (1+0.5)
    }

    #[test]
    fn delay_grows_exponentially_then_caps() {
        let cfg = BackoffConfig { initial: Duration::from_secs(5), max: Duration::from_secs(300), jitter: 0.0 };
        let mut b = Backoff::new(cfg);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(5));
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(10));
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(20));
        // Run forward to verify cap
        for _ in 0..20 {
            let _ = b.next_delay(&mut rng);
        }
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(300));
    }

    #[test]
    fn reset_returns_to_initial() {
        let cfg = BackoffConfig { initial: Duration::from_secs(5), max: Duration::from_secs(300), jitter: 0.0 };
        let mut b = Backoff::new(cfg);
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let _ = b.next_delay(&mut rng);
        let _ = b.next_delay(&mut rng);
        b.reset();
        assert_eq!(b.next_delay(&mut rng), Duration::from_secs(5));
    }
}