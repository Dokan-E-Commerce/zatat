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
        let cutoff = now - self.window;
        let mut ts = self.timestamps.lock();
        while let Some(front) = ts.front() {
            if *front < cutoff {
                ts.pop_front();
            } else {
                break;
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
}
