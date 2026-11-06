use std::{
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::Value;

use crate::{
    cache::ResponseCache,
    config::{Config, ExtensionEntry, interpolate_env, listen_address},
    http::{App, Failed, LoadedExtension},
    log::{self, log_error, log_info, log_warn},
    netguard::Egress,
    ratelimit::{Limiter, RateSetting},
    redis::RedisGuard,
    runtime::{Sandbox, StartOptions},
    scaffold,
    sources::{self, Pins},
    store::Stores,
};

pub struct Core {
    sandbox: Sandbox,
    redis: Option<Arc<RedisGuard>>,
    limiter: Arc<Limiter>,
    stores: Arc<Stores>,
    downloader: Egress,
}

impl Core {
    pub async fn start(cfg: &Config) -> anyhow::Result<Arc<Core>> {
        let redis = match &cfg.redis {
            Some(url) => Some(Arc::new(connect_redis(url).await?)),
            None => None,
        };
        let stores = Arc::new(Stores::default());
        stores.delete_expired_every_minute();
        Ok(Arc::new(Core { sandbox: Sandbox::for_serving(cfg)?, limiter: Arc::new(Limiter::new(redis.clone())), redis, stores, downloader: Egress::for_downloads() }))
    }
}

async fn connect_redis(url: &str) -> anyhow::Result<RedisGuard> {
    let redis = RedisGuard::new(url).context("redis URL")?;
    match redis.ping().await {
        Ok(()) => log_info!("core", "redis connected: rate limits and the response cache are shared"),
        Err(e) => log_warn!("core", "redis is not reachable yet ({e}); falling back to per-process state until it is"),
    }
    Ok(redis)
}

pub async fn build_app(core: &Core, cfg: &Config) -> Arc<App> {
    let stores_before = store_names(&cfg.data_dir);
    let mut loaded = Vec::new();
    let mut failed = Vec::new();
    for (index, (entry, wasm)) in sources::resolve_all(cfg, &core.downloader, Pins::Keep).await.into_iter().enumerate() {
        match load_extension(core, cfg, index, &entry, wasm).await {
            Ok(extension) => loaded.push(extension),
            Err(e) => failed.push(Failed { source: entry.source, error: format!("{e:#}") }),
        }
    }
    let cache = ResponseCache::new(core.redis.clone(), cfg.compression);
    let app = App::new(loaded, failed, cfg.clone(), core.limiter.clone(), cache);
    for m in app.mounted() {
        let manifest = &m.runner.manifest;
        let version = manifest.version.as_deref().unwrap_or("");
        log_info!("core", "loaded \"{}\" {version} ({} routes, network: {}) from {}", m.runner.mount, manifest.routes.len(), m.runner.policy().describe(), m.runner.source);
    }
    for f in app.failed() {
        log_error!("core", "{} was not loaded: {}", f.source, f.error);
    }
    warn_about_left_behind_stores(&cfg.data_dir, &stores_before, &app);
    Arc::new(app)
}

fn store_names(data_dir: &Path) -> Vec<String> {
    let entries = std::fs::read_dir(data_dir.join("ext")).into_iter().flatten().flatten();
    entries.filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().into_string().ok()).collect()
}

fn warn_about_left_behind_stores(data_dir: &Path, before: &[String], app: &App) {
    let mounted: Vec<&str> = app.mounted().iter().map(|m| m.runner.mount.as_str()).collect();
    let unused: Vec<&str> = before.iter().map(String::as_str).filter(|name| !mounted.contains(name)).collect();
    if unused.is_empty() {
        return;
    }
    let folder = data_dir.join("ext");
    for new in mounted.iter().filter(|name| !before.iter().any(|b| b == *name)) {
        log_warn!(
            "core",
            "\"{new}\" starts with an empty store, and {} has data no extension is served under now. If you renamed it, stop samishisa and move that folder to {}",
            unused.iter().map(|name| folder.join(name).display().to_string()).collect::<Vec<_>>().join(", "),
            folder.join(new).display()
        );
    }
}

async fn load_extension(core: &Core, cfg: &Config, index: usize, entry: &ExtensionEntry, wasm: anyhow::Result<PathBuf>) -> anyhow::Result<LoadedExtension> {
    let wasm = wasm?;
    let config = interpolate_env(&entry.config, &format!("extensions[{index}].config"))?;
    let loaded = core.sandbox.load(&wasm).await?;
    let mount = entry.name.clone().unwrap_or_else(|| loaded.manifest.name.clone());
    let options = StartOptions {
        store: core.stores.open(&cfg.data_dir, &mount)?,
        mount,
        source: entry.source.clone(),
        config,
        network_override: entry.network.clone(),
        memory_mb: entry.memory_mb,
    };
    let runner = core.sandbox.start(loaded, options).await?;
    Ok(LoadedExtension { runner, operator_rate_limit: entry.rate_limit })
}

#[derive(Clone)]
struct LiveApp(Arc<RwLock<Arc<App>>>);

