use crate::{FromRequest, IntoResponse, RequestParts, Response, Result, extension::RouteSpec};

/// A function that can serve a route: up to 8 [`FromRequest`] arguments in any order, returning anything [`IntoResponse`]. The impl is automatic for such functions and closures.
///
/// ```no_run
/// use samishisa::prelude::*;
/// fn ping() -> &'static str { "pong" }
/// fn whoami(headers: Headers) -> String { headers.get("user-agent").unwrap_or("?").to_owned() }
/// fn count(ctx: Ctx) -> Json<i64> { Json(ctx.store.incr("count", 1)) }
///
/// let ext = Extension::new("demo")
///     .get("/ping", ping)
///     .get("/whoami", whoami)
///     .post("/count", count)
///     .get("/inline", |ctx: Ctx| format!("{:?}", ctx.request_id()));
/// ```
///
/// If the compiler says a function "is not a handler", one of its arguments isn't an extractor, or its return type isn't `IntoResponse`. Closures need their argument types written out.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a samishisa handler",
    note = "handlers take up to 8 extractor arguments (Path, Query, Json, Headers, Ctx<YourConfig>) and return something IntoResponse (Json, Response, String, (), Result<_, ApiError>)",
    note = "closures need their argument types written out: |ctx: Ctx<Config>| ..."
)]
pub trait Handler<C, Args>: 'static {
    #[doc(hidden)]
    fn call(&self, req: &RequestParts<C>) -> Result<Response>;
    #[doc(hidden)]
    fn describe(route: &mut RouteSpec);
}

macro_rules! impl_handler {
    ($($arg:ident),*) => {
        impl<C, F, R, $($arg,)*> Handler<C, ($($arg,)*)> for F
        where
            F: Fn($($arg),*) -> R + 'static,
            R: IntoResponse,
            $($arg: FromRequest<C>,)*
        {
            #[allow(non_snake_case, unused_variables)]
            fn call(&self, req: &RequestParts<C>) -> Result<Response> {
                $(let $arg = $arg::from_request(req)?;)*
                (self)($($arg),*).into_response()
            }
            #[allow(unused_variables)]
            fn describe(route: &mut RouteSpec) {
                $($arg::describe(route);)*
                R::describe_response(route);
            }
        }
    };
}

impl_handler!();
impl_handler!(A1);
impl_handler!(A1, A2);
impl_handler!(A1, A2, A3);
impl_handler!(A1, A2, A3, A4);
impl_handler!(A1, A2, A3, A4, A5);
impl_handler!(A1, A2, A3, A4, A5, A6);
impl_handler!(A1, A2, A3, A4, A5, A6, A7);
impl_handler!(A1, A2, A3, A4, A5, A6, A7, A8);
