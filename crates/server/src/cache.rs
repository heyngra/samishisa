use std::{
    collections::HashMap,
    future::Future,
    hash::{Hash, Hasher},
    io::Write,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use bytes::Bytes;
use tokio::sync::OnceCell;

use crate::redis::RedisGuard;

pub const GZIP_MIN_BYTES: usize = 1024;

#[derive(Debug)]
pub struct Entry {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    pub gz: Option<Bytes>,
    pub etag: String,
    pub fresh_until: u64,
    pub usable_until: u64,
}

pub struct Produced {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

pub type Producer = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Produced> + Send>> + Send + Sync>;

pub enum Outcome {
    Hit(Arc<Entry>),
    Stale(Arc<Entry>),
    Miss(Arc<Entry>),
    Uncached(Produced),
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn gzip(body: &[u8]) -> Option<Bytes> {
    let mut enc = flate2::write::GzEncoder::new(Vec::with_capacity(body.len() / 3), flate2::Compression::new(6));
    enc.write_all(body).ok()?;
    enc.finish().ok().map(Bytes::from)
}

pub fn etag(body: &[u8]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    format!("\"{:x}-{:x}\"", body.len(), h.finish())
}

fn make_entry(p: &Produced, ttl: u32, stale: u32, compress: bool) -> Entry {
    let now = now_ms();
    let compressible = p.headers.iter().any(|(k, v)| k == "content-type" && is_compressible(v));
    Entry {
        status: p.status,
        headers: p.headers.clone(),
        gz: if compress && compressible && p.body.len() >= GZIP_MIN_BYTES { gzip(&p.body) } else { None },
        etag: etag(&p.body),
        body: p.body.clone(),
        fresh_until: now + ttl as u64 * 1000,
        usable_until: now + (ttl as u64 + stale as u64) * 1000,
    }
}

pub fn is_compressible(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    ct.starts_with("text/") || ct.contains("json") || ct.contains("xml") || ct.contains("javascript")
}

type Flight = Arc<OnceCell<Option<Arc<Entry>>>>;

pub struct ResponseCache {
    map: Mutex<HashMap<String, (Arc<Entry>, u64)>>,
    inflight: Mutex<HashMap<String, Flight>>,
    refreshing: Mutex<std::collections::HashSet<String>>,
    redis: Option<Arc<RedisGuard>>,
    compress: bool,
    max_entries: usize,
    clock: std::sync::atomic::AtomicU64,
}

impl ResponseCache {
    pub fn new(max_entries: usize, redis: Option<Arc<RedisGuard>>, compress: bool) -> Arc<Self> {
        Arc::new(Self {
            map: Mutex::default(),
            inflight: Mutex::default(),
            refreshing: Mutex::default(),
            redis,
            compress,
            max_entries,
            clock: Default::default(),
        })
    }

    fn lookup(&self, key: &str) -> Option<Arc<Entry>> {
        let tick = self.clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut map = self.map.lock().unwrap();
        let slot = map.get_mut(key)?;
        slot.1 = tick;
        Some(slot.0.clone())
    }

    fn store(&self, key: &str, e: Arc<Entry>) {
        let tick = self.clock.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut map = self.map.lock().unwrap();
        map.insert(key.to_owned(), (e, tick));
        if map.len() > self.max_entries {
            let mut ticks: Vec<u64> = map.values().map(|(_, t)| *t).collect();
            ticks.sort_unstable();
            let cutoff = ticks[ticks.len() / 10];
            let now = now_ms();
            map.retain(|_, (e, t)| *t > cutoff && e.usable_until > now);
        }
    }

    pub async fn get(self: &Arc<Self>, key: &str, ttl: u32, stale: u32, produce: Producer) -> Outcome {
        let now = now_ms();
        if let Some(e) = self.lookup(key) {
            if e.fresh_until > now {
                return Outcome::Hit(e);
            }
            if e.usable_until > now {
                self.refresh_in_background(key, ttl, stale, produce);
                return Outcome::Stale(e);
            }
        }
        if let Some(r) = &self.redis {
            if r.available() {
                if let Some(e) = self.read_redis(r, key).await.filter(|e| e.usable_until > now) {
                    let e = Arc::new(e);
                    self.store(key, e.clone());
                    if e.fresh_until <= now {
                        self.refresh_in_background(key, ttl, stale, produce);
                        return Outcome::Stale(e);
                    }
                    return Outcome::Hit(e);
                }
            }
        }
        let (flight, leader) = {
            let mut inflight = self.inflight.lock().unwrap();
            match inflight.get(key) {
                Some(f) => (f.clone(), false),
                None => {
                    let f: Flight = Arc::new(OnceCell::new());
                    inflight.insert(key.to_owned(), f.clone());
                    (f, true)
                }
            }
        };
        let mut uncached = None;
        let slot = &mut uncached;
        let leader_produce = produce.clone();
        let entry = flight
            .get_or_init(move || async move {
                let p = leader_produce().await;
                if p.status != 200 {
                    *slot = Some(p);
                    return None;
                }
                let e = Arc::new(make_entry(&p, ttl, stale, self.compress));
                self.store(key, e.clone());
                self.write_redis(key, &e);
                Some(e)
            })
            .await
            .clone();
        if leader {
            self.inflight.lock().unwrap().remove(key);
        }
        match (entry, uncached) {
            (Some(e), _) => Outcome::Miss(e),
            (None, Some(p)) => Outcome::Uncached(p),
            (None, None) => Outcome::Uncached(produce().await),
        }
    }

    fn refresh_in_background(self: &Arc<Self>, key: &str, ttl: u32, stale: u32, produce: Producer) {
        if !self.refreshing.lock().unwrap().insert(key.to_owned()) {
            return;
        }
        let me = self.clone();
        let key = key.to_owned();
        tokio::spawn(async move {
            let p = produce().await;
            if p.status == 200 {
                let e = Arc::new(make_entry(&p, ttl, stale, me.compress));
                me.store(&key, e.clone());
                me.write_redis(&key, &e);
            }
            me.refreshing.lock().unwrap().remove(&key);
        });
    }

    async fn read_redis(&self, r: &RedisGuard, key: &str) -> Option<Entry> {
        let k = format!("samishisa:rc:{key}");
        let raw: Option<Vec<u8>> = r.run(|mut c| async move { redis::cmd("GET").arg(&k).query_async(&mut c).await }).await.ok()?;
        let raw = raw?;
        let len = u32::from_le_bytes(raw.get(..4)?.try_into().ok()?) as usize;
        let head: serde_json::Value = serde_json::from_slice(raw.get(4..4 + len)?).ok()?;
        let body = Bytes::copy_from_slice(raw.get(4 + len..)?);
        let headers = head["headers"].as_array()?.iter().filter_map(|h| Some((h[0].as_str()?.to_owned(), h[1].as_str()?.to_owned()))).collect::<Vec<_>>();
        let compressible = headers.iter().any(|(k, v)| k == "content-type" && is_compressible(v));
        Some(Entry {
            status: head["status"].as_u64()? as u16,
            gz: if self.compress && compressible && body.len() >= GZIP_MIN_BYTES { gzip(&body) } else { None },
            etag: head["etag"].as_str()?.to_owned(),
            fresh_until: head["fresh"].as_u64()?,
            usable_until: head["usable"].as_u64()?,
            headers,
            body,
        })
    }

    fn write_redis(&self, key: &str, e: &Entry) {
        let Some(r) = self.redis.clone() else { return };
        if !r.available() {
            return;
        }
        let head = serde_json::json!({ "status": e.status, "headers": e.headers, "etag": e.etag, "fresh": e.fresh_until, "usable": e.usable_until }).to_string();
        let mut buf = Vec::with_capacity(4 + head.len() + e.body.len());
        buf.extend_from_slice(&(head.len() as u32).to_le_bytes());
        buf.extend_from_slice(head.as_bytes());
        buf.extend_from_slice(&e.body);
        let ttl_ms = e.usable_until.saturating_sub(now_ms()).max(1);
        let k = format!("samishisa:rc:{key}");
        tokio::spawn(async move {
            let _ = r.run(|mut c| async move { redis::cmd("SET").arg(&k).arg(buf).arg("PX").arg(ttl_ms).query_async::<()>(&mut c).await }).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[tokio::test]
    async fn single_flight_and_hits() {
        let cache = ResponseCache::new(100, None, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let p = producer(calls.clone(), 200);
        let (a, b) = tokio::join!(cache.get("k", 60, 0, p.clone()), cache.get("k", 60, 0, p.clone()));
        assert!(matches!(a, Outcome::Miss(_)) && matches!(b, Outcome::Miss(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "concurrent misses share one call");
        assert!(matches!(cache.get("k", 60, 0, p).await, Outcome::Hit(_)));
    }

    #[tokio::test]
    async fn errors_are_not_cached() {
        let cache = ResponseCache::new(100, None, true);
        let calls = Arc::new(AtomicUsize::new(0));
        let p = producer(calls.clone(), 500);
        assert!(matches!(cache.get("k", 60, 0, p.clone()).await, Outcome::Uncached(_)));
        assert!(matches!(cache.get("k", 60, 0, p).await, Outcome::Uncached(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
