use std::collections::VecDeque;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

pub struct RateLimiter {
    max_attempts: u32,
    window: Duration,
    timestamps: Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    pub fn new(max_attempts: u32, window_seconds: u32) -> Self {
        Self {
            max_attempts,
            window: Duration::from_secs(window_seconds as u64),
            timestamps: Mutex::new(VecDeque::with_capacity(max_attempts as usize + 1)),
        }
    }

    pub fn check(&self) -> bool {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window);
        let mut ts = self.timestamps.lock();
        if let Some(cutoff) = cutoff {
            while let Some(front) = ts.front() {
                if *front < cutoff {
                    ts.pop_front();
                } else {
                    break;
                }
            }
        }
        if ts.len() as u32 >= self.max_attempts {
            return false;
        }
        ts.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_max_then_blocks() {
        let rl = RateLimiter::new(3, 60);
        assert!(rl.check());
        assert!(rl.check());
        assert!(rl.check());
        assert!(!rl.check());
    }

    /// Regression: `check()` used to compute `now - self.window` directly,
    /// which underflows (and panics in debug builds) whenever `window` is
    /// larger than the process's elapsed monotonic time — trivially true for
    /// a huge window like `u32::MAX` seconds. The fix uses `checked_sub` and
    /// skips pruning when the cutoff isn't representable.
    #[test]
    fn huge_window_does_not_panic() {
        let rl = RateLimiter::new(3, u32::MAX);
        assert!(rl.check());
        assert!(rl.check());
        assert!(rl.check());
        assert!(!rl.check());
    }
}
