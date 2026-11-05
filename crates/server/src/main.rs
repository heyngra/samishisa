mod cache;
mod clientip;
mod config;
mod http;
mod log;
mod netguard;
mod openapi;
mod ratelimit;
mod redis;
mod runtime;
mod scaffold;
mod sources;
mod store;

use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant, SystemTime},
};

use anyhow::Context;
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

use crate::{
    cache::ResponseCache,
    config::{Config, Entry, FullEntry, interpolate_env},
    http::{App, Failed, Settings},
    log::{log_error, log_info, log_warn},
    netguard::Egress,
    ratelimit::{Limiter, MemoryLimiter, Setting},
    redis::RedisGuard,
    runtime::{LoadOptions, Runner},
    sources::{Lock, Resolver},
    store::Store,
};

const HELP: &str = "samishisa: one server, many sandboxed WebAssembly extensions

Usage:
  samishisa [serve] [EXT.wasm ...]     serve ./api.config.json (plus any .wasm files given)
  samishisa init [DIR]                 create an api.config.json to start from
  samishisa add <SOURCE> [--name N]    add an extension to api.config.json
  samishisa update                     re-resolve remote extensions and move their pins (api.lock.json)
  samishisa new <NAME> [--sdk PATH]    create an extension crate in ./<NAME>
  samishisa build [DIR]                compile the extension crate in DIR (default .)
  samishisa dev [DIR] [--config-json JSON]  build, serve, and rebuild + reload on every change
  samishisa inspect <EXT.wasm>         show an extension's routes and settings
  samishisa healthcheck                exit 0 if the local server answers /health (for Docker)
  samishisa version | help

Options:
  --config FILE    config file (default ./api.config.json, or $API_CONFIG)
  --port N         listen port (default 3000, or $PORT)
  --threads N      serving threads (default: one per CPU core)

