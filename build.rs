use std::{env, path::PathBuf, process::Command};

fn x264_build_version(header: &str) -> Option<u32> {
    header.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        if words.next()? != "#define" || words.next()? != "X264_BUILD" {
            return None;
        }
        words.next()?.parse().ok()
    })
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_x264)");
    println!("cargo:rerun-if-changed=src/platform/x264/adapter.c");
    println!("cargo:rerun-if-env-changed=AMATERASU_X264_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_LIBDIR");
    if env::var_os("CARGO_FEATURE_SOFTWARE_X264").is_none() {
        return;
    }
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        println!("cargo:warning=x264 software encoding is supported only on Linux");
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
    let header = std::fs::read_to_string(directory.join("x264.h"))
        .expect("x264.h became unreadable after header discovery");
    let x264_build = x264_build_version(&header)
        .filter(|version| *version > 0)
        .expect("x264.h does not declare a numeric X264_BUILD");
    let marker_source = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is unset"))
        .join("x264_build_marker.c");
    std::fs::write(
        &marker_source,
        format!(
            "__attribute__((used)) const char amaterasu_x264_build_marker[] = \"AMATERASU_X264_BUILD={x264_build}\";\n"
        ),
    )
    .expect("could not write the x264 build marker source");
    let mut build = cc::Build::new();
    build.include(directory);
    build
        .file("src/platform/x264/adapter.c")
        .file(&marker_source)
        .compile("amaterasu_x264_adapter");
    // The marker is intentionally not referenced by runtime code. Root its
    // object in the final Linux cdylib so release preflight can read the exact
    // ABI build from the encoder artifact it is about to sign.
    println!("cargo:rustc-link-arg-cdylib=-Wl,--undefined=amaterasu_x264_build_marker");
    println!("cargo:rustc-cfg=amaterasu_x264");
}
