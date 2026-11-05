use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use wasmtime::{
    Config as WasmConfig, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store, StoreLimits, StoreLimitsBuilder, Trap,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::{
    log::{self, Level},
    netguard::{Egress, OutRequest, Policy},
    store::Store as KvStore,
};

wasmtime::component::bindgen!({
    world: "plugin",
    path: "../sdk/wit",
    imports: { "samishisa:extension/host.fetch": async },
    exports: { default: async },
});

pub use samishisa::extension::types::{ApiError as WitError, Request as WitRequest, Response as WitResponse};
use samishisa::extension::host::{self as wit_host, LogLevel};

pub const TICK: Duration = Duration::from_millis(10);
const MAX_INSTANCES_PER_EXT: usize = 64;
const RECYCLE_AFTER_CALLS: u32 = 10_000;

pub fn engine(pool_slots: Option<u32>, max_memory_mb: u32) -> anyhow::Result<Engine> {
    let memory = (max_memory_mb as usize) << 20;
    let make = |pooled: Option<u32>| {
        let mut cfg = WasmConfig::new();
        cfg.epoch_interruption(true)
            .memory_reservation(memory as u64)
            .memory_guard_size(64 << 10);
        if let Some(n) = pooled {
            let mut pool = PoolingAllocationConfig::default();
            pool.total_component_instances(n)
                .total_core_instances(n * 4)
                .total_memories(n)
                .total_tables(n * 4)
                .total_stacks(n)
                .max_memory_size(memory);
            cfg.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
        }
        Engine::new(&cfg).map_err(anyhow::Error::from)
    };
    let engine = match pool_slots.map(|n| make(Some(n))) {
        Some(Ok(e)) => e,
        Some(Err(e)) => {
            log::write(Level::Warn, "core", &format!("could not reserve the instance pool ({e:#}); allocating instances on demand instead (lower \"maxInstances\" to use the pool)"));
            make(None)?
        }
        None => make(None)?,
    };
    let e = engine.clone();
    std::thread::Builder::new().name("epoch".into()).spawn(move || loop {
        std::thread::sleep(TICK);
        e.increment_epoch();
    })?;
    Ok(engine)
}

pub fn load_component(engine: &Engine, wasm_path: &Path, cache_dir: &Path) -> anyhow::Result<Component> {
    let bytes = std::fs::read(wasm_path).with_context(|| format!("reading {}", wasm_path.display()))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let mut compat = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&engine.precompile_compatibility_hash(), &mut compat);
    h.update(std::hash::Hasher::finish(&compat).to_le_bytes());
    let key = format!("{:x}", h.finalize());
    let cached = cache_dir.join(format!("{}.cwasm", &key[..32]));
    if cached.exists() {
        if let Ok(c) = unsafe { Component::deserialize_file(engine, &cached) } {
            return Ok(c);
        }
    }
    let t = Instant::now();
    let native = engine.precompile_component(&bytes).map_err(anyhow::Error::from).with_context(|| format!("{} is not a valid WebAssembly component", wasm_path.display()))?;
    log::write(Level::Debug, "core", &format!("compiled {} in {:?}", wasm_path.display(), t.elapsed()));
    if std::fs::create_dir_all(cache_dir).is_ok() {
        let tmp = cached.with_extension("tmp");
        if std::fs::write(&tmp, &native).is_ok() {
            let _ = std::fs::rename(&tmp, &cached);
        }
    }
    Ok(unsafe { Component::deserialize(engine, &native) }.map_err(anyhow::Error::from)?)
}

pub struct Shared {
    pub name: String,
    pub store: Arc<KvStore>,
    pub egress: Arc<Egress>,
    pub policy: Policy,
}

pub struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
    shared: Arc<Shared>,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl samishisa::extension::types::Host for State {}

