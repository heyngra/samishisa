use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use serde_json::{Value, json};

use crate::{
    config::{self, Config},
    manifest::Manifest,
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
        "\nNext:\n  {cd}samishisa run                    # then: curl localhost:3000/hello/world\n  samishisa add github:owner/repo  # add more extensions\n  samishisa add builtin:docs       # serve interactive API docs at /docs (with \"describeRoutes\": true)\n  samishisa new my-extension       # write your own"
    );
    Ok(())
}

pub fn add(config_path: &Path, source: &str, name: Option<String>, extension_config: Option<Value>) -> anyhow::Result<()> {
    let base_dir = config_path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    sources::parse(source, base_dir)?;
    let source = &local_source_as_absolute(source);
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
    save_config(config_path, &cfg)?;
    println!("added {source} to {}. {}", config_path.display(), apply_hint(false));
    Ok(())
}

pub(crate) fn local_source_as_absolute(source: &str) -> String {
    let remote = source.contains(':') && !Path::new(source).is_absolute() && !source.as_bytes().get(1).is_some_and(|c| *c == b':');
    if remote || source.starts_with("~/") {
        return source.to_owned();
    }
    std::path::absolute(source).map(|p| p.display().to_string()).unwrap_or_else(|_| source.to_owned())
}

pub(crate) fn load_config_json(path: &Path) -> anyhow::Result<Value> {
    if !path.exists() {
        return Ok(json!({ "port": 3000 }));
    }
    serde_json::from_str(&std::fs::read_to_string(path)?).with_context(|| format!("{} is not valid JSON", path.display()))
}

pub(crate) fn apply_hint(server: bool) -> &'static str {
    if server {
        "Restart samishisa to apply."
    } else if cfg!(not(unix)) {
        "A running samishisa picks it up within a few seconds."
    } else {
        "Restart samishisa (or send it SIGHUP) to apply."
    }
}

pub(crate) fn save_config(path: &Path, cfg: &Value) -> anyhow::Result<()> {
    Config::validate(cfg).context("not saved, the config would not load")?;
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(cfg)? + "\n")?;
    Ok(())
}

pub(crate) fn entry_name(entry: &Value) -> String {
    if let Some(name) = entry.get("name").and_then(Value::as_str) {
        return name.to_owned();
    }
    let source = entry.as_str().or_else(|| entry.get("source").and_then(Value::as_str)).unwrap_or("?");
    let source = source.split(['#', '?']).next().unwrap_or(source);
    let last = source.rsplit(['/', '\\', ':']).next().unwrap_or(source);
    last.strip_suffix(".wasm").unwrap_or(last).to_owned()
}

fn extensions_of(cfg: &mut Value) -> anyhow::Result<&mut Vec<Value>> {
    cfg.as_object_mut().context("the config must be a JSON object")?.entry("extensions").or_insert_with(|| json!([])).as_array_mut().context("\"extensions\" must be a list")
}

pub fn list(config_path: &Path) -> anyhow::Result<()> {
    let mut cfg = load_config_json(config_path)?;
    let entries = extensions_of(&mut cfg)?;
    println!("config: {}{}", config_path.display(), if config_path.exists() { "" } else { " (not created yet)" });
    if entries.is_empty() {
        println!("no extensions. Add one with `samishisa add <source>`.");
    }
    for entry in entries.iter() {
        let source = entry.as_str().or_else(|| entry.get("source").and_then(Value::as_str)).unwrap_or("?");
        let mut notes = Vec::new();
        if entry.get("enabled").and_then(Value::as_bool) == Some(false) {
            notes.push("off");
        }
        if entry.get("config").is_some() {
            notes.push("has settings");
        }
        let notes = if notes.is_empty() { String::new() } else { format!("   ({})", notes.join(", ")) };
        println!("  {:<16} {source}{notes}", entry_name(entry));
    }
    Ok(())
}

pub fn remove(config_path: &Path, name: &str) -> anyhow::Result<()> {
    let mut cfg = load_config_json(config_path)?;
    let index = find_entry(&cfg, config_path, name)?;
    extensions_of(&mut cfg)?.remove(index);
    save_config(config_path, &cfg)?;
    println!("removed {name} from {}. {}", config_path.display(), apply_hint(false));
    Ok(())
}

