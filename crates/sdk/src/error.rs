use std::fmt;

pub type Result<T, E = ApiError> = std::result::Result<T, E>;

#[derive(Clone, PartialEq)]
pub struct ApiError {
    pub(crate) status: u16,
    pub(crate) code: Option<String>,
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
    pub(crate) details: Option<serde_json::Value>,
    pub(crate) internal: Option<String>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self { status, code: None, message: message.into(), hint: None, details: None, internal: None }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(409, message)
    }
    pub fn upstream(message: impl Into<String>) -> Self {
        Self::new(502, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, message)
    }

    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn details(mut self, details: impl serde::Serialize) -> Self {
        self.details = serde_json::to_value(details).ok();
        self
    }

    pub fn status(&self) -> u16 {
        self.status
    }
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
