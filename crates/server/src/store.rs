use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{clock, log::log_error, sync::lock};

pub struct Store {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    i64::try_from(clock::now_ms()).unwrap_or(i64::MAX)
}

fn or_log<T>(operation: &str, key: &str, result: rusqlite::Result<T>, fallback: T) -> T {
    result.unwrap_or_else(|e| {
        log_error!("store", "{operation} {key:?}: {e}");
        fallback
    })
}

impl Store {
    pub fn open(path: Option<&Path>) -> anyhow::Result<Self> {
        let conn = match path {
            Some(p) => {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                }
                Connection::open(p).with_context(|| format!("opening {}", p.display()))?
            }
            None => Connection::open_in_memory()?,
        };
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, expires INTEGER);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let c = lock(&self.conn);
        let result = c
            .prepare_cached("SELECT value FROM kv WHERE key = ?1 AND (expires IS NULL OR expires >= ?2)")
            .and_then(|mut s| s.query_row(params![key, now_ms()], |r| r.get(0)).optional());
        or_log("get", key, result, None)
    }

    pub fn set(&self, key: &str, value: &str, ttl: Option<Duration>) {
        let expires = ttl.map(|t| now_ms().saturating_add(i64::try_from(t.as_millis()).unwrap_or(i64::MAX)));
        let c = lock(&self.conn);
        let result = c.prepare_cached("INSERT OR REPLACE INTO kv (key, value, expires) VALUES (?1, ?2, ?3)").and_then(|mut s| s.execute(params![key, value, expires]));
        or_log("set", key, result.map(drop), ());
    }

    pub fn delete(&self, key: &str) {
        let c = lock(&self.conn);
        let result = c.prepare_cached("DELETE FROM kv WHERE key = ?1").and_then(|mut s| s.execute(params![key]));
        or_log("delete", key, result.map(drop), ());
    }

    pub fn incr(&self, key: &str, by: i64) -> i64 {
        let mut c = lock(&self.conn);
        let result = (|| -> rusqlite::Result<i64> {
            let tx = c.transaction()?;
            let row: Option<(String, Option<i64>)> =
                tx.prepare_cached("SELECT value, expires FROM kv WHERE key = ?1")?.query_row(params![key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let (current, expires) = match row {
                Some((value, expires)) if expires.is_none_or(|e| e >= now_ms()) => (value.trim().parse::<i64>().unwrap_or(0), expires),
                _ => (0, None),
            };
            let next = current.saturating_add(by);
            tx.prepare_cached("INSERT OR REPLACE INTO kv (key, value, expires) VALUES (?1, ?2, ?3)")?.execute(params![key, next.to_string(), expires])?;
            tx.commit()?;
            Ok(next)
        })();
        or_log("incr", key, result, 0)
    }

    pub fn keys(&self, prefix: &str) -> Vec<String> {
        let c = lock(&self.conn);
        let result = c.prepare_cached("SELECT key FROM kv WHERE substr(key, 1, ?1) = ?2 AND (expires IS NULL OR expires >= ?3) ORDER BY key").and_then(|mut s| {
            s.query_map(params![i64::try_from(prefix.chars().count()).unwrap_or(i64::MAX), prefix, now_ms()], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()
        });
        or_log("keys", prefix, result, Vec::new())
    }

    pub fn delete_expired(&self) {
        let c = lock(&self.conn);
        let result = c.execute("DELETE FROM kv WHERE expires IS NOT NULL AND expires < ?1", params![now_ms()]);
        or_log("delete expired", "", result.map(drop), ());
    }
}

#[derive(Default)]
pub struct Stores {
    open: Mutex<HashMap<PathBuf, Arc<Store>>>,
}

impl Stores {
    pub fn open(&self, data_dir: &Path, extension: &str) -> anyhow::Result<Arc<Store>> {
        let path = data_dir.join("ext").join(extension).join("store.db");
        let mut open = lock(&self.open);
        if let Some(store) = open.get(&path) {
            return Ok(store.clone());
        }
        let store = Arc::new(Store::open(Some(&path))?);
        open.insert(path, store.clone());
        Ok(store)
    }

    pub fn delete_expired_every_minute(self: &Arc<Self>) {
        let stores = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let all: Vec<Arc<Store>> = lock(&stores.open).values().cloned().collect();
                let swept = tokio::task::spawn_blocking(move || {
                    for store in all {
                        store.delete_expired();
                    }
                });
                if let Err(e) = swept.await {
                    log_error!("store", "deleting expired keys failed: {e}");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let stores = Stores::default();
        let s = stores.open(dir.path(), "x").unwrap();
        assert_eq!(s.get("a"), None);
        s.set("a", "1", None);
        assert_eq!(s.get("a").as_deref(), Some("1"));
        assert_eq!(s.incr("a", 2), 3);
        assert_eq!(s.incr("new", 5), 5);
        s.set("b:1", "\"x\"", None);
        s.set("b:2", "\"y\"", Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(s.keys("b:"), vec!["b:1".to_string()], "expired keys are hidden");
        assert_eq!(s.incr("b:2", 1), 1, "an expired counter restarts at 0");
        s.delete("a");
        assert_eq!(s.get("a"), None);
        assert!(Arc::ptr_eq(&s, &stores.open(dir.path(), "x").unwrap()), "one store per extension");
        drop((s, stores));
        let again = Store::open(Some(&dir.path().join("ext/x/store.db"))).unwrap();
        assert_eq!(again.get("b:1").as_deref(), Some("\"x\""), "data survives reopening");
    }
}
