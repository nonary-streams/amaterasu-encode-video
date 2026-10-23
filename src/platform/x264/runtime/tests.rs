use super::*;

#[test]
fn missing_runtime_library_is_explicitly_unavailable() {
    // SAFETY: the adapter returns only its compile-time x264 ABI version.
    let version = unsafe { amaterasu_x264_build() };
    assert!(matches!(
        CodecLibrary::load_names(version, vec!["amaterasu-x264-does-not-exist-92831".into()]),
        Err("x264_runtime_unavailable")
    ));
}

#[cfg(target_os = "linux")]
#[test]
fn library_without_x264_abi_is_rejected() {
    // SAFETY: the adapter returns only its compile-time x264 ABI version.
    let version = unsafe { amaterasu_x264_build() };
    assert!(matches!(
        CodecLibrary::load_names(version, vec!["libc.so.6".into()]),
        Err("x264_runtime_unavailable")
    ));
}

fn configuration() -> Configuration {
    serde_json::from_value(serde_json::json!({
        "width": 128,
        "height": 96,
        "fps": 60,
        "bitrate": 1_000_000,
        "max_reference_frames": 1
    }))
    .unwrap()
}

fn synthetic_frame(index: usize) -> Vec<u8> {
    let mut bytes = vec![128; 128 * 96 * 3 / 2];
    for row in 0..96 {
        for column in 0..128 {
            bytes[row * 128 + column] = 16 + ((column * 11 + row * 7 + index * 19) % 220) as u8;
        }
    }
    bytes
}

#[test]
#[ignore = "requires matching system x264 runtime"]
fn software_encodes_owned_nv12_frames_with_timestamps_forced_idr_and_drain() {
    let config = configuration();
    let mut encoder = Encoder::open(&identity(), &config).unwrap();
    let mut packets = Vec::new();
    for index in 0..20 {
        encoder
            .submit(
                synthetic_frame(index),
                (index as i64 + 1) * 166_670,
                index == 7,
            )
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
            .submit(synthetic_frame(0), 4_000_000, false)
            .is_err()
    );
}

#[test]
#[ignore = "requires matching system x264 runtime"]
fn software_rejects_invalid_identity_and_configuration_before_encoding() {
    let mut changed = identity();
    changed.friendly_name = "different".into();
    assert!(matches!(
        Encoder::open(&changed, &configuration()),
        Err("encoder_identity_changed")
    ));

    let mut invalid = configuration();
    invalid.width = 0;
    assert!(matches!(
        Encoder::open(&identity(), &invalid),
        Err("invalid_configuration")
    ));
}
