use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

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
    #[serde(default)]
    pub extensions: Vec<Entry>,
    #[serde(skip)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Entry {
    Source(String),
    Full(FullEntry),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullEntry {
    pub source: String,
    pub name: Option<String>,
    #[serde(default)]
    pub config: Option<Value>,
}

impl Entry {
    pub fn source(&self) -> &str {
        match self {
            Entry::Source(s) => s,
            Entry::Full(f) => &f.source,
        }
    }
    pub fn alias(&self) -> Option<&str> {
        match self {
            Entry::Source(_) => None,
            Entry::Full(f) => f.name.as_deref(),
        }
    }
    pub fn config(&self) -> Value {
        match self {
            Entry::Full(FullEntry { config: Some(c), .. }) => c.clone(),
            _ => Value::Object(Default::default()),
        }
    }
}

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
    PathBuf::from(".samishisa")
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
            serde_json::from_str::<Config>(&text).with_context(|| format!("{} is not a valid samishisa config", path.display()))?
        } else if must_exist {
            bail!("config file {} not found", path.display());
        } else {
            Config::default()
        };
        cfg.base_dir = path.parent().map(Path::to_path_buf).filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| PathBuf::from("."));
        if let Ok(p) = std::env::var("PORT") {
            cfg.port = p.parse().context("PORT must be a number")?;
        }
        if cfg.data_dir.is_relative() {
            cfg.data_dir = cfg.base_dir.join(&cfg.data_dir);
        }
        Ok(cfg)
    }

    pub fn resolve(&self, source: &str) -> PathBuf {
        let p = PathBuf::from(source);
        if p.is_absolute() { p } else { self.base_dir.join(p) }
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
