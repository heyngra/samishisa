use schemars::JsonSchema;
use serde::Serialize;

use crate::{ApiError, Json, Result, extension::{RouteSpec, schema_of}};

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    pub fn json<T: Serialize + ?Sized>(value: &T) -> Result<Self> {
        Ok(Self::bytes("application/json", serde_json::to_vec(value)?))
    }
    pub fn text(text: impl Into<String>) -> Self {
        Self::bytes("text/plain; charset=utf-8", text.into().into_bytes())
    }
    pub fn bytes(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self { status: 200, headers: vec![("content-type".into(), content_type.into())], body: body.into() }
    }
    pub fn empty(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: Vec::new() }
    }
    pub fn redirect(location: impl Into<String>) -> Self {
        Self::empty(302).with_header("location", location)
    }
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into().to_ascii_lowercase(), value.into()));
        self
    }
    pub fn status(&self) -> u16 {
        self.status
    }
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

pub trait IntoResponse {
    fn into_response(self) -> Result<Response>;
    fn describe_response(route: &mut RouteSpec) {
        let _ = route;
    }
}

impl IntoResponse for Response {
    fn into_response(self) -> Result<Response> {
        Ok(self)
    }
}

impl<T: Serialize + JsonSchema> IntoResponse for Json<T> {
    fn into_response(self) -> Result<Response> {
        Response::json(&self.0)
    }
    fn describe_response(route: &mut RouteSpec) {
        route.set_response(schema_of::<T>());
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Result<Response> {
        Ok(Response::text(self))
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> Result<Response> {
        Ok(Response::text(self))
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Result<Response> {
        Ok(Response::empty(204))
    }
}

impl<T: IntoResponse, E: Into<ApiError>> IntoResponse for std::result::Result<T, E> {
    fn into_response(self) -> Result<Response> {
        self.map_err(Into::into)?.into_response()
    }
    fn describe_response(route: &mut RouteSpec) {
        T::describe_response(route);
    }
}