impl LiveApp {
    fn new(app: Arc<App>) -> Self {
        LiveApp(Arc::new(RwLock::new(app)))
    }
    fn current(&self) -> Arc<App> {
        crate::sync::read(&self.0).clone()
    }
    fn replace(&self, app: Arc<App>) {
        *crate::sync::write(&self.0) = app;
    }
}

async fn accept_until_shutdown(live: LiveApp, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("cannot listen on {addr}"))?;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log_info!("core", "shutting down");
                log::flush();
                return Ok(());
            }
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let _ = stream.set_nodelay(true);
                let live = live.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let app = live.current();
                        async move { Ok::<_, Infallible>(app.handle(request, peer).await) }
                    });
                    let _ = http1::Builder::new()
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(10))
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let terminated = async {
            match signal(SignalKind::terminate()) {
                Ok(mut terminate) => terminate.recv().await,
                Err(e) => {
                    log_warn!("core", "can't listen for SIGTERM ({e}); stop the server with Ctrl-C");
                    std::future::pending().await
                }
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminated => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn tokio_runtime(threads: usize) -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().worker_threads(threads.max(1)).enable_all().build()?)
}

pub struct ServeRequest {
    pub config_path: PathBuf,
    pub config_must_exist: bool,
    pub extra_wasm: Vec<PathBuf>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub threads: Option<usize>,
}

impl ServeRequest {
    fn load_config(&self) -> anyhow::Result<Config> {
        let mut cfg = Config::load(&self.config_path, self.config_must_exist)?;
        if let Some(host) = &self.host {
            cfg.host = host.clone();
        }
        if let Some(port) = self.port {
            cfg.port = port;
        }
        if let Some(threads) = self.threads {
            cfg.threads = threads;
        }
        for wasm in &self.extra_wasm {
            let absolute = std::fs::canonicalize(wasm).with_context(|| format!("{} not found", wasm.display()))?;
            cfg.extensions.push(ExtensionEntry::from_source(absolute.display().to_string()));
        }
        cfg.extensions.retain(|e| e.enabled);
        if self.config_path.exists() {
            log_info!("core", "config: {}", self.config_path.display());
        }
        let no_extensions = cfg.extensions.iter().all(|e| sources::is_server_builtin(&e.source));
        if cfg.extensions.is_empty() && !self.config_path.exists() && sources::has_builtin_hello() {
            log_info!("core", "no config at {}: serving the built-in example (builtin:hello). Run `samishisa add <source>` to start your own.", self.config_path.display());
            cfg.extensions.push(ExtensionEntry::from_source("builtin:hello"));
        } else if no_extensions {
            log_warn!(
                "core",
                "no extensions are enabled: add one with `samishisa add <source>`, switch some on in `samishisa config`, or pass .wasm files (`samishisa run hello.wasm`)"
            );
        }
        Ok(cfg)
    }
}

pub fn serve(request: ServeRequest) -> anyhow::Result<()> {
    let started = Instant::now();
    let cfg = request.load_config()?;
    warn_if_world_readable(&cfg);
    let threads = cfg.serving_threads();
    tokio_runtime(threads)?.block_on(async move {
        let core = Core::start(&cfg).await?;
        let live = LiveApp::new(build_app(&core, &cfg).await);
        let addr = listen_address(&cfg.host, cfg.port)?;
        let app = live.current();
        let failures = if app.failed().is_empty() { String::new() } else { format!(", {} FAILED", app.failed().len()) };
        log_info!("core", "ready in {:?}: {} extension(s){failures} on http://{addr} ({threads} thread(s))", started.elapsed(), app.mounted().len());
        drop(app);
        let _tunnel = crate::tunnel::start(&cfg);
        let reloader = Arc::new(Reloader { live: live.clone(), core, started_with: cfg.clone(), request, one_at_a_time: tokio::sync::Mutex::new(()) });
        #[cfg(unix)]
        tokio::spawn(reload_on_sighup(reloader.clone()));
        if watches_config() {
            tokio::spawn(reload_on_config_change(reloader));
        }
        accept_until_shutdown(live, addr).await
    })
}

pub fn watches_config() -> bool {
    cfg!(not(unix)) || std::env::var("SAMISHISA_WATCH_CONFIG").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

struct Reloader {
    live: LiveApp,
    core: Arc<Core>,
    request: ServeRequest,
    started_with: Config,
    one_at_a_time: tokio::sync::Mutex<()>,
}

impl Reloader {
    async fn reload(&self, why: &str) {
        let _one = self.one_at_a_time.lock().await;
        log_info!("core", "{why}: reloading {}", self.request.config_path.display());
        match self.request.load_config() {
            Ok(cfg) => {
                let app = build_app(&self.core, &cfg).await;
                let count = app.mounted().len();
                self.live.replace(app);
                log_info!("core", "reloaded: {count} extension(s)");
                let skipped = self.started_with.restart_only_changes(&cfg);
                if !skipped.is_empty() {
                    log_warn!("core", "{} changed, but only a restart applies {}", skipped.join(", "), if skipped.len() == 1 { "it" } else { "them" });
                }
            }
            Err(e) => log_error!("core", "reload failed, still serving the previous config: {e:#}"),
        }
    }
}

#[cfg(unix)]
async fn reload_on_sighup(reloader: Arc<Reloader>) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut hangup) = signal(SignalKind::hangup()) else { return };
    while hangup.recv().await.is_some() {
        reloader.reload("SIGHUP").await;
    }
}

