use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Non-blocking sliding-window rate limiter, mirroring the Python
/// `deque`-based implementation, plus a separate "quota block" timer
/// for hard 429s (`quota` errors), which shuts the provider off for
/// a full minute regardless of the window state.
pub struct GeminiLimiter {
    window: Duration,
    max_requests: u32,
    hits: Mutex<VecDeque<Instant>>,
    blocked_until: Mutex<Option<Instant>>,
}

impl GeminiLimiter {
    pub fn new(max_requests: u32) -> Self {
        Self {
            window: Duration::from_secs(60),
            max_requests,
            hits: Mutex::new(VecDeque::new()),
            blocked_until: Mutex::new(None),
        }
    }

    /// Returns `Some(seconds_remaining)` if Gemini is currently
    /// hard-blocked due to a quota error, otherwise `None`.
    pub async fn blocked_for(&self) -> Option<u64> {
        let guard = self.blocked_until.lock().await;
        guard.and_then(|until| {
            let now = Instant::now();
            if now < until {
                Some((until - now).as_secs())
            } else {
                None
            }
        })
    }

    /// Attempts to reserve a slot in the current window. Returns
    /// `true` if the caller may proceed with a Gemini request.
    pub async fn try_reserve(&self) -> bool {
        if self.blocked_for().await.is_some() {
            return false;
        }

        let now = Instant::now();
        let mut hits = self.hits.lock().await;
        while let Some(&front) = hits.front() {
            if now.duration_since(front) > self.window {
                hits.pop_front();
            } else {
                break;
            }
        }

        if hits.len() < self.max_requests as usize {
            hits.push_back(now);
            true
        } else {
            false
        }
    }

    /// Called after a 429 from Gemini. `is_quota` distinguishes a
    /// hard quota exhaustion (1 minute block) from an ordinary
    /// rate-limit bump (fill the window so we simply wait it out).
    pub async fn block_on_429(&self, is_quota: bool) {
        let now = Instant::now();
        let mut hits = self.hits.lock().await;
        hits.clear();

        if is_quota {
            let mut blocked_until = self.blocked_until.lock().await;
            *blocked_until = Some(now + Duration::from_secs(60));
        } else {
            for _ in 0..self.max_requests {
                hits.push_back(now);
            }
        }
    }
}