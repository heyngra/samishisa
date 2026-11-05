use samishisa::prelude::*;

#[schema]
struct Config {
    #[serde(default = "default_greeting")]
    greeting: String,
}

fn default_greeting() -> String {
    "Hello".into()
}

#[schema]
struct GreetQuery {
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

#[schema]
struct Echo {
    text: String,
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
    Ok(println!("{n}"))
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
