use std::{
    convert::Infallible,
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{Map, Value, json};
use wasmtime::{
    Config, Engine, InstanceAllocationStrategy, PoolingAllocationConfig, Store, StoreLimits, StoreLimitsBuilder, Trap,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

wasmtime::component::bindgen!({ world: "extension", path: "../wit", exports: { default: async } });

use exports::samishisa::proto::handler::{Request as WasmRequest, Response as WasmResponse};

const TICK: Duration = Duration::from_millis(10);
const TIMEOUT_TICKS: u64 = 200;
const MEMORY_LIMIT: usize = 64 << 20;
const MAX_INSTANCES: u32 = 256;

struct State {
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

impl samishisa::proto::host::Host for State {
    fn log(&mut self, msg: String) {
        eprintln!("[ext] {msg}");
    }
}

enum Seg {
    Lit(String),
    Param(String),
}

struct Route {
    method: Method,
    segs: Vec<Seg>,
    params: Option<Value>,
    query: Option<Value>,
}

impl Route {
    fn matches<'a>(&self, parts: &[&'a str]) -> Option<Vec<(&str, &'a str)>> {
        if parts.len() != self.segs.len() {
            return None;
        }
        let mut out = Vec::new();
        for (seg, part) in self.segs.iter().zip(parts) {
            match seg {
                Seg::Lit(l) if l == part => {}
                Seg::Lit(_) => return None,
                Seg::Param(n) => out.push((n.as_str(), *part)),
            }
        }
        Some(out)
    }
}

fn validate(schema: &Value, input: &[(String, String)], where_: &str, out: &mut Map<String, Value>, errors: &mut Vec<String>) {
    let props = schema.get("properties").and_then(Value::as_object);
    let required: Vec<&str> = schema.get("required").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    for (k, _) in input {
        if props.is_none_or(|p| !p.contains_key(k)) {
            errors.push(format!("{where_}.{k}: unknown field"));
        }
    }
    for (name, prop) in props.into_iter().flatten() {
        let given = input.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        let Some(v) = given else {
            if let Some(d) = prop.get("default") {
                out.insert(name.clone(), d.clone());
            } else if required.contains(&name.as_str()) {
                errors.push(format!("{where_}.{name}: required"));
            }
            continue;
        };
        if let Some(min) = prop.get("minLength").and_then(Value::as_u64) {
            if (v.chars().count() as u64) < min {
                errors.push(format!("{where_}.{name}: must be at least {min} characters"));
                continue;
            }
        }
        if let Some(allowed) = prop.get("enum").and_then(Value::as_array) {
            if !allowed.iter().any(|a| a.as_str() == Some(v)) {
                errors.push(format!("{where_}.{name}: must be one of {}", Value::Array(allowed.clone())));
                continue;
            }
        }
        out.insert(name.clone(), Value::String(v.to_owned()));
    }
}

fn percent_decode(s: &str) -> String {
    form_urlencoded::parse(format!("x={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Fresh,
    Reuse,
}

struct App {
    engine: Engine,
    pre: ExtensionPre<State>,
    name: String,
    routes: Vec<Route>,
    mode: Mode,
    pool: Mutex<Vec<(Store<State>, Extension)>>,
    next_id: AtomicU64,
}

type Res = Response<Full<Bytes>>;

fn reply(status: u16, content_type: &str, body: impl Into<Bytes>, id: u64) -> Res {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("x-request-id", format!("{id:x}"))
        .body(Full::new(body.into()))
        .unwrap()
}

fn error(status: u16, code: &str, message: &str, extra: Value, id: u64) -> Res {
    let mut body = json!({ "error": { "status": status, "code": code, "message": message } });
    if let (Some(e), Value::Object(x)) = (body["error"].as_object_mut(), extra) {
        e.extend(x);
    }
    reply(status, "application/json", serde_json::to_vec(&body).unwrap(), id)
}

impl App {
    fn new_store(&self) -> Store<State> {
        let state = State {
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).instances(16).build(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        store.set_epoch_deadline(TIMEOUT_TICKS);
        store.epoch_deadline_trap();
        store
    }

    async fn call(&self, req: WasmRequest) -> wasmtime::Result<WasmResponse> {
        match self.mode {
            Mode::Fresh => {
                let mut store = self.new_store();
                let ext = self.pre.instantiate_async(&mut store).await?;
                ext.samishisa_proto_handler().call_handle(&mut store, &req).await
            }
            Mode::Reuse => {
                let slot = self.pool.lock().unwrap().pop();
                let (mut store, ext) = match slot {
                    Some(s) => s,
                    None => {
                        let mut store = self.new_store();
                        let ext = self.pre.instantiate_async(&mut store).await?;
                        (store, ext)
                    }
                };
                store.set_epoch_deadline(TIMEOUT_TICKS);
                let r = ext.samishisa_proto_handler().call_handle(&mut store, &req).await;
                if r.is_ok() {
                    self.pool.lock().unwrap().push((store, ext));
                }
                r
            }
        }
    }

    async fn handle(&self, req: Request<Incoming>) -> Res {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let path = req.uri().path();
        if path == "/health" {
            return reply(200, "application/json", r#"{"ok":true}"#, id);
        }
        let mut parts = path.trim_matches('/').split('/');
        if parts.next() != Some(self.name.as_str()) {
            return error(404, "not_found", "Unknown extension", json!({ "hint": format!("Try /{}", self.name) }), id);
        }
        let rest: Vec<&str> = parts.collect();
        let rest: &[&str] = if rest == [""] { &[] } else { &rest };

        let mut allowed = Vec::new();
        let found = self.routes.iter().enumerate().find_map(|(i, r)| {
            let m = r.matches(rest)?;
            if r.method != req.method() {
                allowed.push(r.method.as_str());
                return None;
            }
            Some((i, r, m))
        });
        let Some((index, route, raw_params)) = found else {
            if !allowed.is_empty() {
                return error(405, "method_not_allowed", "Method not allowed", json!({ "allow": allowed }), id);
            }
            return error(404, "not_found", "No such route", json!({}), id);
        };

        let params: Vec<(String, String)> = raw_params.into_iter().map(|(k, v)| (k.to_owned(), percent_decode(v))).collect();
        let query: Vec<(String, String)> = form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes()).into_owned().collect();
        let mut input = Map::new();
        let mut errors = Vec::new();
        if let Some(s) = &route.params {
            validate(s, &params, "params", &mut input, &mut errors);
        }
        match &route.query {
            Some(s) => validate(s, &query, "query", &mut input, &mut errors),
            None if !query.is_empty() => errors.push("query: this route takes no query parameters".into()),
            None => {}
        }
        if !errors.is_empty() {
            return error(400, "validation_error", "Invalid request", json!({ "issues": errors, "query": route.query, "params": route.params }), id);
        }

        let wreq = WasmRequest { route: index as u32, input: Value::Object(input).to_string() };
        match self.call(wreq).await {
            Ok(r) => reply(r.status, &r.content_type, r.body, id),
            Err(e) if e.downcast_ref::<Trap>() == Some(&Trap::Interrupt) => error(504, "timeout", "The extension took too long", json!({}), id),
            Err(e) => {
                eprintln!("request {id:x}: extension crashed: {e:?}");
                error(500, "internal_error", "The extension failed", json!({ "requestId": format!("{id:x}") }), id)
            }
        }
    }
}

struct Args {
    wasm: PathBuf,
    port: u16,
    threads: usize,
    mode: Mode,
    reserve_mb: u64,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut a = Args { wasm: PathBuf::new(), port: 3002, threads: 1, mode: Mode::Fresh, reserve_mb: 64 };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().ok_or_else(|| anyhow::anyhow!("{k} needs a value"));
        match k.as_str() {
            "--wasm" => a.wasm = v()?.into(),
            "--port" => a.port = v()?.parse()?,
            "--threads" => a.threads = v()?.parse()?,
            "--reserve-mb" => a.reserve_mb = v()?.parse()?,
            "--mode" => a.mode = if v()? == "reuse" { Mode::Reuse } else { Mode::Fresh },
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    anyhow::ensure!(!a.wasm.as_os_str().is_empty(), "--wasm <file> is required");
    Ok(a)
}

fn engine(reserve_mb: u64) -> anyhow::Result<Engine> {
    let mut pool = PoolingAllocationConfig::default();
    pool.total_component_instances(MAX_INSTANCES)
        .total_core_instances(MAX_INSTANCES * 4)
        .total_memories(MAX_INSTANCES)
        .total_tables(MAX_INSTANCES * 4)
        .total_stacks(MAX_INSTANCES)
        .max_memory_size(MEMORY_LIMIT);
    let mut cfg = Config::new();
    cfg.epoch_interruption(true)
        .memory_reservation(reserve_mb << 20)
        .memory_guard_size(64 << 10)
        .allocation_strategy(InstanceAllocationStrategy::Pooling(pool));
    Ok(Engine::new(&cfg).map_err(anyhow::Error::from)?)
}

fn load_component(engine: &Engine, wasm: &PathBuf) -> anyhow::Result<Component> {
    let cwasm = wasm.with_extension("cwasm");
    if cwasm.exists() {
        let t = Instant::now();
        let c = unsafe { Component::deserialize_file(engine, &cwasm) };
        if let Ok(c) = c {
            eprintln!("loaded precompiled {} in {:?}", cwasm.display(), t.elapsed());
            return Ok(c);
        }
    }
    let bytes = std::fs::read(wasm)?;
    let t = Instant::now();
    let native = engine.precompile_component(&bytes)?;
    eprintln!("compiled {} ({} KB wasm -> {} KB native) in {:?}", wasm.display(), bytes.len() / 1024, native.len() / 1024, t.elapsed());
    std::fs::write(&cwasm, &native)?;
    Ok(unsafe { Component::deserialize(engine, &native)? })
}

fn main() -> anyhow::Result<()> {
    let started = Instant::now();
    let args = parse_args()?;
    let engine = engine(args.reserve_mb)?;
    let component = load_component(&engine, &args.wasm)?;

    let mut linker: Linker<State> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    Extension::add_to_linker::<_, HasSelf<State>>(&mut linker, |s| s)?;
    let pre = ExtensionPre::new(linker.instantiate_pre(&component)?)?;

    {
        let e = engine.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(TICK);
            e.increment_epoch();
        });
    }

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(args.threads).enable_all().build()?;
    rt.block_on(async move {
        let mut app = App { engine, pre, name: String::new(), routes: Vec::new(), mode: args.mode, pool: Mutex::new(Vec::new()), next_id: AtomicU64::new(1) };
        let manifest: Value = {
            let mut store = app.new_store();
            let ext = app.pre.instantiate_async(&mut store).await?;
            serde_json::from_str(&ext.samishisa_proto_handler().call_manifest(&mut store).await?)?
        };
        app.name = manifest["name"].as_str().unwrap_or("ext").to_owned();
        for r in manifest["routes"].as_array().into_iter().flatten() {
            let path = r["path"].as_str().unwrap_or("/");
            app.routes.push(Route {
                method: r["method"].as_str().unwrap_or("GET").parse()?,
                segs: path.trim_matches('/').split('/').filter(|s| !s.is_empty()).map(|s| match s.strip_prefix(':') {
                    Some(p) => Seg::Param(p.to_owned()),
                    None => Seg::Lit(s.to_owned()),
                }).collect(),
                params: r.get("params").cloned(),
                query: r.get("query").cloned(),
            });
        }
        let app = Arc::new(app);

        let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
        let listener = tokio::net::TcpListener::bind(addr).await?;
        eprintln!(
            "ready in {:?}: /{} ({} routes) on http://{addr}, {} thread(s), mode {}",
            started.elapsed(), app.name, app.routes.len(), args.threads, if args.mode == Mode::Fresh { "fresh" } else { "reuse" }
        );
        loop {
            let (stream, _) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let app = app.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let app = app.clone();
                    async move { Ok::<_, Infallible>(app.handle(req).await) }
                });
                let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
            });
        }
        #[allow(unreachable_code)]
        Ok::<_, anyhow::Error>(())
    })
}
