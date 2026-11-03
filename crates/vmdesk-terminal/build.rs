//! Link libghostty-vt, Ghostty's terminal core, and generate its Rust bindings.
//!
//! The library is found with pkg-config, statically when `libghostty-vt-static.pc` is
//! installed, so the binary does not need it at run time. `~/.local/share/pkgconfig` is
//! searched too, which is where `zig build -Demit-lib-vt` installs it with
//! `--prefix ~/.local`. Set `PKG_CONFIG_PATH` to use another installation.
//!
//! This mirrors the build used by Rex's GPUI client: one pinned libghostty-vt build, found
//! rather than compiled here, so the app does not need the Ghostty source or Zig to build.

#![allow(unsafe_code)] // `env::set_var` in a build script, which is single-threaded.

use std::env;
use std::path::PathBuf;

fn main() {
    if let Some(home) = env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/share/pkgconfig");
        let path = match env::var_os("PKG_CONFIG_PATH") {
            Some(existing) => {
                let mut paths: Vec<PathBuf> = env::split_paths(&existing).collect();
                paths.push(local);
                env::join_paths(paths).unwrap()
            }
            None => local.into_os_string(),
        };
        // SAFETY: build scripts are single-threaded.
        unsafe { env::set_var("PKG_CONFIG_PATH", path) };
    }
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    // Find the headers and library directory, but link the static archive ourselves: the
    // static .pc file names the archive by path, which the pkg-config crate doesn't handle.
    let library = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("libghostty-vt")
        .expect("libghostty-vt not found; build it from Ghostty's source (see README)");
    let static_lib = library
        .link_paths
        .iter()
        .find(|dir| dir.join("libghostty-vt.a").exists());
    match static_lib {
        Some(dir) => {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static=ghostty-vt");
            // Relink when libghostty-vt is rebuilt.
            println!(
                "cargo:rerun-if-changed={}",
                dir.join("libghostty-vt.a").display()
            );
        }
        None => {
            for dir in &library.link_paths {
                println!("cargo:rustc-link-search=native={}", dir.display());
            }
            println!("cargo:rustc-link-lib=ghostty-vt");
        }
    }

    let mut builder = bindgen::Builder::default()
        .header_contents("wrapper.h", "#include <ghostty/vt.h>\n")
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_var("GHOSTTY_.*")
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for dir in &library.include_paths {
        builder = builder.clang_arg(format!("-I{}", dir.display()));
        // Regenerate the bindings when the headers change.
        println!(
            "cargo:rerun-if-changed={}",
            dir.join("ghostty/vt.h").display()
        );
    }
    let bindings = builder
        .generate()
        .expect("generating libghostty-vt bindings");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ghostty_vt.rs");
    bindings.write_to_file(out).expect("writing bindings");
}
