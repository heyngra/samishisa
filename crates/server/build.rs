use std::{env, error::Error, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-env-changed=SAMISHISA_HELLO_WASM");
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let out = out_dir.join("hello.wasm");
    let explicit = env::var("SAMISHISA_HELLO_WASM").ok().map(PathBuf::from);
    let guessed = out_dir.ancestors().find(|p| p.join("wasm32-wasip2").is_dir()).map(|t| t.join("wasm32-wasip2/release/hello.wasm"));
    match explicit.or(guessed).filter(|p| p.is_file()) {
        Some(hello) => {
            println!("cargo:rerun-if-changed={}", hello.display());
            fs::copy(&hello, &out).map_err(|e| format!("copying {} into the build: {e}", hello.display()))?;
        }
        None => fs::write(&out, b"")?,
    }
    Ok(())
}