impl wit_host::Host for State {
    fn log(&mut self, level: LogLevel, message: String) {
        let level = match level {
            LogLevel::Debug => Level::Debug,
            LogLevel::Info => Level::Info,
            LogLevel::Warn => Level::Warn,
            LogLevel::Error => Level::Error,
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
    async fn fetch(&mut self, req: wit_host::HttpRequest) -> Result<wit_host::HttpResponse, WitError> {
        let shared = self.shared.clone();
        let out = OutRequest { method: req.method, url: req.url, headers: req.headers, body: req.body };
        let r = shared.egress.fetch(&shared.name, &shared.policy, out).await?;
        Ok(wit_host::HttpResponse { status: r.status, headers: r.headers, body: r.body })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    #[serde(default)]
    pub sdk: Option<String>,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub config: Option<Value>,
    #[serde(default)]
    pub network: Option<Value>,
    #[allow(dead_code)]
    #[serde(default)]
    pub rate_limit: Option<Value>,
    #[serde(default)]
    pub routes: Vec<RouteManifest>,
    #[serde(default)]
    pub jobs: Vec<JobManifest>,
    #[serde(default)]
    pub setup: bool,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteManifest {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub query: Option<Value>,
    #[serde(default)]
    pub body: Option<Value>,
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub cache: Option<Value>,
    #[allow(dead_code)]
    #[serde(default)]
    pub rate_limit: Option<Value>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobManifest {
    pub every_ms: u64,
}

struct Inst {
    store: Store<State>,
    plugin: Plugin,
    calls: u32,
}

#[derive(Debug)]
pub enum CallError {
    Timeout,
    Busy,
    Crashed(String),
}

pub struct Runner {
    pub manifest: Manifest,
    pub mount: String,
    pub source: String,
    pre: PluginPre<State>,
    engine: Engine,
    shared: Arc<Shared>,
    config_json: String,
    memory: usize,
    pool: Mutex<Vec<Inst>>,
    live: AtomicUsize,
}

pub struct LoadOptions {
    pub alias: Option<String>,
    pub config: Value,
    pub source: String,
    pub open_store: Box<dyn FnOnce(&str) -> anyhow::Result<Arc<KvStore>> + Send>,
    pub egress: Arc<Egress>,
    pub network: Option<Value>,
    pub memory_mb: u32,
}

struct Lease<'a> {
    runner: &'a Runner,
    inst: Option<Inst>,
}

impl Lease<'_> {
    fn inst(&mut self) -> &mut Inst {
        self.inst.as_mut().expect("leased")
    }
    fn give_back(mut self) {
        if let Some(i) = self.inst.take() {
            self.runner.give_back(i);
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if self.inst.take().is_some() {
            self.runner.live.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl Runner {
    pub async fn load(engine: &Engine, linker: &Linker<State>, wasm: &Path, cache_dir: &Path, opts: LoadOptions) -> anyhow::Result<Arc<Runner>> {
        let LoadOptions { alias, config, source, open_store, egress, network, memory_mb } = opts;
        let alias = alias.as_deref();
        let memory = (memory_mb as usize) << 20;
        let component = load_component(engine, wasm, cache_dir)?;
        let pre = PluginPre::new(linker.instantiate_pre(&component).map_err(anyhow::Error::from).context("the component needs imports samishisa doesn't provide (was it built with the samishisa SDK?)")?).map_err(anyhow::Error::from)?;

        let probe_shared = Arc::new(Shared {
            name: alias.unwrap_or("extension").to_owned(),
            store: Arc::new(KvStore::open(None)?),
            egress: egress.clone(),
            policy: Policy::None,
        });
        let mut store = new_store(engine, probe_shared, Duration::from_secs(5), memory);
        let plugin = pre.instantiate_async(&mut store).await.map_err(anyhow::Error::from)?;
        let raw = plugin.samishisa_extension_extension().call_manifest(&mut store).await.map_err(anyhow::Error::from).context("reading the extension's manifest")?;
        let manifest: Manifest = serde_json::from_str(&raw).context("the extension's manifest is not valid")?;
        if !manifest.errors.is_empty() {
            bail!("the extension's definition has mistakes:\n  - {}", manifest.errors.join("\n  - "));
        }

        if let Some(schema) = &manifest.config {
            let validator = jsonschema::validator_for(schema).map_err(|e| anyhow::anyhow!("the extension's config schema is invalid: {e}"))?;
            let problems: Vec<String> = validator.iter_errors(&config).map(|e| {
                let at = e.instance_path().to_string();
                format!("config{}: {e}", if at.is_empty() { String::new() } else { at.replace('/', ".") })
            }).collect();
            if !problems.is_empty() {
                bail!("invalid config for \"{}\":\n  - {}", manifest.name, problems.join("\n  - "));
            }
        }

        let mount = alias.unwrap_or(&manifest.name).to_owned();
        let policy = Policy::from_value(network.as_ref().or(manifest.network.as_ref()));
        let shared = Arc::new(Shared { name: mount.clone(), store: open_store(&mount)?, egress, policy });
        let runner = Arc::new(Runner {
            manifest, mount, source, pre, engine: engine.clone(), shared, config_json: config.to_string(), memory,
            pool: Mutex::new(Vec::new()), live: AtomicUsize::new(0),
        });

        let mut lease = runner.checkout().await.map_err(|e| anyhow::anyhow!("{}", call_error_text(&e)))?;
        if runner.manifest.setup {
            let inst = lease.inst();
            inst.store.set_epoch_deadline(ticks(Duration::from_secs(30)));
            let r = inst.plugin.samishisa_extension_extension().call_setup(&mut inst.store).await;
            match r {
                Ok(Ok(())) => {}
                Ok(Err(msg)) => bail!("setup failed: {msg}"),
                Err(e) => bail!("setup crashed: {}", trap_text(&e)),
            }
        }
        lease.give_back();
        Ok(runner)
    }

    pub fn policy(&self) -> &Policy {
        &self.shared.policy
    }

    async fn checkout(&self) -> Result<Lease<'_>, CallError> {
        if let Some(i) = self.pool.lock().unwrap().pop() {
            return Ok(Lease { runner: self, inst: Some(i) });
        }
        if self.live.fetch_add(1, Ordering::SeqCst) >= MAX_INSTANCES_PER_EXT {
            self.live.fetch_sub(1, Ordering::SeqCst);
            return Err(CallError::Busy);
        }
        let made = async {
            let mut store = new_store(&self.engine, self.shared.clone(), Duration::from_secs(5), self.memory);
            let plugin = self.pre.instantiate_async(&mut store).await.map_err(|e| CallError::Crashed(format!("instantiate: {e:?}")))?;
            match plugin.samishisa_extension_extension().call_init(&mut store, &self.config_json).await {
                Ok(Ok(())) => Ok(Inst { store, plugin, calls: 0 }),
                Ok(Err(msg)) => Err(CallError::Crashed(format!("init: {msg}"))),
                Err(e) => Err(classify(e)),
            }
        }
        .await;
        match made {
            Ok(i) => Ok(Lease { runner: self, inst: Some(i) }),
            Err(e) => {
                self.live.fetch_sub(1, Ordering::SeqCst);
                Err(e)
            }
        }
    }

    fn give_back(&self, mut inst: Inst) {
        inst.calls += 1;
        if inst.calls >= RECYCLE_AFTER_CALLS {
            self.live.fetch_sub(1, Ordering::SeqCst);
            return;
        }
        self.pool.lock().unwrap().push(inst);
    }

    pub async fn handle(&self, req: WitRequest, timeout: Duration) -> Result<Result<WitResponse, WitError>, CallError> {
        let mut lease = self.checkout().await?;
        let inst = lease.inst();
        inst.store.set_epoch_deadline(ticks(timeout));
        let r = inst.plugin.samishisa_extension_extension().call_handle(&mut inst.store, &req).await;
        match r {
            Ok(answer) => {
                lease.give_back();
                Ok(answer)
            }
            Err(e) => Err(classify(e)),
        }
    }

    pub async fn run_job(&self, index: u32) -> Result<Result<(), String>, CallError> {
        let mut lease = self.checkout().await?;
        let inst = lease.inst();
        inst.store.set_epoch_deadline(ticks(Duration::from_secs(60)));
        let r = inst.plugin.samishisa_extension_extension().call_run_job(&mut inst.store, index).await;
        match r {
            Ok(answer) => {
                lease.give_back();
                Ok(answer)
            }
            Err(e) => Err(classify(e)),
        }
    }
}

fn new_store(engine: &Engine, shared: Arc<Shared>, deadline: Duration, memory: usize) -> Store<State> {
    let state = State {
        wasi: WasiCtx::builder().build(),
        table: ResourceTable::new(),
        limits: StoreLimitsBuilder::new().memory_size(memory).instances(32).build(),
        shared,
    };
    let mut store = Store::new(engine, state);
    store.limiter(|s| &mut s.limits);
    store.set_epoch_deadline(ticks(deadline));
    store.epoch_deadline_trap();
    store
}

fn ticks(d: Duration) -> u64 {
    (d.as_millis() / TICK.as_millis()).max(1) as u64
}

fn classify(e: wasmtime::Error) -> CallError {
    if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) { CallError::Timeout } else { CallError::Crashed(trap_text(&e)) }
}

fn trap_text(e: &wasmtime::Error) -> String {
    format!("{e:?}")
}

pub fn call_error_text(e: &CallError) -> String {
    match e {
        CallError::Timeout => "timed out".into(),
        CallError::Busy => "all instances busy".into(),
        CallError::Crashed(m) => m.clone(),
    }
}

pub fn linker(engine: &Engine) -> anyhow::Result<Linker<State>> {
    let mut linker: Linker<State> = Linker::new(engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(anyhow::Error::from)?;
    Plugin::add_to_linker::<_, HasSelf<State>>(&mut linker, |s| s).map_err(anyhow::Error::from)?;
    Ok(linker)
}
