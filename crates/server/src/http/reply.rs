use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    Response, StatusCode,
    header::{CONTENT_TYPE, HeaderValue},
};
use serde_json::{Value, json};

use crate::cache::Produced;

pub type Res = Response<Full<Bytes>>;

pub fn json(status: u16, value: &Value) -> Res {
    bytes(status, "application/json", Bytes::from(serde_json::to_vec(value).unwrap_or_default()))
}

pub fn bytes(status: u16, content_type: &'static str, body: Bytes) -> Res {
    let mut response = with_status(status, body);
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

pub fn empty(status: u16) -> Res {
    with_status(status, Bytes::new())
}

fn with_status(status: u16, body: Bytes) -> Res {
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    response
}

pub fn from_produced(produced: Produced, request_id: &str) -> Res {
    let mut builder = Response::builder().status(produced.status);
    for (name, value) in &produced.headers {
        builder = builder.header(name, value);
    }
    builder.body(Full::new(produced.body)).unwrap_or_else(|_| ErrorReply::new(500, "internal_error", "The extension returned an invalid response").into_response(request_id))
}

pub struct ErrorReply {
    status: u16,
    code: String,
    message: String,
    hint: Option<String>,
    details: Option<Value>,
}

impl ErrorReply {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { status, code: code.into(), message: message.into(), hint: None, details: None }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    fn body(&self, request_id: &str) -> Bytes {
        let mut error = json!({ "status": self.status, "code": self.code, "message": self.message, "requestId": request_id });
        if let Some(hint) = &self.hint {
            error["hint"] = json!(hint);
        }
        if let Some(details) = &self.details {
            error["details"] = details.clone();
        }
        Bytes::from(serde_json::to_vec(&json!({ "error": error })).unwrap_or_default())
    }

    pub fn into_response(self, request_id: &str) -> Res {
        bytes(self.status, "application/json", self.body(request_id))
    }

    pub fn into_produced(self, request_id: &str) -> Produced {
        Produced { status: self.status, headers: vec![("content-type".into(), "application/json".into())], body: self.body(request_id) }
    }
}

pub fn default_code(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        422 => "unprocessable",
        429 => "rate_limited",
        500 => "internal_error",
        501 => "not_implemented",
        502 => "upstream_error",
        503 => "unavailable",
        504 => "timeout",
        _ if status < 500 => "client_error",
        _ => "server_error",
    }
}