pub(crate) fn manifests(doc: &Value, config_path: &Path, only: Option<usize>) -> anyhow::Result<Vec<(String, anyhow::Result<Manifest>)>> {
    let mut cfg = Config::from_value(doc.clone(), config_path)?;
    if let Some(i) = only {
        cfg.extensions = cfg.extensions.into_iter().skip(i).take(1).collect();
    }
    single_thread_runtime()?.block_on(async {
        let sandbox = Sandbox::for_inspecting()?;
        let mut out = Vec::new();
        let mut resolved = sources::resolve_all(&cfg, &Egress::for_downloads(), Pins::Keep).await.into_iter();
        for entry in &cfg.extensions {
            if sources::is_server_builtin(&entry.source) {
                out.push((entry.source.clone(), Ok(server_builtin_manifest())));
                continue;
            }
            let Some((entry, wasm)) = resolved.next() else { break };
            let manifest = match wasm {
                Ok(path) => sandbox.load(&path).await.map(|loaded| loaded.manifest),
                Err(e) => Err(e),
            };
            out.push((entry.source, manifest));
        }
        Ok(out)
    })
}

fn server_builtin_manifest() -> Manifest {
    Manifest {
        name: "docs".into(),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        description: Some(
            "The interactive API docs page at /docs (needs \"describeRoutes\": true). It loads its viewer (Scalar) from cdn.jsdelivr.net in the visitor's browser.".into(),
        ),
        ..Manifest::default()
    }
}

fn find_entry(cfg: &Value, config_path: &Path, name: &str) -> anyhow::Result<usize> {
    let entries = cfg.get("extensions").and_then(Value::as_array).cloned().unwrap_or_default();
    if let Some(i) = entries.iter().position(|e| entry_name(e) == name) {
        return Ok(i);
    }
    let served: Vec<Option<String>> = match manifests(cfg, config_path, None) {
        Ok(list) if !entries.is_empty() => list.into_iter().map(|(_, m)| m.ok().map(|m| m.name)).collect(),
        _ => vec![None; entries.len()],
    };
    if let Some(i) = (0..entries.len()).find(|&i| entries[i].get("name").is_none() && served.get(i).and_then(Option::as_deref) == Some(name)) {
        return Ok(i);
    }
    let known: Vec<String> = entries
        .iter()
        .zip(served.iter().chain(std::iter::repeat(&None)))
        .map(|(e, s)| match s {
            Some(s) if *s != entry_name(e) && e.get("name").is_none() => format!("{} (served as {s})", entry_name(e)),
            _ => entry_name(e),
        })
        .collect();
    anyhow::bail!("no extension named {name} in {} (have: {})", config_path.display(), if known.is_empty() { "none".into() } else { known.join(", ") })
}

fn extension_value(doc: &Value, config_path: &Path, index: usize, key: &str, value: &str) -> anyhow::Result<Value> {
    use crate::tui::schema::{Kind, fields_of, parse_input};
    let path: Vec<&str> = key.split('.').collect();
    let entry = &doc["extensions"][index];
    let existing = path.iter().try_fold(entry.get("config").unwrap_or(&Value::Null), |node, part| node.get(*part));
    let kind = match existing {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Some(Kind::Integer),
        Some(Value::Number(_)) => Some(Kind::Number),
        Some(Value::Bool(_)) => Some(Kind::Bool),
        Some(Value::String(_)) => Some(Kind::Text),
        _ => manifests(doc, config_path, Some(index))
            .ok()
            .and_then(|mut list| list.pop())
            .and_then(|(_, manifest)| manifest.ok()?.config)
            .and_then(|schema| fields_of(&schema).into_iter().find(|f| f.label() == key).map(|f| f.kind)),
    };
    match kind {
        Some(kind) => parse_input(&kind, value).map_err(|why| anyhow::anyhow!("{key}: {why}")),
        None => Ok(match serde_json::from_str::<Value>(value) {
            Ok(Value::Number(_)) | Err(_) => Value::String(value.to_owned()),
            Ok(json) => json,
        }),
    }
}

