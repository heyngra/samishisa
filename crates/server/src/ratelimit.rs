use std::{
    collections::HashMap,
    hash::{BuildHasher, Hash, Hasher},
    net::IpAddr,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::{clock::now_ms, ipnet::unmap, redis::RedisGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub limit: u32,
    pub window_sec: u32,
    pub global: bool,
}

impl Limit {
    pub fn to_json(self) -> Value {
        json!({ "limit": self.limit, "windowSec": self.window_sec, "scope": if self.global { "global" } else { "client" } })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RateSetting {
    #[default]
    Inherit,
    Off,
    On(Limit),
}

impl RateSetting {
    fn from_json(value: Option<&Value>) -> Result<RateSetting, String> {
        let object = match value {
            None | Some(Value::Null) => return Ok(RateSetting::Inherit),
            Some(Value::Bool(false)) => return Ok(RateSetting::Off),
            Some(Value::Object(o)) => o,
            Some(other) => return Err(format!("a rate limit must be false or {{ \"limit\": N, \"windowSec\": S, \"scope\": \"client\"|\"global\" }} (got {other})")),
        };
        let positive = |key: &str| -> Result<Option<u32>, String> {
            match object.get(key) {
                None => Ok(None),
                Some(v) => match v.as_u64().filter(|n| *n > 0) {
                    Some(n) => Ok(Some(n.min(u32::MAX as u64) as u32)),
                    None => Err(format!("a rate limit's \"{key}\" must be a positive integer (got {v})")),
                },
            }
        };
        let limit = positive("limit")?.ok_or("a rate limit needs \"limit\": a positive integer")?;
        let window_sec = positive("windowSec")?.unwrap_or(60);
        let global = match object.get("scope").and_then(Value::as_str) {
            None | Some("client") => false,
            Some("global") => true,
            Some(other) => return Err(format!("a rate limit's \"scope\" must be \"client\" or \"global\" (got {other:?})")),
        };
        Ok(RateSetting::On(Limit { limit, window_sec, global }))
    }
}

impl<'de> Deserialize<'de> for RateSetting {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Option::<Value>::deserialize(deserializer)?;
        RateSetting::from_json(value.as_ref()).map_err(serde::de::Error::custom)
    }
}

pub struct RateSettings {
    pub server: RateSetting,
    pub operator: RateSetting,
    pub extension: RateSetting,
    pub route: RateSetting,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteLimit {
    pub limit: Limit,
    budget: String,
}

impl RouteLimit {
    pub fn resolve(settings: RateSettings, extension: &str, route_index: usize) -> Option<RouteLimit> {
        use RateSetting::{Inherit, Off, On};
        if settings.server == Off || settings.operator == Off {
            return None;
        }
        let limited = |limit: Limit, budget: String| Some(RouteLimit { limit, budget });
        match settings.route {
            Off => return None,
            On(limit) => return limited(limit, format!("{extension}:{route_index}")),
            Inherit => {}
        }
        let extension_wide = match settings.operator {
            On(limit) => On(limit),
            _ => settings.extension,
        };
        match (extension_wide, settings.server) {
            (On(limit), _) => limited(limit, format!("{extension}:*")),
            (Off, _) => None,
            (Inherit, On(limit)) => limited(limit, "_".into()),
            (Inherit, _) => None,
        }
    }

    pub fn key_for(&self, client: IpAddr) -> String {
        if self.limit.global { self.budget.clone() } else { format!("{}|{}", self.budget, client_bucket(client)) }
    }
}

fn client_bucket(ip: IpAddr) -> String {
    match unmap(ip) {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Decision {
    pub allowed: bool,
    pub limit: u32,
    pub remaining: u32,
    pub reset_sec: u64,
}

fn decide(previous: u64, current: u64, limit: Limit, now: u64) -> Decision {
    let window_ms = limit.window_sec as u64 * 1000;
    let elapsed_fraction = (now % window_ms) as f64 / window_ms as f64;
    let used = previous as f64 * (1.0 - elapsed_fraction) + current as f64;
    let reset_sec = ((window_ms - now % window_ms) as f64 / 1000.0).ceil().max(1.0) as u64;
    let remaining = (limit.limit as f64 - used - 1.0).max(0.0).floor() as u32;
    let allowed = used < limit.limit as f64;
    Decision { allowed, limit: limit.limit, remaining: if allowed { remaining } else { 0 }, reset_sec }
}

struct Counter {
    window: u64,
    current: u64,
    previous: u64,
}

const SHARDS: usize = 16;
const MAX_KEYS_PER_SHARD: usize = 16_384;

#[derive(Default)]
pub struct MemoryLimiter {
    shards: [Mutex<HashMap<String, Counter>>; SHARDS],
    hasher: std::collections::hash_map::RandomState,
}

impl MemoryLimiter {
    pub fn check(&self, key: &str, limit: Limit) -> Decision {
        self.check_at(key, limit, now_ms())
    }

    fn check_at(&self, key: &str, limit: Limit, now: u64) -> Decision {
        let window = now / (limit.window_sec as u64 * 1000);
        let mut h = self.hasher.build_hasher();
        key.hash(&mut h);
        let mut shard = self.shards[(h.finish() as usize) % SHARDS].lock().unwrap();
        if shard.len() >= MAX_KEYS_PER_SHARD && !shard.contains_key(key) {
            shard.retain(|_, c| c.window + 1 >= window);
            if shard.len() >= MAX_KEYS_PER_SHARD {
                shard.clear();
            }
        }
        let counter = shard.entry(key.to_owned()).or_insert(Counter { window, current: 0, previous: 0 });
        if counter.window != window {
            counter.previous = if counter.window + 1 == window { counter.current } else { 0 };
            counter.current = 0;
            counter.window = window;
        }
        let decision = decide(counter.previous, counter.current, limit, now);
        if decision.allowed {
            counter.current += 1;
        }
        decision
    }
}

pub enum Limiter {
    Memory(MemoryLimiter),
    Redis { redis: Arc<RedisGuard>, fallback: MemoryLimiter },
}

impl Limiter {
    pub fn new(redis: Option<Arc<RedisGuard>>) -> Limiter {
        match redis {
            Some(redis) => Limiter::Redis { redis, fallback: MemoryLimiter::default() },
            None => Limiter::Memory(MemoryLimiter::default()),
        }
    }

    pub async fn check(&self, key: &str, limit: Limit) -> Decision {
        match self {
            Limiter::Memory(memory) => memory.check(key, limit),
            Limiter::Redis { redis, fallback } if redis.available() => match check_redis(redis, key, limit).await {
                Some(decision) => decision,
                None => fallback.check(key, limit),
            },
            Limiter::Redis { fallback, .. } => fallback.check(key, limit),
        }
    }
}

async fn check_redis(redis: &RedisGuard, key: &str, limit: Limit) -> Option<Decision> {
    let window_ms = limit.window_sec as u64 * 1000;
    let now = now_ms();
    let window = now / window_ms;
    let current_key = format!("samishisa:rl:{key}:{window}");
    let previous_key = format!("samishisa:rl:{key}:{}", window.saturating_sub(1));
    let (current, previous) = redis
        .run(|mut c| async move {
            let (current, previous): (u64, Option<u64>) = redis::pipe().cmd("INCR").arg(&current_key).cmd("GET").arg(&previous_key).query_async(&mut c).await?;
            if current == 1 {
                let _: () = redis::cmd("PEXPIRE").arg(&current_key).arg(window_ms * 2).query_async(&mut c).await?;
            }
            Ok((current, previous.unwrap_or(0)))
        })
        .await
        .ok()?;
    Some(decide(previous, current - 1, limit, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREE_PER_10S: Limit = Limit { limit: 3, window_sec: 10, global: false };

    #[test]
    fn limits_and_slides() {
        let l = MemoryLimiter::default();
        let t0 = 1_000_000_000;
        for i in 0..3 {
            assert!(l.check_at("k", THREE_PER_10S, t0 + i).allowed);
        }
        let d = l.check_at("k", THREE_PER_10S, t0 + 5);
        assert!(!d.allowed);
        assert_eq!(d.remaining, 0);
        assert!(l.check_at("k", THREE_PER_10S, t0 + 15_000).allowed);
        assert!(l.check_at("k", THREE_PER_10S, t0 + 15_001).allowed);
        assert!(!l.check_at("k", THREE_PER_10S, t0 + 15_002).allowed);
        assert!(l.check_at("other", THREE_PER_10S, t0 + 5).allowed, "other callers are independent");
    }

    #[test]
    fn settings_parse() {
        let parse = |s: &str| serde_json::from_str::<RateSetting>(s);
        assert_eq!(parse("null").unwrap(), RateSetting::Inherit);
        assert_eq!(parse("false").unwrap(), RateSetting::Off);
        assert_eq!(parse(r#"{"limit": 5}"#).unwrap(), RateSetting::On(Limit { limit: 5, window_sec: 60, global: false }));
        assert!(parse("true").is_err());
        assert!(parse(r#"{"limit": 0}"#).is_err());
        assert!(parse(r#"{"limit": 5, "scope": "everyone"}"#).is_err());
    }

    #[test]
    fn most_specific_setting_wins() {
        use RateSetting::{Inherit, Off, On};
        let server = On(Limit { limit: 600, window_sec: 60, global: false });
        let own = Limit { limit: 3, window_sec: 60, global: false };
        let ext = Limit { limit: 30, window_sec: 60, global: false };
        let resolve = |operator, extension, route| RouteLimit::resolve(RateSettings { server, operator, extension, route }, "x", 2);
        let budget = |r: Option<RouteLimit>| r.map(|r| (r.limit.limit, r.budget));
        assert_eq!(budget(resolve(Inherit, Inherit, Inherit)), Some((600, "_".into())));
        assert_eq!(budget(resolve(Inherit, On(ext), Inherit)), Some((30, "x:*".into())));
        assert_eq!(budget(resolve(On(ext), Off, Inherit)), Some((30, "x:*".into())), "the operator overrides the extension");
        assert_eq!(budget(resolve(On(ext), Inherit, On(own))), Some((3, "x:2".into())), "the route's own limit wins");
        assert_eq!(budget(resolve(Off, Inherit, On(own))), None, "the operator can turn it off");
        assert_eq!(budget(resolve(Inherit, Off, Inherit)), None);
        assert_eq!(RouteLimit::resolve(RateSettings { server: Off, operator: Inherit, extension: Inherit, route: On(own) }, "x", 2), None);
    }

    #[test]
    fn ipv6_callers_share_a_64() {
        assert_eq!(client_bucket("2001:db8:1:2:3:4:5:6".parse().unwrap()), "2001:db8:1:2::/64");
        assert_eq!(client_bucket("::ffff:1.2.3.4".parse().unwrap()), "1.2.3.4");
    }
}
