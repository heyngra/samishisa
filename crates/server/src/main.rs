mod cache;
mod clientip;
mod clock;
mod commands;
mod compression;
mod config;
mod http;
mod ipnet;
mod jobs;
mod log;
mod manifest;
mod netguard;
mod ratelimit;
mod redis;
mod runtime;
mod scaffold;
mod serve;
mod sources;
mod store;

use std::{path::PathBuf, str::FromStr};

use anyhow::Context;
use serde_json::Value;

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
  --host ADDR      listen address (default 0.0.0.0 = every interface; 127.0.0.1 = this machine only)
  --threads N      serving threads (default: one per CPU core)

Files: ./api.config.json (settings), ./api.lock.json (pinned downloads), ./data/ (extension stores,
downloads, compiled code; move it with dataDir in the config or $SAMISHISA_DATA).
Sources: ./ext.wasm, ./crate-folder, builtin:hello, github:owner/repo[#tag], https://.../ext.wasm
Environment: API_CONFIG, PORT, REDIS_URL, SAMISHISA_DATA, LOG_LEVEL (debug|info|warn|error), GITHUB_TOKEN
Reload: send SIGHUP to re-read the config without dropping connections.";

fn main() {
    log::init_from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(all: Vec<String>) -> anyhow::Result<()> {
    let command = all.first().map(String::as_str).unwrap_or("serve");
    let mut args = Args(all.get(1..).unwrap_or_default().to_vec());
    match command {
        "help" | "--help" | "-h" => println!("{HELP}"),
        "version" | "--version" | "-V" => println!("samishisa {}", env!("CARGO_PKG_VERSION")),
        "serve" => serve_command(args)?,
        "dev" => {
            let port = args.number("--port")?;
            let extension_config = args.json("--config-json")?;
            let dir = commands::dir_or_current(args.positional()?.first());
            serve::dev(dir, port, extension_config)?;
        }
        "init" => commands::init(&commands::dir_or_current(args.positional()?.first()))?,
        "add" => {
            let (config_path, _) = args.config_path()?;
            let name = args.flag("--name")?;
            let extension_config = args.json("--config-json")?;
            let [source] = args.positional()?.try_into().map_err(|_| anyhow::anyhow!("usage: samishisa add <SOURCE> [--name N] [--config-json JSON]"))?;
            commands::add(&config_path, &source, name, extension_config)?;
        }
        "update" => {
            let (config_path, must_exist) = args.config_path()?;
            args.positional()?;
            commands::update(&config_path, must_exist)?;
        }
        "new" => {
            let sdk = args.flag("--sdk")?.map(PathBuf::from);
            let [name] = args.positional()?.try_into().map_err(|_| anyhow::anyhow!("usage: samishisa new <name> [--sdk PATH]"))?;
            commands::new(&name, sdk.as_deref())?;
        }
        "build" => commands::build(&commands::dir_or_current(args.positional()?.first()))?,
        "inspect" => {
            let [wasm] = args.positional()?.try_into().map_err(|_| anyhow::anyhow!("usage: samishisa inspect <EXT.wasm>"))?;
            commands::inspect(&PathBuf::from(wasm), true)?;
        }
        "healthcheck" => {
            let port = args.number("--port")?.or_else(|| std::env::var("PORT").ok().and_then(|p| p.parse().ok())).unwrap_or(3000);
            commands::healthcheck(port);
        }
        _ => serve_command(Args(all.clone()))?,
    }
    Ok(())
}

fn serve_command(mut args: Args) -> anyhow::Result<()> {
    let (config_path, config_must_exist) = args.config_path()?;
    let request = serve::ServeRequest {
        config_path,
        config_must_exist,
        host: args.flag("--host")?,
        port: args.number("--port")?,
        threads: args.number("--threads")?,
        extra_wasm: args.positional()?.into_iter().map(PathBuf::from).collect(),
    };
    serve::serve(request)
}

struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, name: &str) -> anyhow::Result<Option<String>> {
        let Some(i) = self.0.iter().position(|a| a == name) else { return Ok(None) };
        anyhow::ensure!(i + 1 < self.0.len(), "{name} needs a value");
        let value = self.0.remove(i + 1);
        self.0.remove(i);
        Ok(Some(value))
    }

    fn number<T: FromStr>(&mut self, name: &str) -> anyhow::Result<Option<T>> {
        self.flag(name)?.map(|v| v.parse::<T>().map_err(|_| anyhow::anyhow!("{name} must be a number (got {v:?})"))).transpose()
    }

    fn json(&mut self, name: &str) -> anyhow::Result<Option<Value>> {
        self.flag(name)?.map(|v| serde_json::from_str(&v).with_context(|| format!("{name} must be JSON"))).transpose()
    }

    fn config_path(&mut self) -> anyhow::Result<(PathBuf, bool)> {
        Ok(match self.flag("--config")?.or_else(|| std::env::var("API_CONFIG").ok()) {
            Some(path) => (PathBuf::from(path), true),
            None => (PathBuf::from("api.config.json"), false),
        })
    }

    fn positional(self) -> anyhow::Result<Vec<String>> {
        if let Some(unknown) = self.0.iter().find(|a| a.starts_with("--")) {
            anyhow::bail!("unknown option {unknown} (see `samishisa help`)");
        }
        Ok(self.0)
    }
}
