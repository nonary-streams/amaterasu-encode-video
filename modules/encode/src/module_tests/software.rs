use super::probe_support::*;
use super::*;
use std::{
    io::Write,
    process::{Command as ProcessCommand, Stdio},
};

fn independently_decode(packet: &CapturedPacket, expected: &crate::model::Configuration) {
    let mut decoder = ProcessCommand::new("ffmpeg")
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
        .expect("this qualification requires an independent FFmpeg decoder");
    decoder
        .stdin
        .take()
        .unwrap()
        .write_all(&packet.bytes)
        .unwrap();
    let output = decoder.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pixels = expected.width as usize * expected.height as usize;
    assert_eq!(output.stdout.len(), pixels * 3 / 2);
    assert!(
        output.stdout[..pixels]
            .iter()
            .all(|value| value.abs_diff(16) <= 2)
    );
    assert!(
        output.stdout[pixels..]
            .iter()
            .all(|value| value.abs_diff(128) <= 2)
    );
}

#[test]
#[ignore = "requires matching system x264 runtime and independent FFmpeg decoder"]
fn registered_fault_factory_then_native_x264_probe_decodes_and_normal_open_uses_registered_abi() {
    let (host, mut module) = Host::start();
    let (observations, software) = AcceptanceFactory::install(&module, SoftwareMode::Native);
    let config = configuration();
    assert_eq!(config.software_preset(), "superfast");
    assert_eq!(config.software_tune(), "zerolatency");
    let enumeration = host.request(&mut module, json!({"action":"enumerate"}));
    assert_eq!(enumeration["ok"], true);
    assert_eq!(enumeration["encoders"][0]["hardware"], true);
    assert_eq!(
        enumeration["encoders"][0]["identity"],
        json!(hardware_identity())
    );
    assert_eq!(enumeration["encoders"][1]["hardware"], false);
    assert_eq!(enumeration["encoders"][1]["identity"], json!(software));

    let explicit = host.request(&mut module, open_request(&hardware_identity(), &config));
    assert_eq!(explicit, json!({"ok":false,"code":"encoder_open_failed"}));
    assert_no_session(&module);
    assert_eq!(observations.lock().unwrap().attempts.len(), 1);

    let mut winner = None;
    for description in enumeration["encoders"].as_array().unwrap() {
        let candidate = serde_json::from_value(description["identity"].clone()).unwrap();
        let reply = host.request(&mut module, probe_request(&candidate, &config));
        let usable = candidate == software;
        assert_probe_reply(&reply, &candidate, &config, usable);
        assert_no_session(&module);
        if usable {
            winner = Some(candidate);
            break;
        }
        assert_eq!(reply["reason"], "encoder_open_failed");
        assert_eq!(reply["code"], "encoder_unusable");
    }
    let winner = winner.unwrap();
    assert_eq!(json!(winner), json!(software));
    let probe_packet = {
        let observations = observations.lock().unwrap();
        assert_eq!(observations.enumerations, 1);
        assert_eq!(observations.attempts.len(), 3);
        assert_eq!(
            json!(observations.attempts[0].identity),
            json!(hardware_identity())
        );
        assert_eq!(
            json!(observations.attempts[1].identity),
            json!(hardware_identity())
        );
        assert_eq!(json!(observations.attempts[2].identity), json!(software));
        assert!(
            observations.attempts[1..]
                .iter()
                .all(|attempt| attempt.shared_lock_available)
        );
        for attempt in &observations.attempts {
            assert_eq!(json!(attempt.configuration), json!(config));
        }
        assert_eq!(observations.submitted, 1);
        assert_eq!(observations.drained, 1);
        assert_eq!(
            observations.closed, 1,
            "probe worker must close before the winner opens"
        );
        assert_eq!(observations.packets.len(), 1);
        let packet = observations.packets[0].clone();
        assert!(packet.keyframe);
        assert_eq!(packet.timestamp, 1_000_000);
        packet
    };
    independently_decode(&probe_packet, &config);

    let opened = host.request(&mut module, open_request(&winner, &config));
    assert_eq!(opened["ok"], true, "{opened}");
    assert_eq!(opened["session_id"], 1);
    assert_eq!(opened["configuration"], json!(config));
    assert_eq!(observations.lock().unwrap().attempts.len(), 4);
    assert_eq!(observations.lock().unwrap().closed, 1);
    exercise_native_winner(&module, &config);
    let closed = host.request(&mut module, json!({"action":"close","session_id":1}));
    assert_eq!(closed["ok"], true);
    assert_eq!(observations.lock().unwrap().closed, 2);
}

unsafe extern "C" fn collect(context: *mut c_void, packet: *const EncodedPacket) -> Status {
    // SAFETY: exercise_native_winner owns the callback vector and the encoder
    // borrows a validated packet and its bytes for this synchronous callback.
    let packets = unsafe { &mut *context.cast::<Vec<(Vec<u8>, u32)>>() };
    let packet = unsafe { &*packet };
    packets.push((unsafe { packet.bytes().unwrap() }.to_vec(), packet.flags));
    STATUS_OK
}

fn exercise_native_winner(module: &EncodeModule, config: &crate::model::Configuration) {
    let query = NativeInterfaceQuery {
        header: AbiHeader::of::<NativeInterfaceQuery>(),
        interface_major: 1,
        min_minor: 0,
        min_size: size_of::<EncoderApi>() as u32,
    };
    let mut output = std::ptr::null();
    let factory = &module._registration.factory;
    // SAFETY: the module retains its registered factory, query and output remain
    // live, and this function releases the returned binding before returning.
    unsafe {
        assert_eq!(
            factory.bind.unwrap()(
                factory.context,
                BorrowedBytes::new(b"test.video"),
                &query,
                &mut output
            ),
            STATUS_OK
        );
        let api = &*output.cast::<EncoderApi>();
        let pixels = config.width as usize * config.height as usize;
        let mut bytes = vec![128; pixels * 3 / 2];
        bytes[..pixels].fill(16);
        let frame = CpuFrame {
            width: config.width,
            height: config.height,
            format: PIXEL_NV12,
            stride: config.width,
            data: bytes.as_ptr(),
            len: bytes.len(),
            timestamp_us: 200_000,
            sequence: 1,
        };
        assert_eq!((api.submit)(api.base.context, 1, &frame, 1), STATUS_OK);
        assert_eq!((api.drain)(api.base.context, 1), STATUS_OK);
        let mut packets = Vec::<(Vec<u8>, u32)>::new();
        let sink = PacketSink {
            context: (&mut packets as *mut Vec<(Vec<u8>, u32)>).cast(),
            write: collect,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let receive_ready = || loop {
            let status = (api.receive)(api.base.context, 1, sink);
            if status != MEDIA_PENDING {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "native encode receive timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(receive_ready(), STATUS_OK);
        assert_eq!(receive_ready(), STATUS_OK);
        assert_eq!((api.receive)(api.base.context, 1, sink), MEDIA_PENDING);
        api.base.release.unwrap()(api.base.context);
        assert_eq!(packets.len(), 2);
        assert!(!packets[0].0.is_empty());
        assert_eq!(packets[0].1, FLAG_KEYFRAME | FLAG_END_OF_ACCESS_UNIT);
        assert!(packets[1].0.is_empty());
        assert_eq!(packets[1].1, FLAG_END_OF_STREAM);
    }
}
