use serde_json::{Map, Value, json};

use crate::runtime::Runner;

pub const DOCS_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>API docs</title>
<meta name="viewport" content="width=device-width, initial-scale=1"></head>
<body><script id="api-reference" data-url="/openapi.json"></script>
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script></body></html>"#;

fn hoist(mut schema: Value, prefix: &str, components: &mut Map<String, Value>) -> Value {
    if let Some(Value::Object(defs)) = schema.as_object_mut().and_then(|o| o.remove("$defs")) {
        for (name, def) in defs {
            let mut def = def;
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

fn rewrite_refs(v: &mut Value, prefix: &str) {
    match v {
        Value::Object(o) => {
            if let Some(Value::String(r)) = o.get_mut("$ref") {
                if let Some(name) = r.strip_prefix("#/$defs/") {
                    *r = format!("#/components/schemas/{prefix}.{name}");
                }
            }
            for x in o.values_mut() {
                rewrite_refs(x, prefix);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| rewrite_refs(x, prefix)),
        _ => {}
    }
}

fn error_response(description: &str) -> Value {
    json!({ "description": description, "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } })
}

pub fn build<'a>(runners: impl Iterator<Item = &'a Runner>, title: &str) -> Value {
    let mut paths = Map::new();
    let mut components = Map::new();
    components.insert(
        "Error".into(),
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
        }),
    );
    let mut tags = Vec::new();

    for r in runners {
        let m = &r.manifest;
        let mount = &r.mount;
        tags.push(json!({ "name": mount, "description": m.description.clone().unwrap_or_default() }));
        for route in &m.routes {
            let oa_path = format!(
                "/{mount}{}",
                route.path.split('/').map(|s| s.strip_prefix(':').map(|p| format!("{{{p}}}")).unwrap_or_else(|| s.to_owned())).collect::<Vec<_>>().join("/")
            );
            let oa_path = if route.path == "/" { format!("/{mount}") } else { oa_path };
            let prefix = format!("{mount}");
            let mut parameters = Vec::new();
            for (place, schema) in [("path", &route.params), ("query", &route.query)] {
                let Some(schema) = schema else { continue };
                let schema = hoist(schema.clone(), &prefix, &mut components);
                let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                for (name, prop) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
                    let mut p = json!({ "name": name, "in": place, "required": place == "path" || required.contains(&name.as_str()), "schema": prop });
                    if let Some(d) = prop.get("description") {
                        p["description"] = d.clone();
                    }
                    parameters.push(p);
                }
            }
            let mut op = json!({
                "tags": [mount],
                "operationId": format!("{mount}_{}_{}", route.method.to_lowercase(), route.path.trim_matches('/').replace(['/', ':'], "_")),
                "parameters": parameters,
                "responses": {
                    "200": { "description": "Success" },
                    "400": error_response("Invalid input: every problem is listed in details.problems"),
                    "429": error_response("Rate limited: see Retry-After"),
                    "500": error_response("Internal error (details are in the server log under requestId)"),
                }
            });
            if let Some(s) = &route.summary {
                op["summary"] = json!(s);
            }
            if let Some(d) = &route.description {
                op["description"] = json!(d);
            }
            if let Some(b) = &route.body {
                op["requestBody"] = json!({ "required": true, "content": { "application/json": { "schema": hoist(b.clone(), &prefix, &mut components) } } });
            }
            if let Some(resp) = &route.response {
                op["responses"]["200"]["content"] = json!({ "application/json": { "schema": hoist(resp.clone(), &prefix, &mut components) } });
            }
            let entry = paths.entry(oa_path).or_insert_with(|| json!({}));
            entry[route.method.to_lowercase()] = op;
        }
    }
    json!({
        "openapi": "3.1.0",
        "info": { "title": title, "version": env!("CARGO_PKG_VERSION") },
        "tags": tags,
        "paths": paths,
        "components": { "schemas": components },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hoists_defs() {
        let mut comps = Map::new();
        let s = json!({
            "$schema": "x", "type": "object",
            "properties": { "u": { "$ref": "#/$defs/Units" } },
            "$defs": { "Units": { "enum": ["c", "f"] } }
        });
        let out = hoist(s, "weather", &mut comps);
        assert_eq!(out["properties"]["u"]["$ref"], "#/components/schemas/weather.Units");
        assert!(out.get("$defs").is_none() && out.get("$schema").is_none());
        assert_eq!(comps["weather.Units"]["enum"][0], "c");
    }
}
