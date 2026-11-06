use serde::Deserialize;
use serde_json::Value;

use crate::{cache::CacheRule, netguard::Policy, ratelimit::RateSetting};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub sdk: Option<String>,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub config: Option<Value>,
    #[serde(default)]
    pub network: Policy,
    #[serde(default)]
    pub rate_limit: RateSetting,
    #[serde(default)]
    pub routes: Vec<RouteManifest>,
    #[serde(default)]
    pub jobs: Vec<JobManifest>,
    #[serde(default)]
    pub setup: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteManifest {
    pub method: String,
    pub path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub params: Option<Value>,
    pub query: Option<Value>,
    pub body: Option<Value>,
    pub response: Option<Value>,
    pub cache: Option<CacheRule>,
    #[serde(default)]
    pub rate_limit: RateSetting,
    pub timeout_ms: Option<u64>,
}

impl RouteManifest {
    pub fn full_path(&self, mount: &str) -> String {
        if self.path == "/" { format!("/{mount}") } else { format!("/{mount}{}", self.path) }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobManifest {
    pub every_ms: u64,
}
