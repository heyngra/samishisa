mod host;
mod runner;

use std::{
    collections::hash_map::DefaultHasher,
    hash::{BuildHasher, BuildHasherDefault},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};
use wasmtime::{
    Config as WasmConfig, Engine, InstanceAllocationStrategy, PoolingAllocationConfig,
    component::{Component, Linker},
};

pub use runner::{CallError, Runner};

use crate::{
    config::{Config, DEFAULT_MEMORY_MB},
    log::{log_debug, log_warn},
    manifest::Manifest,
    netguard::{Egress, Policy},
    store::Store,
};
use host::{Shared, State};

wasmtime::component::bindgen!({
    world: "plugin",
    path: "../sdk/wit",
    imports: { "samishisa:extension/host.fetch": async },
    exports: { default: async },
});

pub use samishisa::extension::types::{ApiError as WitError, Request as WitRequest, Response as WitResponse};

const TICK: Duration = Duration::from_millis(10);
const MANIFEST_DEADLINE: Duration = Duration::from_secs(5);

fn ticks(deadline: Duration) -> u64 {
    u64::try_from(deadline.as_millis() / TICK.as_millis()).unwrap_or(u64::MAX).max(1)
}

fn megabytes(mb: u32) -> usize {
    usize::try_from(mb).unwrap_or(usize::MAX).saturating_mul(1 << 20)
}

pub struct Sandbox {
    engine: Engine,
    linker: Linker<State>,
    egress: Arc<Egress>,
    compile_cache: Option<PathBuf>,
}

pub struct Loaded {
    pub manifest: Manifest,
    pre: PluginPre<State>,
}

pub struct StartOptions {
    pub mount: String,
    pub source: String,
    pub config: Value,
    pub store: Arc<Store>,
    pub network_override: Option<Policy>,
    pub memory_mb: u32,
}

impl Sandbox {
    pub fn for_serving(cfg: &Config) -> anyhow::Result<Sandbox> {
        Sandbox::new(Some(cfg.max_instances), cfg.max_memory_mb(), Egress::for_extensions(cfg.network.allow_private), Some(cfg.data_dir.join("cache")))
    }

    pub fn for_inspecting() -> anyhow::Result<Sandbox> {
        Sandbox::new(None, DEFAULT_MEMORY_MB, Egress::for_extensions(false), None)
    }

    fn new(pool_slots: Option<u32>, max_memory_mb: u32, egress: Egress, compile_cache: Option<PathBuf>) -> anyhow::Result<Sandbox> {
        let engine = match pool_slots.map(|slots| new_engine(Some(slots), max_memory_mb)) {
            Some(Ok(engine)) => engine,
            Some(Err(e)) => {
                log_warn!("core", "could not reserve the instance pool ({e:#}); allocating instances on demand instead (lower \"maxInstances\" to use the pool)");
                new_engine(None, max_memory_mb)?
            }
            None => new_engine(None, max_memory_mb)?,
        };
        let ticking = engine.clone();
        std::thread::Builder::new().name("epoch".into()).spawn(move || {
            loop {
                std::thread::sleep(TICK);
                ticking.increment_epoch();
            }
        })?;
        let linker = host::linker(&engine)?;
        Ok(Sandbox { engine, linker, egress: Arc::new(egress), compile_cache })
    }

    pub async fn load(&self, wasm: &Path) -> anyhow::Result<Loaded> {
        let (engine, cache, path) = (self.engine.clone(), self.compile_cache.clone(), wasm.to_owned());
        let component = tokio::task::spawn_blocking(move || compile(&engine, cache.as_deref(), &path)).await.context("compiling the extension panicked")??;
        let pre = self
            .linker
            .instantiate_pre(&component)
            .map_err(anyhow::Error::from)
            .context("the component needs imports samishisa doesn't provide (was it built with the samishisa SDK?)")?;
        let pre = PluginPre::new(pre).map_err(anyhow::Error::from)?;
        let manifest = self.read_manifest(&pre).await?;
        if !manifest.errors.is_empty() {
            bail!("the extension's definition has mistakes:\n  - {}", manifest.errors.join("\n  - "));
        }
        Ok(Loaded { manifest, pre })
    }