Sources: ./ext.wasm, ./crate-folder, builtin:hello, github:owner/repo[#tag], https://.../ext.wasm
Environment: API_CONFIG, PORT, REDIS_URL, SAMISHISA_DATA, LOG_LEVEL (debug|info|warn|error), GITHUB_TOKEN
Reload: send SIGHUP to re-read the config without dropping connections.";

fn main() {
    log::init_from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("help" | "--help" | "-h") => {
            println!("{HELP}");
            Ok(())
        }
        Some("version" | "--version" | "-V") => {
            println!("samishisa {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("new") => cmd_new(rest),
        Some("build") => cmd_build(rest),
        Some("inspect") => cmd_inspect(rest),
        Some("init") => cmd_init(rest),
        Some("add") => cmd_add(rest),
        Some("update") => cmd_update(rest),
        Some("healthcheck") => cmd_healthcheck(rest),
        Some("dev") => cmd_dev(rest),
        Some("serve") => cmd_serve(rest),
        _ => cmd_serve(&args),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> anyhow::Result<Option<String>> {
    match args.iter().position(|a| a == flag) {
        Some(i) => {
            anyhow::ensure!(i + 1 < args.len(), "{flag} needs a value");
            let v = args.remove(i + 1);
            args.remove(i);
            Ok(Some(v))
        }
        None => Ok(None),
    }
}

fn config_path(args: &mut Vec<String>) -> anyhow::Result<(PathBuf, bool)> {
    let explicit = take_flag(args, "--config")?.or_else(|| std::env::var("API_CONFIG").ok());
    Ok(match explicit {
        Some(p) => (PathBuf::from(p), true),
        None => (PathBuf::from("api.config.json"), false),
    })
}

fn runtime(threads: usize) -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().worker_threads(threads.max(1)).enable_all().build()?)
}

struct Core {
    engine: wasmtime::Engine,
    linker: wasmtime::component::Linker<runtime::State>,
    limiter: Arc<Limiter>,
    redis: Option<Arc<RedisGuard>>,
    egress: Arc<Egress>,
    downloader: Egress,
    stores: Mutex<HashMap<PathBuf, Arc<Store>>>,
}

impl Core {
    async fn new(cfg: &Config, pooled: bool) -> anyhow::Result<Arc<Core>> {
        let engine = runtime::engine(pooled.then_some(cfg.max_instances), cfg.max_memory_mb())?;
        let linker = runtime::linker(&engine)?;
        let redis = match &cfg.redis {
            Some(url) => {
                let g = Arc::new(RedisGuard::new(url).context("redis URL")?);
                match g.ping().await {
                    Ok(()) => log_info!("core", "redis connected: rate limits and the response cache are shared"),
                    Err(e) => log_warn!("core", "redis is not reachable yet ({e}); falling back to per-process state until it is"),
                }
                Some(g)
            }
            None => None,
        };
        let limiter = Arc::new(match &redis {
            Some(g) => Limiter::Redis { guard: g.clone(), fallback: MemoryLimiter::default() },
            None => Limiter::Memory(MemoryLimiter::default()),
        });
        let mut downloader = Egress::new(true);
        downloader.max_bytes = 64 << 20;
        downloader.timeout = Duration::from_secs(120);
        Ok(Arc::new(Core { engine, linker, limiter, redis, egress: Arc::new(Egress::new(cfg.network.allow_private)), downloader, stores: Mutex::default() }))
    }

    fn store(&self, path: PathBuf) -> anyhow::Result<Arc<Store>> {
        let mut stores = self.stores.lock().unwrap();
        if let Some(s) = stores.get(&path) {
            return Ok(s.clone());
        }
        let s = Arc::new(Store::open(Some(&path))?);
        stores.insert(path, s.clone());
        Ok(s)
    }
}

fn lock_path(cfg: &Config) -> PathBuf {
    cfg.base_dir.join("api.lock.json")
}

async fn resolve_all(core: &Core, cfg: &Config, update: bool) -> Vec<(FullEntry, anyhow::Result<PathBuf>)> {
    let lock_file = lock_path(cfg);
    let mut lock = match Lock::read(&lock_file) {
        Ok(l) => l,
        Err(e) => {
            log_error!("core", "{e:#}");
            Lock::default()
        }
    };
    let before = lock.clone();
    let mut out = Vec::new();
    {
        let mut r = Resolver { data_dir: &cfg.data_dir, lock: &mut lock, update, downloader: &core.downloader };
        for e in &cfg.extensions {
            let full = e.full();
            let res = r.resolve(&full, |p| cfg.resolve_path(p)).await;
            out.push((full, res));
        }
    }
    if lock != before {
        lock.version = 1;
        if let Err(e) = lock.write(&lock_file) {
            log_warn!("core", "could not save {} ({e:#}): extensions loaded, but their versions are not pinned", lock_file.display());
        }
    }
    out
}

async fn build_app(core: &Arc<Core>, cfg: &Config) -> anyhow::Result<Arc<App>> {
    let mut runners = Vec::new();
    let mut failed = Vec::new();
    for (i, (entry, resolved)) in resolve_all(core, cfg, false).await.into_iter().enumerate() {
        let source = entry.source.clone();
        let loaded = async {
            let wasm = resolved?;
            let config = interpolate_env(entry.config.as_ref().unwrap_or(&json!({})), &format!("extensions[{i}].config"))?;
            let operator_rate = Setting::from_value(entry.rate_limit.as_ref(), &format!("extensions[{i}].rateLimit"))?;
            let (core2, data_dir) = (core.clone(), cfg.data_dir.clone());
            let opts = LoadOptions {
                alias: entry.name.clone(),
                config,
                source: source.clone(),
                open_store: Box::new(move |mount| core2.store(data_dir.join("ext").join(mount).join("store.db"))),
                egress: core.egress.clone(),
                network: entry.network.clone(),
                memory_mb: entry.memory_mb.unwrap_or(config::DEFAULT_MEMORY_MB),
            };
            let runner = Runner::load(&core.engine, &core.linker, &wasm, &cfg.data_dir.join("cache"), opts).await?;
            anyhow::Ok((runner, operator_rate))
        }
        .await;
        match loaded {
            Ok((r, rate)) => {
                log_info!("core", "loaded \"{}\" {} ({} routes, network: {}) from {source}", r.mount, r.manifest.version.as_deref().unwrap_or(""), r.manifest.routes.len(), r.policy().describe());
                runners.push((r, rate));
            }
            Err(e) => {
                log_error!("core", "extension #{i} ({source}) failed to load: {e:#}");
                failed.push(Failed { source, error: format!("{e:#}") });
            }
        }
    }
    let settings = Settings {
        default_timeout: Duration::from_millis(cfg.request_timeout_ms),
        max_body: cfg.max_body_bytes,
        default_rate: cfg.rate_setting()?,
        trust: cfg.trust()?,
        cors: cfg.cors,
        compression: cfg.compression,
        access_log: cfg.access_log,
        title: cfg.title.clone().unwrap_or_else(|| "samishisa".into()),
    };
    let cache = ResponseCache::new(5000, core.redis.clone(), cfg.compression);
    let app = Arc::new(App::new(runners, failed, settings, core.limiter.clone(), cache)?);
    for f in app.failed().iter().filter(|f| f.error.contains("is already used") || f.error.contains("is reserved")) {
        log_error!("core", "{} not mounted: {}", f.source, f.error);
    }
    start_jobs(&app);
    Ok(app)
}

fn start_jobs(app: &Arc<App>) {
    let mut handles = Vec::new();
    for m in app.mounted() {
        for (i, job) in m.runner.manifest.jobs.iter().enumerate() {
            let runner = m.runner.clone();
            let every = Duration::from_millis(job.every_ms.max(1000));
            handles.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    let r = match runner.run_job(i as u32).await {
                        Ok(r) => r,
                        Err(e) => Err(runtime::call_error_text(&e)),
                    };
                    if let Err(msg) = r {
                        log_warn!(&runner.mount, "job {i} failed: {msg}");
                    }
                }
            }));
        }
    }
    app.jobs.lock().unwrap().extend(handles);
}

