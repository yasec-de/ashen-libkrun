use std::env;
use std::path::PathBuf;

/// Link against libkrun.
///
/// A `make install`ed libkrun is found through pkg-config. Falling back to the
/// workspace target directory covers the usual case here, where libkrun was
/// just built out of this same tree and pkg-config knows nothing about it. The
/// rpath is what lets the binary run on the target after being copied over
/// alongside libkrun.so, without an LD_LIBRARY_PATH in the way.
fn main() {
    if pkg_config::probe_library("libkrun").is_ok() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let profile = env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());

    // Cross builds put artifacts under target/<triple>/<profile>.
    let host = env::var("HOST").unwrap_or_default();
    let target = env::var("TARGET").unwrap_or_default();
    let workspace_target = manifest_dir.join("../../target");
    let lib_dir = if target.is_empty() || target == host {
        workspace_target.join(&profile)
    } else {
        workspace_target.join(&target).join(&profile)
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=krun");
    // $ORIGIN first, so a binary shipped next to libkrun.so just works.
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}
