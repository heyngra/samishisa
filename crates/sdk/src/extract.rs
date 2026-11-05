use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::{
    ApiError, Ctx, Result,
    extension::{RouteSpec, is_object_schema, schema_of},
};

pub struct RequestParts<C> {
    pub(crate) input: Value,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) ctx: Ctx<C>,
}

impl<C> RequestParts<C> {
    pub fn method(&self) -> &str {
        &self.method
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
    pub fn input(&self, part: &str) -> &Value {
        self.input.get(part).unwrap_or(&Value::Null)
    }
    pub fn ctx(&self) -> &Ctx<C> {
        &self.ctx
    }
}

pub trait FromRequest<C>: Sized {
    fn from_request(req: &RequestParts<C>) -> Result<Self>;
    fn describe(route: &mut RouteSpec) {
        let _ = route;
    }
}

fn invalid(part: &str, e: serde_json::Error) -> ApiError {
    ApiError::bad_request(format!("Invalid {part}: {e}")).code("validation_error")
}

#[derive(Debug, Clone)]
pub struct Path<T>(pub T);

impl<C, T: DeserializeOwned + JsonSchema> FromRequest<C> for Path<T> {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        let params = req.input("params");
        match serde_json::from_value::<T>(params.clone()) {
            Ok(v) => Ok(Path(v)),
            Err(e) => match params {
                Value::Object(m) if m.len() == 1 => {
                    let only = m.values().next().cloned().unwrap_or(Value::Null);
                    serde_json::from_value(only).map(Path).map_err(|e| invalid("path parameter", e))
                }
                _ => Err(invalid("path parameters", e)),
            },
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

#[derive(Debug, Clone)]
pub struct Query<T>(pub T);

impl<C, T: DeserializeOwned + JsonSchema> FromRequest<C> for Query<T> {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        let q = match req.input("query") {
            Value::Null => Value::Object(Default::default()),
            v => v.clone(),
        };
        serde_json::from_value(q).map(Query).map_err(|e| invalid("query", e))
    }
    fn describe(route: &mut RouteSpec) {
        route.set_query(schema_of::<T>());
    }
}

#[derive(Debug, Clone)]
pub struct Json<T>(pub T);

impl<C, T: DeserializeOwned + JsonSchema> FromRequest<C> for Json<T> {
    fn from_request(req: &RequestParts<C>) -> Result<Self> {
        serde_json::from_value(req.input("body").clone()).map(Json).map_err(|e| invalid("body", e))
    }
    fn describe(route: &mut RouteSpec) {
        route.set_body(schema_of::<T>());
    }
}

#[derive(Debug, Clone, Default)]
pub struct Headers(pub Vec<(String, String)>);

impl Headers {
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
