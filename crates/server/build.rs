use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=SAMISHISA_HELLO_WASM");
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("hello.wasm");
    let explicit = env::var("SAMISHISA_HELLO_WASM").ok().map(PathBuf::from);
    let guessed = PathBuf::from(env::var("OUT_DIR").unwrap())
        .ancestors()
        .find(|p| p.join("wasm32-wasip2").is_dir())
        .map(|t| t.join("wasm32-wasip2/release/hello.wasm"));
    let found = explicit.or(guessed).filter(|p| p.is_file());
    match &found {
        Some(p) => {
            println!("cargo:rerun-if-changed={}", p.display());
            fs::copy(p, &out).expect("copying hello.wasm");
        }
        None => fs::write(&out, b"").expect("writing empty builtin"),
    }
}
