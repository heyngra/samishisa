use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

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
    /// Serves route listings (`GET /`, `GET /<name>`, `/openapi.json`, `/openapi/<name>.json`, `/docs`) and lists routes in 404 replies. Off by default, so callers can't see routes they don't already know.
    pub describe_routes: bool,
    pub extensions: Vec<ExtensionEntry>,
    pub tunnel: TunnelConfig,
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
            describe_routes: false,
            extensions: Vec::new(),
            tunnel: TunnelConfig::default(),
            base_dir: PathBuf::from("."),
            path: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct NetworkConfig {
    pub allow_private: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct TunnelConfig {
    pub enabled: bool,
    pub token: Option<String>,
    pub url: Option<String>,
    pub binary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExtensionEntry {
    pub enabled: bool,
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
            enabled: true,
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
    enabled: Option<bool>,
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
            enabled: object.enabled.unwrap_or(true),
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

pub fn user_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("samishisa"))
}

pub fn default_path() -> PathBuf {
    let local = PathBuf::from("api.config.json");
    if local.exists() {
        return local;
    }
    user_dir().map(|d| d.join("config.json")).unwrap_or(local)
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
        cfg.finish(path)?;
        Ok(cfg)
    }

    pub fn from_value(value: Value, path: &Path) -> anyhow::Result<Self> {
        let mut cfg: Config = serde_json::from_value(value).context("not a valid samishisa config")?;
        cfg.path = Some(path.to_path_buf());
        cfg.finish(path)?;
        Ok(cfg)
    }

    fn finish(&mut self, path: &Path) -> anyhow::Result<()> {
        let cfg = self;
        cfg.base_dir = path.parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        if let Ok(port) = std::env::var("PORT") {
            cfg.port = port.parse().context("PORT must be a number")?;
        }
        if let Ok(url) = std::env::var("REDIS_URL").map(|u| u.trim().to_owned())
            && !url.is_empty()
        {
            cfg.redis = Some(url);
        }
        if let Ok(dir) = std::env::var("SAMISHISA_DATA") {
            cfg.data_dir = PathBuf::from(dir);
        }
        if cfg.data_dir.is_relative() {
            cfg.data_dir = cfg.base_dir.join(&cfg.data_dir);
        }
        cfg.check()
    }

    fn check(&self) -> anyhow::Result<()> {
        if self.max_instances == 0 {
            bail!("maxInstances must be at least 1");
        }
        listen_ip(&self.host)?;
        Ok(())
    }

    pub fn validate(value: &Value) -> anyhow::Result<()> {
        let cfg: Config = serde_json::from_value(value.clone())?;
        cfg.check()
    }

    pub fn restart_only_changes(&self, newer: &Config) -> Vec<&'static str> {
        let mut changed = Vec::new();
        let mut differs = |name, d: bool| {
            if d {
                changed.push(name);
            }
        };
        differs("port", self.port != newer.port);
        differs("host", self.host != newer.host);
        differs("threads", self.threads != newer.threads);
        differs("redis", self.redis != newer.redis);
        differs("maxInstances", self.max_instances != newer.max_instances);
        differs("network", self.network != newer.network);
        differs("dataDir", self.data_dir != newer.data_dir);
        differs("tunnel", self.tunnel != newer.tunnel);
        changed
    }

    pub fn docs_page(&self) -> bool {
        self.extensions.iter().any(|e| e.enabled && e.source.trim() == "builtin:docs")
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

pub fn listen_ip(host: &str) -> anyhow::Result<IpAddr> {
    let host = host.trim();
    if host.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .map_err(|_| anyhow::anyhow!("host {host:?} is not an IP address: use 0.0.0.0 (every interface), 127.0.0.1 (this machine only), :: or ::1"))
}

pub fn listen_address(host: &str, port: u16) -> anyhow::Result<SocketAddr> {
    Ok(SocketAddr::new(listen_ip(host)?, port))
}

pub fn local_address(host: &str, port: u16) -> SocketAddr {
    let ip = match listen_ip(host) {
        Ok(ip) if ip.is_unspecified() && ip.is_ipv6() => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        Ok(ip) if !ip.is_unspecified() => ip,
        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    SocketAddr::new(ip, port)
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
        // SAFETY: no other test in this binary reads these variables.
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
        assert!(!c.describe_routes, "route listings are opt-in");
        let on: Config = serde_json::from_str(r#"{ "describeRoutes": true }"#).unwrap();
        assert!(on.describe_routes);
    }

    #[test]
    fn mistakes_are_errors() {
        let error = |json: &str| serde_json::from_str::<Config>(json).unwrap_err().to_string();
        assert!(error(r#"{ "prot": 1 }"#).contains("prot"));
        assert!(error(r#"{ "extensions": [{ "source": "x", "nmae": "y" }] }"#).contains("nmae"));
        assert!(error(r#"{ "extensions": [{ "source": "x", "network": "example.com" }] }"#).contains("network"));
        assert!(error(r#"{ "rateLimit": { "limit": -1 } }"#).contains("limit"));
        assert!(error(r#"{ "trustProxy": "sometimes" }"#).contains("trustProxy"));
        assert!(Config::validate(&serde_json::json!({ "port": 99999 })).is_err());
        assert!(Config::validate(&serde_json::json!({ "port": "abc" })).is_err());
        assert!(Config::validate(&serde_json::json!({ "host": "my-laptop" })).is_err());
        assert!(Config::validate(&serde_json::json!({ "port": 8080, "host": "::" })).is_ok());
    }

    #[test]
    fn listen_addresses() {
        assert_eq!(listen_address("::", 3000).unwrap().to_string(), "[::]:3000");
        assert_eq!(listen_address("0.0.0.0", 80).unwrap().to_string(), "0.0.0.0:80");
        assert_eq!(listen_address("localhost", 80).unwrap().to_string(), "127.0.0.1:80");
        assert_eq!(local_address("0.0.0.0", 3000).to_string(), "127.0.0.1:3000");
        assert_eq!(local_address("::", 3000).to_string(), "[::1]:3000");
        assert_eq!(local_address("192.168.1.5", 3000).to_string(), "192.168.1.5:3000");
    }
}
