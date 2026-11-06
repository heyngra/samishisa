use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use serde_json::{Map, Value, json};

use super::reply::ErrorReply;
use crate::manifest::RouteManifest;

pub struct Input {
    pub params: Value,
    pub query: Value,
    pub body: Value,
}

pub struct Validators {
    params: Option<jsonschema::Validator>,
    query: Option<jsonschema::Validator>,
    body: Option<jsonschema::Validator>,
}

impl Validators {
    pub fn compile(route: &RouteManifest) -> anyhow::Result<Validators> {
        let compile = |schema: &Option<Value>| -> anyhow::Result<Option<jsonschema::Validator>> {
            schema.as_ref().map(|s| jsonschema::validator_for(s).map_err(|e| anyhow::anyhow!("{} {}: invalid schema: {e}", route.method, route.path))).transpose()
        };
        Ok(Validators { params: compile(&route.params)?, query: compile(&route.query)?, body: compile(&route.body)? })
    }

    pub fn takes_body(&self) -> bool {
        self.body.is_some()
    }

    pub fn problems(&self, input: &Input) -> Vec<String> {
        let mut problems = Vec::new();
        for (part, validator, value) in [("params", &self.params, &input.params), ("query", &self.query, &input.query), ("body", &self.body, &input.body)] {
            let Some(validator) = validator else { continue };
            if validator.is_valid(value) {
                continue;
            }
            for e in validator.iter_errors(value) {
                problems.push(format!("{part}{}: {e}", e.instance_path().to_string().replace('/', ".")));
            }
        }
        problems
    }
}

pub fn params_from_path(matched: &[(&str, &str)], schema: Option<&Value>) -> Value {
    let params: Map<String, Value> = matched.iter().map(|(name, raw)| (name.to_string(), Value::String(percent_decode(raw)))).collect();
    coerce(Value::Object(params), schema)
}

pub fn query_from_url(query_string: Option<&str>, schema: Option<&Value>) -> Value {
    let mut query = Map::new();
    for (key, value) in form_urlencoded::parse(query_string.unwrap_or("").as_bytes()).into_owned() {
        match query.get_mut(&key) {
            Some(Value::Array(values)) => values.push(Value::String(value)),
            Some(first) => *first = Value::Array(vec![first.take(), Value::String(value)]),
            None => {
                query.insert(key, Value::String(value));
            }
        }
    }
    coerce(Value::Object(query), schema)
}

pub async fn read_json_body(body: Incoming, max_bytes: usize) -> Result<Value, ErrorReply> {
    let bytes = match Limited::new(body, max_bytes).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Err(ErrorReply::new(413, "body_too_large", format!("The request body is larger than {max_bytes} bytes"))),
    };
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| ErrorReply::new(400, "invalid_json", format!("The request body is not valid JSON: {e}")).hint("Send a JSON body with content-type: application/json"))
}

pub fn validation_error(problems: Vec<String>, route: &RouteManifest, usage: &str) -> ErrorReply {
    let message = format!("Invalid request ({} problem{})", problems.len(), if problems.len() == 1 { "" } else { "s" });
    let details = json!({ "problems": problems, "usage": usage, "expected": { "params": route.params, "query": route.query, "body": route.body } });
    ErrorReply::new(400, "validation_error", message).hint(format!("Usage: {usage}")).details(details)
}

pub fn resolve_ref<'a>(schema: &'a Value, root: Option<&'a Value>) -> &'a Value {
    match (schema.get("$ref").and_then(Value::as_str), root) {
        (Some(reference), Some(root)) => reference.strip_prefix('#').and_then(|pointer| root.pointer(pointer)).unwrap_or(schema),
        _ => schema,
    }
}

pub fn primary_type(schema: &Value) -> Option<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => Some(t),
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).find(|t| *t != "null"),
        _ => None,
    }
}

fn coerce(value: Value, schema: Option<&Value>) -> Value {
    let Some(schema) = schema else { return value };
    let Value::Object(fields) = value else { return value };
    let properties = schema.get("properties").and_then(Value::as_object);
    let converted = fields
        .into_iter()
        .map(|(key, field)| {
            let field = match properties.and_then(|p| p.get(&key)) {
                Some(property) => coerce_one(field, resolve_ref(property, Some(schema)), schema),
                None => field,
            };
            (key, field)
        })
        .collect();
    Value::Object(converted)
}

fn coerce_one(value: Value, schema: &Value, root: &Value) -> Value {
    match (primary_type(schema), value) {
        (Some("array"), Value::Array(items)) => {
            let item_schema = schema.get("items").map(|i| resolve_ref(i, Some(root))).cloned().unwrap_or(Value::Bool(true));
            Value::Array(items.into_iter().map(|x| coerce_one(x, &item_schema, root)).collect())
        }
        (Some("array"), single @ Value::String(_)) => coerce_one(Value::Array(vec![single]), schema, root),
        (Some(t), Value::Array(mut items)) if t != "array" => coerce_one(items.pop().unwrap_or(Value::Null), schema, root),
        (Some("integer"), Value::String(s)) => s.trim().parse::<i64>().map(Value::from).or_else(|_| s.trim().parse::<u64>().map(Value::from)).unwrap_or(Value::String(s)),
        (Some("number"), Value::String(s)) => s.trim().parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).unwrap_or(Value::String(s)),
        (Some("boolean"), Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "" => Value::Bool(true),
            "false" | "0" | "no" | "off" => Value::Bool(false),
            _ => Value::String(s),
        },
        (_, other) => other,
    }
}

fn percent_decode(s: &str) -> String {
    form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coercion() {
        let schema = json!({
            "type": "object",
            "properties": {
                "n": { "type": "integer" }, "f": { "type": "number" }, "b": { "type": "boolean" },
                "tags": { "type": "array", "items": { "type": "integer" } }, "o": { "type": ["string", "null"] }
            }
        });
        let input = json!({ "n": "42", "f": "1.5", "b": "false", "tags": ["1", "2"], "o": "x", "extra": "kept" });
        assert_eq!(coerce(input, Some(&schema)), json!({ "n": 42, "f": 1.5, "b": false, "tags": [1, 2], "o": "x", "extra": "kept" }));
        assert_eq!(coerce(json!({ "n": "abc" }), Some(&schema)), json!({ "n": "abc" }), "unconvertible stays a string for the validator");
        assert_eq!(coerce(json!({ "tags": "7" }), Some(&schema)), json!({ "tags": [7] }), "a single value for a list becomes a list");
        assert_eq!(coerce(json!({ "n": ["1", "2"] }), Some(&schema)), json!({ "n": 2 }), "last repeated key wins");
    }

    #[test]
    fn query_and_path() {
        let schema = json!({ "properties": { "tag": { "type": "array", "items": { "type": "string" } } } });
        assert_eq!(query_from_url(Some("tag=a&tag=b+c&x=1"), Some(&schema)), json!({ "tag": ["a", "b c"], "x": "1" }));
        assert_eq!(params_from_path(&[("name", "Ada%20L+")], None), json!({ "name": "Ada L+" }));
    }
}
