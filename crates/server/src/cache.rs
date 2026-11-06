use std::{
    collections::{HashMap, HashSet},
    future::Future,
    hash::{Hash, Hasher},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::Response;
use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::{
    clock::now_ms,
    compression::{Precompressed, gzip_if_worthwhile},
    redis::RedisGuard,
    sync::{defer, lock},
};

const MAX_ENTRIES: usize = 5000;
const REDIS_PREFIX: &str = "samishisa:rc:";

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct CacheRule {
    #[serde(rename = "ttl")]
    pub fresh_secs: u32,
    #[serde(rename = "staleTtl", default)]
    pub stale_secs: u32,
}

pub struct Produced {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Produced {
    fn content_type(&self) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.as_str())
    }
}

fn invalid_response() -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
    response
}

pub type Producer = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Produced> + Send>> + Send + Sync>;

#[derive(Debug)]
pub struct Entry {
    status: u16,
    headers: Vec<(String, String)>,
    body: Bytes,
    gz: Option<Bytes>,
    etag: String,
    fresh_until: u64,
    usable_until: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CacheState {
    Hit,
    Stale,
    Miss,
}

pub enum Outcome {
    Cached(Arc<Entry>, CacheState),
    Uncached(Produced),
}

impl Entry {
    fn new(produced: Produced, rule: CacheRule, gzip: bool) -> Entry {
        let now = now_ms();
        let gz = if gzip { gzip_if_worthwhile(produced.content_type(), &produced.body) } else { None };
        Entry {
            etag: etag_of(&produced.body),
            status: produced.status,
            headers: produced.headers,
            body: produced.body,
            gz,
            fresh_until: now + u64::from(rule.fresh_secs) * 1000,
            usable_until: now + (u64::from(rule.fresh_secs) + u64::from(rule.stale_secs)) * 1000,
        }
    }

    pub fn to_response(&self, state: CacheState, rule: CacheRule, if_none_match: Option<&str>) -> Response<Full<Bytes>> {
        let max_age = match state {
            CacheState::Stale => 0,
            _ => (self.fresh_until.saturating_sub(now_ms()) / 1000).min(u64::from(rule.fresh_secs)),
        };
        let cache_control = match rule.stale_secs {
            0 => format!("public, max-age={max_age}"),
            stale => format!("public, max-age={max_age}, stale-while-revalidate={stale}"),
        };
        let x_cache = match state {
            CacheState::Hit => "HIT",
            CacheState::Stale => "STALE",
            CacheState::Miss => "MISS",
        };
        let not_modified = if_none_match.is_some_and(|v| v.split(',').any(|tag| tag.trim() == self.etag || tag.trim() == "*"));
        let mut builder =
            Response::builder().status(if not_modified { 304 } else { self.status }).header("etag", &self.etag).header("cache-control", cache_control).header("x-cache", x_cache);
        if not_modified {
            return builder.body(Full::new(Bytes::new())).unwrap_or_else(|_| invalid_response());
        }
        for (k, v) in &self.headers {
            builder = builder.header(k, v);
        }
        let mut response = builder.body(Full::new(self.body.clone())).unwrap_or_else(|_| invalid_response());
        if let Some(gz) = &self.gz {
            response.extensions_mut().insert(Precompressed(gz.clone()));
        }
        response
    }

    fn to_redis_bytes(&self) -> Option<Vec<u8>> {
        let head = serde_json::json!({ "status": self.status, "headers": self.headers, "etag": self.etag, "fresh": self.fresh_until, "usable": self.usable_until }).to_string();
        let head_len = u32::try_from(head.len()).ok()?;
        let mut bytes = Vec::with_capacity(4 + head.len() + self.body.len());
        bytes.extend_from_slice(&head_len.to_le_bytes());
        bytes.extend_from_slice(head.as_bytes());
        bytes.extend_from_slice(&self.body);
        Some(bytes)
    }

