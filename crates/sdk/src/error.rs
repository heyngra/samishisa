use std::fmt;

/// `Result` with [`ApiError`] as the default error. Most handlers return this.
///
/// ```
/// use samishisa::prelude::*;
/// fn lookup(id: u32) -> Result<String> {
///     if id == 0 { return Err(ApiError::not_found("No item with id 0")); }
///     Ok(format!("item {id}"))
/// }
/// ```
pub type Result<T, E = ApiError> = std::result::Result<T, E>;

/// An error sent to the caller in the standard envelope:
///
/// ```json
/// { "error": { "status": 404, "code": "city_not_found", "message": "No such city", "hint": "Try /weather/london", "requestId": "..." } }
/// ```
///
/// Build one with a constructor and add the optional parts:
///
/// ```
/// use samishisa::ApiError;
/// let e = ApiError::not_found("No such city")
///     .code("city_not_found")
///     .hint("Try /weather/london");
/// assert_eq!(e.status(), 404);
/// ```
///
/// Any other error converts into an `ApiError` with `?`. It becomes an internal error: the caller gets a `500` without details, and the original message goes to the server log with the request id.
///
/// ```
/// use samishisa::prelude::*;
/// fn parse(s: &str) -> Result<u32> {
///     Ok(s.parse::<u32>()?)
/// }
/// ```
///
/// `ApiError` doesn't implement [`std::error::Error`] on purpose. That is what makes the conversion above possible.
#[derive(Clone, PartialEq)]
#[must_use = "an ApiError does nothing until you return it"]
pub struct ApiError {
    pub(crate) status: u16,
    pub(crate) code: Option<String>,
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
    pub(crate) details: Option<serde_json::Value>,
    /// Logged, never sent to the caller. The source of an internal error.
    pub(crate) internal: Option<String>,
}

impl ApiError {
    /// An error with any HTTP error status (400 to 599) and a message.
    ///
    /// Statuses outside that range are sent as `500`, and the server log notes the status you passed:
    ///
    /// ```
    /// use samishisa::ApiError;
    /// assert_eq!(ApiError::new(418, "I'm a teapot").status(), 418);
    /// assert_eq!(ApiError::new(200, "not an error").status(), 500);
    /// ```
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        let (status, internal) = match status {
            400..=599 => (status, None),
            other => (500, Some(format!("ApiError::new was given {other}, which is not an error status (400-599)"))),
        };
        Self { status, code: None, message: message.into(), hint: None, details: None, internal }
    }

    /// `400 Bad Request`: well-formed, but it makes no sense. Schema errors never reach your handler.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    /// `401 Unauthorized`: missing or wrong credentials.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, message)
    }
    /// `403 Forbidden`: credentials are fine, but not allowed to do this.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, message)
    }
    /// `404 Not Found`: the thing the caller asked for does not exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
    /// `409 Conflict`: the request conflicts with the current state (e.g. a duplicate).
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, message)
    }
    /// `502 Bad Gateway`: a third-party service you depend on failed or answered nonsense.
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(502, message)
    }
    /// `500 Internal Server Error`, with a message the caller sees. Prefer `?` on the underlying error instead: it logs the details and shows nothing.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    /// Machine-readable code, e.g. `"city_not_found"`. Defaults to one per status.
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    /// How the caller can fix the request.
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    /// Attach extra JSON for the caller, e.g. `serde_json::json!({ "retryIn": 30 })`.
    pub fn details(mut self, details: impl serde::Serialize) -> Self {
        self.details = serde_json::to_value(details).ok();
        self
    }

    /// The HTTP status.
    pub fn status(&self) -> u16 {
        self.status
    }
    /// The message shown to the caller.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Debug for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("ApiError");
        d.field("status", &self.status).field("message", &self.message);
        if let Some(c) = &self.code {
            d.field("code", c);
        }
        if let Some(h) = &self.hint {
            d.field("hint", h);
        }
        if let Some(i) = &self.internal {
            d.field("internal", i);
        }
        d.finish()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.status, self.message)?;
        if let Some(i) = &self.internal {
            write!(f, " ({i})")?;
        }
        Ok(())
    }
}

impl<E: std::error::Error> From<E> for ApiError {
    fn from(e: E) -> Self {
        let mut err = Self::new(500, "Internal error");
        err.internal = Some(e.to_string());
        err
    }
}
