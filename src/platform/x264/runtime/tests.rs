use super::*;
use std::{
    io::Write,
    process::{Command, Stdio},
};

unsafe extern "C" {
    fn amaterasu_x264_thread_count(encoder: *mut std::ffi::c_void) -> i32;
}

#[test]
fn missing_runtime_library_is_explicitly_unavailable() {
    // SAFETY: adapter returns only its compile-time header ABI version.
    let version = unsafe { amaterasu_x264_build() };
    assert!(matches!(
        CodecLibrary::load_names(version, vec!["amaterasu-x264-does-not-exist-92831".into()]),
        Err("x264_runtime_unavailable")
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn library_without_x264_abi_is_rejected() {
    // SAFETY: adapter returns only its compile-time header ABI version.
    let version = unsafe { amaterasu_x264_build() };
    assert!(matches!(
        CodecLibrary::load_names(version, vec!["libc.so.6".into()]),
        Err("x264_runtime_unavailable")
    ));
}

fn configuration(preset: Option<&str>, tune: Option<&str>) -> Configuration {
    serde_json::from_value(serde_json::json!({
        "width":128,"height":96,"fps":60,"bitrate":1_000_000,
        "max_reference_frames":1,"sw_preset":preset,"sw_tune":tune
    }))
    .unwrap()
}

fn synthetic_frame(index: usize, width: usize, height: usize) -> Vec<u8> {
    let mut bytes = vec![128; width * height * 3 / 2];
    for row in 0..height {
        for column in 0..width {
            bytes[row * width + column] = 16 + ((column * 11 + row * 7 + index * 19) % 220) as u8;
        }
    }
    bytes
}

fn encoded_stream(config: Configuration) -> Vec<Packet> {
    let mut encoder = Encoder::open(&identity(), &config).unwrap();
    let mut packets = Vec::new();
    for index in 0..20 {
        let bytes = synthetic_frame(index, config.width as usize, config.height as usize);
        encoder
            .submit(bytes, (index as i64 + 1) * 166_670, index == 7)
            .unwrap();
        while let Some(packet) = encoder.receive().unwrap() {
            packets.push(packet);
        }
    }
    encoder.drain().unwrap();
    while let Some(packet) = encoder.receive().unwrap() {
        packets.push(packet);
    }
    assert_eq!(packets.len(), 20);
    for (index, packet) in packets.iter().enumerate() {
        assert_eq!(packet.timestamp_100ns, (index as i64 + 1) * 166_670);
        assert_eq!(packet.keyframe, index == 0 || index == 7);
        assert!(packet.bytes.starts_with(&[0, 0, 0, 1]));
    }
    assert!(encoder.receive().unwrap().is_none());
    assert!(
        encoder
            .submit(synthetic_frame(0, 128, 96), 4_000_000, false)
            .is_err()
    );
    packets
}

fn independently_decode(packets: &[Packet]) -> Vec<u8> {
    let mut decoder = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-threads",
            "1",
            "-f",
            "h264",
            "-i",
            "pipe:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "nv12",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("independent FFmpeg decoder must be installed for qualification");
    let mut input = decoder.stdin.take().unwrap();
    let bytes: Vec<u8> = packets
        .iter()
        .flat_map(|packet| packet.bytes.iter().copied())
        .collect();
    let writer = std::thread::spawn(move || input.write_all(&bytes).unwrap());
    let output = decoder.wait_with_output().unwrap();
    writer.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.len(), 20 * 128 * 96 * 3 / 2);
    output.stdout
}

#[test]
#[ignore = "requires matching system x264 runtime and independent FFmpeg decoder"]
fn software_encodes_real_nv12_with_timestamps_forced_idr_and_independent_decode() {
    let packets = encoded_stream(configuration(None, None));
    let decoded = independently_decode(&packets);
    let original = synthetic_frame(0, 128, 96);
    let error: usize = decoded[..original.len()]
        .iter()
        .zip(&original)
        .map(|(actual, expected)| usize::from(actual.abs_diff(*expected)))
        .sum();
    assert!(
        error / original.len() < 25,
        "decoded picture differs excessively from source"
    );
}

#[test]
#[ignore = "requires matching system x264 runtime and independent FFmpeg decoder"]
fn software_preset_and_tune_change_real_decodable_output() {
    let default = encoded_stream(configuration(None, None));
    let preset = encoded_stream(configuration(Some("ultrafast"), Some("zerolatency")));
    let tune = encoded_stream(configuration(Some("superfast"), Some("grain,zerolatency")));
    let delayed = encoded_stream(configuration(Some("medium"), Some("film")));
    let payload = |packets: &[Packet]| {
        packets
            .iter()
            .flat_map(|packet| packet.bytes.iter().copied())
            .collect::<Vec<_>>()
    };
    assert_ne!(payload(&default), payload(&preset));
    assert_ne!(payload(&default), payload(&tune));
    let default_decoded = independently_decode(&default);
    assert_ne!(default_decoded, independently_decode(&preset));
    assert_ne!(default_decoded, independently_decode(&tune));
    independently_decode(&delayed);
}

#[test]
#[ignore = "requires matching system x264 runtime"]
fn software_rejects_invalid_codec_options_and_changed_identity() {
    assert!(matches!(
        Encoder::open(&identity(), &configuration(Some("invalid-preset"), None)),
        Err("invalid_software_configuration")
    ));
    assert!(matches!(
        Encoder::open(&identity(), &configuration(None, Some("invalid-tune"))),
        Err("invalid_software_configuration")
    ));
    let mut changed = identity();
    changed.friendly_name = "different".into();
    assert!(matches!(
        Encoder::open(&changed, &configuration(None, None)),
        Err("encoder_identity_changed")
    ));
}

#[test]
#[ignore = "requires matching system x264 runtime"]
fn software_uses_requested_effective_x264_thread_count() {
    for min_threads in [1, 3, 256] {
        let mut config = configuration(None, None);
        config.width = 640;
        config.height = 480;
        config.min_threads = min_threads;
        let encoder = Encoder::open(&identity(), &config).unwrap();
        // SAFETY: the live Encoder owns a native x264 encoder using this adapter.
        let effective = unsafe { amaterasu_x264_thread_count(encoder.native.as_ptr()) };
        if min_threads <= 3 {
            assert_eq!(effective, min_threads as i32);
        } else {
            // x264 caps the effective frame-thread count for the stream's
            // dimensions and runtime. The configured value is an upper-bound
            // request, not a promise that the backend allocates that many.
            assert!(effective > 0 && effective <= min_threads as i32);
        }
    }
}
