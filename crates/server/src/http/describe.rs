use serde_json::{Value, json};

use super::{
    Failed, Mounted,
    input::{primary_type, resolve_ref},
};
use crate::manifest::RouteManifest;

pub fn index(title: &str, docs: bool, mounted: &[Mounted], failed: &[Failed]) -> Value {
    json!({
        "name": title,
        "version": env!("CARGO_PKG_VERSION"),
        "docs": docs.then_some("/docs"),
        "openapi": "/openapi.json",
        "extensions": mounted.iter().map(|m| json!({
            "name": m.runner.mount,
            "version": m.runner.manifest.version,
            "description": m.runner.manifest.description,
            "openapi": format!("/openapi/{}.json", m.runner.mount),
            "network": m.runner.policy().describe(),
            "routes": m.routes.iter().map(|r| r.usage.clone()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "failed": failed.iter().map(|f| json!({ "source": f.source, "error": f.error })).collect::<Vec<_>>(),
    })
}

pub fn extension(m: &Mounted) -> Value {
    let manifest = &m.runner.manifest;
    json!({
        "name": m.runner.mount,
        "version": manifest.version,
        "description": manifest.description,
        "sdk": manifest.sdk,
        "openapi": format!("/openapi/{}.json", m.runner.mount),
        "network": m.runner.policy().describe(),
        "routes": m.routes.iter().map(|route| {
            let spec = &route.spec;
            json!({
                "method": spec.method,
                "path": spec.full_path(&m.runner.mount),
                "usage": route.usage,
                "summary": spec.summary,
                "description": spec.description,
                "params": spec.params,
                "query": spec.query,
                "body": spec.body,
                "response": spec.response,
                "cache": route.cache.map(|c| json!({ "ttl": c.fresh_secs, "staleTtl": c.stale_secs })),
                "rateLimit": route.rate_limit.as_ref().map(|r| r.limit.to_json()),
            })
        }).collect::<Vec<_>>(),
    })
}

pub fn usage(mount: &str, route: &RouteManifest) -> String {
    let mut usage = format!("{} {}", route.method, route.full_path(mount));
    if let Some(properties) = route.query.as_ref().and_then(|q| q.get("properties")).and_then(Value::as_object) {
        let pairs: Vec<String> = properties.iter().map(|(key, schema)| format!("{key}=<{}>", type_name(schema, route.query.as_ref()))).collect();
        if !pairs.is_empty() {
            usage.push('?');
            usage.push_str(&pairs.join("&"));
        }
    }
    if route.body.is_some() {
        usage.push_str(" + JSON body");
    }
    usage
}

fn type_name(schema: &Value, root: Option<&Value>) -> String {
    let schema = resolve_ref(schema, root);
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("|");
    }
    primary_type(schema).unwrap_or("value").to_owned()
}