async fn reload_on_config_change(reloader: Arc<Reloader>) {
    async fn stamp(path: &Path) -> Option<(Option<SystemTime>, u64)> {
        tokio::fs::metadata(path).await.ok().map(|m| (m.modified().ok(), m.len()))
    }
    let path = reloader.request.config_path.clone();
    log_info!("core", "watching {} for changes", path.display());
    let mut last = stamp(&path).await;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        if stamp(&path).await == last {
            continue;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        last = stamp(&path).await;
        reloader.reload("the config file changed").await;
    }
}

fn warn_if_world_readable(cfg: &Config) {
    #[cfg(unix)]
    if let Some(path) = &cfg.path {
        use std::os::unix::fs::PermissionsExt;
        let text = std::fs::read_to_string(path).unwrap_or_default().to_ascii_lowercase();
        let looks_secret = ["key", "token", "secret", "password"].iter().any(|word| text.contains(word));
        if looks_secret && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o004 != 0) {
            log_warn!("core", "{} is readable by every user and looks like it holds secrets: `chmod 600` it, or use \"${{ENV_VAR}}\" values", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = cfg;
}

pub fn dev(dir: PathBuf, port: Option<u16>, extension_config: Option<Value>) -> anyhow::Result<()> {
    anyhow::ensure!(dir.join("Cargo.toml").exists(), "{} has no Cargo.toml (run `samishisa new <name>` first)", dir.display());
    if std::env::var("LOG_LEVEL").is_err() {
        log::set_min_level(log::Level::Debug);
    }
    let wasm = scaffold::build(&dir)?;
    let mut cfg = Config { base_dir: dir.clone(), data_dir: dir.join("target/samishisa-dev"), rate_limit: RateSetting::Off, ..Config::default() };
    cfg.network.allow_private = true;
    cfg.describe_routes = true;
    if let Some(port) = port.or_else(|| std::env::var("PORT").ok().and_then(|p| p.parse().ok())) {
        cfg.port = port;
    }
    let mut entry = ExtensionEntry::from_source(wasm.display().to_string());
    if let Some(config) = extension_config {
        entry.config = config;
    }
    cfg.extensions = vec![entry, ExtensionEntry::from_source("builtin:docs")];

    tokio_runtime(2)?.block_on(async move {
        let core = Core::start(&cfg).await?;
        let live = LiveApp::new(build_app(&core, &cfg).await);
        let addr = listen_address("127.0.0.1", cfg.port)?;
        log_info!("dev", "serving {} on http://{addr} (docs: http://{addr}/docs); rebuilding on every change in {}", wasm.display(), dir.display());
        tokio::spawn(rebuild_on_change(live.clone(), core, cfg, dir));
        accept_until_shutdown(live, addr).await
    })
}

async fn rebuild_on_change(live: LiveApp, core: Arc<Core>, cfg: Config, dir: PathBuf) {
    let mut last_change = newest_change(&dir);
    loop {
        tokio::time::sleep(Duration::from_millis(600)).await;
        let change = newest_change(&dir);
        if change <= last_change {
            continue;
        }
        last_change = change;
        log_info!("dev", "change detected, rebuilding...");
        let crate_dir = dir.clone();
        match tokio::task::spawn_blocking(move || scaffold::build(&crate_dir)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                log_error!("dev", "build failed (still serving the previous version): {e:#}");
                continue;
            }
            Err(e) => {
                log_error!("dev", "{e}");
                continue;
            }
        }
        let app = build_app(&core, &cfg).await;
        match app.failed().first() {
            None => {
                live.replace(app);
                log_info!("dev", "reloaded");
            }
            Some(failure) => log_error!("dev", "the new build doesn't load (still serving the previous one): {}", failure.error),
        }
    }
}

fn newest_change(dir: &Path) -> SystemTime {
    fn walk(path: &Path, newest: &mut SystemTime) {
        let Ok(meta) = std::fs::metadata(path) else { return };
        if meta.is_dir() {
            for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
                walk(&entry.path(), newest);
            }
        } else if let Ok(modified) = meta.modified() {
            *newest = (*newest).max(modified);
        }
    }
    let mut newest = SystemTime::UNIX_EPOCH;
    for part in ["src", "Cargo.toml", "wit"] {
        walk(&dir.join(part), &mut newest);
    }
    newest
}
