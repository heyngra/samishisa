use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::{
    ApiError, Ctx, Result,
    extension::{RouteSpec, is_object_schema, schema_of},
};

/// The incoming request, as extractors see it. Handlers rarely need it directly.
pub struct RequestParts<C> {
    /// Validated input as raw JSON text. Only the extractors that ask deserialize it.
    pub(crate) params: Box<serde_json::value::RawValue>,
    pub(crate) query: Box<serde_json::value::RawValue>,
    pub(crate) body: Box<serde_json::value::RawValue>,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) ctx: Ctx<C>,
}

impl<C> RequestParts<C> {
    /// The request method, e.g. `"GET"`.
    pub fn method(&self) -> &str {
        &self.method
    }
    /// The full path the caller requested, e.g. `"/hello/Ada"`.
    pub fn path(&self) -> &str {
        &self.path
    }
    /// The forwarded request headers (lowercase names).
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
    /// The validated path params as JSON text, e.g. `{"city": "london"}`.
    pub fn params_json(&self) -> &str {
        self.params.get()
    }
    /// The validated query string as JSON text, e.g. `{"days": 3}`.
    pub fn query_json(&self) -> &str {
        self.query.get()
    }
    /// The validated JSON body as text (`null` if there is none).
    pub fn body_json(&self) -> &str {
        self.body.get()
    }
    /// The extension's services.
    pub fn ctx(&self) -> &Ctx<C> {
        &self.ctx
    }
}

/// Something a handler can take as an argument. [`Path`], [`Query`], [`Json`], [`Headers`] and [`Ctx`] implement it. Implement it yourself to share request parsing between handlers.
///
/// ```no_run
/// use samishisa::prelude::*;
/// use samishisa::{FromRequest, RequestParts};
///
/// struct ApiKey(String);
///
/// impl<C> FromRequest<C> for ApiKey {
///     fn from_request(req: &RequestParts<C>) -> Result<Self> {
///         req.headers().iter()
///             .find(|(k, _)| k == "x-api-key")
///             .map(|(_, v)| ApiKey(v.clone()))
///             .ok_or_else(|| ApiError::unauthorized("Send your key in x-api-key"))
///     }
/// }
/// ```
pub trait FromRequest<C>: Sized {
    /// Build the value, or reject the request with an [`ApiError`].
    fn from_request(req: &RequestParts<C>) -> Result<Self>;
    /// Declares the schemas this extractor needs, for validation and docs. Called once per route, at load time.
    fn describe(route: &mut RouteSpec) {
        let _ = route;
    }
}

fn invalid(part: &str, e: serde_json::Error) -> ApiError {
    ApiError::bad_request(format!("Invalid {part}: {e}")).code("validation_error")
}

/// The path params, e.g. `:city` in `"/:city"`.
///
/// With one param, take it directly: `Path(city): Path<String>`, or any type that deserializes from a string, like `u32`. With several, use a struct whose fields match the param names:
///
/// ```no_run
/// use samishisa::prelude::*;
/// #[schema]
/// struct Ids { user: u32, post: u32 }
///
/// fn post(Path(ids): Path<Ids>) -> String { format!("post {} of user {}", ids.post, ids.user) }
/// ```
///
/// Params arrive as strings. The server converts and validates them before your handler runs, so `Path<u32>` rejects `/users/abc` with a `400`.
#[derive(Debug, Clone)]
pub struct Path<T>(pub T);

impl<C, T: DeserializeOwned + JsonSchema> FromRequest<C> for Path<T> {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        let raw = req.params_json();
        match serde_json::from_str::<T>(raw) {
            Ok(v) => Ok(Path(v)),
            Err(e) => {
                let map: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>> = serde_json::from_str(raw).unwrap_or_default();
                match (map.len(), map.into_values().next()) {
                    (1, Some(only)) => serde_json::from_str(only.get()).map(Path).map_err(|e| invalid("path parameter", e)),
                    _ => Err(invalid("path parameters", e)),
                }
            }
        }
    }

    fn describe(route: &mut RouteSpec) {
        let schema = schema_of::<T>();
        if is_object_schema(&schema) {
            return route.set_params(schema);
        }
        match route.param_names().as_slice() {
            [one] => {
                let one = one.to_string();
                let mut properties = serde_json::Map::new();
                properties.insert(one.clone(), schema);
                route.set_params(json!({ "type": "object", "properties": properties, "required": [one] }))
            }
            names => {
                let msg = format!(
                    "{}: Path<{}> takes a single value, but the path has {} params {names:?}; use a struct with one field per param",
                    route.path(),
                    std::any::type_name::<T>(),
                    names.len()
                );
                route.error(msg)
            }
        }
    }
}

/// The query string, e.g. `?units=metric&days=3`, as a struct.
///
/// ```no_run
/// use samishisa::prelude::*;
/// #[schema]
/// struct Forecast {
///     /// Number of days, 1 to 14.
///     #[serde(default = "three")]
///     days: u8,
///     #[serde(default)]
///     units: Units,
/// }
/// #[schema]
/// #[derive(Default)]
/// #[serde(rename_all = "lowercase")]
/// enum Units { #[default] Metric, Imperial }
/// fn three() -> u8 { 3 }
///
/// fn forecast(Query(q): Query<Forecast>) -> String { format!("{} days", q.days) }
/// ```
///
/// Use `#[serde(default)]` for optional parameters. Values arrive as strings and the server converts them to the schema's types. A repeated key (`?tag=a&tag=b`) becomes a list for `Vec` fields.
#[derive(Debug, Clone)]
pub struct Query<T>(pub T);

impl<C, T: DeserializeOwned + JsonSchema> FromRequest<C> for Query<T> {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        let raw = match req.query_json() {
            "null" => "{}",
            q => q,
        };
        serde_json::from_str(raw).map(Query).map_err(|e| invalid("query", e))
    }
    fn describe(route: &mut RouteSpec) {
        route.set_query(schema_of::<T>());
    }
}

/// A JSON request body, or a JSON response.
///
/// As an argument, the server validates the body against `T`'s schema before your handler runs. This applies to `POST`, `PUT` and `PATCH`. As a return value, `T` is sent with status `200`, and its schema appears in the docs.
///
/// ```no_run
/// use samishisa::prelude::*;
/// #[schema]
/// struct NewNote { text: String }
/// #[schema]
/// struct Note { id: i64, text: String }
///
/// fn create(Json(new): Json<NewNote>, ctx: Ctx) -> Result<Json<Note>> {
///     let id = ctx.store.incr("next-id", 1);
///     let note = Note { id, text: new.text };
///     ctx.store.set(&format!("note:{id}"), &note)?;
///     Ok(Json(note))
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Json<T>(pub T);

impl<C, T: DeserializeOwned + JsonSchema> FromRequest<C> for Json<T> {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        serde_json::from_str(req.body_json()).map(Json).map_err(|e| invalid("body", e))
    }
    fn describe(route: &mut RouteSpec) {
        route.set_body(schema_of::<T>());
    }
}

/// The request headers the server forwards (lowercase names): `accept`, `accept-language`,
/// `authorization`, `content-type`, `user-agent`, and any `x-...` header.
#[derive(Debug, Clone, Default)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
    /// The first value of a header (case-insensitive name).
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

impl<C> FromRequest<C> for Headers {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        Ok(Headers(req.headers.clone()))
    }
}

impl<C> FromRequest<C> for Ctx<C> {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        Ok(req.ctx.clone())
    }
}
