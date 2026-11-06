use serde_json::{Map, Value, json};

use super::Mounted;
use crate::manifest::RouteManifest;

const SCALAR_SHA384: &str = "sha384-xgelKlDcc+BYp0O+ZYzFesLnWCgGpEbXVVZWNQT6WyZYygN01tbEMygnjPXb5OPI";

pub fn docs_html(title: &str) -> String {
    let title: String = title
        .chars()
        .map(|c| match c {
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '&' => "&amp;".to_owned(),
            '"' => "&quot;".to_owned(),
            c => c.to_string(),
        })
        .collect();
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>{title}: API docs</title>
<meta name="viewport" content="width=device-width, initial-scale=1"></head>
<body><script id="api-reference" data-url="openapi.json"></script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.75.0/dist/browser/standalone.js" integrity="{SCALAR_SHA384}" crossorigin="anonymous"></script></body></html>"#
    )
}

pub fn build(mounted: &[Mounted], title: &str) -> Value {
    spec(mounted, json!({ "title": title, "version": env!("CARGO_PKG_VERSION") }))
}

pub fn build_one(m: &Mounted) -> Value {
    let manifest = &m.runner.manifest;
    let mut info = json!({ "title": m.runner.mount, "version": manifest.version.as_deref().unwrap_or("0.0.0") });
    if let Some(description) = &manifest.description {
        info["description"] = json!(description);
    }
    spec(std::slice::from_ref(m), info)
}

fn spec(mounted: &[Mounted], info: Value) -> Value {
    let mut components = Map::new();
    components.insert("Error".into(), error_schema());
    let mut paths = Map::new();
    let mut tags = Vec::new();
    for m in mounted {
        let mount = &m.runner.mount;
        tags.push(json!({ "name": mount, "description": m.runner.manifest.description.clone().unwrap_or_default() }));
        for route in &m.routes {
            let path = paths.entry(openapi_path(mount, &route.spec)).or_insert_with(|| json!({}));
            path[route.spec.method.to_lowercase()] = operation(mount, &route.spec, &mut components);
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": info,
        "tags": tags,
        "paths": paths,
        "components": { "schemas": components },
    })
}

fn openapi_path(mount: &str, route: &RouteManifest) -> String {
    route
        .full_path(mount)
        .split('/')
        .map(|segment| match segment.strip_prefix(':') {
            Some(param) => format!("{{{param}}}"),
            None => segment.to_owned(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn operation(mount: &str, route: &RouteManifest, components: &mut Map<String, Value>) -> Value {
    let mut parameters = Vec::new();
    for (place, schema) in [("path", &route.params), ("query", &route.query)] {
        let Some(schema) = schema else { continue };
        let schema = hoist(schema.clone(), mount, components);
        let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        for (name, property) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
            let mut parameter = json!({ "name": name, "in": place, "required": place == "path" || required.contains(&name.as_str()), "schema": property });
            if let Some(description) = property.get("description") {
                parameter["description"] = description.clone();
            }
            parameters.push(parameter);
        }
    }
    let method = route.method.to_lowercase();
    let mut operation = json!({
        "tags": [mount],
        "operationId": format!("{mount}_{method}_{}", route.path.trim_matches('/').replace(['/', ':'], "_")),
        "parameters": parameters,
        "responses": {
            "200": { "description": "Success" },
            "400": error_response("Invalid input: every problem is listed in details.problems"),
            "429": error_response("Rate limited: see Retry-After"),
            "500": error_response("Internal error (details are in the server log under requestId)"),
        }
    });
    if let Some(summary) = &route.summary {
        operation["summary"] = json!(summary);
    }
    if let Some(description) = &route.description {
        operation["description"] = json!(description);
    }
    if let Some(body) = &route.body {
        operation["requestBody"] = json!({ "required": true, "content": { "application/json": { "schema": hoist(body.clone(), mount, components) } } });
    }
    if let Some(response) = &route.response {
        operation["responses"]["200"]["content"] = json!({ "application/json": { "schema": hoist(response.clone(), mount, components) } });
    }
    operation
}

fn error_schema() -> Value {
    json!({
        "type": "object",
        "required": ["error"],
        "properties": { "error": {
            "type": "object",
            "required": ["status", "code", "message", "requestId"],
            "properties": {
                "status": { "type": "integer" },
                "code": { "type": "string", "examples": ["validation_error", "not_found", "rate_limited"] },
                "message": { "type": "string" },
                "hint": { "type": "string" },
                "details": {},
                "requestId": { "type": "string" }
            }
        }}
    })
}

fn error_response(description: &str) -> Value {
    json!({ "description": description, "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } })
}

fn hoist(mut schema: Value, prefix: &str, components: &mut Map<String, Value>) -> Value {
    if let Some(Value::Object(defs)) = schema.as_object_mut().and_then(|o| o.remove("$defs")) {
        for (name, mut def) in defs {
            rewrite_refs(&mut def, prefix);
            components.insert(format!("{prefix}.{name}"), def);
        }
    }
    if let Some(o) = schema.as_object_mut() {
        o.remove("$schema");
    }
    rewrite_refs(&mut schema, prefix);
    schema
}

fn rewrite_refs(value: &mut Value, prefix: &str) {
    match value {
        Value::Object(o) => {
            if let Some(Value::String(reference)) = o.get_mut("$ref")
                && let Some(name) = reference.strip_prefix("#/$defs/")
            {
                *reference = format!("#/components/schemas/{prefix}.{name}");
            }
            for v in o.values_mut() {
                rewrite_refs(v, prefix);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| rewrite_refs(v, prefix)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hoists_defs() {
        let mut components = Map::new();
        let schema = json!({
            "$schema": "x", "type": "object",
            "properties": { "u": { "$ref": "#/$defs/Units" } },
            "$defs": { "Units": { "enum": ["c", "f"] } }
        });
        let out = hoist(schema, "weather", &mut components);
        assert_eq!(out["properties"]["u"]["$ref"], "#/components/schemas/weather.Units");
        assert!(out.get("$defs").is_none() && out.get("$schema").is_none());
        assert_eq!(components["weather.Units"]["enum"][0], "c");
    }
}