type Current = Arc<RwLock<Arc<App>>>;

async fn serve_loop(current: Current, addr: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("cannot listen on {addr}"))?;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                log_info!("core", "shutting down");
                return Ok(());
            }
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let _ = stream.set_nodelay(true);
                let current = current.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |req| {
                        let app = current.read().unwrap().clone();
                        async move { Ok::<_, Infallible>(app.handle(req, peer).await) }
                    });
                    let _ = http1::Builder::new()
                        .timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(10))
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

fn prune_stores(core: &Arc<Core>) {
    let core = core.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let stores: Vec<Arc<Store>> = core.stores.lock().unwrap().values().cloned().collect();
            for s in stores {
                s.prune();
            }
        }
    });
}

fn summary(app: &App, started: Instant, addr: SocketAddr, threads: usize) {
    let n_ok = app.mounted().count();
    log_info!(
        "core",
        "ready in {:?}: {n_ok} extension(s){} on http://{addr} ({threads} thread(s)); docs at /docs",
        started.elapsed(),
        if app.failed().is_empty() { String::new() } else { format!(", {} FAILED", app.failed().len()) }
    );
}

fn cmd_serve(args: &[String]) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut args = args.to_vec();
    let (path, must_exist) = config_path(&mut args)?;
    let port = take_flag(&mut args, "--port")?;
    let threads = take_flag(&mut args, "--threads")?;
    let mut cfg = Config::load(&path, must_exist)?;
    if let Some(p) = port {
        cfg.port = p.parse().context("--port must be a number")?;
    }
    if let Some(t) = threads {
        cfg.threads = t.parse().context("--threads must be a number")?;
    }
    let extra: Vec<String> = args.clone();
    for a in &extra {
        anyhow::ensure!(!a.starts_with("--"), "unknown option {a} (see `samishisa help`)");
        let abs = std::fs::canonicalize(a).with_context(|| format!("{a} not found"))?;
        cfg.extensions.push(Entry::Source(abs.display().to_string()));
    }
    if cfg.extensions.is_empty() && !path.exists() && !sources::BUILTIN_HELLO.is_empty() {
        log_info!("core", "no {} here: serving the built-in example (builtin:hello). Run `samishisa init` to create a config.", path.display());
        cfg.extensions.push(Entry::Source("builtin:hello".into()));
    } else if cfg.extensions.is_empty() {
        log_warn!("core", "no extensions configured: run `samishisa init`, add some to api.config.json, or pass .wasm files (`samishisa serve hello.wasm`)");
    }
    warn_if_world_readable(&cfg);

    let threads = if cfg.threads == 0 { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) } else { cfg.threads };
    runtime(threads)?.block_on(async move {
        let core = Core::new(&cfg, true).await?;
        let current: Current = Arc::new(RwLock::new(build_app(&core, &cfg).await?));
        prune_stores(&core);
        let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse().context("host/port")?;
        summary(&current.read().unwrap(), started, addr, threads);

        #[cfg(unix)]
        {
            let (current, core, path) = (current.clone(), core.clone(), path.clone());
            tokio::spawn(async move {
                let Ok(mut hup) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) else { return };
                while hup.recv().await.is_some() {
                    log_info!("core", "SIGHUP: reloading {}", path.display());
                    let reloaded = async {
                        let mut cfg = Config::load(&path, must_exist)?;
                        for a in &extra {
                            cfg.extensions.push(Entry::Source(std::fs::canonicalize(a)?.display().to_string()));
                        }
                        build_app(&core, &cfg).await
                    }
                    .await;
                    match reloaded {
                        Ok(app) => {
                            let n = app.mounted().count();
                            *current.write().unwrap() = app;
                            log_info!("core", "reloaded: {n} extension(s)");
                        }
                        Err(e) => log_error!("core", "reload failed, still serving the previous config: {e:#}"),
                    }
                }
            });
        }
        serve_loop(current, addr).await
    })
}

