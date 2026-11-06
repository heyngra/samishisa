//! Write extensions for the [samishisa](https://github.com/heyngra/samishisa) API server.
//!
//! An extension is a small Rust crate that compiles to a WebAssembly component. The server loads it, mounts its routes under `/<name>/...`, and handles the rest: routing, input validation, rate limits, caching, generated docs, timeouts and sandboxing. Your handler only sees requests that already passed validation.
//!
//! # A complete extension
//!
//! ```no_run
//! use samishisa::prelude::*;
//!
//! /// Settings the operator can set in `api.config.json`.
//! #[schema]
//! struct Config {
//!     /// How to greet people.
//!     #[serde(default = "default_greeting")]
//!     greeting: String,
//! }
//! fn default_greeting() -> String { "Hello".into() }
//!
//! #[schema]
//! struct GreetQuery {
//!     /// Uppercase the greeting.
//!     #[serde(default)]
//!     shout: bool,
//! }
//!
//! #[schema]
//! struct Greeting { message: String }
//!
//! fn greet(Path(name): Path<String>, Query(q): Query<GreetQuery>, ctx: Ctx<Config>) -> Json<Greeting> {
//!     let message = format!("{}, {name}!", ctx.config.greeting);
//!     Json(Greeting { message: if q.shout { message.to_uppercase() } else { message } })
//! }
//!
//! fn extension() -> Extension<Config> {
//!     Extension::new("hello")
//!         .get("/:name", greet).summary("Greet someone")
//!         .get("/visits", |ctx: Ctx<Config>| Json(ctx.store.incr("visits", 1)))
//!             .summary("Count visits")
//! }
//!
//! samishisa::export!(extension);
//! ```
//!
//! Build it with `cargo build --release --target wasm32-wasip2` (or `samishisa build`), then add the resulting `.wasm` to the server's config.
//!
//! # How a request flows
//!
//! 1. The server matches `GET /hello/Ada?shout=true` to the `"/:name"` route.
//! 2. It converts and validates the path params, query and JSON body against the schemas generated from your types (`#[schema]`). Bad input gets a `400` that lists every problem, and your handler never runs.
//! 3. Handler arguments are *extractors* ([`Path`], [`Query`], [`Json`], [`Ctx`], [`Headers`]), in any combination and order.
//! 4. The return value becomes the response ([`IntoResponse`]): [`Json`], a [`Response`], a `String`, `()` (204), or a [`Result`] whose error is an [`ApiError`].
//!
//! # Errors
//!
//! Return [`ApiError`] for expected failures, e.g. `ApiError::not_found("No such city")`. The caller gets your status, message and hint in a consistent JSON envelope. Any other error converts into `ApiError` with `?`: the caller sees an opaque `500` with a request id, and the details go to the server log. A panic is handled the same way, and the instance that panicked is discarded.
//!
//! # The sandbox
//!
//! Extensions can't open files or sockets, or read environment variables. They reach the outside world only through [`Ctx`]: [`Ctx::store`], [`Ctx::log`] and [`Ctx::http`], the last one limited to the hosts you declare with [`Extension::network`]. The server sets a time limit per call and a memory limit per instance.
//!
//! Handlers are plain synchronous functions. A call that waits, such as an HTTP request or a store read, pauses only its own instance. Several instances of an extension can exist at once, so keep state that must be shared in [`Ctx::store`], not in statics.

#![deny(missing_docs)]
#![cfg_attr(doctest, doc = include_str!("../../../README.md"))]
#![warn(rustdoc::broken_intra_doc_links)]

#[doc(hidden)]
pub mod __rt;
mod ctx;
mod error;
mod extension;
mod extract;
mod handler;
mod response;

pub use ctx::{Ctx, Http, HttpRequestBuilder, HttpResponse, Log, NoConfig, Store};
pub use error::{ApiError, Result};
pub use extension::{Config, Extension, Method, RateLimit, RouteSpec};
pub use extract::{FromRequest, Headers, Json, Path, Query, RequestParts};
pub use handler::Handler;
pub use response::{IntoResponse, Response};
pub use samishisa_macros::schema;

/// Everything most extensions need: `use samishisa::prelude::*;`
pub mod prelude {
    pub use crate::{ApiError, Ctx, Extension, Headers, IntoResponse, Json, Path, Query, RateLimit, Response, Result, schema};
    pub use std::time::Duration;
}

/// Re-exports used by [`macro@schema`] and [`export!`]. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde;
    pub use serde_json;
}

/// The WIT bindings (generated). Not part of the public API.
#[doc(hidden)]
pub mod __bindings {
    wit_bindgen::generate!({
        world: "plugin",
        path: "wit",
        pub_export_macro: true,
        export_macro_name: "__export_plugin",
        default_bindings_module: "samishisa::__bindings",
    });
}

/// Turns a function that returns your [`Extension`] into the component's entry point. Put it once, at the end of `lib.rs`:
///
/// ```ignore
/// fn extension() -> Extension<Config> { Extension::new("weather") }
/// samishisa::export!(extension);
/// ```
///
/// `version` and `description` come from `Cargo.toml`, unless the builder sets [`Extension::version`] or [`Extension::about`].
#[macro_export]
macro_rules! export {
    ($build:path) => {
        #[doc(hidden)]
        pub struct __SamishisaExtension;

        const _: () = {
            use $crate::__bindings::exports::samishisa::extension::extension as __ext;
            use $crate::__bindings::samishisa::extension::types as __types;

            fn __build() -> ::std::boxed::Box<dyn $crate::__rt::Erased> {
                ::std::boxed::Box::new($crate::__rt::Built::new(
                    $build(),
                    env!("CARGO_PKG_VERSION"),
                    env!("CARGO_PKG_DESCRIPTION"),
                ))
            }

            impl __ext::Guest for __SamishisaExtension {
                fn manifest() -> ::std::string::String {
                    $crate::__rt::manifest(__build)
                }
                fn init(config_json: ::std::string::String) -> ::std::result::Result<(), ::std::string::String> {
                    $crate::__rt::init(__build, &config_json)
                }
                fn setup() -> ::std::result::Result<(), ::std::string::String> {
                    $crate::__rt::setup(__build)
                }
                fn handle(req: __types::Request) -> ::std::result::Result<__types::Response, __types::ApiError> {
                    $crate::__rt::handle(__build, req)
                }
                fn run_job(job: u32) -> ::std::result::Result<(), ::std::string::String> {
                    $crate::__rt::run_job(__build, job)
                }
            }
        };

        $crate::__bindings::__export_plugin!(__SamishisaExtension with_types_in $crate::__bindings);
    };
}
