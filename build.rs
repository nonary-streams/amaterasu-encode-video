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
    let nvenc_abi = build_linux_ffmpeg_nvenc();
    let vaapi_abi = build_linux_ffmpeg_vaapi(nvenc_abi.is_none());
    build_linux_x265_hevc();
    if let (Some(nvenc), Some(vaapi)) = (nvenc_abi, vaapi_abi) {
        assert_eq!(
            nvenc, vaapi,
            "FFmpeg backends resolved different target ABIs"
        );
    }
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

fn build_linux_x265_hevc() {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_x265_hevc)");
    println!("cargo:rerun-if-changed=src/platform/x265_hevc.c");
    if env::var_os("CARGO_FEATURE_LINUX_X265_HEVC").is_none() {
        return;
    }
    assert_eq!(
        env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("linux"),
        "linux-x265-hevc is supported only on Linux"
    );
    // Resolve the target ABI through pkg-config rather than borrowing host
    // headers during a cross build. A feature-on build fails if the target
    // encoder is unavailable; no stub is advertised as an encoder.
    let x265 = pkg_config::Config::new()
        .probe("x265")
        .expect("linux-x265-hevc requires target x265 headers and library");
    cc::Build::new()
        .includes(x265.include_paths)
        .file("src/platform/x265_hevc.c")
        .compile("amaterasu_x265_hevc_adapter");
    println!("cargo:rustc-cfg=amaterasu_x265_hevc");
}

fn build_linux_ffmpeg_nvenc() -> Option<(u32, u32)> {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_ffmpeg_nvenc)");
    println!("cargo:rerun-if-changed=src/platform/ffmpeg_nvenc.c");
    env::var_os("CARGO_FEATURE_LINUX_FFMPEG_NVENC")?;
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        println!("cargo:warning=FFmpeg NVENC encoding is supported only on Linux");
        return None;
    }

    // This opt-in backend requires target FFmpeg development headers and
    // loader libraries. pkg-config honors the target sysroot for cross builds.
    let avcodec = pkg_config::Config::new()
        .atleast_version("59")
        .probe("libavcodec")
        .expect("linux-ffmpeg-nvenc requires target libavcodec headers and library");
    let avutil = pkg_config::Config::new()
        .atleast_version("57")
        .probe("libavutil")
        .expect("linux-ffmpeg-nvenc requires target libavutil headers and library");
    let abi_major = |version: &str| -> u32 {
        version
            .split('.')
            .next()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .expect("FFmpeg pkg-config version must start with a positive ABI major")
    };
    let avcodec_major = abi_major(&avcodec.version);
    let avutil_major = abi_major(&avutil.version);
    let mut build = cc::Build::new();
    build
        .includes(avcodec.include_paths)
        .includes(avutil.include_paths)
        .file("src/platform/ffmpeg_nvenc.c");
    add_ffmpeg_abi_marker(&mut build, avcodec_major, avutil_major);
    build.compile("amaterasu_ffmpeg_nvenc_adapter");
    println!("cargo:rustc-cfg=amaterasu_ffmpeg_nvenc");
    Some((avcodec_major, avutil_major))
}

fn build_linux_ffmpeg_vaapi(emit_abi_marker: bool) -> Option<(u32, u32)> {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_ffmpeg_vaapi)");
    println!("cargo:rerun-if-changed=src/platform/ffmpeg_vaapi.c");
    env::var_os("CARGO_FEATURE_LINUX_FFMPEG_VAAPI")?;
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        println!("cargo:warning=FFmpeg VAAPI encoding is supported only on Linux");
        return None;
    }

    // As with NVENC, pkg-config must resolve the target FFmpeg libraries.
    // The FFmpeg VAAPI hwcontext API owns the libva/driver interaction; this
    // adapter does not directly link a system libva implementation.
    let avcodec = pkg_config::Config::new()
        .atleast_version("59")
        .probe("libavcodec")
        .expect("linux-ffmpeg-vaapi requires target libavcodec headers and library");
    let avutil = pkg_config::Config::new()
        .atleast_version("57")
        .probe("libavutil")
        .expect("linux-ffmpeg-vaapi requires target libavutil headers and library");
    let abi_major = |version: &str| -> u32 {
        version
            .split('.')
            .next()
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .expect("FFmpeg pkg-config version must start with a positive ABI major")
    };
    let avcodec_major = abi_major(&avcodec.version);
    let avutil_major = abi_major(&avutil.version);
    let mut build = cc::Build::new();
    build
        .includes(avcodec.include_paths)
        .includes(avutil.include_paths)
        .file("src/platform/ffmpeg_vaapi.c");
    if emit_abi_marker {
        add_ffmpeg_abi_marker(&mut build, avcodec_major, avutil_major);
    }
    build.compile("amaterasu_ffmpeg_vaapi_adapter");
    println!("cargo:rustc-cfg=amaterasu_ffmpeg_vaapi");
    Some((avcodec_major, avutil_major))
}

fn add_ffmpeg_abi_marker(build: &mut cc::Build, avcodec_major: u32, avutil_major: u32) {
    let marker_source = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is unset"))
        .join("ffmpeg_abi_marker.c");
    std::fs::write(
        &marker_source,
        format!(
            "__attribute__((used)) const char amaterasu_ffmpeg_abi_marker[] = \"AMATERASU_FFMPEG_ABI={avcodec_major},{avutil_major}\";\n"
        ),
    )
    .expect("could not write the FFmpeg ABI marker source");
    build.file(&marker_source);
    // Exactly one adapter archive roots the marker when both backends are on.
    println!("cargo:rustc-link-arg-cdylib=-Wl,--undefined=amaterasu_ffmpeg_abi_marker");
}
