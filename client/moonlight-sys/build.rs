//! Compiles the vendored `moonlight-common-c` (+ its `enet` and `nanors` submodule sources,
//! vendored flat under `vendor/moonlight-common-c` since this repo doesn't otherwise use git
//! submodules) and generates Rust bindings for its public API (`src/Limelight.h`).
//!
//! `Limelight.h` itself only depends on `<stdint.h>`/`<stdbool.h>` (confirmed by inspection) —
//! it deliberately doesn't leak `enet` or platform socket types into the public API, so bindgen
//! only ever needs to see that one header.
//!
//! Linux/glibc is this client's only target, so the `enet` `HAS_*` feature macros (which
//! upstream CMake detects via `check_function_exists`) are hardcoded true here rather than
//! reimplementing autoconf-style detection — all of them are always true on a normal glibc Linux
//! system.

use std::path::PathBuf;

fn main() {
    let vendor = PathBuf::from("vendor/moonlight-common-c");
    let src = vendor.join("src");
    let enet = vendor.join("enet");
    let nanors = vendor.join("nanors");

    let mut build = cc::Build::new();
    build
        .include(&src)
        .include(enet.join("include"))
        .include(&nanors)
        .include(nanors.join("deps"))
        .include(nanors.join("deps/obl"))
        .define("HAS_SOCKLEN_T", None)
        .define("NDEBUG", None)
        .warnings(false);

    for entry in std::fs::read_dir(&src).expect("reading moonlight-common-c src dir") {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "c") {
            build.file(path);
        }
    }

    for name in ["callbacks.c", "compress.c", "host.c", "list.c", "packet.c", "peer.c", "protocol.c", "unix.c"] {
        build.file(enet.join(name));
    }
    build
        .define("HAS_FCNTL", "1")
        .define("HAS_IOCTL", "1")
        .define("HAS_POLL", "1")
        .define("HAS_GETADDRINFO", "1")
        .define("HAS_GETNAMEINFO", "1")
        .define("HAS_GETHOSTBYNAME_R", "1")
        .define("HAS_GETHOSTBYADDR_R", "1")
        .define("HAS_INET_PTON", "1")
        .define("HAS_INET_NTOP", "1")
        .define("HAS_MSGHDR_FLAGS", "1");

    build.file(nanors.join("rs.c"));
    build.file(nanors.join("deps/obl/oblas_common.c"));
    build.file(nanors.join("deps/obl/oblas_lite.c"));

    build.compile("moonlight-common-c");

    println!("cargo:rustc-link-lib=crypto");
    println!("cargo:rustc-link-lib=ssl");

    // This system has no unversioned `libclang.so`/resource-dir symlink (only versioned
    // `/usr/lib/clang/<N>`), which leaves libclang unable to find its own freestanding headers
    // (stdbool.h etc.) unless told explicitly where to look.
    let resource_dir = ["/usr/lib/clang/22", "/usr/lib/clang/19"]
        .into_iter()
        .find(|p| std::path::Path::new(p).join("include/stdbool.h").exists());

    let mut bindgen_builder = bindgen::Builder::default()
        .header(src.join("Limelight.h").to_str().unwrap())
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    if let Some(dir) = resource_dir {
        bindgen_builder = bindgen_builder.clang_arg(format!("-resource-dir={dir}"));
    }
    let bindings = bindgen_builder.generate().expect("generating bindgen bindings for Limelight.h");

    let out_path = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("writing bindings.rs");

    println!("cargo:rerun-if-changed={}", src.join("Limelight.h").display());
}
