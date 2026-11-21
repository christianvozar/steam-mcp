//! Token-bucket rate limiting, both single-bucket and keyed (per principal).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// A classic token bucket: `rate` tokens/second, capacity `burst`.
#[derive(Debug)]
pub struct TokenBucket {
    rate: f64,
    burst: f64,
    state: Mutex<Bucket>,
}

impl TokenBucket {
    pub fn new(rate: f64, burst: f64) -> Self {
        let burst = burst.max(1.0);
        TokenBucket {
            rate: rate.max(0.001),
            burst,
            state: Mutex::new(Bucket {
                tokens: burst,
                last: Instant::now(),
            }),
        }
    }

    fn refill(&self, b: &mut Bucket) {
        let now = Instant::now();
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate).min(self.burst);
        b.last = now;
    }

    /// Takes a token if available; otherwise returns the wait until one is.
    pub fn try_acquire(&self) -> Result<(), Duration> {
        let mut b = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.refill(&mut b);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - b.tokens) / self.rate))
        }
    }

    /// Blocks until a token is available or `max_wait` elapses.
    pub fn acquire(&self, max_wait: Duration) -> bool {
        let deadline = Instant::now() + max_wait;
        loop {
            match self.try_acquire() {
                Ok(()) => return true,
                Err(wait) => {
                    let now = Instant::now();
                    if now + wait > deadline {
                        return false;
                    }
                    std::thread::sleep(wait.min(Duration::from_millis(250)));
                }
            }
        }
    }
}

/// Independent buckets per key, with idle-bucket garbage collection.
pub struct KeyedLimiter {
    rate: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, (Bucket, Instant)>>,
}

impl KeyedLimiter {
    pub fn new(rate: f64, burst: f64) -> Self {
        KeyedLimiter {
            rate: rate.max(0.001),
            burst: burst.max(1.0),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn check(&self, key: &str) -> Result<(), Duration> {
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if map.len() > 10_000 {
            map.retain(|_, (_, seen)| now.duration_since(*seen) < Duration::from_secs(600));
        }
        let (b, seen) = map.entry(key.to_string()).or_insert_with(|| {
            (
                Bucket {
                    tokens: self.burst,
                    last: now,
                },
                now,
            )
        });
        *seen = now;
        let elapsed = now.duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + elapsed * self.rate).min(self.burst);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - b.tokens) / self.rate))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_limits() {
        let b = TokenBucket::new(1.0, 2.0);
        assert!(b.try_acquire().is_ok());
        assert!(b.try_acquire().is_ok());
        assert!(b.try_acquire().is_err());
        let k = KeyedLimiter::new(1.0, 1.0);
        assert!(k.check("a").is_ok());
        assert!(k.check("a").is_err());
        assert!(k.check("b").is_ok());
    }
}