fn warn_if_world_readable(cfg: &Config) {
    #[cfg(unix)]
    if let Some(p) = &cfg.path {
        use std::os::unix::fs::PermissionsExt;
        let text = std::fs::read_to_string(p).unwrap_or_default().to_ascii_lowercase();
        let looks_secret = ["key", "token", "secret", "password"].iter().any(|w| text.contains(w));
        if looks_secret && std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o004 != 0) {
            log_warn!("core", "{} is readable by every user and looks like it holds secrets: `chmod 600` it, or use \"${{ENV_VAR}}\" values", p.display());
        }
    }
    let _ = cfg;
}

fn cmd_dev(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let port = take_flag(&mut args, "--port")?;
    let ext_config = take_flag(&mut args, "--config-json")?.map(|j| serde_json::from_str::<Value>(&j)).transpose().context("--config-json must be JSON")?;
    let dir = PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    anyhow::ensure!(dir.join("Cargo.toml").exists(), "{} has no Cargo.toml (run `samishisa new <name>` first)", dir.display());
    if std::env::var("LOG_LEVEL").is_err() {
        unsafe { std::env::set_var("LOG_LEVEL", "debug") };
        log::init_from_env();
    }
    let wasm = scaffold::build(&dir)?;
    let mut cfg = Config::default();
    cfg.base_dir = dir.clone();
    cfg.data_dir = dir.join("target/samishisa-dev");
    cfg.network.allow_private = true;
    cfg.rate_limit = Value::Bool(false);
    if let Some(p) = port.or_else(|| std::env::var("PORT").ok()) {
        cfg.port = p.parse().context("--port must be a number")?;
    }
    cfg.extensions = vec![Entry::Full(FullEntry { source: wasm.display().to_string(), config: ext_config, ..Default::default() })];
    runtime(2)?.block_on(async move {
        let core = Core::new(&cfg, false).await?;
        let current: Current = Arc::new(RwLock::new(build_app(&core, &cfg).await?));
        let addr: SocketAddr = format!("127.0.0.1:{}", cfg.port).parse()?;
        log_info!("dev", "serving {} on http://{addr} (docs: http://{addr}/docs); rebuilding on every change in {}", wasm.display(), dir.display());
        {
            let (current, core, cfg, dir) = (current.clone(), core.clone(), cfg.clone(), dir.clone());
            tokio::spawn(async move {
                let mut seen = newest_mtime(&dir);
                loop {
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let now = newest_mtime(&dir);
                    if now <= seen {
                        continue;
                    }
                    seen = now;
                    log_info!("dev", "change detected, rebuilding...");
                    let d = dir.clone();
                    match tokio::task::spawn_blocking(move || scaffold::build(&d)).await {
                        Ok(Ok(_)) => match build_app(&core, &cfg).await {
                            Ok(app) if app.failed().is_empty() => {
                                *current.write().unwrap() = app;
                                log_info!("dev", "reloaded");
                            }
                            Ok(app) => log_error!("dev", "the new build doesn't load (still serving the previous one): {}", app.failed()[0].error),
                            Err(e) => log_error!("dev", "{e:#}"),
                        },
                        Ok(Err(e)) => log_error!("dev", "build failed (still serving the previous version): {e:#}"),
                        Err(e) => log_error!("dev", "{e}"),
                    }
                }
            });
        }
        serve_loop(current, addr).await
    })
}

