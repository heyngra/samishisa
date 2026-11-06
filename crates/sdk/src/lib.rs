#![deny(missing_docs)]
#![cfg_attr(doctest, doc = include_str!("../../../README.md"))]
#![warn(rustdoc::broken_intra_doc_links)]

mod ctx;
mod error;
mod extension;
mod extract;
mod handler;
mod response;
#[doc(hidden)]
pub mod __rt;

pub use ctx::{Ctx, Http, HttpRequestBuilder, HttpResponse, Log, NoConfig, Store};
pub use error::{ApiError, Result};
pub use extension::{Config, Extension, Method, RateLimit, RouteSpec};
pub use extract::{FromRequest, Headers, Json, Path, Query, RequestParts};
pub use handler::Handler;
pub use response::{IntoResponse, Response};
pub use samishisa_macros::schema;

pub mod prelude {
    pub use crate::{
        ApiError, Ctx, Extension, Headers, IntoResponse, Json, Path, Query, RateLimit, Response, Result, schema,
    };
    pub use std::time::Duration;
}

#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde;
    pub use serde_json;
}

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
