use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    ApiError, Json, Result,
    extension::{RouteSpec, schema_of},
};

/// A response with full control over status, headers and body. For plain JSON, return [`Json`] instead.
///
/// ```
/// use samishisa::Response;
/// let created = Response::json(&serde_json::json!({ "id": 7 })).unwrap().with_status(201);
/// let csv = Response::bytes("text/csv", "a,b\n1,2\n");
/// let gone = Response::empty(410);
/// assert_eq!(created.status(), 201);
/// ```
#[derive(Debug, Clone, PartialEq)]
#[must_use = "a Response does nothing until you return it"]
pub struct Response {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Response {
    /// `200` with `value` as JSON.
    pub fn json<T: Serialize + ?Sized>(value: &T) -> Result<Self> {
        Ok(Self::bytes("application/json", serde_json::to_vec(value)?))
    }
    /// `200` with a UTF-8 text body.
    pub fn text(text: impl Into<String>) -> Self {
        Self::bytes("text/plain; charset=utf-8", text.into().into_bytes())
    }
    /// `200` with any body and content type.
    pub fn bytes(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self { status: 200, headers: vec![("content-type".into(), content_type.into())], body: body.into() }
    }
    /// A response with no body, e.g. `204` or `410`.
    pub fn empty(status: u16) -> Self {
        Self { status, headers: Vec::new(), body: Vec::new() }
    }
    /// A `302` redirect to `location`.
    pub fn redirect(location: impl Into<String>) -> Self {
        Self::empty(302).with_header("location", location)
    }
    /// Change the status (e.g. `201` after creating something).
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    /// Adds a header. The server sets `content-length`, `x-request-id`, and its own CORS and caching headers.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into().to_ascii_lowercase(), value.into()));
        self
    }
    /// The status.
    pub fn status(&self) -> u16 {
        self.status
    }
    /// The body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

/// Anything a handler can return, and the response it becomes:
///
/// | Return type | Response |
/// |---|---|
/// | [`Json<T>`](Json) | `200`, `T` as JSON (its schema appears in the docs) |
/// | [`Response`] | exactly that |
/// | `String`, `&'static str` | `200`, `text/plain` |
/// | `()` | `204 No Content` |
/// | `Result<T, E>` | `T`'s response, or the error: an [`ApiError`] as is, any other error as a logged `500` |
pub trait IntoResponse {
    /// Produce the response.
    fn into_response(self) -> Result<Response>;
    /// Schema of the success response, for the docs. Called once per route, at load time.
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