fn newest_mtime(dir: &Path) -> SystemTime {
    fn walk(p: &Path, best: &mut SystemTime) {
        let Ok(meta) = std::fs::metadata(p) else { return };
        if meta.is_dir() {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                walk(&e.path(), best);
            }
        } else if let Ok(m) = meta.modified() {
            *best = (*best).max(m);
        }
    }
    let mut best = SystemTime::UNIX_EPOCH;
    for sub in ["src", "Cargo.toml", "wit"] {
        walk(&dir.join(sub), &mut best);
    }
    best
}

fn cmd_new(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let sdk = take_flag(&mut args, "--sdk")?;
    let [name] = args.as_slice() else { anyhow::bail!("usage: samishisa new <name> [--sdk PATH]") };
    let dir = scaffold::new(name, Path::new("."), sdk.as_deref().map(Path::new))?;
    println!(
        "created {}\n\nNext:\n  cd {}\n  samishisa dev          # build, serve on http://localhost:3000, rebuild on every change\n  samishisa build        # just compile: target/wasm32-wasip2/release/{}.wasm",
        dir.display(),
        dir.display(),
        name.replace('-', "_")
    );
    Ok(())
}

fn cmd_build(args: &[String]) -> anyhow::Result<()> {
    let dir = PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    let wasm = scaffold::build(&dir)?;
    let size = std::fs::metadata(&wasm).map(|m| m.len()).unwrap_or(0);
    println!("built {} ({} KB)", wasm.display(), size / 1024);
    inspect(&wasm, false)
}

fn cmd_inspect(args: &[String]) -> anyhow::Result<()> {
    let [path] = args else { anyhow::bail!("usage: samishisa inspect <EXT.wasm>") };
    inspect(Path::new(path), true)
}

