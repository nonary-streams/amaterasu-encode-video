use super::*;
use std::{
    io::Write,
    process::{Command, Stdio},
};

unsafe extern "C" {
    fn amaterasu_x264_threading(
        encoder: *mut std::ffi::c_void,
        threads: *mut i32,
        sliced_threads: *mut i32,
        slices: *mut i32,
    ) -> i32;
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

fn h264_slice_count(bytes: &[u8]) -> usize {
    let mut nal_units = Vec::new();
    let mut offset = 0;
    while offset + 3 < bytes.len() {
        let start_code_length = if bytes[offset..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[offset..].starts_with(&[0, 0, 1]) {
            3
        } else {
            offset += 1;
            continue;
        };
        let nal_start = offset + start_code_length;
        let next_start = (nal_start..bytes.len().saturating_sub(2)).find(|index| {
            bytes[*index..].starts_with(&[0, 0, 1]) || bytes[*index..].starts_with(&[0, 0, 0, 1])
        });
        let nal_end = next_start.unwrap_or(bytes.len());
        if nal_start < nal_end {
            nal_units.push(bytes[nal_start] & 0x1f);
        }
        offset = nal_end;
    }
    nal_units
        .into_iter()
        .filter(|nal_type| matches!(nal_type, 1 | 5))
        .count()
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
    let mut configuration = configuration(None, None);
    configuration.max_reference_frames = 3;
    let packets = encoded_stream(configuration);
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
fn software_emits_the_maximum_of_client_and_minimum_slice_counts() {
    for (client_slices, min_threads, expected_slices) in
        [(1, 1, 1), (1, 3, 3), (4, 3, 4), (4, -9, 4)]
    {
        let mut config = configuration(None, None);
        config.width = 640;
        config.height = 480;
        config.slices_per_frame = client_slices;
        config.min_threads = min_threads;
        let packets = encoded_stream(config);
        for packet in packets {
            assert_eq!(h264_slice_count(&packet.bytes), expected_slices);
        }
        let mut config = configuration(None, None);
        config.width = 640;
        config.height = 480;
        config.slices_per_frame = client_slices;
        config.min_threads = min_threads;
        let encoder = Encoder::open(&identity(), &config).unwrap();
        let (mut threads, mut sliced_threads, mut slices) = (0, 0, 0);
        // SAFETY: encoder owns a live adapter handle and all output pointers are valid.
        assert_eq!(
            unsafe {
                amaterasu_x264_threading(
                    encoder.native.as_ptr(),
                    &mut threads,
                    &mut sliced_threads,
                    &mut slices,
                )
            },
            0
        );
        assert_eq!(threads, expected_slices as i32);
        assert_eq!(sliced_threads, i32::from(expected_slices > 1));
        assert_eq!(slices, expected_slices as i32);
    }
}