    async fn read_manifest(&self, pre: &PluginPre<State>) -> anyhow::Result<Manifest> {
        let shared = Arc::new(Shared { name: "extension".into(), store: Arc::new(Store::open(None)?), egress: self.egress.clone(), policy: Policy::None });
        let mut store = host::new_store(&self.engine, shared, MANIFEST_DEADLINE, megabytes(DEFAULT_MEMORY_MB));
        let plugin = pre.instantiate_async(&mut store).await.map_err(anyhow::Error::from)?;
        let json = plugin.samishisa_extension_extension().call_manifest(&mut store).await.map_err(anyhow::Error::from).context("reading the extension's manifest")?;
        serde_json::from_str(&json).context("the extension's manifest is not valid")
    }

    pub async fn start(&self, loaded: Loaded, options: StartOptions) -> anyhow::Result<Arc<Runner>> {
        check_config(&loaded.manifest, &options.config)?;
        let policy = options.network_override.unwrap_or_else(|| loaded.manifest.network.clone());
        let shared = Arc::new(Shared { name: options.mount.clone(), store: options.store, egress: self.egress.clone(), policy });
        let runner = Arc::new(Runner::new(loaded.manifest, loaded.pre, self.engine.clone(), shared, options.source, &options.config, megabytes(options.memory_mb)));
        runner.warm_up().await?;
        Ok(runner)
    }
}

fn compile(engine: &Engine, compile_cache: Option<&Path>, wasm: &Path) -> anyhow::Result<Component> {
    let bytes = std::fs::read(wasm).with_context(|| format!("reading {}", wasm.display()))?;
    let invalid = || format!("{} is not a valid WebAssembly component", wasm.display());
    let Some(cache_dir) = compile_cache else {
        return Component::new(engine, &bytes).map_err(anyhow::Error::from).with_context(invalid);
    };
    let cached = cache_dir.join(format!("{}.cwasm", cache_key(engine, &bytes)));
    if cached.exists() {
        // SAFETY: the file is our own precompiled output, written atomically into the private data folder.
        if let Ok(component) = unsafe { Component::deserialize_file(engine, &cached) } {
            return Ok(component);
        }
    }
    let started = Instant::now();
    let native = engine.precompile_component(&bytes).map_err(anyhow::Error::from).with_context(invalid)?;
    log_debug!("core", "compiled {} in {:?}", wasm.display(), started.elapsed());
    if std::fs::create_dir_all(cache_dir).is_ok() {
        let partial = cached.with_extension("tmp");
        if std::fs::write(&partial, &native).is_ok() {
            let _ = std::fs::rename(&partial, &cached);
        }
    }
    // SAFETY: `native` was produced by `precompile_component` of this same engine just now.
    unsafe { Component::deserialize(engine, &native) }.map_err(anyhow::Error::from)
}

fn cache_key(engine: &Engine, wasm: &[u8]) -> String {
    let engine_hash = BuildHasherDefault::<DefaultHasher>::default().hash_one(engine.precompile_compatibility_hash());
    let mut h = Sha256::new();
    h.update(wasm);
    h.update(engine_hash.to_le_bytes());
    let mut key = format!("{:x}", h.finalize());
    key.truncate(32);
    key
}

fn new_engine(pool_slots: Option<u32>, max_memory_mb: u32) -> anyhow::Result<Engine> {
    let memory = megabytes(max_memory_mb);
    let mut cfg = WasmConfig::new();
    cfg.epoch_interruption(true)
        .memory_reservation(u64::try_from(memory).unwrap_or(u64::MAX))
        .memory_guard_size(64 << 10);
    if let Some(n) = pool_slots {
        let mut pool = PoolingAllocationConfig::default();
        pool.total_component_instances(n).total_core_instances(n * 4).total_memories(n).total_tables(n * 4).total_stacks(n).max_memory_size(memory);
        cfg.allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    }
    Engine::new(&cfg).map_err(anyhow::Error::from)
}

fn check_config(manifest: &Manifest, config: &Value) -> anyhow::Result<()> {
    let Some(schema) = &manifest.config else { return Ok(()) };
    let validator = jsonschema::validator_for(schema).map_err(|e| anyhow::anyhow!("the extension's config schema is invalid: {e}"))?;
    let problems: Vec<String> = validator.iter_errors(config).map(|e| format!("config{}: {e}", e.instance_path().to_string().replace('/', "."))).collect();
    if !problems.is_empty() {
        bail!("invalid config for \"{}\":\n  - {}", manifest.name, problems.join("\n  - "));
    }
    Ok(())
}
