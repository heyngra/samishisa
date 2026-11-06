//! A small extension that shows every SDK feature. Copy the folder to start your own, or run `samishisa new <name>`.

use samishisa::prelude::*;

/// Settings the operator can set in `api.config.json`:
/// `{ "source": "./hello.wasm", "config": { "greeting": "Hey" } }`
#[schema]
struct Config {
    /// The word used to greet people.
    #[serde(default = "default_greeting")]
    greeting: String,
}

fn default_greeting() -> String {
    "Hello".into()
}

/// Query string of `GET /hello/:name`.
#[schema]
struct GreetQuery {
    /// Uppercase the greeting.
    #[serde(default)]
    shout: bool,
}

#[schema]
struct Greeting {
    message: String,
}

fn greet(Path(name): Path<String>, Query(q): Query<GreetQuery>, ctx: Ctx<Config>) -> Json<Greeting> {
    let message = format!("{}, {name}!", ctx.config.greeting);
    Json(Greeting { message: if q.shout { message.to_uppercase() } else { message } })
}

/// Body of `POST /hello/echo`.
#[schema]
struct Echo {
    text: String,
    /// How many times to repeat it (1 to 10).
    #[serde(default = "one")]
    #[schemars(range(min = 1, max = 10))]
    repeat: u8,
}

fn one() -> u8 {
    1
}

fn echo(Json(body): Json<Echo>) -> Json<Vec<String>> {
    Json(vec![body.text; body.repeat as usize])
}

#[schema]
struct Visits {
    visits: i64,
}

fn visits(ctx: Ctx<Config>) -> Json<Visits> {
    Json(Visits { visits: ctx.store.incr("visits", 1) })
}

#[schema]
struct Time {
    /// Milliseconds since the Unix epoch, when this response was generated.
    now_ms: u128,
}

fn time() -> Result<Json<Time>> {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    Ok(Json(Time { now_ms }))
}

fn teapot() -> Result<()> {
    Err(ApiError::new(418, "I'm a teapot").code("teapot").hint("Ask for coffee elsewhere."))
}

fn boom() -> Result<()> {
    let n: u32 = "secret internal detail that must not leak".parse()?;
    println!("{n}");
    Ok(())
}

fn extension() -> Extension<Config> {
    Extension::new("hello")
        .setup(|ctx: &Ctx<Config>| {
            ctx.log.info(format!("ready, greeting with {:?}", ctx.config.greeting));
            Ok(())
        })
        .get("/:name", greet)
        .summary("Greet someone")
        .post("/echo", echo)
        .summary("Echo a JSON body back")
        .get("/visits", visits)
        .summary("Persistent counter (uses ctx.store)")
        .no_rate_limit()
        .get("/time", time)
        .summary("Server time, cached for 5 seconds for all callers")
        .cache(5)
        .get("/teapot", teapot)
        .summary("Shows how to return an expected error")
        .get("/boom", boom)
        .summary("Fails unexpectedly (the server turns it into a clean 500)")
}

samishisa::export!(extension);
