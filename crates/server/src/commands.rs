use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use serde_json::{Value, json};

use crate::{
    config::Config,
    netguard::Egress,
    runtime::Sandbox,
    scaffold,
    sources::{self, Pins},
};

fn single_thread_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

pub fn init(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join("api.config.json");
    if path.exists() {
        println!("kept     {} (already exists)", path.display());
    } else {
        let hello = if sources::has_builtin_hello() { "builtin:hello" } else { "github:heyngra/samishisa" };
        let cfg = json!({ "port": 3000, "extensions": [{ "source": hello, "config": { "greeting": "Hello" } }] });
        std::fs::write(&path, serde_json::to_string_pretty(&cfg)? + "\n")?;
        println!("created  {}", path.display());
    }
    let cd = if dir == Path::new(".") { String::new() } else { format!("cd {} && ", dir.display()) };
    println!(
        "\nNext:\n  {cd}samishisa                        # then open http://localhost:3000/docs, or: curl localhost:3000/hello/world\n  samishisa add github:owner/repo  # add more extensions\n  samishisa new my-extension       # write your own"
    );
    Ok(())
}

pub fn add(config_path: &Path, source: &str, name: Option<String>, extension_config: Option<Value>) -> anyhow::Result<()> {
    let base_dir = config_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    sources::parse(source, base_dir)?;
    let mut cfg: Value = if config_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(config_path)?).with_context(|| format!("{} is not valid JSON", config_path.display()))?
    } else {
        json!({ "port": 3000 })
    };
    let list = cfg.as_object_mut().context("the config must be a JSON object")?.entry("extensions").or_insert_with(|| json!([]));
    let list = list.as_array_mut().context("\"extensions\" must be a list")?;
    let has_source = |entry: &Value| entry.as_str() == Some(source) || entry.get("source").and_then(Value::as_str) == Some(source);
    if list.iter().any(has_source) {
        anyhow::bail!("{source} is already in {}", config_path.display());
    }
    list.push(match (name, extension_config) {
        (None, None) => json!(source),
        (name, config) => {
            let mut entry = json!({ "source": source });
            if let Some(name) = name {
                entry["name"] = json!(name);
            }
            if let Some(config) = config {
                entry["config"] = config;
            }
            entry
        }
    });
    std::fs::write(config_path, serde_json::to_string_pretty(&cfg)? + "\n")?;
    println!("added {source} to {}. Restart samishisa (or send it SIGHUP) to load it.", config_path.display());
    Ok(())
}

pub fn update(config_path: &Path, must_exist: bool) -> anyhow::Result<()> {
    let cfg = Config::load(config_path, must_exist)?;
    let failures = single_thread_runtime()?.block_on(async {
        let mut failures = 0;
        for (entry, resolved) in sources::resolve_all(&cfg, &Egress::for_downloads(), Pins::Update).await {
            match resolved {
                Ok(path) => println!("ok      {}  ->  {}", entry.source, path.display()),
                Err(e) => {
                    failures += 1;
                    println!("FAILED  {}: {e:#}", entry.source);
                }
            }
        }
        failures
    });
    println!("pins saved in {}", sources::lock_path(&cfg).display());
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

pub fn new(name: &str, sdk: Option<&Path>) -> anyhow::Result<()> {
    let dir = scaffold::new(name, Path::new("."), sdk)?;
    println!(
        "created {dir}\n\nNext:\n  cd {dir}\n  samishisa dev          # build, serve on http://localhost:3000, rebuild on every change\n  samishisa build        # just compile: target/wasm32-wasip2/release/{crate_name}.wasm",
        dir = dir.display(),
        crate_name = name.replace('-', "_")
    );
    Ok(())
}

pub fn build(dir: &Path) -> anyhow::Result<()> {
    let wasm = scaffold::build(dir)?;
    let size = std::fs::metadata(&wasm).map(|m| m.len()).unwrap_or(0);
    println!("built {} ({} KB)", wasm.display(), size / 1024);
    inspect(&wasm, false)
}

pub fn inspect(wasm: &Path, verbose: bool) -> anyhow::Result<()> {
    single_thread_runtime()?.block_on(async {
        let manifest = Sandbox::for_inspecting()?.load(wasm).await?.manifest;
        let description = manifest.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default();
        println!("{} {}{description}", manifest.name, manifest.version.as_deref().unwrap_or("?"));
        for route in &manifest.routes {
            println!("  {:<6} {:<34} {}", route.method, route.full_path(&manifest.name), route.summary.as_deref().unwrap_or(""));
        }
        if verbose {
            println!("network: {}", manifest.network.describe());
            if !manifest.jobs.is_empty() {
                let jobs: Vec<String> = manifest.jobs.iter().map(|j| format!("every {}s", j.every_ms / 1000)).collect();
                println!("jobs: {}", jobs.join(", "));
            }
            if let Some(schema) = &manifest.config {
                println!("config schema: {}", serde_json::to_string_pretty(schema)?);
            }
        }
        Ok(())
    })
}

pub fn healthcheck(port: u16) -> ! {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let healthy = (|| -> std::io::Result<bool> {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.write_all(b"GET /health HTTP/1.0\r\nhost: localhost\r\n\r\n")?;
        let mut start = [0u8; 16];
        let n = stream.read(&mut start)?;
        Ok(start[..n].starts_with(b"HTTP/1.1 200") || start[..n].starts_with(b"HTTP/1.0 200"))
    })()
    .unwrap_or(false);
    std::process::exit(if healthy { 0 } else { 1 })
}

pub fn dir_or_current(arg: Option<&String>) -> PathBuf {
    PathBuf::from(arg.map(String::as_str).unwrap_or("."))
}