fn inspect(wasm: &Path, verbose: bool) -> anyhow::Result<()> {
    runtime(1)?.block_on(async {
        let engine = runtime::engine(None, config::DEFAULT_MEMORY_MB)?;
        let linker = runtime::linker(&engine)?;
        let cache = std::env::temp_dir().join("samishisa-inspect");
        let opts = LoadOptions {
            alias: None,
            config: json!({}),
            source: wasm.display().to_string(),
            open_store: Box::new(|_| Ok(Arc::new(Store::open(None)?))),
            egress: Arc::new(Egress::new(false)),
            network: None,
            memory_mb: config::DEFAULT_MEMORY_MB,
        };
        let runner = match Runner::load(&engine, &linker, wasm, &cache, opts).await {
            Ok(r) => r,
            Err(e) if format!("{e:#}").contains("invalid config") => {
                println!("note: {e:#}\n(expected without the operator's config; the component itself loads fine)");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let m = &runner.manifest;
        println!("{} {}{}", m.name, m.version.as_deref().unwrap_or("?"), m.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default());
        for r in &m.routes {
            println!("  {:<6} /{}{:<28} {}", r.method, m.name, r.path, r.summary.as_deref().unwrap_or(""));
        }
        if verbose {
            println!("network: {}", runner.policy().describe());
            if !m.jobs.is_empty() {
                println!("jobs: {}", m.jobs.iter().map(|j| format!("every {}s", j.every_ms / 1000)).collect::<Vec<_>>().join(", "));
            }
            if let Some(c) = &m.config {
                println!("config schema: {}", serde_json::to_string_pretty(c)?);
            }
        }
        Ok(())
    })
}

fn cmd_init(args: &[String]) -> anyhow::Result<()> {
    let dir = PathBuf::from(args.first().map(String::as_str).unwrap_or("."));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("api.config.json");
    if path.exists() {
        println!("kept     {} (already exists)", path.display());
    } else {
        let hello = if sources::BUILTIN_HELLO.is_empty() { "github:heyngra/samishisa" } else { "builtin:hello" };
        let cfg = json!({ "port": 3000, "extensions": [{ "source": hello, "config": { "greeting": "Hello" } }] });
        std::fs::write(&path, serde_json::to_string_pretty(&cfg)? + "\n")?;
        println!("created  {}", path.display());
    }
    println!(
        "\nNext:\n  {}samishisa                        # then open http://localhost:3000/docs, or: curl localhost:3000/hello/world\n  samishisa add github:owner/repo  # add more extensions\n  samishisa new my-extension       # write your own",
        if dir == Path::new(".") { String::new() } else { format!("cd {} && ", dir.display()) }
    );
    Ok(())
}

fn cmd_add(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let (path, _) = config_path(&mut args)?;
    let name = take_flag(&mut args, "--name")?;
    let ext_config = take_flag(&mut args, "--config-json")?.map(|j| serde_json::from_str::<Value>(&j)).transpose().context("--config-json must be JSON")?;
    let [source] = args.as_slice() else { anyhow::bail!("usage: samishisa add <SOURCE> [--name N] [--config-json JSON]") };
    sources::parse(source, |p| PathBuf::from(p))?;
    let mut raw: Value = if path.exists() { serde_json::from_str(&std::fs::read_to_string(&path)?).with_context(|| format!("{} is not valid JSON", path.display()))? } else { json!({ "port": 3000 }) };
    let list = raw.as_object_mut().context("the config must be a JSON object")?.entry("extensions").or_insert_with(|| json!([]));
    let list = list.as_array_mut().context("\"extensions\" must be a list")?;
    if list.iter().any(|e| e.as_str() == Some(source) || e.get("source").and_then(Value::as_str) == Some(source)) {
        anyhow::bail!("{source} is already in {}", path.display());
    }
    list.push(match (name, ext_config) {
        (None, None) => Value::String(source.clone()),
        (n, c) => serde_json::to_value(FullEntry { source: source.clone(), name: n, config: c, ..Default::default() })?,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&raw)? + "\n")?;
    println!("added {source} to {}. Restart samishisa (or send it SIGHUP) to load it.", path.display());
    Ok(())
}

fn cmd_update(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let (path, must_exist) = config_path(&mut args)?;
    let cfg = Config::load(&path, must_exist)?;
    runtime(1)?.block_on(async {
        let core = Core::new(&cfg, false).await?;
        let mut failed = 0;
        for (entry, r) in resolve_all(&core, &cfg, true).await {
            match r {
                Ok(p) => println!("ok      {}  ->  {}", entry.source, p.display()),
                Err(e) => {
                    failed += 1;
                    println!("FAILED  {}: {e:#}", entry.source);
                }
            }
        }
        println!("pins saved in {}", lock_path(&cfg).display());
        if failed > 0 {
            std::process::exit(1);
        }
        Ok(())
    })
}

fn cmd_healthcheck(args: &[String]) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    let mut args = args.to_vec();
    let port = take_flag(&mut args, "--port")?.or_else(|| std::env::var("PORT").ok()).unwrap_or_else(|| "3000".into());
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let ok = (|| -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /health HTTP/1.0\r\nhost: localhost\r\n\r\n")?;
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf)?;
        Ok(buf[..n].starts_with(b"HTTP/1.1 200") || buf[..n].starts_with(b"HTTP/1.0 200"))
    })()
    .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 });
}
