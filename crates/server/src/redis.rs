use std::{
    future::Future,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use redis::aio::ConnectionManager;
use tokio::sync::Mutex;

use crate::{clock::now_ms, log::log_warn};

const CALL_TIMEOUT: Duration = Duration::from_millis(150);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const COOLDOWN_MS: u64 = 5_000;
const WARN_AT_MOST_EVERY_MS: u64 = 30_000;

pub struct RedisGuard {
    client: redis::Client,
    conn: Mutex<Option<ConnectionManager>>,
    down_until: AtomicU64,
    warned_at: AtomicU64,
}

impl RedisGuard {
    pub fn new(url: &str) -> anyhow::Result<Self> {
        Ok(Self { client: redis::Client::open(url)?, conn: Mutex::new(None), down_until: AtomicU64::new(0), warned_at: AtomicU64::new(0) })
    }

    pub fn available(&self) -> bool {
        now_ms() >= self.down_until.load(Ordering::Relaxed)
    }

    async fn connection(&self) -> Result<ConnectionManager, String> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref() {
            return Ok(c.clone());
        }
        let c = tokio::time::timeout(CONNECT_TIMEOUT, self.client.get_connection_manager())
            .await
            .map_err(|_| "connecting timed out".to_owned())?
            .map_err(|e| e.to_string())?;
        *slot = Some(c.clone());
        Ok(c)
    }

    pub async fn run<T, F, Fut>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: Future<Output = redis::RedisResult<T>>,
    {
        if !self.available() {
            return Err("redis circuit open".into());
        }
        let result = async {
            let c = self.connection().await?;
            match tokio::time::timeout(CALL_TIMEOUT, f(c)).await {
                Ok(Ok(v)) => Ok(v),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("timed out after {} ms", CALL_TIMEOUT.as_millis())),
            }
        }
        .await;
        if let Err(e) = &result {
            let now = now_ms();
            self.down_until.store(now + COOLDOWN_MS, Ordering::Relaxed);
            if now.saturating_sub(self.warned_at.load(Ordering::Relaxed)) > WARN_AT_MOST_EVERY_MS {
                self.warned_at.store(now, Ordering::Relaxed);
                log_warn!("core", "redis unavailable ({e}); using per-process rate limits and cache for {}s", COOLDOWN_MS / 1000);
            }
        }
        result
    }

    pub async fn ping(&self) -> Result<(), String> {
        self.run(|mut c| async move { redis::cmd("PING").query_async::<String>(&mut c).await.map(|_| ()) }).await
    }
}
