use std::{sync::Arc, time::Duration};

use wasmtime::{
    Engine, ResourceLimiter, Store,
    component::{HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use super::{
    Plugin, WitError,
    samishisa::extension::{host as wit_host, types},
    ticks,
};
use crate::{
    log::{self, Level},
    netguard::{Egress, FetchError, OutRequest, Policy},
    store::Store as KvStore,
};

pub struct Shared {
    pub name: String,
    pub store: Arc<KvStore>,
    pub egress: Arc<Egress>,
    pub policy: Policy,
}

pub struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    pub(super) memory: MemoryCap,
    shared: Arc<Shared>,
}

pub(super) struct MemoryCap {
    pub(super) max_bytes: usize,
    pub(super) was_hit: bool,
}

const MAX_TABLE_ELEMENTS: usize = 1_000_000;
const MAX_INSTANCES_PER_COMPONENT: usize = 32;

impl ResourceLimiter for MemoryCap {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        if desired > self.max_bytes {
            self.was_hit = true;
            return Ok(false);
        }
        Ok(true)
    }
    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        Ok(desired <= MAX_TABLE_ELEMENTS)
    }
    fn instances(&self) -> usize {
        MAX_INSTANCES_PER_COMPONENT
    }
}

pub(super) fn new_store(engine: &Engine, shared: Arc<Shared>, deadline: Duration, max_memory_bytes: usize) -> Store<State> {
    let state = State { wasi: WasiCtx::builder().build(), table: ResourceTable::new(), memory: MemoryCap { max_bytes: max_memory_bytes, was_hit: false }, shared };
    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.memory);
    store.set_epoch_deadline(ticks(deadline));
    store.epoch_deadline_trap();
    store
}

pub(super) fn linker(engine: &Engine) -> anyhow::Result<Linker<State>> {
    let mut linker: Linker<State> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(anyhow::Error::from)?;
    Plugin::add_to_linker::<_, HasSelf<State>>(&mut linker, |state| state).map_err(anyhow::Error::from)?;
    Ok(linker)
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl types::Host for State {}

impl wit_host::Host for State {
    fn log(&mut self, level: wit_host::LogLevel, message: String) {
        let level = match level {
            wit_host::LogLevel::Debug => Level::Debug,
            wit_host::LogLevel::Info => Level::Info,
            wit_host::LogLevel::Warn => Level::Warn,
            wit_host::LogLevel::Error => Level::Error,
        };
        log::write(level, &self.shared.name, &message);
    }

    fn store_get(&mut self, key: String) -> Option<String> {
        self.shared.store.get(&key)
    }

    fn store_set(&mut self, key: String, value_json: String, ttl_seconds: Option<u32>) {
        self.shared.store.set(&key, &value_json, ttl_seconds.map(|s| Duration::from_secs(s.into())))
    }

    fn store_delete(&mut self, key: String) {
        self.shared.store.delete(&key)
    }

    fn store_incr(&mut self, key: String, by: i64) -> i64 {
        self.shared.store.incr(&key, by)
    }

    fn store_keys(&mut self, prefix: String) -> Vec<String> {
        self.shared.store.keys(&prefix)
    }

    async fn fetch(&mut self, request: wit_host::HttpRequest) -> Result<wit_host::HttpResponse, WitError> {
        let shared = self.shared.clone();
        let request = OutRequest { method: request.method, url: request.url, headers: request.headers, body: request.body };
        let response = shared.egress.fetch(&shared.name, &shared.policy, request).await?;
        Ok(wit_host::HttpResponse { status: response.status, headers: response.headers, body: response.body })
    }
}

impl From<FetchError> for WitError {
    fn from(e: FetchError) -> Self {
        WitError { status: e.status, code: Some(e.code.into()), message: e.message, hint: e.hint, details_json: e.details.map(|d| d.to_string()) }
    }
}
