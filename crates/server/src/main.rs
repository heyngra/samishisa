mod config;
mod http;
mod log;
mod runtime;
mod scaffold;

use std::{
    convert::Infallible,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;

use crate::{
    config::{Config, Entry, interpolate_env},
    http::{App, Failed},
    log::{log_error, log_info, log_warn},
    runtime::Runner,
};

const HELP: &str = "samishisa: one server, many sandboxed WebAssembly extensions

Usage:
  samishisa [serve] [EXT.wasm ...]   serve the extensions in ./api.config.json (and any given)
  samishisa new <name> [--sdk PATH]  create an extension crate in ./<name>
  samishisa build [DIR]              compile the extension in DIR (default .) to a .wasm
  samishisa inspect <EXT.wasm>       show an extension's routes and settings
  samishisa version | help

Serve options:
  --config FILE    config file (default ./api.config.json, or $API_CONFIG)
  --port N         listen port (default 3000, or $PORT)
  --threads N      serving threads (default: one per CPU core)

Environment: API_CONFIG, PORT, LOG_LEVEL (debug|info|warn|error)";

fn main() {
    log::init_from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("help" | "--help" | "-h") => {
            println!("{HELP}");
            Ok(())
        }
        Some("version" | "--version" | "-V") => {
            println!("samishisa {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("new") => cmd_new(&args[1..]),
        Some("build") => cmd_build(&args[1..]),
        Some("inspect") => cmd_inspect(&args[1..]),
        Some("serve") => cmd_serve(&args[1..]),
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

fn cmd_new(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let sdk = take_flag(&mut args, "--sdk")?;
    let [name] = args.as_slice() else { anyhow::bail!("usage: samishisa new <name> [--sdk PATH]") };
    let dir = scaffold::new(name, Path::new("."), sdk.as_deref().map(Path::new))?;
    println!("created {}\n\nNext:\n  cd {}\n  samishisa build\n  samishisa serve target/wasm32-wasip2/release/{}.wasm", dir.display(), dir.display(), name.replace('-', "_"));
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
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async {
        let engine = runtime::engine(false)?;
        let linker = runtime::linker(&engine)?;
        let cache = std::env::temp_dir().join("samishisa-inspect");
        let runner = Runner::load(&engine, &linker, wasm, &cache, None, serde_json::json!({}), wasm.display().to_string()).await;
        let runner = match runner {
            Ok(r) => r,
            Err(e) if format!("{e:#}").contains("invalid config") => {
                println!("note: {e:#}");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let m = &runner.manifest;
        println!("{} {}{}", m.name, m.version.as_deref().unwrap_or("?"), m.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default());
        for r in &m.routes {
            println!("  {:<6} /{}{:<24} {}", r.method, m.name, r.path, r.summary.as_deref().unwrap_or(""));
        }
        if verbose {
            println!("network: {}", m.network.as_ref().map(|n| n.to_string()).unwrap_or_else(|| "none".into()));
            if !m.jobs.is_empty() {
                println!("jobs: {}", m.jobs.iter().map(|j| format!("every {}s", j.every_ms / 1000)).collect::<Vec<_>>().join(", "));
            }
            if let Some(c) = &m.config {
                println!("config schema: {c}");
            }
        }
        Ok(())
    })
}

fn cmd_serve(args: &[String]) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut args = args.to_vec();
    let config_path = take_flag(&mut args, "--config")?.or_else(|| std::env::var("API_CONFIG").ok());
    let port = take_flag(&mut args, "--port")?;
    let threads = take_flag(&mut args, "--threads")?;
    let mut cfg = Config::load(Path::new(config_path.as_deref().unwrap_or("api.config.json")), config_path.is_some())?;
    if let Some(p) = port {
        cfg.port = p.parse().context("--port must be a number")?;
    }
    if let Some(t) = threads {
        cfg.threads = t.parse().context("--threads must be a number")?;
    }
    for a in &args {
        anyhow::ensure!(!a.starts_with("--"), "unknown option {a} (see `samishisa help`)");
        let abs = std::fs::canonicalize(a).with_context(|| format!("{a} not found"))?;
        cfg.extensions.push(Entry::Source(abs.display().to_string()));
    }
    if cfg.extensions.is_empty() {
        log_warn!("core", "no extensions configured: add some to api.config.json, or pass .wasm files (`samishisa serve hello.wasm`)");
    }

    let threads = if cfg.threads == 0 { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) } else { cfg.threads };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(threads).enable_all().build()?;
    rt.block_on(async move {
        let engine = runtime::engine(true)?;
        let linker = runtime::linker(&engine)?;
        let cache = cfg.data_dir.join("cache");

        let mut runners = Vec::new();
        let mut failed = Vec::new();
        for (i, entry) in cfg.extensions.iter().enumerate() {
            let source = entry.source().to_owned();
            let loaded = async {
                let config = interpolate_env(&entry.config(), &format!("extensions[{i}].config"))?;
                Runner::load(&engine, &linker, &cfg.resolve(&source), &cache, entry.alias(), config, source.clone()).await
            }
            .await;
            match loaded {
                Ok(r) => {
                    log_info!("core", "loaded \"{}\" {} ({} routes) from {source}", r.mount, r.manifest.version.as_deref().unwrap_or(""), r.manifest.routes.len());
                    runners.push(r);
                }
                Err(e) => {
                    log_error!("core", "extension #{i} ({source}) failed to load: {e:#}");
                    failed.push(Failed { source, error: format!("{e:#}") });
                }
            }
        }

        let app = Arc::new(App::new(runners, failed, Duration::from_millis(cfg.request_timeout_ms), cfg.max_body_bytes)?);
        for f in app.failed().iter().filter(|f| f.error.contains("is already used") || f.error.contains("is reserved")) {
            log_error!("core", "{} not mounted: {}", f.source, f.error);
        }
        start_jobs(&app);

        let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse().context("host/port")?;
        let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("cannot listen on {addr}"))?;
        let n_ok = app.mounted().count();
        log_info!(
            "core",
            "ready in {:?}: {n_ok} extension(s){} on http://{addr} ({threads} thread(s)). Store is in memory for now (not persisted)",
            started.elapsed(),
            if app.failed().is_empty() { String::new() } else { format!(", {} FAILED", app.failed().len()) }
        );

        let shutdown = tokio::signal::ctrl_c();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => {
                    log_info!("core", "shutting down");
                    return Ok(());
                }
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let _ = stream.set_nodelay(true);
                    let app = app.clone();
                    tokio::spawn(async move {
                        let svc = service_fn(move |req| {
                            let app = app.clone();
                            async move { Ok::<_, Infallible>(app.handle(req).await) }
                        });
                        let _ = http1::Builder::new().timer(hyper_util::rt::TokioTimer::new()).header_read_timeout(Duration::from_secs(10)).serve_connection(TokioIo::new(stream), svc).await;
                    });
                }
            }
        }
    })
}

fn start_jobs(app: &Arc<App>) {
    for m in app.mounted() {
        for (i, job) in m.runner.manifest.jobs.iter().enumerate() {
            let runner = m.runner.clone();
            let every = Duration::from_millis(job.every_ms.max(1000));
            tokio::spawn(async move {
                let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tick.tick().await;
                    match runner.handle_job(i as u32).await {
                        Ok(()) => {}
                        Err(msg) => log_warn!(&runner.mount, "job {i} failed: {msg}"),
                    }
                }
            });
        }
    }
}

impl Runner {
    async fn handle_job(&self, index: u32) -> Result<(), String> {
        match self.run_job(index).await {
            Ok(r) => r,
            Err(e) => Err(runtime::call_error_text(&e)),
        }
    }
}
