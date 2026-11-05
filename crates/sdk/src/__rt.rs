use std::{cell::RefCell, rc::Rc};

use serde_json::{Value, json};

use crate::{
    ApiError, Config, Ctx, Extension, RequestParts,
    __bindings::samishisa::extension::{host, types as wit},
    extension::config_schema_of,
};

pub trait Erased {
    fn manifest(&self) -> String;
    fn init(&mut self, config_json: &str) -> Result<(), String>;
    fn setup(&self) -> Result<(), String>;
    fn handle(&self, req: wit::Request) -> Result<wit::Response, wit::ApiError>;
    fn run_job(&self, job: u32) -> Result<(), String>;
}

pub struct Built<C> {
    ext: Extension<C>,
    crate_version: &'static str,
    crate_description: &'static str,
    config: Option<Rc<C>>,
}

impl<C> Built<C> {
    pub fn new(ext: Extension<C>, crate_version: &'static str, crate_description: &'static str) -> Self {
        Self { ext, crate_version, crate_description, config: None }
    }
}

impl ApiError {
    pub(crate) fn from_wit(e: wit::ApiError) -> Self {
        Self {
            status: e.status,
            code: e.code,
            message: e.message,
            hint: e.hint,
            details: e.details_json.and_then(|d| serde_json::from_str(&d).ok()),
            internal: None,
        }
    }

    fn into_wit(self, request_id: Option<&str>) -> wit::ApiError {
        if let Some(internal) = &self.internal {
            let id = request_id.map(|i| format!("[{i}] ")).unwrap_or_default();
            host::log(host::LogLevel::Error, &format!("{id}{} {}: {internal}", self.status, self.message));
        }
        wit::ApiError {
            status: self.status.clamp(400, 599),
            code: self.code,
            message: self.message,
            hint: self.hint,
            details_json: self.details.map(|d| d.to_string()),
        }
    }
}

impl<C: Config> Built<C> {
    fn ctx(&self, request_id: Option<Rc<str>>) -> Result<Ctx<C>, String> {
        let config = self.config.clone().ok_or("the extension was called before init")?;
        Ok(Ctx::new(config, request_id))
    }
}

impl<C: Config> Erased for Built<C> {
    fn manifest(&self) -> String {
        let e = &self.ext;
        let mut errors = e.errors.clone();
        for r in &e.routes {
            errors.extend(r.spec.errors.iter().cloned());
        }
        let description = e.about.clone().or_else(|| (!self.crate_description.is_empty()).then(|| self.crate_description.to_owned()));
        json!({
            "sdk": concat!("samishisa ", env!("CARGO_PKG_VERSION")),
            "name": e.name,
            "version": e.version.clone().unwrap_or_else(|| self.crate_version.to_owned()),
            "description": description,
            "config": config_schema_of::<C>(),
            "network": e.network,
            "rateLimit": e.rate_limit.map(|r| r.to_json()),
            "routes": e.routes.iter().map(|r| r.spec.to_json()).collect::<Vec<_>>(),
            "jobs": e.jobs.iter().map(|j| json!({ "everyMs": j.every.as_millis() as u64 })).collect::<Vec<_>>(),
            "setup": e.setup.is_some(),
            "errors": errors,
        })
        .to_string()
    }

    fn init(&mut self, config_json: &str) -> Result<(), String> {
        let raw = if config_json.trim().is_empty() { "{}" } else { config_json };
        let config: C = serde_json::from_str(raw).map_err(|e| format!("invalid config: {e}"))?;
        self.config = Some(Rc::new(config));
        Ok(())
    }

    fn setup(&self) -> Result<(), String> {
        match &self.ext.setup {
            Some(f) => f(&self.ctx(None)?).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }

    fn handle(&self, req: wit::Request) -> Result<wit::Response, wit::ApiError> {
        let request_id: Rc<str> = Rc::from(req.request_id.as_str());
        let fail = |e: ApiError| e.into_wit(Some(&request_id));
        let route = self.ext.routes.get(req.route as usize).ok_or_else(|| fail(ApiError::internal("Unknown route index")))?;
        let ctx = self.ctx(Some(request_id.clone())).map_err(|m| fail(ApiError::internal(m)))?;
        let input: Value = serde_json::from_str(&req.input_json).map_err(|e| fail(e.into()))?;
        let parts = RequestParts { input, method: req.method, path: req.path, headers: req.headers, ctx };
        match (route.handler)(&parts) {
            Ok(r) => Ok(wit::Response { status: r.status, headers: r.headers, body: r.body }),
            Err(e) => Err(fail(e)),
        }
    }

    fn run_job(&self, job: u32) -> Result<(), String> {
        let job = self.ext.jobs.get(job as usize).ok_or("unknown job index")?;
        (job.run)(&self.ctx(None)?).map_err(|e| e.to_string())
    }
}

thread_local! {
    static EXT: RefCell<Option<Box<dyn Erased>>> = const { RefCell::new(None) };
}

fn with<R>(build: Option<fn() -> Box<dyn Erased>>, f: impl FnOnce(&mut dyn Erased) -> R) -> Option<R> {
    EXT.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(build?());
        }
        Some(f(slot.as_mut()?.as_mut()))
    })
}

const NOT_READY: &str = "the extension was called before manifest/init";

pub fn manifest(build: fn() -> Box<dyn Erased>) -> String {
    with(Some(build), |e| e.manifest()).unwrap_or_default()
}

pub fn init(build: fn() -> Box<dyn Erased>, config_json: &str) -> Result<(), String> {
    with(Some(build), |e| e.init(config_json)).unwrap_or(Err(NOT_READY.into()))
}

pub fn setup() -> Result<(), String> {
    with(None, |e| e.setup()).unwrap_or(Err(NOT_READY.into()))
}

pub fn handle(req: wit::Request) -> Result<wit::Response, wit::ApiError> {
    with(None, |e| e.handle(req)).unwrap_or_else(|| Err(ApiError::internal(NOT_READY).into_wit(None)))
}

pub fn run_job(job: u32) -> Result<(), String> {
    with(None, |e| e.run_job(job)).unwrap_or(Err(NOT_READY.into()))
}
