use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Text,
    Number,
    Integer,
    Bool,
    Choice(Vec<String>),
    Json,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub path: Vec<String>,
    pub kind: Kind,
    pub description: Option<String>,
    pub required: bool,
    pub default: Option<Value>,
}

impl Field {
    pub fn new(path: &[&str], kind: Kind, description: &str) -> Field {
        Field { path: path.iter().map(|s| s.to_string()).collect(), kind, description: Some(description.to_owned()).filter(|d| !d.is_empty()), required: false, default: None }
    }

    pub fn with_default(mut self, default: Value) -> Field {
        self.default = Some(default);
        self
    }

    pub fn label(&self) -> String {
        self.path.join(".")
    }

    pub fn secret(&self) -> bool {
        let name = self.label().to_ascii_lowercase();
        ["secret", "token", "password", "api_key", "apikey"].iter().any(|word| name.contains(word))
    }
}

pub fn fields_of(schema: &Value) -> Vec<Field> {
    let mut out = Vec::new();
    walk(schema, schema, &mut Vec::new(), true, &mut out, 0);
    out
}

pub fn fields_from_value(config: &Value) -> Vec<Field> {
    fn leaves(value: &Value, prefix: &mut Vec<String>, out: &mut Vec<Field>) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (key, inner) in map {
                    prefix.push(key.clone());
                    leaves(inner, prefix, out);
                    prefix.pop();
                }
            }
            other => {
                if prefix.is_empty() {
                    return;
                }
                let kind = match other {
                    Value::String(_) => Kind::Text,
                    Value::Bool(_) => Kind::Bool,
                    Value::Number(n) if n.is_i64() || n.is_u64() => Kind::Integer,
                    Value::Number(_) => Kind::Number,
                    _ => Kind::Json,
                };
                out.push(Field { path: prefix.clone(), kind, description: None, required: false, default: None });
            }
        }
    }
    let mut out = Vec::new();
    leaves(config, &mut Vec::new(), &mut out);
    out
}

fn resolve<'a>(root: &'a Value, node: &'a Value, depth: usize) -> &'a Value {
    if depth > 8 {
        return node;
    }
    if let Some(target) = node.get("$ref").and_then(Value::as_str).and_then(|r| r.strip_prefix('#')).and_then(|p| root.pointer(p)) {
        return resolve(root, target, depth + 1);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(options) = node.get(key).and_then(Value::as_array) {
            let real: Vec<&Value> = options.iter().filter(|o| o.get("type").and_then(Value::as_str) != Some("null")).collect();
            if let [only] = real.as_slice() {
                return resolve(root, only, depth + 1);
            }
        }
    }
    node
}

fn walk(root: &Value, original: &Value, prefix: &mut Vec<String>, required: bool, out: &mut Vec<Field>, depth: usize) {
    let node = resolve(root, original, 0);
    if let (Some(properties), true) = (node.get("properties").and_then(Value::as_object), depth < 6) {
        let needed: Vec<&str> = node.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        for (key, inner) in properties {
            prefix.push(key.clone());
            walk(root, inner, prefix, required && needed.contains(&key.as_str()), out, depth + 1);
            prefix.pop();
        }
        return;
    }
    if prefix.is_empty() {
        return;
    }
    let kind = if let Some(options) = node.get("enum").and_then(Value::as_array) {
        Kind::Choice(options.iter().filter_map(Value::as_str).map(str::to_owned).collect())
    } else {
        let type_name = match node.get("type") {
            Some(Value::String(t)) => Some(t.as_str()),
            Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).find(|t| *t != "null"),
            _ => None,
        };
        match type_name {
            Some("string") => Kind::Text,
            Some("number") => Kind::Number,
            Some("integer") => Kind::Integer,
            Some("boolean") => Kind::Bool,
            _ => Kind::Json,
        }
    };
    let description = original.get("description").or_else(|| node.get("description")).and_then(Value::as_str).map(|d| d.trim().to_owned());
    let default = original.get("default").or_else(|| node.get("default")).cloned();
    out.push(Field { path: prefix.clone(), kind, description, required, default });
}

impl Kind {
    pub fn hint(&self) -> String {
        match self {
            Kind::Text => "text".into(),
            Kind::Number => "a number".into(),
            Kind::Integer => "a whole number".into(),
            Kind::Bool => "true or false".into(),
            Kind::Choice(options) => format!("one of {}", options.join(", ")),
            Kind::Json => "JSON".into(),
        }
    }
}

pub fn parse_input(kind: &Kind, text: &str) -> Result<Value, String> {
    let text = text.trim();
    match kind {
        Kind::Text => Ok(Value::String(text.to_owned())),
        Kind::Integer => text.parse::<i64>().map(Value::from).map_err(|_| format!("{text:?} is not a whole number")),
        Kind::Number => text.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(Value::Number).ok_or_else(|| format!("{text:?} is not a number")),
        Kind::Bool => match text {
            "true" | "yes" | "on" => Ok(Value::Bool(true)),
            "false" | "no" | "off" => Ok(Value::Bool(false)),
            _ => Err("type true or false".into()),
        },
        Kind::Choice(options) => options.iter().find(|o| o.as_str() == text).map(|o| Value::String(o.clone())).ok_or_else(|| format!("one of: {}", options.join(", "))),
        Kind::Json => serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattens_nested_settings_and_follows_refs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "public_url": { "type": "string", "description": "Where it lives.", "default": "https://x" },
                "anilist": { "$ref": "#/$defs/AniList", "description": "The client." },
                "optional": { "type": "object", "properties": { "inner": { "type": "string" } }, "required": ["inner"] },
                "retries": { "type": ["integer", "null"] },
                "mode": { "enum": ["fast", "slow"] }
            },
            "required": ["anilist"],
            "$defs": { "AniList": { "type": "object", "properties": { "client_id": { "type": "string" }, "client_secret": { "type": "string" } }, "required": ["client_id"] } }
        });
        let fields = fields_of(&schema);
        let labels: Vec<String> = fields.iter().map(Field::label).collect();
        assert_eq!(labels, ["anilist.client_id", "anilist.client_secret", "mode", "optional.inner", "public_url", "retries"]);
        let by = |l: &str| fields.iter().find(|f| f.label() == l).unwrap();
        assert!(by("anilist.client_id").required);
        assert!(!by("anilist.client_secret").required);
        assert!(!by("optional.inner").required, "required inside an optional object is not required");
        assert_eq!(by("public_url").default, Some(json!("https://x")));
        assert!(by("anilist.client_secret").secret());
        assert_eq!(by("retries").kind, Kind::Integer);
        assert_eq!(by("mode").kind, Kind::Choice(vec!["fast".into(), "slow".into()]));
        assert_eq!(by("public_url").description.as_deref(), Some("Where it lives."));
    }

    #[test]
    fn falls_back_to_the_written_values() {
        let fields = fields_from_value(&json!({ "a": { "b": "x", "n": 3 }, "on": true }));
        let kinds: Vec<(String, Kind)> = fields.iter().map(|f| (f.label(), f.kind.clone())).collect();
        assert_eq!(kinds, [("a.b".into(), Kind::Text), ("a.n".into(), Kind::Integer), ("on".into(), Kind::Bool)]);
    }
}
