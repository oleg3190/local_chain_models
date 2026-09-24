use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::providers::ProviderClient;

/// Persistent key pool for a single provider.
///
/// The pool keeps using the current key until the upstream reports 429.
/// That key is then cooled down and the next available key is selected.
/// This gives us account-level quota failover without ever falling back
/// to a different provider.
pub struct ApiKeyPool {
    clients: Vec<ProviderClient>,
    current: Mutex<usize>,
    blocked_until: Mutex<Vec<Option<Instant>>>,
    cooldown: Duration,
}

impl ApiKeyPool {
    pub fn new(clients: Vec<ProviderClient>, cooldown: Duration) -> Arc<Self> {
        let blocked_until = vec![None; clients.len()];
        Arc::new(Self {
            clients,
            current: Mutex::new(0),
            blocked_until: Mutex::new(blocked_until),
            cooldown,
        })
    }

    pub fn len(&self) -> usize {
        self.clients.len()
    }

    /// Returns the current/next available client and its stable pool index.
    pub async fn next_client(&self) -> Option<(usize, ProviderClient)> {
        if self.clients.is_empty() {
            return None;
        }

        let now = Instant::now();
        let current = {
            let current_guard = self.current.lock().await;
            *current_guard
        };
        let mut blocked = self.blocked_until.lock().await;
        let mut cursor = current % self.clients.len();

        for _ in 0..self.clients.len() {
            let available = blocked[cursor]
                .map(|until| now >= until)
                .unwrap_or(true);

            if available {
                blocked[cursor] = None;
                drop(blocked);

                let mut current_guard = self.current.lock().await;
                *current_guard = cursor;
                return Some((cursor, self.clients[cursor].clone()));
            }

            cursor = (cursor + 1) % self.clients.len();
        }

        None
    }

    /// Marks one key as exhausted/rate-limited and advances to the next
    /// key. The key becomes eligible again after the configured cooldown.
    pub async fn mark_limited(&self, index: usize) {
        if self.clients.is_empty() || index >= self.clients.len() {
            return;
        }

        let until = Instant::now() + self.cooldown;
        let mut blocked = self.blocked_until.lock().await;
        blocked[index] = Some(until);

        let mut next = (index + 1) % self.clients.len();
        for _ in 0..self.clients.len() {
            if next == index {
                break;
            }
            if blocked[next].map(|t| Instant::now() >= t).unwrap_or(true) {
                let mut current = self.current.lock().await;
                *current = next;
                return;
            }
            next = (next + 1) % self.clients.len();
        }

        // If every key is currently blocked, leave the cursor on the
        // current key; next_client() will automatically pick the first
        // key whose cooldown has expired.
        let mut current = self.current.lock().await;
        *current = index;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderConfig;
    use crate::providers::ProviderClient;

    fn client(key: &str) -> ProviderClient {
        ProviderClient::build(
            "opencode-go",
            "http://127.0.0.1",
            &ProviderConfig {
                api_key: key.to_string(),
                model: "kimi-k2.7-code".to_string(),
                use_proxy: false,
            },
            None,
        )
        .expect("test client")
    }

    #[tokio::test]
    async fn rotates_to_next_key_when_current_is_limited() {
        let pool = ApiKeyPool::new(
            vec![client("key-1"), client("key-2"), client("key-3")],
            Duration::from_secs(3600),
        );

        let (index, first) = pool.next_client().await.expect("first key");
        assert_eq!(index, 0);
        assert_eq!(first.api_key, "key-1");

        pool.mark_limited(index).await;

        let (index, second) = pool.next_client().await.expect("second key");
        assert_eq!(index, 1);
        assert_eq!(second.api_key, "key-2");

        pool.mark_limited(index).await;

        let (index, third) = pool.next_client().await.expect("third key");
        assert_eq!(index, 2);
        assert_eq!(third.api_key, "key-3");
    }

    #[tokio::test]
    async fn returns_none_when_all_keys_are_cooling_down() {
        let pool = ApiKeyPool::new(
            vec![client("key-1"), client("key-2")],
            Duration::from_secs(3600),
        );

        pool.mark_limited(0).await;
        pool.mark_limited(1).await;

        assert!(pool.next_client().await.is_none());
    }
}
