use std::{
    collections::HashMap,
    hash::{BuildHasher, Hash, Hasher},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use crate::redis::RedisGuard;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub limit: u32,
    pub window_sec: u32,
    pub global: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Inherit,
    Off,
    On(Spec),
}

impl Setting {
    pub fn from_value(v: Option<&Value>, at: &str) -> anyhow::Result<Setting> {
        Ok(match v {
            None | Some(Value::Null) => Setting::Inherit,
            Some(Value::Bool(false)) => Setting::Off,
            Some(Value::Object(o)) => {
                let limit = o.get("limit").and_then(Value::as_u64).filter(|l| *l > 0).ok_or_else(|| anyhow::anyhow!("{at}.limit must be a positive integer"))?;
                let window_sec = o.get("windowSec").map(|w| w.as_u64().filter(|w| *w > 0).ok_or_else(|| anyhow::anyhow!("{at}.windowSec must be a positive integer"))).transpose()?.unwrap_or(60);
                let global = match o.get("scope").and_then(Value::as_str) {
                    None | Some("client") => false,
                    Some("global") => true,
                    Some(s) => anyhow::bail!("{at}.scope must be \"client\" or \"global\" (got {s:?})"),
                };
                Setting::On(Spec { limit: limit.min(u32::MAX as u64) as u32, window_sec: window_sec.min(u32::MAX as u64) as u32, global })
            }
            Some(other) => anyhow::bail!("{at} must be false or {{ \"limit\": N, \"windowSec\": S, \"scope\": \"client\"|\"global\" }} (got {other})"),
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub ok: bool,
    pub limit: u32,
    pub remaining: u32,
    pub reset_sec: u64,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn decide(prev: u64, cur_before: u64, limit: u32, window_ms: u64, now: u64) -> Decision {
    let elapsed = (now % window_ms) as f64 / window_ms as f64;
    let used = prev as f64 * (1.0 - elapsed) + cur_before as f64;
    let reset_sec = ((window_ms - now % window_ms) as f64 / 1000.0).ceil().max(1.0) as u64;
    if used >= limit as f64 {
        Decision { ok: false, limit, remaining: 0, reset_sec }
    } else {
        Decision { ok: true, limit, remaining: (limit as f64 - used - 1.0).max(0.0).floor() as u32, reset_sec }
    }
}

struct Slot {
    window: u64,
    count: u64,
    prev: u64,
}

const SHARDS: usize = 16;
const MAX_KEYS_PER_SHARD: usize = 16_384;

#[derive(Default)]
pub struct MemoryLimiter {
    shards: [Mutex<HashMap<String, Slot>>; SHARDS],
    hasher: std::collections::hash_map::RandomState,
}

impl MemoryLimiter {
    pub fn check(&self, key: &str, limit: u32, window_sec: u32) -> Decision {
        self.check_at(key, limit, window_sec, now_ms())
    }

    fn check_at(&self, key: &str, limit: u32, window_sec: u32, now: u64) -> Decision {
        let window_ms = window_sec as u64 * 1000;
        let w = now / window_ms;
        let mut h = self.hasher.build_hasher();
        key.hash(&mut h);
        let mut shard = self.shards[(h.finish() as usize) % SHARDS].lock().unwrap();
        if shard.len() >= MAX_KEYS_PER_SHARD && !shard.contains_key(key) {
            shard.retain(|_, s| s.window + 1 >= w);
            if shard.len() >= MAX_KEYS_PER_SHARD {
                shard.clear();
            }
        }
        let slot = shard.entry(key.to_owned()).or_insert(Slot { window: w, count: 0, prev: 0 });
        if slot.window != w {
            slot.prev = if slot.window + 1 == w { slot.count } else { 0 };
            slot.count = 0;
            slot.window = w;
        }
        let d = decide(slot.prev, slot.count, limit, window_ms, now);
        if d.ok {
            slot.count += 1;
        }
        d
    }
}

pub enum Limiter {
    Memory(MemoryLimiter),
    Redis { guard: Arc<RedisGuard>, fallback: MemoryLimiter },
}

impl Limiter {
    pub async fn check(&self, key: &str, spec: Spec) -> Decision {
        match self {
            Limiter::Memory(m) => m.check(key, spec.limit, spec.window_sec),
            Limiter::Redis { guard, fallback } => {
                if !guard.available() {
                    return fallback.check(key, spec.limit, spec.window_sec);
                }
                let window_ms = spec.window_sec as u64 * 1000;
                let now = now_ms();
                let w = now / window_ms;
                let cur = format!("samishisa:rl:{key}:{w}");
                let prev = format!("samishisa:rl:{key}:{}", w.saturating_sub(1));
                let r = guard
                    .run(|mut c| async move {
                        let (count, previous): (u64, Option<u64>) = redis::pipe().cmd("INCR").arg(&cur).cmd("GET").arg(&prev).query_async(&mut c).await?;
                        if count == 1 {
                            let _: () = redis::cmd("PEXPIRE").arg(&cur).arg(window_ms * 2).query_async(&mut c).await?;
                        }
                        Ok((count, previous.unwrap_or(0)))
                    })
                    .await;
                match r {
                    Ok((count, previous)) => decide(previous, count - 1, spec.limit, window_ms, now),
                    Err(_) => fallback.check(key, spec.limit, spec.window_sec),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_and_slides() {
        let l = MemoryLimiter::default();
        let t0 = 1_000_000_000;
        for i in 0..3 {
            assert!(l.check_at("k", 3, 10, t0 + i).ok);
        }
        let d = l.check_at("k", 3, 10, t0 + 5);
        assert!(!d.ok);
        assert_eq!(d.remaining, 0);
        assert!(l.check_at("k", 3, 10, t0 + 15_000).ok);
        assert!(l.check_at("k", 3, 10, t0 + 15_001).ok);
        assert!(!l.check_at("k", 3, 10, t0 + 15_002).ok);
        assert!(l.check_at("other", 3, 10, t0 + 5).ok);
    }

    #[test]
    fn settings() {
        use serde_json::json;
        assert_eq!(Setting::from_value(None, "x").unwrap(), Setting::Inherit);
        assert_eq!(Setting::from_value(Some(&json!(false)), "x").unwrap(), Setting::Off);
        assert_eq!(Setting::from_value(Some(&json!({"limit": 5})), "x").unwrap(), Setting::On(Spec { limit: 5, window_sec: 60, global: false }));
        assert!(Setting::from_value(Some(&json!(true)), "x").is_err());
        assert!(Setting::from_value(Some(&json!({"limit": 5, "scope": "everyone"})), "x").is_err());
    }
}
