use std::time::Instant;

/// Simple token bucket: `burst` tokens, refilled at `per_sec`.
pub struct TokenBucket {
    capacity: f64,
    tokens: f64,
    per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(burst: u32, per_sec: f64) -> Self {
        let capacity = f64::from(burst.max(1));
        Self {
            capacity,
            tokens: capacity,
            per_sec,
            last: Instant::now(),
        }
    }

    pub fn take(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_limited() {
        let mut b = TokenBucket::new(3, 0.0);
        assert!(b.take());
        assert!(b.take());
        assert!(b.take());
        assert!(!b.take());
    }
}
