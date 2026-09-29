use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_x264)");
    println!("cargo:rerun-if-changed=src/platform/x264/adapter.c");
    println!("cargo:rerun-if-env-changed=AMATERASU_X264_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_LIBDIR");
    if env::var_os("CARGO_FEATURE_SOFTWARE_X264").is_none() {
        return;
    }
    let directory = if let Some(directory) = env::var_os("AMATERASU_X264_INCLUDE_DIR") {
        let directory = PathBuf::from(directory);
        assert!(directory.join("x264.h").is_file(), "x264.h is unavailable");
        directory
    } else {
        // Host pkg-config cannot identify target headers during a cross build.
        if env::var("HOST").ok() != env::var("TARGET").ok() {
            println!(
                "cargo:warning=x264 unavailable: cross builds require AMATERASU_X264_INCLUDE_DIR"
            );
            return;
        }
        let Ok(output) = Command::new("pkg-config")
            .args(["--variable=includedir", "x264"])
            .output()
        else {
            println!("cargo:warning=x264 unavailable: pkg-config or development headers missing");
            return;
        };
        if !output.status.success() {
            println!("cargo:warning=x264 unavailable: development headers missing");
            return;
        }
        let directory = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        if !directory.is_absolute() || !directory.join("x264.h").is_file() {
            println!("cargo:warning=x264 unavailable: development headers missing");
            return;
        }
        directory
    };
    println!(
        "cargo:rerun-if-changed={}",
        directory.join("x264.h").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        directory.join("x264_config.h").display()
    );
    let mut build = cc::Build::new();
    build.include(directory);
    build
        .file("src/platform/x264/adapter.c")
        .compile("amaterasu_x264_adapter");
    println!("cargo:rustc-cfg=amaterasu_x264");
}
