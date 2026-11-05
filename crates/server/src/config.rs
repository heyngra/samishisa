use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{clientip::TrustProxy, ratelimit::Setting};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default)]
    pub threads: usize,
    #[serde(default = "default_timeout")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_body")]
    pub max_body_bytes: usize,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_rate")]
    pub rate_limit: Value,
    #[serde(default = "default_trust")]
    pub trust_proxy: Value,
    #[serde(default)]
    pub redis: Option<String>,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default = "yes")]
    pub access_log: bool,
    #[serde(default = "yes")]
    pub compression: bool,
    #[serde(default = "yes")]
    pub cors: bool,
    #[serde(default = "default_instances")]
    pub max_instances: u32,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub extensions: Vec<Entry>,
    #[serde(skip)]
    pub base_dir: PathBuf,
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NetworkConfig {
    #[serde(default)]
    pub allow_private: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Entry {
    Source(String),
    Full(FullEntry),
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FullEntry {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl Entry {
    pub fn full(&self) -> FullEntry {
        match self {
            Entry::Source(s) => FullEntry { source: s.clone(), ..Default::default() },
            Entry::Full(f) => f.clone(),
        }
    }
    pub fn source(&self) -> &str {
        match self {
            Entry::Source(s) => s,
            Entry::Full(f) => &f.source,
        }
    }
}

pub const DEFAULT_MEMORY_MB: u32 = 64;

fn default_port() -> u16 {
    3000
}
fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_timeout() -> u64 {
    10_000
}
fn default_body() -> usize {
    1 << 20
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}
fn default_rate() -> Value {
    serde_json::json!({ "limit": 600, "windowSec": 60 })
}
fn default_trust() -> Value {
    Value::String("private".into())
}
fn default_instances() -> u32 {
    256
}
fn yes() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        serde_json::from_str("{}").expect("defaults")
    }
}

impl Config {
    pub fn load(path: &Path, must_exist: bool) -> anyhow::Result<Self> {
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let mut c = serde_json::from_str::<Config>(&text).with_context(|| format!("{} is not a valid samishisa config", path.display()))?;
            c.path = Some(path.to_path_buf());
            c
        } else if must_exist {
            bail!("config file {} not found (create one with `samishisa init`)", path.display());
        } else {
            Config::default()
        };
        cfg.base_dir = path.parent().map(Path::to_path_buf).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from("."));
        if let Ok(p) = std::env::var("PORT") {
            cfg.port = p.parse().context("PORT must be a number")?;
        }
        if let Ok(r) = std::env::var("REDIS_URL") {
            if !r.is_empty() {
                cfg.redis = Some(r);
            }
        }
        if let Ok(d) = std::env::var("SAMISHISA_DATA") {
            cfg.data_dir = PathBuf::from(d);
        }
        if cfg.data_dir.is_relative() {
            cfg.data_dir = cfg.base_dir.join(&cfg.data_dir);
        }
        cfg.rate_setting()?;
        cfg.trust()?;
        if cfg.max_instances == 0 {
            bail!("maxInstances must be at least 1");
        }
        for (i, e) in cfg.extensions.iter().enumerate() {
            let f = e.full();
            Setting::from_value(f.rate_limit.as_ref(), &format!("extensions[{i}].rateLimit"))?;
            if let Some(n) = &f.network {
                if !(n.as_str() == Some("*") || n.as_array().is_some_and(|a| a.iter().all(Value::is_string))) {
                    bail!("extensions[{i}].network must be a list of hosts or \"*\"");
                }
            }
        }
        Ok(cfg)
    }

    pub fn rate_setting(&self) -> anyhow::Result<Setting> {
        Setting::from_value(Some(&self.rate_limit), "rateLimit")
    }

    pub fn trust(&self) -> anyhow::Result<TrustProxy> {
        TrustProxy::from_value(&self.trust_proxy)
    }

    pub fn resolve_path(&self, p: &str) -> PathBuf {
        let p = match p.strip_prefix("~/") {
            Some(rest) => std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_default().join(rest),
            None => PathBuf::from(p),
        };
        if p.is_absolute() { p } else { self.base_dir.join(p) }
    }

    pub fn max_memory_mb(&self) -> u32 {
        self.extensions.iter().filter_map(|e| e.full().memory_mb).max().unwrap_or(DEFAULT_MEMORY_MB).max(DEFAULT_MEMORY_MB)
    }
}

pub fn interpolate_env(v: &Value, at: &str) -> anyhow::Result<Value> {
    Ok(match v {
        Value::String(s) if s.contains("${") => {
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(start) = rest.find("${") {
                out.push_str(&rest[..start]);
                let end = rest[start..].find('}').with_context(|| format!("{at}: unclosed ${{ in {s:?}"))? + start;
                let var = &rest[start + 2..end];
                let val = std::env::var(var).with_context(|| format!("{at}: environment variable {var} is not set"))?;
                out.push_str(&val);
                rest = &rest[end + 1..];
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().enumerate().map(|(i, x)| interpolate_env(x, &format!("{at}[{i}]"))).collect::<Result<_, _>>()?),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| Ok((k.clone(), interpolate_env(x, &format!("{at}.{k}"))?))).collect::<anyhow::Result<_>>()?),
        other => other.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_interpolation() {
        unsafe { std::env::set_var("SAMISHISA_TEST_KEY", "k123") };
        let v = serde_json::json!({ "a": "${SAMISHISA_TEST_KEY}", "b": ["x-${SAMISHISA_TEST_KEY}-y"], "c": 1 });
        let out = interpolate_env(&v, "config").unwrap();
        assert_eq!(out, serde_json::json!({ "a": "k123", "b": ["x-k123-y"], "c": 1 }));
        let err = interpolate_env(&serde_json::json!("${SAMISHISA_MISSING_VAR}"), "config.x").unwrap_err();
        assert!(err.to_string().contains("SAMISHISA_MISSING_VAR"));
    }

    #[test]
    fn entries() {
        let c: Config = serde_json::from_str(r#"{ "extensions": ["./a.wasm", { "source": "github:o/r#v1", "name": "x", "rateLimit": false }] }"#).unwrap();
        assert_eq!(c.extensions[0].source(), "./a.wasm");
        assert_eq!(c.extensions[1].full().name.as_deref(), Some("x"));
        assert!(serde_json::from_str::<Config>(r#"{ "prot": 1 }"#).is_err(), "typos in the config are errors");
    }
}
