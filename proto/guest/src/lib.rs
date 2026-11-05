wit_bindgen::generate!({ world: "extension", path: "../wit" });

use exports::samishisa::proto::handler::{Guest, Request, Response};
use serde::Deserialize;

const MANIFEST: &str = r#"{
  "name": "hello",
  "routes": [
    { "method": "GET", "path": "/spin" },
    { "method": "GET", "path": "/oom" },
    { "method": "GET", "path": "/:name",
      "params": { "type": "object", "required": ["name"], "properties": { "name": { "type": "string", "minLength": 1 } } },
      "query":  { "type": "object", "properties": { "shout": { "type": "string", "enum": ["yes", "no"], "default": "no" } } } }
  ]
}"#;

#[derive(Deserialize)]
struct Greet {
    name: String,
    shout: String,
}

fn json(status: u16, v: &serde_json::Value) -> Response {
    Response { status, content_type: "application/json".into(), body: serde_json::to_vec(v).unwrap() }
}

struct Hello;

impl Guest for Hello {
    fn manifest() -> String {
        MANIFEST.into()
    }

    fn handle(req: Request) -> Response {
        match req.route {
            2 => {
                let g: Greet = serde_json::from_str(&req.input).expect("server validated the input");
                let msg = format!("Hello, {}!", g.name);
                let msg = if g.shout == "yes" { msg.to_uppercase() } else { msg };
                json(200, &serde_json::json!({ "message": msg }))
            }
            0 => loop { std::hint::black_box(()); },
            1 => {
                let mut v: Vec<Vec<u8>> = Vec::new();
                loop { v.push(vec![1u8; 1 << 20]); }
            }
            _ => json(404, &serde_json::json!({ "error": "no such route" })),
        }
    }
}

export!(Hello);
