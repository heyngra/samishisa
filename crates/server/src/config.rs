use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::{
    clientip::TrustProxy,
    netguard::Policy,
    ratelimit::{Limit, RateSetting},
};

pub const DEFAULT_MEMORY_MB: u32 = 64;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    pub port: u16,
    pub host: String,
    pub threads: usize,
    pub request_timeout_ms: u64,
    pub max_body_bytes: usize,
    pub data_dir: PathBuf,
    pub rate_limit: RateSetting,
    pub trust_proxy: TrustProxy,
    pub redis: Option<String>,
    pub network: NetworkConfig,
    pub access_log: bool,
    pub compression: bool,
    pub cors: bool,
    pub max_instances: u32,
    pub title: String,
    pub extensions: Vec<ExtensionEntry>,
    #[serde(skip)]
    pub base_dir: PathBuf,
    #[serde(skip)]
    pub path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: 3000,
            host: "0.0.0.0".into(),
            threads: 0,
            request_timeout_ms: 10_000,
            max_body_bytes: 1 << 20,
            data_dir: PathBuf::from("data"),
            rate_limit: RateSetting::On(Limit { limit: 600, window_sec: 60, global: false }),
            trust_proxy: TrustProxy::Private,
            redis: None,
            network: NetworkConfig::default(),
            access_log: true,
            compression: true,
            cors: true,
            max_instances: 256,
            title: "samishisa".into(),
            extensions: Vec::new(),
            base_dir: PathBuf::from("."),
            path: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct NetworkConfig {
    pub allow_private: bool,
}

#[derive(Debug, Clone)]
pub struct ExtensionEntry {
    pub source: String,
    pub name: Option<String>,
    pub config: Value,
    pub network: Option<Policy>,
    pub rate_limit: RateSetting,
    pub memory_mb: u32,
    pub asset: Option<String>,
    pub sha256: Option<String>,
}

impl ExtensionEntry {
    pub fn from_source(source: impl Into<String>) -> Self {
        ExtensionEntry {
            source: source.into(),
            name: None,
            config: Value::Object(Default::default()),
            network: None,
            rate_limit: RateSetting::Inherit,
            memory_mb: DEFAULT_MEMORY_MB,
            asset: None,
            sha256: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct EntryObject {
    source: String,
    name: Option<String>,
    config: Option<Value>,
    network: Option<Policy>,
    #[serde(default)]
    rate_limit: RateSetting,
    memory_mb: Option<u32>,
    asset: Option<String>,
    sha256: Option<String>,
}

impl<'de> Deserialize<'de> for ExtensionEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let object = match Value::deserialize(deserializer)? {
            Value::String(source) => return Ok(ExtensionEntry::from_source(source)),
            other => EntryObject::deserialize(other).map_err(|e| serde::de::Error::custom(format!("in an extension entry: {e}")))?,
        };
        Ok(ExtensionEntry {
            source: object.source,
            name: object.name,
            config: object.config.unwrap_or_else(|| Value::Object(Default::default())),
            network: object.network,
            rate_limit: object.rate_limit,
            memory_mb: object.memory_mb.unwrap_or(DEFAULT_MEMORY_MB),
            asset: object.asset,
            sha256: object.sha256,
        })
    }
}

impl Config {
    pub fn load(path: &Path, must_exist: bool) -> anyhow::Result<Self> {
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
            let mut cfg: Config = serde_json::from_str(&text).with_context(|| format!("{} is not a valid samishisa config", path.display()))?;
            cfg.path = Some(path.to_path_buf());
            cfg
        } else if must_exist {
            bail!("config file {} not found (create one with `samishisa init`)", path.display());
        } else {
            Config::default()
        };
        cfg.base_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        if let Ok(port) = std::env::var("PORT") {
            cfg.port = port.parse().context("PORT must be a number")?;
        }
        if let Ok(url) = std::env::var("REDIS_URL").map(|u| u.trim().to_owned()) {
            if !url.is_empty() {
                cfg.redis = Some(url);
            }
        }
        if let Ok(dir) = std::env::var("SAMISHISA_DATA") {
            cfg.data_dir = PathBuf::from(dir);
        }
        if cfg.data_dir.is_relative() {
            cfg.data_dir = cfg.base_dir.join(&cfg.data_dir);
        }
        if cfg.max_instances == 0 {
            bail!("maxInstances must be at least 1");
        }
        Ok(cfg)
    }

    pub fn serving_threads(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            n => n,
        }
    }

    pub fn max_memory_mb(&self) -> u32 {
        self.extensions.iter().map(|e| e.memory_mb).max().unwrap_or(DEFAULT_MEMORY_MB).max(DEFAULT_MEMORY_MB)
    }
}

pub fn interpolate_env(value: &Value, at: &str) -> anyhow::Result<Value> {
    Ok(match value {
        Value::String(s) if s.contains("${") => {
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(start) = rest.find("${") {
                out.push_str(&rest[..start]);
                let end = rest[start..].find('}').with_context(|| format!("{at}: unclosed ${{ in {s:?}"))? + start;
                let var = &rest[start + 2..end];
                out.push_str(&std::env::var(var).with_context(|| format!("{at}: environment variable {var} is not set"))?);
                rest = &rest[end + 1..];
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(items) => Value::Array(items.iter().enumerate().map(|(i, x)| interpolate_env(x, &format!("{at}[{i}]"))).collect::<anyhow::Result<_>>()?),
        Value::Object(fields) => Value::Object(fields.iter().map(|(k, x)| Ok((k.clone(), interpolate_env(x, &format!("{at}.{k}"))?))).collect::<anyhow::Result<_>>()?),
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
        assert_eq!(interpolate_env(&v, "config").unwrap(), serde_json::json!({ "a": "k123", "b": ["x-k123-y"], "c": 1 }));
        let err = interpolate_env(&serde_json::json!("${SAMISHISA_MISSING_VAR}"), "config.x").unwrap_err();
        assert!(err.to_string().contains("SAMISHISA_MISSING_VAR"));
    }

    #[test]
    fn entries() {
        let c: Config = serde_json::from_str(r#"{ "extensions": ["./a.wasm", { "source": "github:o/r#v1", "name": "x", "rateLimit": false, "memoryMb": 128 }] }"#).unwrap();
        assert_eq!(c.extensions[0].source, "./a.wasm");
        assert_eq!(c.extensions[0].memory_mb, DEFAULT_MEMORY_MB);
        assert_eq!(c.extensions[1].name.as_deref(), Some("x"));
        assert_eq!(c.extensions[1].rate_limit, RateSetting::Off);
        assert_eq!(c.max_memory_mb(), 128);
        assert_eq!(c.port, 3000, "missing fields take their defaults");
    }

    #[test]
    fn mistakes_are_errors() {
        let error = |json: &str| serde_json::from_str::<Config>(json).unwrap_err().to_string();
        assert!(error(r#"{ "prot": 1 }"#).contains("prot"));
        assert!(error(r#"{ "extensions": [{ "source": "x", "nmae": "y" }] }"#).contains("nmae"));
        assert!(error(r#"{ "extensions": [{ "source": "x", "network": "example.com" }] }"#).contains("network"));
        assert!(error(r#"{ "rateLimit": { "limit": -1 } }"#).contains("limit"));
        assert!(error(r#"{ "trustProxy": "sometimes" }"#).contains("trustProxy"));
    }
}