    fn from_redis_bytes(raw: &[u8], gzip: bool) -> Option<Entry> {
        let head_len = usize::try_from(u32::from_le_bytes(raw.get(..4)?.try_into().ok()?)).ok()?;
        let head_end = head_len.checked_add(4)?;
        let head: serde_json::Value = serde_json::from_slice(raw.get(4..head_end)?).ok()?;
        let body = Bytes::copy_from_slice(raw.get(head_end..)?);
        let headers: Vec<(String, String)> = head["headers"].as_array()?.iter().filter_map(|h| Some((h[0].as_str()?.to_owned(), h[1].as_str()?.to_owned()))).collect();
        let content_type = headers.iter().find(|(k, _)| k == "content-type").map(|(_, v)| v.as_str());
        Some(Entry {
            status: u16::try_from(head["status"].as_u64()?).ok()?,
            gz: if gzip { gzip_if_worthwhile(content_type, &body) } else { None },
            etag: head["etag"].as_str()?.to_owned(),
            fresh_until: head["fresh"].as_u64()?,
            usable_until: head["usable"].as_u64()?,
            headers,
            body,
        })
    }
}

fn etag_of(body: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    format!("\"{:x}-{:x}\"", body.len(), h.finish())
}

type Flight = Arc<OnceCell<Option<Arc<Entry>>>>;

enum Filled {
    Cached(Arc<Entry>),
    NotCacheable(Produced),
}

pub struct ResponseCache {
    entries: Mutex<HashMap<String, (Arc<Entry>, u64)>>,
    use_counter: AtomicU64,
    in_flight: Mutex<HashMap<String, Flight>>,
    refreshing: Mutex<HashSet<String>>,
    redis: Option<Arc<RedisGuard>>,
    gzip: bool,
}

impl ResponseCache {
    pub fn new(redis: Option<Arc<RedisGuard>>, gzip: bool) -> Arc<Self> {
        Arc::new(Self { entries: Mutex::default(), use_counter: AtomicU64::new(0), in_flight: Mutex::default(), refreshing: Mutex::default(), redis, gzip })
    }

    pub async fn get(self: &Arc<Self>, key: &str, rule: CacheRule, produce: Producer) -> Outcome {
        let now = now_ms();
        let existing = match self.memory_entry(key, now) {
            Some(entry) => Some(entry),
            None => self.redis_entry(key, now).await,
        };
        match existing {
            Some(entry) if entry.fresh_until > now => Outcome::Cached(entry, CacheState::Hit),
            Some(entry) => {
                self.refresh_in_background(key, rule, produce);
                Outcome::Cached(entry, CacheState::Stale)
            }
            None => self.produce_once(key, rule, produce).await,
        }
    }

    async fn produce_once(self: &Arc<Self>, key: &str, rule: CacheRule, produce: Producer) -> Outcome {
        let (flight, is_leader) = {
            let mut in_flight = lock(&self.in_flight);
            match in_flight.get(key) {
                Some(flight) => (flight.clone(), false),
                None => {
                    let flight: Flight = Arc::default();
                    in_flight.insert(key.to_owned(), flight.clone());
                    (flight, true)
                }
            }
        };
        let _leaving = is_leader.then(|| {
            let flight = flight.clone();
            defer(move || {
                let mut in_flight = lock(&self.in_flight);
                if in_flight.get(key).is_some_and(|current| Arc::ptr_eq(current, &flight)) {
                    in_flight.remove(key);
                }
            })
        });
        let mut not_cacheable = None;
        let leaders_answer = &mut not_cacheable;
        let leader_produce = produce.clone();
        let entry = flight
            .get_or_init(move || async move {
                match self.fill(key, leader_produce().await, rule) {
                    Filled::Cached(entry) => Some(entry),
                    Filled::NotCacheable(produced) => {
                        *leaders_answer = Some(produced);
                        None
                    }
                }
            })
            .await
            .clone();
        match (entry, not_cacheable) {
            (Some(entry), _) => Outcome::Cached(entry, CacheState::Miss),
            (None, Some(produced)) => Outcome::Uncached(produced),
            (None, None) => Outcome::Uncached(produce().await),
        }
    }

    fn refresh_in_background(self: &Arc<Self>, key: &str, rule: CacheRule, produce: Producer) {
        if !lock(&self.refreshing).insert(key.to_owned()) {
            return;
        }
        let cache = self.clone();
        let key = key.to_owned();
        tokio::spawn(async move {
            let _done = defer(|| {
                lock(&cache.refreshing).remove(&key);
            });
            cache.fill(&key, produce().await, rule);
        });
    }

    fn fill(&self, key: &str, produced: Produced, rule: CacheRule) -> Filled {
        if produced.status != 200 || produced.headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("set-cookie")) {
            return Filled::NotCacheable(produced);
        }
        let entry = Arc::new(Entry::new(produced, rule, self.gzip));
        self.remember(key, entry.clone());
        self.write_redis(key, &entry);
        Filled::Cached(entry)
    }

    fn memory_entry(&self, key: &str, now: u64) -> Option<Arc<Entry>> {
        let used = self.use_counter.fetch_add(1, Ordering::Relaxed);
        let mut entries = lock(&self.entries);
        let (entry, last_used) = entries.get_mut(key)?;
        *last_used = used;
        (entry.usable_until > now).then(|| entry.clone())
    }

