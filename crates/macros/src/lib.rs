//! Procedural macros for [`samishisa`](https://docs.rs/samishisa). Use them through that crate.
//! No dependencies on purpose: the macros only add attributes, so extension builds stay fast.

use proc_macro::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};

/// Emits a `compile_error!` with the given message.
fn compile_error(message: &str) -> TokenStream {
    [
        TokenTree::Ident(Ident::new("compile_error", Span::call_site())),
        TokenTree::Punct(Punct::new('!', Spacing::Alone)),
        TokenTree::Group(Group::new(Delimiter::Parenthesis, TokenTree::Literal(Literal::string(message)).into())),
        TokenTree::Punct(Punct::new(';', Spacing::Alone)),
    ]
    .into_iter()
    .collect()
}

/// Makes a struct or enum usable as route input, route output and config. Derives `Serialize`, `Deserialize` and `JsonSchema` through the copies `samishisa` re-exports, so you don't need `serde` or `schemars` yourself.
///
/// ```ignore
/// use samishisa::prelude::*;
///
/// /// Query string of `GET /hello/:name`.
/// #[schema]
/// struct GreetQuery {
///     /// Uppercase the greeting.
///     #[serde(default)]
///     shout: bool,
/// }
/// ```
///
/// Doc comments become descriptions in the generated API docs. `#[serde(...)]` attributes work and show up in the schema.
///
/// Use `#[schema(input)]` to derive only `Deserialize` and `JsonSchema`, or `#[schema(output)]` for only `Serialize` and `JsonSchema`.
#[proc_macro_attribute]
pub fn schema(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = args.to_string();
    let derives = match args.trim() {
        "" => "samishisa::__private::serde::Serialize, samishisa::__private::serde::Deserialize,",
        "input" => "samishisa::__private::serde::Deserialize,",
        "output" => "samishisa::__private::serde::Serialize,",
        other => return compile_error(&format!("#[schema] takes no argument, `input` or `output` (got `{other}`)")),
    };
    let prefix = format!(
        "#[derive({derives} samishisa::__private::schemars::JsonSchema)] \
         #[serde(crate = \"samishisa::__private::serde\")] \
         #[schemars(crate = \"samishisa::__private::schemars\")]"
    );
    let mut out: TokenStream = match prefix.parse() {
        Ok(attributes) => attributes,
        Err(e) => return compile_error(&format!("samishisa: #[schema] produced invalid attributes ({e}); please report this")),
    };
    out.extend(item);
    out
}
