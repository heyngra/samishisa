use samishisa::prelude::*;

#[schema]
struct Config {
    #[serde(default)]
    label: String,
}

#[schema]
struct FetchQuery {
    url: String,
}

#[schema]
struct Fetched {
    status: u16,
    body: String,
}

fn fetch(Query(q): Query<FetchQuery>, ctx: Ctx<Config>) -> Result<Json<Fetched>> {
    let r = ctx.http.get(&q.url).send()?;
    Ok(Json(Fetched { status: r.status(), body: r.text().chars().take(200).collect() }))
}

#[schema]
struct Counter {
    n: i64,
}

fn cached(ctx: Ctx<Config>) -> Json<Counter> {
    Json(Counter { n: ctx.store.incr("cached-calls", 1) })
}

fn big() -> Json<Vec<String>> {
    Json((0..2000).map(|i| format!("item number {i}")).collect())
}

#[schema]
struct Kv {
    value: String,
}

fn put(Path(key): Path<String>, Json(kv): Json<Kv>, ctx: Ctx<Config>) -> Result<()> {
    ctx.store.set(&key, &kv.value)?;
    Ok(())
}

fn get(Path(key): Path<String>, ctx: Ctx<Config>) -> Result<Json<String>> {
    ctx.store.get(&key).map(Json).ok_or_else(|| ApiError::not_found(format!("no key {key:?}")))
}

#[schema]
struct Spin {
    #[serde(default)]
    ms: u64,
}

fn spin(Query(q): Query<Spin>) -> &'static str {
    let start = std::time::Instant::now();
    loop {
        if q.ms > 0 && start.elapsed().as_millis() as u64 >= q.ms {
            return "done";
        }
        std::hint::black_box(());
    }
}

fn oom() -> &'static str {
    let mut v: Vec<Vec<u8>> = Vec::new();
    loop {
        v.push(vec![1u8; 1 << 20]);
    }
}

fn panics() -> &'static str {
    panic!("probe panicked on purpose")
}

fn ticks(ctx: Ctx<Config>) -> Json<Counter> {
    Json(Counter { n: ctx.store.get("ticks").unwrap_or(0) })
}

fn extension() -> Extension<Config> {
    Extension::new("probe")
        .network(["127.0.0.1", "localhost", "example.com"])
        .every(Duration::from_secs(1), |ctx: &Ctx<Config>| {
            ctx.store.incr("ticks", 1);
            Ok(())
        })
        .get("/fetch", fetch)
        .get("/cached", cached)
            .cache(60)
        .get("/big", big)
        .put("/kv/:key", put)
        .get("/kv/:key", get)
        .get("/spin", spin)
            .timeout(Duration::from_millis(500))
        .get("/oom", oom)
        .get("/panic", panics)
        .get("/limited", || "ok")
            .rate_limit(RateLimit::per_minute(3))
        .get("/free", || "ok")
            .no_rate_limit()
        .get("/ticks", ticks)
}

samishisa::export!(extension);
