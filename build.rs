use std::env;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(amaterasu_swscale)");
    println!("cargo:rerun-if-changed=src/plane/convert.c");
    if env::var_os("CARGO_FEATURE_LINUX_FFMPEG_CONVERT").is_none()
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
    {
        return;
    }
    let scale = pkg_config::Config::new()
        .probe("libswscale")
        .expect("linux-ffmpeg-convert requires target libswscale headers and library");
    let utility = pkg_config::Config::new()
        .probe("libavutil")
        .expect("linux-ffmpeg-convert requires target libavutil headers and library");
    let major = |version: &str| {
        version
            .split('.')
            .next()
            .unwrap()
            .parse::<u32>()
            .expect("invalid FFmpeg ABI major")
    };
    let marker = std::path::PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("swscale_abi.c");
    let declaration = format!(
        "__attribute__((used)) const char amaterasu_swscale_abi_marker[] = \
         \"AMATERASU_SWSCALE_ABI={},{}\";\n",
        major(&scale.version),
        major(&utility.version),
    );
    std::fs::write(&marker, declaration).expect("write FFmpeg conversion ABI marker");
    cc::Build::new()
        .includes(scale.include_paths)
        .includes(utility.include_paths)
        .file("src/plane/convert.c")
        .file(marker)
        .compile("amaterasu_video_convert");
    println!("cargo:rustc-cfg=amaterasu_swscale");
    println!("cargo:rustc-link-arg-cdylib=-Wl,--undefined=amaterasu_swscale_abi_marker");
}