pub fn config(config_path: &Path, args: Vec<String>) -> anyhow::Result<()> {
    let mut args = args.into_iter();
    let sub = args.next();
    if sub.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin()) && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return crate::tui::run(config_path);
    }
    match sub.as_deref() {
        None | Some("path") => {
            println!("{}{}", config_path.display(), if config_path.exists() { "" } else { "   (not created yet)" });
            if sub.is_none() {
                println!(
                    "\nChange it with:\n  samishisa config                              (in a terminal) the config screen\n  samishisa config edit                         open the file in your editor\n  samishisa config show                         print it\n  samishisa config set port 8080                a server setting\n  samishisa config set mio anilist.client_id 123   an extension's setting\n  samishisa add <source> / remove <name> / list"
                );
            }
        }
        Some("show") => print!("{}", if config_path.exists() { std::fs::read_to_string(config_path)? } else { "{}\n".into() }),
        Some("edit") => {
            if !config_path.exists() {
                save_config(config_path, &json!({ "port": 3000, "extensions": [] }))?;
            }
            open_in_editor(config_path)?;
        }
        Some("set") => {
            let rest: Vec<String> = args.collect();
            let (extension, key, value) = match rest.as_slice() {
                [key, value] => (None, key, value),
                [extension, key, value] => (Some(extension.as_str()), key, value),
                _ => anyhow::bail!(
                    "usage: samishisa config set [EXTENSION] KEY VALUE   (KEY may be dotted: anilist.client_id; VALUE is text; true/false/null, {{..}} and [..] are read as JSON)"
                ),
            };
            let mut cfg = load_config_json(config_path)?;
            if extension.is_none() && cfg.get("extensions").and_then(Value::as_array).is_some_and(|list| list.iter().any(|e| entry_name(e) == *key)) {
                anyhow::bail!("{key} is an extension: set one of its settings with `samishisa config set {key} SETTING VALUE`");
            }
            let index = match extension {
                Some(name) => Some(find_entry(&cfg, config_path, name)?),
                None => None,
            };
            let parsed = match index {
                Some(i) => extension_value(&cfg, config_path, i, key, value)?,
                None => serde_json::from_str::<Value>(value).unwrap_or_else(|_| Value::String(value.clone())),
            };
            let target: &mut Value = match index {
                None => &mut cfg,
                Some(i) => {
                    let entries = extensions_of(&mut cfg)?;
                    if let Some(source) = entries[i].as_str().map(str::to_owned) {
                        entries[i] = json!({ "source": source });
                    }
                    let entry = &mut entries[i];
                    let entry = entry.as_object_mut().with_context(|| format!("extension entry {i} must be a source string or an object"))?;
                    entry.entry("config").or_insert_with(|| json!({}))
                }
            };
            let mut node = target;
            let parts: Vec<&str> = key.split('.').collect();
            for (n, part) in parts.iter().enumerate() {
                let object = node.as_object_mut().with_context(|| format!("{key}: {} is not an object", parts[..n].join(".")))?;
                if n + 1 == parts.len() {
                    object.insert((*part).to_owned(), parsed.clone());
                    break;
                }
                node = object.entry((*part).to_owned()).or_insert_with(|| json!({}));
            }
            save_config(config_path, &cfg)?;
            println!("set {} in {}. {}", extension.map(|e| format!("{e}.{key}")).unwrap_or_else(|| key.clone()), config_path.display(), apply_hint(extension.is_none()));
        }
        Some(other) => anyhow::bail!("unknown `config` command {other:?}: use path, show, edit or set"),
    }
    Ok(())
}

fn open_in_editor(path: &Path) -> anyhow::Result<()> {
    let chosen = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).ok().filter(|e| !e.trim().is_empty());
    let candidates: Vec<String> = match chosen {
        Some(editor) => vec![editor],
        None if cfg!(windows) => vec!["notepad".into()],
        None => vec!["nano".into(), "vi".into()],
    };
    for editor in candidates {
        let mut words = editor.split_whitespace();
        let Some(program) = words.next() else { continue };
        match std::process::Command::new(program).args(words).arg(path).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => anyhow::bail!("{editor} exited with {status}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("running {editor}")),
        }
    }
    anyhow::bail!("no editor found: set $EDITOR, or open {} yourself", path.display())
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
                let jobs: Vec<String> =
                    manifest.jobs.iter().map(|j| if j.every_ms % 1000 == 0 { format!("every {}s", j.every_ms / 1000) } else { format!("every {}ms", j.every_ms) }).collect();
                println!("jobs: {}", jobs.join(", "));
            }
            if let Some(schema) = &manifest.config {
                println!("config schema: {}", serde_json::to_string_pretty(schema)?);
            }
        }
        Ok(())
    })
}

pub fn healthcheck(config_path: &Path, port: Option<u16>) -> ! {
    let cfg = Config::load(config_path, false).unwrap_or_default();
    let port = port.or_else(|| std::env::var("PORT").ok()?.parse().ok()).unwrap_or(cfg.port);
    let addr: SocketAddr = config::local_address(&cfg.host, port);
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
