use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};

pub struct Store {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
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
        let c = self.conn.lock().unwrap();
        c.prepare_cached("SELECT value FROM kv WHERE key = ?1 AND (expires IS NULL OR expires >= ?2)")
            .and_then(|mut s| s.query_row(params![key, now_ms()], |r| r.get(0)).optional())
            .unwrap_or_else(|e| {
                crate::log::write(crate::log::Level::Error, "store", &format!("get {key:?}: {e}"));
                None
            })
    }

    pub fn set(&self, key: &str, value: &str, ttl: Option<Duration>) {
        let expires = ttl.map(|t| now_ms() + t.as_millis() as i64);
        let c = self.conn.lock().unwrap();
        if let Err(e) = c.prepare_cached("INSERT OR REPLACE INTO kv (key, value, expires) VALUES (?1, ?2, ?3)").and_then(|mut s| s.execute(params![key, value, expires])) {
            crate::log::write(crate::log::Level::Error, "store", &format!("set {key:?}: {e}"));
        }
    }

    pub fn delete(&self, key: &str) {
        let c = self.conn.lock().unwrap();
        let _ = c.prepare_cached("DELETE FROM kv WHERE key = ?1").and_then(|mut s| s.execute(params![key]));
    }

    pub fn incr(&self, key: &str, by: i64) -> i64 {
        let mut c = self.conn.lock().unwrap();
        let run = |c: &mut Connection| -> rusqlite::Result<i64> {
            let tx = c.transaction()?;
            let row: Option<(String, Option<i64>)> = tx
                .prepare_cached("SELECT value, expires FROM kv WHERE key = ?1")?
                .query_row(params![key], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            let now = now_ms();
            let (current, expires) = match row {
                Some((v, exp)) if exp.is_none_or(|e| e >= now) => (v.trim().parse::<i64>().unwrap_or(0), exp),
                _ => (0, None),
            };
            let next = current.saturating_add(by);
            tx.prepare_cached("INSERT OR REPLACE INTO kv (key, value, expires) VALUES (?1, ?2, ?3)")?.execute(params![key, next.to_string(), expires])?;
            tx.commit()?;
            Ok(next)
        };
        run(&mut c).unwrap_or_else(|e| {
            crate::log::write(crate::log::Level::Error, "store", &format!("incr {key:?}: {e}"));
            0
        })
    }

    pub fn keys(&self, prefix: &str) -> Vec<String> {
        let c = self.conn.lock().unwrap();
        let r = c.prepare_cached("SELECT key FROM kv WHERE substr(key, 1, ?1) = ?2 AND (expires IS NULL OR expires >= ?3) ORDER BY key").and_then(|mut s| {
            s.query_map(params![prefix.chars().count() as i64, prefix, now_ms()], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()
        });
        r.unwrap_or_default()
    }

    pub fn prune(&self) {
        let c = self.conn.lock().unwrap();
        let _ = c.execute("DELETE FROM kv WHERE expires IS NOT NULL AND expires < ?1", params![now_ms()]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(Some(&dir.path().join("ext/x/store.db"))).unwrap();
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
        drop(s);
        let again = Store::open(Some(&dir.path().join("ext/x/store.db"))).unwrap();
        assert_eq!(again.get("b:1").as_deref(), Some("\"x\""), "data survives reopening");
    }
}