    fn remember(&self, key: &str, entry: Arc<Entry>) {
        let used = self.use_counter.fetch_add(1, Ordering::Relaxed);
        let mut entries = lock(&self.entries);
        entries.insert(key.to_owned(), (entry, used));
        if entries.len() > MAX_ENTRIES {
            let mut uses: Vec<u64> = entries.values().map(|(_, used)| *used).collect();
            uses.sort_unstable();
            let cutoff = uses[uses.len() / 10];
            let now = now_ms();
            entries.retain(|_, (entry, used)| *used > cutoff && entry.usable_until > now);
        }
    }

    async fn redis_entry(&self, key: &str, now: u64) -> Option<Arc<Entry>> {
        let redis = self.redis.as_ref().filter(|r| r.available())?;
        let redis_key = format!("{REDIS_PREFIX}{key}");
        let raw: Option<Vec<u8>> = redis.run(|mut c| async move { redis::cmd("GET").arg(&redis_key).query_async(&mut c).await }).await.ok()?;
        let entry = Arc::new(Entry::from_redis_bytes(&raw?, self.gzip).filter(|e| e.usable_until > now)?);
        self.remember(key, entry.clone());
        Some(entry)
    }

    fn write_redis(&self, key: &str, entry: &Entry) {
        let Some(redis) = self.redis.clone().filter(|r| r.available()) else { return };
        let Some(bytes) = entry.to_redis_bytes() else { return };
        let ttl_ms = entry.usable_until.saturating_sub(now_ms()).max(1);
        let redis_key = format!("{REDIS_PREFIX}{key}");
        tokio::spawn(async move {
            let _ = redis.run(|mut c| async move { redis::cmd("SET").arg(&redis_key).arg(bytes).arg("PX").arg(ttl_ms).query_async::<()>(&mut c).await }).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const RULE: CacheRule = CacheRule { fresh_secs: 60, stale_secs: 0 };

    fn producer(calls: Arc<AtomicUsize>, status: u16) -> Producer {
        Arc::new(move || {
            let calls = calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                Produced { status, headers: vec![("content-type".into(), "application/json".into())], body: Bytes::from_static(b"{}") }
            })
        })
    }

    fn state(outcome: &Outcome) -> Option<CacheState> {
        match outcome {
            Outcome::Cached(_, state) => Some(*state),
            Outcome::Uncached(_) => None,
        }
    }

    #[tokio::test]
    async fn single_flight_and_hits() {
        let cache = ResponseCache::new(None, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let p = producer(calls.clone(), 200);
        let (a, b) = tokio::join!(cache.get("k", RULE, p.clone()), cache.get("k", RULE, p.clone()));
        assert_eq!((state(&a), state(&b)), (Some(CacheState::Miss), Some(CacheState::Miss)));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "concurrent misses share one call");
        assert_eq!(state(&cache.get("k", RULE, p).await), Some(CacheState::Hit));
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let cache = ResponseCache::new(None, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let p = producer(calls.clone(), 500);
        assert_eq!(state(&cache.get("k", RULE, p.clone()).await), None);
        assert_eq!(state(&cache.get("k", RULE, p).await), None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_response_that_sets_a_cookie_is_not_cached() {
        let cache = ResponseCache::new(None, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let sets_cookie: Producer = Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Produced { status: 200, headers: vec![("Set-Cookie".into(), "session=a".into())], body: Bytes::from_static(b"{}") } })
        });
        assert_eq!(state(&cache.get("k", RULE, sets_cookie.clone()).await), None);
        assert_eq!(state(&cache.get("k", RULE, sets_cookie).await), None);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "every caller gets its own answer");
    }

    #[tokio::test]
    async fn a_cancelled_fill_does_not_pin_its_entry() {
        let cache = ResponseCache::new(None, true);
        let never: Producer = Arc::new(|| Box::pin(std::future::pending()));
        let cancelled = tokio::time::timeout(std::time::Duration::from_millis(10), cache.get("k", RULE, never)).await;
        assert!(cancelled.is_err());
        assert!(lock(&cache.in_flight).is_empty(), "the cancelled leader's flight is gone");
        let calls = Arc::new(AtomicUsize::new(0));
        assert_eq!(state(&cache.get("k", RULE, producer(calls.clone(), 200)).await), Some(CacheState::Miss));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(lock(&cache.in_flight).is_empty());
    }
}
