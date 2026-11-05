use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn schema(args: TokenStream, item: TokenStream) -> TokenStream {
    let args = args.to_string();
    let derives = match args.trim() {
        "" => "samishisa::__private::serde::Serialize, samishisa::__private::serde::Deserialize,",
        "input" => "samishisa::__private::serde::Deserialize,",
        "output" => "samishisa::__private::serde::Serialize,",
        other => {
            return format!(r##"compile_error!("#[schema] takes no argument, `input` or `output` (got `{}`)");"##, other.escape_default())
                .parse()
                .unwrap();
        }
    };
    let prefix = format!(
        "#[derive({derives} samishisa::__private::schemars::JsonSchema)] \
         #[serde(crate = \"samishisa::__private::serde\")] \
         #[schemars(crate = \"samishisa::__private::schemars\")]"
    );
    let mut out: TokenStream = prefix.parse().expect("valid attribute tokens");
    out.extend(item);
    out
}
