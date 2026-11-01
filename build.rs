//! Embeds the Needle speech engine (`.needle/needle.dll`, built by
//! `scripts/build-needle.sh`, or the file named by `NEEDLE_DLL`) if present.
//! Without it the app builds fine; voice input then says the engine is missing.

use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(needle)");
    println!("cargo:rerun-if-env-changed=NEEDLE_DLL");
    let dll = std::env::var_os("NEEDLE_DLL").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join(".needle").join("needle.dll")
    });
    println!("cargo:rerun-if-changed={}", dll.display());
    if dll.is_file() {
        let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("needle.dll");
        std::fs::copy(&dll, &out).expect("copy needle.dll");
        println!("cargo:rustc-cfg=needle");
    } else {
        println!("cargo:warning=speech engine not embedded (run scripts/build-needle.sh); voice input will be unavailable");
    }
}
