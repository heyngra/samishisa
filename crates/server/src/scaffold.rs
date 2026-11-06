use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, bail};

const TEMPLATE_LIB: &str = r#"//! __NAME__: a samishisa extension.
//!
//! Build: `samishisa build` (or `cargo build --release`)
//! Run:   `samishisa run target/wasm32-wasip2/release/__CRATE__.wasm`

use samishisa::prelude::*;

/// Settings from `api.config.json`.
#[schema]
struct Config {
    /// How to greet people.
    #[serde(default = "default_greeting")]
    greeting: String,
}

fn default_greeting() -> String {
    "Hello".into()
}

#[schema]
struct Greeting {
    message: String,
}

fn greet(Path(name): Path<String>, ctx: Ctx<Config>) -> Json<Greeting> {
    Json(Greeting { message: format!("{}, {name}!", ctx.config.greeting) })
}

fn extension() -> Extension<Config> {
    Extension::new("__NAME__")
        .get("/:name", greet)
            .summary("Greet someone")
}

samishisa::export!(extension);
"#;

pub fn new(name: &str, parent: &Path, sdk_path: Option<&Path>) -> anyhow::Result<PathBuf> {
    let valid = name.len() <= 40 && name.starts_with(|c: char| c.is_ascii_lowercase()) && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        bail!("\"{name}\" is not a valid extension name: use lowercase letters, digits and dashes, starting with a letter");
    }
    let dir = parent.join(name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    let crate_name = name.replace('-', "_");
    let sdk = match sdk_path {
        Some(p) => {
            let abs = std::fs::canonicalize(p).with_context(|| format!("--sdk {}", p.display()))?;
            format!("samishisa = {{ path = {:?} }}", abs.display().to_string())
        }
        None => format!("samishisa = \"{}\"", env!("CARGO_PKG_VERSION")),
    };
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::create_dir_all(dir.join(".cargo"))?;
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\ndescription = \"A samishisa extension\"\n\n\
             [lib]\ncrate-type = [\"cdylib\"]\n\n[dependencies]\n{sdk}\n\n\
             [profile.release]\nopt-level = \"s\"\nstrip = true\n"
        ),
    )?;
    std::fs::write(dir.join(".cargo/config.toml"), "# Extensions are WebAssembly components.\n[build]\ntarget = \"wasm32-wasip2\"\n")?;
    std::fs::write(dir.join(".gitignore"), "/target\n")?;
    std::fs::write(dir.join("src/lib.rs"), TEMPLATE_LIB.replace("__NAME__", name).replace("__CRATE__", &crate_name))?;
    Ok(dir)
}

pub fn build(dir: &Path) -> anyhow::Result<PathBuf> {
    let manifest = dir.join("Cargo.toml");
    if !manifest.exists() {
        bail!("{} has no Cargo.toml (run `samishisa new <name>` to start an extension)", dir.display());
    }
    let mut child = Command::new("cargo")
        .args(["build", "--release", "--target", "wasm32-wasip2", "--message-format=json-render-diagnostics", "--manifest-path"])
        .arg(&manifest)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("could not run cargo (is Rust installed? https://rustup.rs)")?;
    let stdout = child.stdout.take().context("cargo's output was not captured")?;
    let out = std::io::read_to_string(stdout)?;
    let status = child.wait()?;
    let manifest_abs = std::fs::canonicalize(&manifest)?;
    let mut wasm = None;
    for line in out.lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match msg["reason"].as_str() {
            Some("compiler-message") => {
                if let Some(r) = msg["message"]["rendered"].as_str() {
                    eprint!("{r}");
                }
            }
            Some("compiler-artifact") if msg["manifest_path"].as_str().map(PathBuf::from).as_deref() == Some(manifest_abs.as_path()) => {
                if let Some(f) = msg["filenames"].as_array().into_iter().flatten().filter_map(|f| f.as_str()).find(|f| f.ends_with(".wasm")) {
                    wasm = Some(PathBuf::from(f));
                }
            }
            _ => {}
        }
    }
    if !status.success() {
        if out.contains("wasm32-wasip2") && out.contains("may not be installed") {
            bail!("the wasm32-wasip2 target is missing: run `rustup target add wasm32-wasip2`");
        }
        bail!("cargo build failed");
    }
    wasm.context("cargo succeeded but produced no .wasm file: is `crate-type = [\"cdylib\"]` set in [lib]?")
}
